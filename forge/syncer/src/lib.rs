//! flint-forge: the per-repo syncer (design of record:
//! `docs/plans/flint-forge-design.md`).
//!
//! Forge serves real git. `flint-forge-gitcgi` + `git http-backend`
//! answer clones, fetches and pushes over the smart protocol; the
//! repository on local disk is a CACHE; S3 is the truth. This crate is
//! the one process that stands between the two — one syncer per
//! repository, the pod's main process, holding the repo's writer lock
//! for every path to the bucket and for every write to
//! `objects/pack/`.
//!
//! The load-bearing rules, each of which the design §4 records a
//! measurement or a mutation for:
//!
//! - **`receive-pack` serialises nothing, and under
//!   `receive.procReceiveRefs` git performs no old-oid check and no
//!   `denyNonFastForwards` for the handed-off commands** (verified on
//!   git 2.50.1). The hook therefore decides nothing: it hands its
//!   commands to this process and relays the report. Every serialising
//!   decision — fast-forward, protection, staleness — is made here,
//!   under the writer lock.
//! - **A command is stale unless its old-oid equals BOTH the local ref
//!   and the last-synced snapshot's ref.** The local ref alone is not
//!   enough: a syncer that restored from a snapshot it has not
//!   re-read, or that lost a CAS, would accept a push against a ref the
//!   bucket has already moved.
//! - **One snapshot CAS per batch**, `If-Match` on the etag this
//!   syncer last wrote or read. Under the writer lock a 412 can only
//!   mean a second server, so it is the fence: report `ng` to every
//!   waiting hook, stop serving reads as well as writes, exit.
//! - **Ref updates as one `git update-ref --stdin` transaction, THEN
//!   the reports.** Per-ref `ok` reaches the client as it is emitted,
//!   so a report interleaved with the updates would acknowledge a
//!   subset of a snapshot the bucket already holds in full.
//! - **Objects that the server itself created must be packed before
//!   the upload.** `merge-tree --write-tree` and `commit-tree` write
//!   LOOSE objects; a pack-only upload would acknowledge a merge the
//!   bucket does not hold and a restore could not `fsck`.
//! - **The lease is renewed on a timer, not on a push.** A quiet
//!   server that renews only inside a push lets its lease expire and
//!   leaves a straggler unfenced. A 412 on the renew takes lean's
//!   lost-response rule first (re-read; a cell that still names this
//!   holder at this epoch can only have been written by us), and is
//!   otherwise the fence.
//! - **A successor rotates the snapshot before serving a byte** on an
//!   unreleased takeover, so a straggler's `If-Match` is stale before
//!   it can land (lean's `rotate_for_takeover`; the mutation
//!   `LeanNoRotate` is why).
//! - **Packs are immutable and content-named; the snapshot is the only
//!   mutable object the server trusts.** The sweep deletes packs the
//!   current snapshot does not name, under `LeanChunkGC`'s four rules
//!   with "chunk" read as "pack".

pub mod batch;
pub mod bundle;
pub mod export;
pub mod fold;
pub mod gitcmd;
pub mod hook;
pub mod lease;
pub mod lfs;
pub mod packio;
pub mod policy;
pub mod prune;
pub mod pktline;
pub mod restore;
pub mod server;
pub mod snapshot;
pub mod status;
pub mod sweep;
pub mod uds;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use flint_store::{EpochLease, ObjectStore};

/// The syncer's own version, echoed into the lease cell and the status
/// document — the mixed-fleet tell lean's `LeaseEcho` exists for.
pub const SYNCER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Process exit code for `ForgeError::Refused` (sysexits `EX_CONFIG`),
/// the same code and the same meaning as lean's: final, do not restart
/// in place. A fence is NOT refused — it is an ordinary non-zero exit,
/// because restarting is exactly the right response to a deposal.
pub const EXIT_REFUSED: i32 = 78;

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("store: {0}")]
    Store(#[from] flint_store::StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git: {0}")]
    Git(String),
    #[error("state: {0}")]
    State(String),
    /// The lease was lost, or a snapshot CAS failed under the writer
    /// lock: a second server holds this repository. Stop serving —
    /// reads included — and exit.
    #[error("fenced: {0}")]
    Fenced(String),
    /// A precondition that no retry can fix: a foreign claim, a
    /// snapshot naming a pack the bucket does not hold, a git below the
    /// floor. Exits `EXIT_REFUSED`.
    #[error("refused: {0}")]
    Refused(String),
    /// The export's `flint-sync` child outlived its timeout and was
    /// killed. Distinct from `State` because the caller BACKS OFF on
    /// it: whatever blocked the child does not clear in the time one
    /// batch takes, and re-entering it every batch would rebuild the
    /// outage the timeout exists to bound.
    #[error("export blocked: {0}")]
    ExportBlocked(String),
}

