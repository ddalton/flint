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
            // Kept in step with `config::default_tier_heartbeat` /
            // `default_tier_lease_misses`, where the reasoning lives. A
            // split between the two would give a config-less caller a
            // different lease than the chart's.
            heartbeat: Duration::from_secs(10),
            lease_misses: 6,
        }
    }
}

/// The fencing flag shared between the heartbeat and the flusher. The
/// flusher re-verifies through this before EVERY publish; the
/// heartbeat fences it the moment a renewal fails.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct EpochGuard {
    epoch: AtomicU64,
    fenced: AtomicBool,
    /// WHY `fenced` was set, and the whole reason this flag exists.
    ///
    /// `fenced` alone conflates two opposite situations: "a rival
    /// deposed us, the cell is theirs" and "we closed the barrier on
    /// our own way out". Both must stop publishing, so one flag was
    /// enough — until the clean shutdown started fencing one line
    /// before it calls `release()`. Then `is_fenced()` was true on
    /// every clean exit and the release CAS was never issued at all:
    /// the cell stayed held, and the next hub paid the full
    /// `heartbeat × lease_misses` wait instead of claiming instantly.
    /// That is a silent cost on every identity-changing wake.
    ///
    /// `true` means WE fenced ourselves to shut down cleanly, and the
    /// release is still owed. Deposition never sets it.
    fenced_for_shutdown: AtomicBool,
    /// Unix time of the last successful renew (the claim counts as
    /// one) — the A12 reporter's lease-age gauge. Telemetry only;
    /// liveness decisions stay with the heartbeat/fence machinery.
    last_renew_unix: AtomicU64,
}

impl EpochGuard {
    /// Held at `epoch` (the claim's result).
    pub fn held(epoch: u64) -> Arc<Self> {
        Arc::new(EpochGuard {
            epoch: AtomicU64::new(epoch),
            fenced: AtomicBool::new(false),
            fenced_for_shutdown: AtomicBool::new(false),
            last_renew_unix: AtomicU64::new(now_unix()),
        })
    }

    /// The heartbeat notes each successful CAS renew here.
    pub fn note_renew(&self) {
        self.last_renew_unix.store(now_unix(), Ordering::Relaxed);
    }

    /// Seconds since the last successful renew (telemetry).
    pub fn renew_age_secs(&self) -> u64 {
        now_unix().saturating_sub(self.last_renew_unix.load(Ordering::Relaxed))
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
        self.fenced.store(true, Ordering::Release);
    }

    /// Close the barrier as part of OUR OWN clean shutdown.
    ///
    /// Identical to [`fence`] for every publisher — `current()` answers
    /// `None` from here on — but it records that the fence is ours, so
    /// the heartbeat still issues the release CAS on the way out.
    /// Order matters: the reason is stored before the flag, so anyone
    /// who observes `fenced` also observes why.
    pub fn fence_for_shutdown(&self) {
        self.fenced_for_shutdown.store(true, Ordering::Relaxed);
        self.fenced.store(true, Ordering::Release);
    }

