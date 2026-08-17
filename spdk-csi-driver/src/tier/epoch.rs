//! The volume epoch — L2 step 7 (design review A8).
//!
//! One hub owns a volume's bucket prefix at a time. Ownership is an
//! epoch object in the bucket itself (`<prefix>.flint/epoch`), renewed
//! by CAS heartbeat; the number it carries stamps every publish
//! (`x-amz-meta-flint-epoch`). The state machine:
//!
//! - **Claim** ([`claim`]): create if absent. If the recorded holder is
//!   OUR server_id, a previous incarnation of this hub died holding it —
//!   supersede immediately (**self-recognition**; without it every
//!   routine spot reclaim would wedge the mount behind a manual CAS).
//!   A FOREIGN holder is judged dead only by the store's own evidence:
//!   its token (S3: the epoch object's ETag, which every renewal
//!   rotates) must stay unchanged across `lease_misses` consecutive
//!   polls one heartbeat apart. We never compare the store's clock with
//!   ours — only the store's observations with each other.
//! - **Fence at claim**: every successful claim sweeps
//!   ListMultipartUploads under the data prefix and aborts them ALL.
//!   Any in-flight assembly belonged to a dead or deposed holder; its
//!   eventual CompleteMultipartUpload now fails `NoSuchUpload` — the
//!   deposed hub's publish is fenced by the store itself.
//! - **Heartbeat** ([`spawn_heartbeat`]): periodic CAS renew. A 412
//!   means deposed: fence the [`EpochGuard`] (the flusher refuses every
//!   further publish) and fire the caller's `on_deposed`. Renewals that
//!   keep failing for a full lease window fence the same way — a holder
//!   that cannot prove it still holds must assume it does not.
//!
//! Residual window, documented per A8: a deposed hub's already-started
//! plain PUT is fenced only by the heartbeat interval plus the A6
//! If-Match guard (the successor's first publish rotates the ETag the
//! straggler is guarding on). Handoff for clients: flint-lite's hub is
//! a single StatefulSet pod behind one Service — the successor IS the
//! restarted pod, so the address follows automatically; there is no
//! multi-hub client migration in v1 (runbr: clients pinned to a dead
//! address hang, which is why serve() refuses to start unfenced rather
//! than serving on a stale epoch).

use crate::tier::meter::{self, Counter};
use crate::tier::store::{EpochLease, ObjectStore, StoreError, StoreResult};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// First path component under the key prefix reserved for tier control
/// objects (the epoch; step 12's manifests). `key_for` refuses to tier
/// client files under it so none can shadow a control object.
pub const RESERVED_DIR: &str = ".flint";

/// Where a volume's epoch object lives.
pub fn epoch_key(key_prefix: &str) -> String {
    format!("{}{}/epoch", key_prefix, RESERVED_DIR)
}

#[derive(Debug, Clone)]
pub struct EpochConfig {
    /// The epoch object's bucket key (see [`epoch_key`]).
    pub key: String,
    /// Stable holder identity: the hub's persisted server_id. Survives
    /// restart — that is what self-recognition recognizes.
    pub holder_id: String,
    /// Renew interval; also the claim loop's poll interval.
    pub heartbeat: Duration,
    /// Consecutive unchanged-token observations before a foreign
    /// holder is judged dead (lease TTL ≈ heartbeat × misses).
    pub lease_misses: u32,
}

impl EpochConfig {
    pub fn new(key_prefix: &str, holder_id: String) -> Self {
        EpochConfig {
            key: epoch_key(key_prefix),
            holder_id,
            heartbeat: Duration::from_secs(10),
            lease_misses: 6,
        }
    }
}

/// The fencing flag shared between the heartbeat and the flusher. The
/// flusher re-verifies through this before EVERY publish; the
/// heartbeat fences it the moment a renewal fails.
pub struct EpochGuard {
    epoch: AtomicU64,
    fenced: AtomicBool,
}

impl EpochGuard {
    /// Held at `epoch` (the claim's result).
    pub fn held(epoch: u64) -> Arc<Self> {
        Arc::new(EpochGuard { epoch: AtomicU64::new(epoch), fenced: AtomicBool::new(false) })
    }