pub type ForgeResult<T> = Result<T, ForgeError>;

/// Everything under the repository's prefix. `git/` mirrors a bare
/// repository so that the bucket, with the server down, is a read-only
/// dumb remote (design §3).
#[derive(Debug, Clone)]
pub struct ForgeConfig {
    /// Bucket prefix for this repository, no trailing slash.
    pub prefix: String,
    /// The bare repository on local disk (the cache).
    pub repo: PathBuf,
    /// The syncer's durable bookkeeping, beside the repository and so
    /// on the same `emptyDir`: it survives a container restart and
    /// dies with the pod. That scope is exactly what makes
    /// self-recognition of our own lease safe (only the same pod's
    /// restarted container inherits the incarnation id) and a
    /// replacement pod's takeover slow, as it must be.
    pub state_dir: PathBuf,
    /// Lease heartbeat period. The renew rides this timer whether or
    /// not pushes arrive.
    pub heartbeat_secs: u64,
    /// How long the syncer waits for more pushes once one has arrived,
    /// before closing the batch. `0` (the default since X20) waits for
    /// nothing: a batch is the pushes that queued while the previous
    /// batch ran, so group commit under load costs no wait and a lone
    /// push pays none. The batch is what turns a per-push round-trip
    /// chain into a per-batch one (design §4); the fixed 400 ms window
    /// that used to open it was 0.48 s of a 1 KiB push's 0.58 s on the
    /// wire.
    pub batch_window_ms: u64,
    /// Ceiling on pushes in one batch, so a storm cannot make a single
    /// CAS carry unbounded work.
    pub batch_max: usize,
    /// The CONTROL rule (X18): with `fold_factor == 0`, repack when the
    /// repository holds more than this many packs — the shipped
    /// `repack -a -d -b`, kept until the tiers' measurement has run.
    pub repack_threshold: usize,
    /// Compaction tiers (`docs/plans/forge-compaction-tiers-design.md`).
    /// git's geometric factor over pack bytes; 0 = the control rule.
    pub fold_factor: u64,
    /// Rebuild the base once the tiers reach this percent of it.
    pub base_tier_percent: u64,
    /// With no base yet, build one once the named packs reach this.
    pub base_min_bytes: u64,
    /// At most one base rebuild per this many seconds.
    pub base_rebuild_min_secs: u64,
    /// Superseded packs stay on disk this long for readers that opened
    /// them before the commit (git's own `repack -d` race).
    pub fold_retain_secs: u64,
    /// A fold whose upload moves no bytes for this long is aborted.
    pub fold_stall_secs: u64,
    /// The full LIST sweep runs at most once per this many seconds
    /// (the ledger sweep covers what folds unname; this one covers what
    /// a crashed incarnation or a straggler left).
    pub sweep_every_secs: u64,
    /// A tier fold smaller than this is skipped, unless the cap forces it.
    pub fold_min_bytes: u64,
    /// Fold regardless when the tier count reaches this.
    pub fold_max_packs: usize,
    /// How long an unreferenced pack must have sat before the sweep may
    /// take it. Must outlive the LONGEST upload, not the longest
    /// plausible one (`LeanChunkGCRacyGrace`).
    pub orphan_grace_secs: u64,
    /// The project this repository claims to be, stamped from the CR.
    /// When set, the claim cell is checked before the first claim step
    /// and a foreign project is `Refused` (lean's finding 5, on the
    /// data plane).
    pub project_id: Option<String>,
    /// Bounded concurrency for pack uploads and restore fetches
    /// (`FLINT_FORGE_FANOUT`). It is a RAM bound as much as a request
    /// bound: an upload in flight holds up to `packio::WHOLE_PUT_MAX`
    /// and a fetch in flight `packio::FETCH_CHUNK`, so the default is
    /// sized to the pod's memory request, not to the bucket's request
    /// rate. Declared at 16 and read nowhere until 2026-09-05: uploads
    /// ran at a hard-coded bound and the restore one file at a time.
    pub fanout: usize,
    /// `core.hooksPath` for the repository, when the hooks live
    /// somewhere other than `<repo>/hooks`.
    ///
    /// They do in the pod: the hook binary ships in the git image and
    /// the repository lives on a shared `emptyDir`, so the path is
    /// resolved inside whichever container runs git and the repository
    /// itself carries no binaries. `None` = git's default, which is
    /// what the rigs and the local spike use.
    pub hooks_path: Option<String>,
    /// What `HEAD` points at in a repository nobody has pushed to.
    ///
    /// Passed to `git init --initial-branch` rather than left to git's
    /// built-in default, which is `master` and which
    /// `init.defaultBranch` can move — a server's default branch must
    /// not depend on the image's global config or on which git built
    /// the base layer.
    pub default_branch: String,
}