    /// Fenced by something OTHER than our own shutdown — deposed, or
    /// unable to prove liveness. The clean release is suppressed only
    /// for these; a shutdown fence still owes the mark.
    pub fn fenced_by_deposition(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
            && !self.fenced_for_shutdown.load(Ordering::Relaxed)
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
            Some(state)
                if state.holder_id == cfg.holder_id
                    && crate::state_backend::is_single_occupant() =>
            {
                // Self-recognition (A8): our previous incarnation died
                // holding the epoch. Supersede immediately — no wait,
                // no operator CAS.
                //
                // Gated on holding the state directory's occupancy
                // lock, which is what makes "our own id" proof that the
                // previous incarnation is GONE rather than merely
                // unresponsive. Without that gate a second process on
                // the same PVC — the wake-during-drain window — reads
                // the same server_id out of the same database and
                // deposes a hub that is mid-flush, the one split-brain
                // the epoch cannot otherwise fence. Unlocked (memory
                // backend, or a filesystem where flock does not hold)
                // we fall through to the foreign-holder path and wait
                // the lease out, which is slower and always safe.
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
            Some(state) if state.released => {
                // A clean handoff: the previous holder finished its
                // final flush, fenced itself and marked the cell before
                // exiting. There is nothing to wait for — waiting would
                // burn `lease_misses × heartbeat` (a minute by default)
                // proving a hub is dead when it already said so.
                //
                // This is what makes waking a hibernated volume fast:
                // its PVC was deleted, so the woken hub has a NEW
                // server_id and cannot take the self-recognition arm
                // above — without this it would sit through the full
                // foreign-holder timeout on every wake.
                match store.epoch_acquire(&cfg.key, &cfg.holder_id, Some(&state)).await {
                    Ok(lease) => {
                        info!(
                            "tier epoch: {} released cleanly at epoch {} — {} claims \
                             epoch {} with no wait",
                            state.holder_id, state.epoch, cfg.holder_id, lease.epoch
                        );
                        takeover_sweep(store, data_prefix).await?;
                        return Ok(lease);
                    }
                    // Someone else claimed it first; re-read and retry.
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

/// Startup re-verify — FlintTierEpoch's import-route fence. After the
/// claim and before the import/flush machinery acts, confirm the
/// store's epoch object still carries OUR reign: a hub frozen between
/// its claim CAS and this point wakes into a successor's world — the
/// import would ingest the successor's objects (their etags become our
/// rows) and the first flush would If-Match-land OVER the live
/// successor. An epoch ahead of our guard is machine-readable
/// deposition: fence and refuse startup; the restart re-judges and
/// waits behind the live holder. A MISSING epoch object is not
/// deposition (the heartbeat's NotFound arm owns that); a fenced
/// guard refuses outright.
pub async fn startup_reverify(
    store: &Arc<dyn ObjectStore>,
    key: &str,
    guard: &Arc<EpochGuard>,
) -> StoreResult<()> {
    let state = store.epoch_read(key).await?;
    match (state, guard.current()) {
        (Some(s), Some(ours)) if s.epoch > ours => {
            error!(
                "tier epoch: DEPOSED during startup — the store's epoch object is at \
                 {} (holder {}), past our {}; fencing and refusing to serve",
                s.epoch, s.holder_id, ours
            );
            guard.fence();
            Err(StoreError::PreconditionFailed(format!(
                "deposed during startup: store epoch {} (holder {}) is past ours ({})",
                s.epoch, s.holder_id, ours
            )))
        }
        (_, None) => Err(StoreError::PreconditionFailed(
            "guard already fenced at startup re-verify".into(),
        )),
        _ => Ok(()),
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

/// What a shutdown release did to the epoch cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The cell is marked released; a successor claims with no wait.
    Released,
    /// We were deposed before or during the release — the cell belongs
    /// to someone else and was deliberately left untouched.
    LostCas,
    /// The store refused the write; the cell stays held and the next
    /// claimant waits out the lease the slow way.
    Failed,
}

/// A running heartbeat, plus the handle a clean shutdown uses to stop
/// it and release the epoch.
pub struct HeartbeatHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    outcome: tokio::sync::oneshot::Receiver<ReleaseOutcome>,
}

impl HeartbeatHandle {
    /// Stop beating and mark the cell released, then wait for the
    /// answer. Bounded by `timeout`: a shutdown must not hang on a
    /// slow bucket past the pod's termination grace.
    ///
    /// Call this AFTER the final flush and AFTER fencing the guard —
    /// the released mark is a barrier, and anything that publishes
    /// after it lands on a prefix another hub may already own.
    pub async fn release(mut self, timeout: Duration) -> ReleaseOutcome {
        let Some(tx) = self.shutdown.take() else {
            return ReleaseOutcome::Failed;
        };
        if tx.send(()).is_err() {
            // The heartbeat already exited — deposed, most likely.
            return ReleaseOutcome::LostCas;
        }
        match tokio::time::timeout(timeout, &mut self.outcome).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => ReleaseOutcome::Failed,
            Err(_) => {
                warn!("tier epoch: release timed out — cell left held");
                ReleaseOutcome::Failed
            }
        }
    }

    /// Stop the heartbeat WITHOUT releasing (the dirty-shutdown path:
    /// a hub that could not flush must leave the cell held so no
    /// successor claims instantly and serves a stale bucket).
    pub fn abort(self) {
        self.task.abort();
    }
}

/// Renew the lease every heartbeat until deposed or shut down. On
/// deposition (412) or a full lease window of failed renewals: fence
/// the guard, fire `on_deposed`, exit. The task never unfences —
/// recovery is a fresh [`claim`] by a restarted process.
pub fn spawn_heartbeat(
    store: Arc<dyn ObjectStore>,
    cfg: EpochConfig,
    lease: EpochLease,
    guard: Arc<EpochGuard>,
    on_deposed: Box<dyn FnOnce() + Send>,
) -> HeartbeatHandle {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel::<ReleaseOutcome>();
    let task = tokio::spawn(async move {
        let mut lease = lease;
        let mut consecutive_failures: u32 = 0;
        // Cleared when the shutdown channel closes without a signal —
        // i.e. the caller dropped the handle. That is NOT a shutdown
        // request: a dropped oneshot sender resolves the receiver with
        // an error, and treating that as "release the epoch" would
        // hand the volume away the instant anyone stopped holding the
        // handle. The heartbeat keeps beating; only an explicit signal
        // releases.
        let mut shutdown_live = true;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(cfg.heartbeat) => {}
                res = &mut shutdown_rx, if shutdown_live => {
                    if res.is_err() {
                        shutdown_live = false;
                        continue;
                    }
                    // Clean shutdown. The release CAS is issued HERE,
                    // by the task that owns the live token, because the
                    // token rotates on every renewal and no one else
                    // has the current one — a caller CASing with the
                    // claim-time token would 412 on any hub older than
                    // one heartbeat, i.e. always, silently.
                    let outcome = if guard.fenced_by_deposition() {
                        // Deposed: marking the cell now would stamp
                        // `released` on a live successor's reign. NOT
                        // `is_fenced()` — a clean shutdown fences
                        // itself immediately before calling us, so
                        // that test suppressed every release there
                        // has ever been.
                        ReleaseOutcome::LostCas
                    } else {
                        match store.epoch_release(&cfg.key, &lease).await {
                            Ok(()) => {
                                info!(
                                    "tier epoch: released cleanly at epoch {} — the next \
                                     hub claims with no lease wait",
                                    lease.epoch
                                );
                                ReleaseOutcome::Released
                            }
                            Err(StoreError::PreconditionFailed(_))
                            | Err(StoreError::NotFound(_)) => {
                                warn!(
                                    "tier epoch: release lost the CAS — deposed during \
                                     shutdown; leaving the cell to its new holder"
                                );
                                ReleaseOutcome::LostCas
                            }
                            Err(e) => {
                                warn!("tier epoch: release failed: {} — cell left held", e);
                                ReleaseOutcome::Failed
                            }
                        }
                    };
                    let _ = outcome_tx.send(outcome);
                    return;
                }
            }
            match store.epoch_renew(&cfg.key, &lease).await {
                Ok(next) => {
                    lease = next;
                    consecutive_failures = 0;
                    guard.note_renew();
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
    });
    HeartbeatHandle { task, shutdown: Some(shutdown_tx), outcome: outcome_rx }
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

    /// FlintTierEpoch's import-route fence: a claim that completed into
    /// a superseded world (frozen between the CAS and startup) must be
    /// caught by the re-verify — fence + refuse; a store still carrying
    /// our reign (or no epoch object at all) passes.
    #[tokio::test]
    async fn startup_reverify_fences_a_superseded_claim_and_passes_our_reign() {
        let (_mem, store) = stores();
        let key = epoch_key("vol/");
        let lease = claim(&store, &cfg("hub-a", 10, 3), "vol/").await.unwrap();
        let guard = EpochGuard::held(lease.epoch);

        // Our reign intact: passes.
        startup_reverify(&store, &key, &guard).await.expect("own reign passes");

        // A successor supersedes while we were frozen.
        let state = store.epoch_read(&key).await.unwrap().unwrap();
        store.epoch_acquire(&key, "hub-b", Some(&state)).await.unwrap();
        let err = startup_reverify(&store, &key, &guard).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "{}", err);
        assert!(guard.is_fenced(), "the re-verify must fence the guard");
        assert!(guard.current().is_none());
    }

    #[tokio::test]
    async fn startup_reverify_tolerates_a_missing_epoch_object() {
        let (_mem, store) = stores();
        let guard = EpochGuard::held(1);
        // No epoch object at all (foreign deletion): NOT deposition —
        // the heartbeat's NotFound arm owns that lane.
        startup_reverify(&store, &epoch_key("vol/"), &guard)
            .await
            .expect("missing epoch object passes");
        assert!(!guard.is_fenced());
    }

    #[tokio::test]
    async fn restart_resumes_by_self_recognition_without_waiting() {
        let (_mem, store) = stores();
        // heartbeat = 1h, misses = 1000: the foreign-wait path would
        // hang for weeks. Self-recognition must return immediately.
        // Self-recognition is gated on proven single occupancy — in
        // production the state-directory lock; here, the memory store's
        // structurally private state.
        crate::state_backend::declare_private_state();
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

    /// The hibernate/wake path. A woken hub reads its state from a
    /// FRESH PVC, so it has a new server_id and is a FOREIGN holder to
    /// the cell its predecessor left — self-recognition cannot save it.
    /// Without the released mark it would sit out the full
    /// `lease_misses × heartbeat` timeout proving a hub is dead that
    /// already announced its own death.
    #[tokio::test]
    async fn a_released_cell_is_claimed_by_a_stranger_with_no_wait() {
        let (_mem, store) = stores();
        // Same absurd timeout as the self-recognition test: if the
        // foreign-wait path is taken at all, this hangs for weeks.
        let ca = cfg("hub-a", 3_600_000, 1000);
        let la = claim(&store, &ca, "vol/").await.unwrap();
        assert_eq!(la.epoch, 1);

        // hub-a shuts down cleanly.
        store.epoch_release(&ca.key, &la).await.unwrap();

        // A DIFFERENT holder id — the woken hub on a fresh PVC.
        let cb = cfg("hub-b", 3_600_000, 1000);
        let lb = tokio::time::timeout(Duration::from_secs(5), claim(&store, &cb, "vol/"))
            .await
            .expect("a released cell must be claimable immediately")
            .unwrap();
        assert_eq!(lb.epoch, 2, "numbering continues from the released epoch");

        // And the predecessor is fenced, released or not.
        let err = store.epoch_renew(&cb.key, &la).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
    }

    /// A hub deposed mid-shutdown must not be able to stamp `released`
    /// on the cell its successor now holds — that would invite a third
    /// hub to claim instantly while the successor is serving.
    #[tokio::test]
    async fn a_deposed_holder_cannot_mark_a_live_successors_cell() {
        let (_mem, store) = stores();
        let ca = cfg("hub-a", 3_600_000, 1000);
        let la = claim(&store, &ca, "vol/").await.unwrap();
        let observed = store.epoch_read(&ca.key).await.unwrap().unwrap();
        let lb = store.epoch_acquire(&ca.key, "hub-b", Some(&observed)).await.unwrap();

        let err = store.epoch_release(&ca.key, &la).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
        let cell = store.epoch_read(&ca.key).await.unwrap().unwrap();
        assert!(!cell.released, "the live successor's cell stays unreleased");
        assert_eq!(cell.epoch, lb.epoch);
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

        // The heartbeat must exit on its own — deposed. Poll the
        // observable effect rather than joining the task: the handle
        // now owns a shutdown channel, and dropping it must NOT be
        // read as a shutdown request.
        for _ in 0..200 {
            if guard.is_fenced() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(hb);
        assert!(guard.is_fenced());
        assert_eq!(guard.current(), None);
        assert!(deposed.load(Ordering::SeqCst));
    }

    /// THE REGRESSION, found by the flint-lite cluster drill.
    ///
    /// `server.rs` fences the guard one line before it calls
    /// `release()` — barrier first, then the mark, deliberately
    /// adjacent. While the release arm tested `is_fenced()`, that
    /// ordering made the clean release UNREACHABLE: every clean
    /// shutdown took the LostCas arm without ever touching the store,
    /// left the cell HELD, and cost the next hub the full
    /// `heartbeat × lease_misses` wait. Measured on a real cluster as
    /// 79s instead of 13s on every identity-changing wake.
    #[tokio::test(start_paused = true)]
    async fn a_clean_shutdown_fence_still_releases_the_cell() {
        let (_mem, store) = stores();
        let ca = cfg("hub-a", 50, 3);
        let la = claim(&store, &ca, "vol/").await.unwrap();
        let guard = EpochGuard::held(la.epoch);
        let hb = spawn_heartbeat(
            Arc::clone(&store),
            ca.clone(),
            la,
            Arc::clone(&guard),
            Box::new(|| {}),
        );

        // Exactly the ordering the shutdown path uses.
        guard.fence_for_shutdown();
        assert!(guard.is_fenced(), "publishes must still be barred");
        assert_eq!(guard.current(), None, "nothing may publish after the barrier");
        assert!(!guard.fenced_by_deposition(), "our own fence is not a deposition");

        let outcome = hb.release(Duration::from_secs(15)).await;
        assert!(
            matches!(outcome, ReleaseOutcome::Released),
            "clean shutdown must release, got {:?}",
            outcome
        );
        let cell = store.epoch_read(&ca.key).await.unwrap().unwrap();
        assert!(cell.released, "the cell must carry the released mark");

        // And the whole point of the mark: no successor waits.
        let cb = cfg("hub-b", 50, 3);
        let started = tokio::time::Instant::now();
        let lb = claim(&store, &cb, "vol/").await.unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "a stranger waited {:?} on a released cell",
            started.elapsed()
        );
        assert_eq!(lb.epoch, 2);
    }

    /// The property the broken check was reaching for, kept intact: a
    /// hub fenced because it was DEPOSED must not stamp `released`
    /// over its successor's reign.
    #[tokio::test(start_paused = true)]
    async fn a_deposed_guard_still_suppresses_the_clean_release() {
        let (_mem, store) = stores();
        let ca = cfg("hub-a", 50, 3);
        let la = claim(&store, &ca, "vol/").await.unwrap();
        let guard = EpochGuard::held(la.epoch);
        let hb = spawn_heartbeat(
            Arc::clone(&store),
            ca.clone(),
            la,
            Arc::clone(&guard),
            Box::new(|| {}),
        );

        guard.fence();
        assert!(guard.fenced_by_deposition());

        let outcome = hb.release(Duration::from_secs(15)).await;
        assert!(
            matches!(outcome, ReleaseOutcome::LostCas),
            "a deposed hub must not release, got {:?}",
            outcome
        );
        let cell = store.epoch_read(&ca.key).await.unwrap().unwrap();
        assert!(!cell.released, "the successor's cell must stay unreleased");
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