    /// The current epoch if still held; `None` once fenced. Publishing
    /// against `None` is forbidden.
    pub fn current(&self) -> Option<u64> {
        if self.fenced.load(Ordering::Relaxed) {
            return None;
        }
        match self.epoch.load(Ordering::Relaxed) {
            0 => None,
            e => Some(e),
        }
    }

    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::Relaxed)
    }

    /// Deposed (or liveness no longer provable): every subsequent
    /// `current()` answers `None`. One-way — a fenced incumbent
    /// re-enters through a fresh [`claim`], never by unfencing.
    pub fn fence(&self) {
        self.fenced.store(true, Ordering::Relaxed);
    }

    /// Tests only: production guards are constructed at [`held`] and
    /// change only by fencing — the epoch number never moves in place.
    #[cfg(test)]
    pub(crate) fn set_held(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::Relaxed);
    }
}

/// Claim the volume epoch, waiting out a foreign holder's lease if one
/// is live. Every path out with a lease has already run the takeover
/// MPU abort-sweep — the fence is part of the claim, not a follow-up
/// the caller can forget.
pub async fn claim(
    store: &Arc<dyn ObjectStore>,
    cfg: &EpochConfig,
    data_prefix: &str,
) -> StoreResult<EpochLease> {
    let mut last_token: Option<String> = None;
    let mut quiet_polls: u32 = 0;
    loop {
        let observed = store.epoch_read(&cfg.key).await?;
        match observed {
            None => match store.epoch_acquire(&cfg.key, &cfg.holder_id, None).await {
                Ok(lease) => {
                    info!("tier epoch: created — {} holds epoch {}", cfg.holder_id, lease.epoch);
                    takeover_sweep(store, data_prefix).await?;
                    return Ok(lease);
                }
                Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                    // Lost the create race; the next read shows the winner.
                    continue;
                }
                Err(e) => return Err(e),
            },
            Some(state) if state.holder_id == cfg.holder_id => {
                // Self-recognition (A8): our previous incarnation died
                // holding the epoch. Supersede immediately — no wait,
                // no operator CAS.
                match store.epoch_acquire(&cfg.key, &cfg.holder_id, Some(&state)).await {
                    Ok(lease) => {
                        info!(
                            "tier epoch: self-recognition — {} resumes at epoch {} \
                             (previous incarnation held {})",
                            cfg.holder_id, lease.epoch, state.epoch
                        );
                        takeover_sweep(store, data_prefix).await?;
                        return Ok(lease);
                    }
                    Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            Some(state) => {
                // Foreign holder: dead only when the store's token has
                // not advanced for lease_misses consecutive polls.
                if last_token.as_deref() == Some(state.token.as_str()) {
                    quiet_polls += 1;
                } else {
                    if last_token.is_some() {
                        warn!(
                            "tier epoch: {} is ALIVE at epoch {} (token advanced) — \
                             this hub waits; two hubs on one prefix is a \
                             misconfiguration unless a handoff is in progress",
                            state.holder_id, state.epoch
                        );
                    } else {
                        warn!(
                            "tier epoch: held by {} at epoch {} — watching its lease \
                             ({}×{:?} before takeover)",
                            state.holder_id, state.epoch, cfg.lease_misses, cfg.heartbeat
                        );
                    }
                    quiet_polls = 0;
                    last_token = Some(state.token.clone());
                }
                if quiet_polls >= cfg.lease_misses {
                    match store.epoch_acquire(&cfg.key, &cfg.holder_id, Some(&state)).await {
                        Ok(lease) => {
                            warn!(
                                "tier epoch: TAKEOVER — {} judged dead ({} quiet polls), \
                                 {} now holds epoch {}",
                                state.holder_id, quiet_polls, cfg.holder_id, lease.epoch
                            );
                            meter::bump(Counter::EpochTakeovers);
                            takeover_sweep(store, data_prefix).await?;
                            return Ok(lease);
                        }
                        Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                            // It moved at the last instant — alive after all.
                            quiet_polls = 0;
                            last_token = None;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                tokio::time::sleep(cfg.heartbeat).await;
            }
        }
    }
}

/// Abort every in-flight multipart assembly under the prefix. After
/// this, a dead/deposed holder's CompleteMultipartUpload fails
/// `NoSuchUpload` — the data-plane teeth of the epoch. An error here
/// fails the claim: holding the epoch WITHOUT the sweep would leave a
/// deposed publish un-fenced, which is the exact bug A8 exists to kill.
async fn takeover_sweep(store: &Arc<dyn ObjectStore>, data_prefix: &str) -> StoreResult<usize> {
    let pending = store.list_uploads(data_prefix).await?;
    for u in &pending {
        store.abort_upload(&u.key, &u.upload_id).await?;
        meter::bump(Counter::TakeoverMpuAborts);
        info!(
            "tier epoch: fenced in-flight assembly {} on {} (its Complete now fails \
             NoSuchUpload)",
            u.upload_id, u.key
        );
    }
    Ok(pending.len())
}

/// Renew the lease every heartbeat until deposed. On deposition (412)
/// or a full lease window of failed renewals: fence the guard, fire
/// `on_deposed`, exit. The task never unfences — recovery is a fresh
/// [`claim`] by a restarted process.
pub fn spawn_heartbeat(
    store: Arc<dyn ObjectStore>,
    cfg: EpochConfig,
    mut lease: EpochLease,
    guard: Arc<EpochGuard>,
    on_deposed: Box<dyn FnOnce() + Send>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut consecutive_failures: u32 = 0;
        loop {
            tokio::time::sleep(cfg.heartbeat).await;
            match store.epoch_renew(&cfg.key, &lease).await {
                Ok(next) => {
                    lease = next;
                    consecutive_failures = 0;
                    meter::bump(Counter::EpochRenews);
                }
                Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                    error!(
                        "tier epoch: DEPOSED — the epoch object no longer carries our \
                         lease (holder {}, epoch {}); fencing all publishes",
                        lease.holder_id, lease.epoch
                    );
                    guard.fence();
                    on_deposed();
                    return;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    meter::bump(Counter::EpochRenewFailures);
                    warn!(
                        "tier epoch: renew failed ({}/{}): {}",
                        consecutive_failures, cfg.lease_misses, e
                    );
                    if consecutive_failures >= cfg.lease_misses {
                        // A successor may now lawfully judge us dead —
                        // we can no longer prove otherwise. Self-fence
                        // (A8: an incumbent that fails its own renewal
                        // stops publishing).
                        error!(
                            "tier epoch: {} consecutive renew failures — a full lease \
                             window has passed; self-fencing (holder {}, epoch {})",
                            consecutive_failures, lease.holder_id, lease.epoch
                        );
                        guard.fence();
                        on_deposed();
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::store::memory::MemoryStore;

    fn stores() -> (Arc<MemoryStore>, Arc<dyn ObjectStore>) {
        let mem = Arc::new(MemoryStore::new());
        let dyn_store: Arc<dyn ObjectStore> = mem.clone();
        (mem, dyn_store)
    }

    fn cfg(holder: &str, heartbeat_ms: u64, misses: u32) -> EpochConfig {
        let mut c = EpochConfig::new("vol/", holder.to_string());
        c.heartbeat = Duration::from_millis(heartbeat_ms);
        c.lease_misses = misses;
        c
    }

    #[tokio::test]
    async fn fresh_claim_creates_the_epoch_and_sweeps_orphans() {
        let (mem, store) = stores();
        // Two orphan assemblies from a crashed previous holder.
        let u1 = mem.raw_begin_upload("vol/a.bin");
        let _u2 = mem.raw_begin_upload("vol/b.bin");
        let lease = claim(&store, &cfg("hub-a", 10, 3), "vol/").await.unwrap();
        assert_eq!(lease.epoch, 1);
        assert_eq!(lease.holder_id, "hub-a");
        assert!(
            store.list_uploads("vol/").await.unwrap().is_empty(),
            "the claim must fence every in-flight assembly"
        );
        assert!(matches!(
            mem.raw_complete_upload(&u1),
            Err(StoreError::NoSuchUpload(_))
        ));
    }

    #[tokio::test]
    async fn restart_resumes_by_self_recognition_without_waiting() {
        let (_mem, store) = stores();
        // heartbeat = 1h, misses = 1000: the foreign-wait path would
        // hang for weeks. Self-recognition must return immediately.
        let c = cfg("hub-a", 3_600_000, 1000);
        let l1 = claim(&store, &c, "vol/").await.unwrap();
        assert_eq!(l1.epoch, 1);
        // "Crash": no release, no heartbeat. Same holder_id restarts.
        let l2 = tokio::time::timeout(Duration::from_secs(5), claim(&store, &c, "vol/"))
            .await
            .expect("self-recognition must not wait out its own lease")
            .unwrap();
        assert_eq!(l2.epoch, 2, "resume supersedes the dead incarnation");
        // The dead incarnation's lease is fenced.
        let err = store.epoch_renew(&c.key, &l1).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn foreign_live_holder_is_not_claimed_until_its_lease_goes_quiet() {
        let (_mem, store) = stores();
        let ca = cfg("hub-a", 50, 3);
        let la = claim(&store, &ca, "vol/").await.unwrap();

        // hub-a renews every 20 ms for 300 ms (virtual), then dies.
        let store_a = Arc::clone(&store);
        let key = ca.key.clone();
        let renewer = tokio::spawn(async move {
            let mut lease = la;
            for _ in 0..15 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                lease = store_a.epoch_renew(&key, &lease).await.unwrap();
            }
        });

        let started = tokio::time::Instant::now();
        let cb = cfg("hub-b", 50, 3);
        let lb = claim(&store, &cb, "vol/").await.unwrap();
        let waited = started.elapsed();
        renewer.await.unwrap();

        assert_eq!(lb.holder_id, "hub-b");
        assert_eq!(lb.epoch, 2);
        assert!(
            waited >= Duration::from_millis(300),
            "hub-b claimed after {:?} — while hub-a was still renewing",
            waited
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deposed_heartbeat_self_fences_and_fires_the_callback() {
        let (_mem, store) = stores();
        let ca = cfg("hub-a", 50, 3);
        let la = claim(&store, &ca, "vol/").await.unwrap();
        let guard = EpochGuard::held(la.epoch);
        assert_eq!(guard.current(), Some(1));

        let deposed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&deposed);
        let hb = spawn_heartbeat(
            Arc::clone(&store),
            ca.clone(),
            la,
            Arc::clone(&guard),
            Box::new(move || flag.store(true, Ordering::SeqCst)),
        );

        // hub-b takes over directly (the takeover judgment is tested
        // above; here we need only the deposition).
        let observed = store.epoch_read(&ca.key).await.unwrap().unwrap();
        let lb = store.epoch_acquire(&ca.key, "hub-b", Some(&observed)).await.unwrap();
        assert_eq!(lb.epoch, 2);

        hb.await.unwrap(); // must exit on its own — deposed
        assert!(guard.is_fenced());
        assert_eq!(guard.current(), None);
        assert!(deposed.load(Ordering::SeqCst));
    }

    /// The A8 drill's second half: true takeover with an in-flight MPU —
    /// the deposed hub's Complete must fail NoSuchUpload.
    #[tokio::test(start_paused = true)]
    async fn takeover_sweep_fences_the_deposed_hubs_complete() {
        let (mem, store) = stores();
        let ca = cfg("hub-a", 10, 2);
        let _la = claim(&store, &ca, "vol/").await.unwrap();
        // hub-a starts an assembly, then goes silent mid-flush.
        let upload = mem.raw_begin_upload("vol/data.bin");

        let lb = claim(&store, &cfg("hub-b", 10, 2), "vol/").await.unwrap();
        assert_eq!(lb.epoch, 2);

        // hub-a wakes up and tries to publish: fenced by the store.
        let err = mem.raw_complete_upload(&upload).unwrap_err();
        assert!(
            matches!(err, StoreError::NoSuchUpload(_)),
            "deposed Complete must fail NoSuchUpload, got {:?}",
            err
        );
    }

    #[test]
    fn epoch_key_lives_under_the_reserved_namespace() {
        assert_eq!(epoch_key("vol1/"), "vol1/.flint/epoch");
        assert_eq!(epoch_key(""), ".flint/epoch");
    }
}