impl ForgeConfig {
    /// The holder's term: the challenger's takeover window, on the
    /// holder's own clock (X13). `heartbeat_secs × QUIET_POLLS`.
    pub fn renew_term(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.heartbeat_secs * lease::QUIET_POLLS as u64)
    }
    pub fn new(prefix: &str, repo: impl Into<PathBuf>) -> Self {
        let repo = repo.into();
        let state_dir = repo.join("flint-forge");
        ForgeConfig {
            prefix: prefix.trim_end_matches('/').to_string(),
            repo,
            state_dir,
            heartbeat_secs: 10,
            batch_window_ms: 0,
            batch_max: 64,
            repack_threshold: 24,
            fold_factor: 2,
            base_tier_percent: 50,
            base_min_bytes: 64 * 1024 * 1024,
            base_rebuild_min_secs: 3600,
            fold_retain_secs: 900,
            fold_stall_secs: 300,
            sweep_every_secs: 3600,
            fold_min_bytes: 0,
            fold_max_packs: 64,
            orphan_grace_secs: 3600,
            project_id: None,
            fanout: 4,
            default_branch: "main".into(),
            hooks_path: None,
        }
    }

    /// The bare repository's root in the bucket. Everything git's dumb
    /// protocol needs to serve a read-only clone lives under it.
    pub fn git_prefix(&self) -> String {
        format!("{}/git", self.prefix)
    }
    /// One pack, or its `.idx`/`.bitmap`/`.rev`. Content-named by git,
    /// so the key is immutable and the PUT is unconditional.
    pub fn pack_key(&self, name: &str) -> String {
        format!("{}/git/objects/pack/{name}", self.prefix)
    }
    pub fn pack_prefix(&self) -> String {
        format!("{}/git/objects/pack/", self.prefix)
    }
    /// THE pointer: the only mutable object the server trusts.
    pub fn snapshot_key(&self) -> String {
        format!("{}/git/snapshot", self.prefix)
    }
    pub fn epoch_key(&self) -> String {
        format!("{}/git/epoch", self.prefix)
    }
    /// The operator's claim cell; the syncer only READS it.
    pub fn claim_key(&self) -> String {
        format!("{}/git/claim", self.prefix)
    }
    /// Derived for git's dumb protocol; the server never reads these.
    pub fn info_refs_key(&self) -> String {
        format!("{}/git/info/refs", self.prefix)
    }
    pub fn info_packs_key(&self) -> String {
        format!("{}/git/objects/info/packs", self.prefix)
    }
    pub fn head_key(&self) -> String {
        format!("{}/git/HEAD", self.prefix)
    }
    pub fn bundle_key(&self, name: &str) -> String {
        format!("{}/git/bundles/{name}", self.prefix)
    }
    /// Where the EXPORT's `flint-sync` baseline is kept.
    ///
    /// It lives under the repository's own control prefix rather than
    /// the export's, because it is forge's state about the export and
    /// not part of the workspace a reader sees.
    pub fn export_baseline_key(&self) -> String {
        format!("{}/git/export-baseline.json", self.prefix)
    }
    pub fn bundle_prefix(&self) -> String {
        format!("{}/git/bundles/", self.prefix)
    }
}

/// The lease, held jointly by the serving loop and the renewer task.
///
/// Before this the lease was a field on `Syncer`, renewed by a timer
/// arm of the serving loop's `select!` — so nothing renewed it while
/// that loop was inside a batch, a restore or an export. At 10 GiB the
/// token was measured silent for 125 s during a push and 141 s during
/// a restore, against a 60 s takeover window (design §5): a live pod,
/// mid-work, could lose its repository to a challenger.
///
/// The renewer (`lease::spawn_renewer`) runs on its own task from the
/// moment the claim lands, but it is NOT unconditional. While the loop
/// reports a phase that must move (`Phase::must_progress`), it renews
/// only if `progress` advanced since its last renewal. A wedged restore
/// or a wedged upload therefore lets the token go quiet, and the quiet
/// polls a challenger counts are exactly the takeover a wedged holder
/// is supposed to get. Renewing for a wedged pod would have traded "a
/// live pod loses its repository" for "a dead one keeps it".
pub struct Hold {
    state: std::sync::Mutex<HoldState>,
    /// Serialises every renew and release against the store, whichever
    /// task issues it: two renews in flight would 412 each other and
    /// take the lost-response path for nothing.
    gate: tokio::sync::Mutex<()>,
    /// Units of work landed by the operation in flight — bytes of a
    /// transfer, commands judged — shared with `packio` and, through
    /// `ComposeSpec::progress`, with the store's part loop.
    progress: Arc<std::sync::atomic::AtomicU64>,
    last_renew_unix: std::sync::atomic::AtomicU64,
    /// The same moment on the runtime's clock, which virtual time can
    /// drive in a test where the wall clock cannot: the holder's own
    /// term (X13) is judged against it.
    last_renew_at: std::sync::Mutex<Option<tokio::time::Instant>>,
    /// Set the moment this syncer is deposed, and never cleared. Every
    /// path checks it before touching the store, and the server stops
    /// answering reads too: a deposed server that kept serving
    /// `upload-pack` would serve stale refs indefinitely. A `watch`, so
    /// the serving loop wakes on it from whatever it is awaiting.
    fenced: tokio::sync::watch::Sender<Option<String>>,
}

#[derive(Default)]
struct HoldState {
    lease: Option<EpochLease>,
    released: bool,
}

impl Default for Hold {
    fn default() -> Self {
        Self::new()
    }
}

impl Hold {
    pub fn new() -> Self {
        let (fenced, _) = tokio::sync::watch::channel(None);
        Hold {
            state: std::sync::Mutex::new(HoldState::default()),
            gate: tokio::sync::Mutex::new(()),
            progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_renew_unix: std::sync::atomic::AtomicU64::new(0),
            last_renew_at: std::sync::Mutex::new(None),
            fenced,
        }
    }
    pub fn lease(&self) -> Option<EpochLease> {
        self.state.lock().unwrap().lease.clone()
    }
    /// A claim or a renewal landed.
    pub fn set_lease(&self, lease: EpochLease) {
        self.state.lock().unwrap().lease = Some(lease);
        self.last_renew_unix.store(now_unix(), std::sync::atomic::Ordering::Relaxed);
        *self.last_renew_at.lock().unwrap() = Some(tokio::time::Instant::now());
    }
    /// How long since a renewal (or the claim) last landed; `None`
    /// before the first.
    pub fn since_renew(&self) -> Option<std::time::Duration> {
        self.last_renew_at.lock().unwrap().map(|t| t.elapsed())
    }
    /// The holder's own term (X13). A challenger deposes a holder whose
    /// token has not moved for `QUIET_POLLS` heartbeats; a holder that
    /// has landed no renewal for that long must assume it may already
    /// have been deposed, and stop serving reads until one lands. It
    /// does NOT give the lease up: nobody else can claim while the
    /// store is unreachable for everyone, and if it was unreachable for
    /// this holder alone the next renewal's 412 is the fence as before.
    pub fn renewal_overdue(&self, term: std::time::Duration) -> bool {
        matches!(self.since_renew(), Some(d) if d > term)
    }
    pub fn take_lease(&self) -> Option<EpochLease> {
        self.state.lock().unwrap().lease.take()
    }
    /// A clean release is under way or done: the renewer stops.
    pub fn mark_released(&self) {
        self.state.lock().unwrap().released = true;
    }
    pub fn is_released(&self) -> bool {
        self.state.lock().unwrap().released
    }
    /// Terminal for the process: nothing clears it.
    pub fn fence(&self, why: impl Into<String>) -> ForgeError {
        let why = why.into();
        self.state.lock().unwrap().lease = None;
        self.fenced.send_replace(Some(why.clone()));
        ForgeError::Fenced(why)
    }
    pub fn fenced(&self) -> Option<String> {
        self.fenced.borrow().clone()
    }
    pub fn check_fence(&self) -> ForgeResult<()> {
        match self.fenced() {
            Some(why) => Err(ForgeError::Fenced(why)),
            None => Ok(()),
        }
    }
    /// Resolves (`changed`) the moment the fence is set.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.fenced.subscribe()
    }
    pub fn progress_handle(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.progress.clone()
    }
    pub fn progress(&self) -> u64 {
        self.progress.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn tick(&self, units: u64) {
        self.progress.fetch_add(units, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn last_renew_unix(&self) -> u64 {
        self.last_renew_unix.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub(crate) async fn gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }
}

/// The syncer's live state: the store, the config, the repository, the
/// lease, and the snapshot cell it is entitled to CAS.
pub struct Syncer {
    pub store: Arc<dyn ObjectStore>,
    pub cfg: ForgeConfig,
    pub git: gitcmd::Git,
    /// The lease and the fence, shared with the renewer task.
    pub hold: Arc<Hold>,
    /// The snapshot this syncer last read or wrote, with the etag its
    /// next CAS must match. `None` means "not loaded yet" — a state in
    /// which no batch may run.
    pub cell: Option<snapshot::Cell>,
    /// Stable across container restarts within one pod (the emptyDir
    /// state file), which is what makes self-recognition of our own
    /// lease safe and a replacement pod's takeover slow.
    pub holder_id: String,
    /// Unix seconds of the last acknowledged push, for `/status`.
    pub last_push_unix: u64,
    /// A commit the export published that the snapshot does not name
    /// yet. The NEXT batch's single CAS carries it — the export must
    /// never write the snapshot itself, or it becomes a second writer
    /// racing pushes for the one object that has exactly one.
    pub pending_exported_commit: Option<String>,
    /// A clone bundle the bucket holds that the snapshot does not name
    /// yet. Same rule as the export: the NEXT batch's single CAS
    /// carries it, because there is exactly one writer of that object
    /// and it is the batch.
    pub pending_bundle: Option<String>,
    /// The `HEAD` target this syncer has already published, so the
    /// derived write is skipped while it is unchanged. §3 calls HEAD
    /// "derived, once"; before this it went up on EVERY batch, a fifth
    /// of the fixed per-push S3 cost spent restating `ref:
    /// refs/heads/main`. Cleared on restart and after a takeover, so a
    /// new server republishes once rather than trusting a predecessor.
    pub published_head: Option<String>,
    pub started_unix: u64,
    /// The fold beside the loop, at most one (X18, `fold.rs`).
    pub fold: Option<fold::InFlight>,
    /// Superseded packs kept on disk for readers; subtracted from every
    /// listing so no batch re-names them.
    pub retained: Vec<fold::Retained>,
    /// What fold commits unnamed in the bucket, for the ledger sweep.
    pub fold_ledger: Vec<fold::LedgerEntry>,
    pub last_base_rebuild_unix: u64,
    pub last_full_sweep_unix: u64,
}

impl Syncer {
    pub fn new(store: Arc<dyn ObjectStore>, cfg: ForgeConfig, holder_id: String) -> Self {
        let git = gitcmd::Git::new(&cfg.repo);
        Syncer {
            store,
            cfg,
            git,
            hold: Arc::new(Hold::new()),
            cell: None,
            holder_id,
            last_push_unix: 0,
            pending_exported_commit: None,
            pending_bundle: None,
            published_head: None,
            started_unix: now_unix(),
            fold: None,
            retained: Vec::new(),
            fold_ledger: Vec::new(),
            last_base_rebuild_unix: 0,
            last_full_sweep_unix: 0,
        }
    }

    /// The packs a batch may list, upload and name: what git sees,
    /// minus the packs a fold superseded and retention keeps on disk.
    /// A retained pack re-listed would be re-named and re-uploaded
    /// (rule 4 refreshes its age) — the collision every "keep the old
    /// packs a while" fix has.
    pub fn listed_packs(&self) -> ForgeResult<Vec<String>> {
        let mut v = self.git.local_packs()?;
        if !self.retained.is_empty() {
            let held: std::collections::BTreeSet<&String> =
                self.retained.iter().map(|r| &r.name).collect();
            v.retain(|p| !held.contains(p));
        }
        Ok(v)
    }

    /// The one gate every store-touching path takes first. A fence is
    /// terminal for the process: nothing clears it.
    pub fn check_fence(&self) -> ForgeResult<()> {
        self.hold.check_fence()
    }

    pub fn fence(&mut self, why: impl Into<String>) -> ForgeError {
        self.hold.fence(why)
    }

    pub fn fenced(&self) -> Option<String> {
        self.hold.fenced()
    }

    pub fn lease(&self) -> ForgeResult<EpochLease> {
        self.hold.lease().ok_or_else(|| ForgeError::State("no lease held".into()))
    }

    pub fn cell(&self) -> ForgeResult<&snapshot::Cell> {
        self.cell.as_ref().ok_or_else(|| ForgeError::State("snapshot not loaded".into()))
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
