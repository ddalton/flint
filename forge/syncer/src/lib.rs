//! flint-forge: the per-repo syncer (design of record:
//! `docs/plans/flint-forge-design.md`).
//!
//! Forge serves real git. `nginx` + `fcgiwrap` + `git http-backend`
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
pub mod gitcmd;
pub mod lease;
pub mod packio;
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
    /// before closing the batch. The batch is what turns a per-push
    /// round-trip chain into a per-batch one; see the design's
    /// throughput arithmetic (§4).
    pub batch_window_ms: u64,
    /// Ceiling on pushes in one batch, so a storm cannot make a single
    /// CAS carry unbounded work.
    pub batch_max: usize,
    /// Repack when the repository holds more than this many packs.
    /// The knob that trades clone cost against repack cost (§10).
    pub repack_threshold: usize,
    /// How long an unreferenced pack must have sat before the sweep may
    /// take it. Must outlive the LONGEST upload, not the longest
    /// plausible one (`LeanChunkGCRacyGrace`).
    pub orphan_grace_secs: u64,
    /// The project this repository claims to be, stamped from the CR.
    /// When set, the claim cell is checked before the first claim step
    /// and a foreign project is `Refused` (lean's finding 5, on the
    /// data plane).
    pub project_id: Option<String>,
    /// Bounded concurrency for pack uploads and restore fetches.
    pub fanout: usize,
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
    pub fn new(prefix: &str, repo: impl Into<PathBuf>) -> Self {
        let repo = repo.into();
        let state_dir = repo.join("flint-forge");
        ForgeConfig {
            prefix: prefix.trim_end_matches('/').to_string(),
            repo,
            state_dir,
            heartbeat_secs: 10,
            batch_window_ms: 400,
            batch_max: 64,
            repack_threshold: 24,
            orphan_grace_secs: 3600,
            project_id: None,
            fanout: 16,
            default_branch: "main".into(),
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
}

/// The syncer's live state: the store, the config, the repository, the
/// lease, and the snapshot cell it is entitled to CAS.
pub struct Syncer {
    pub store: Arc<dyn ObjectStore>,
    pub cfg: ForgeConfig,
    pub git: gitcmd::Git,
    /// `None` before the claim, and again the moment we are fenced.
    pub lease: Option<EpochLease>,
    /// The snapshot this syncer last read or wrote, with the etag its
    /// next CAS must match. `None` means "not loaded yet" — a state in
    /// which no batch may run.
    pub cell: Option<snapshot::Cell>,
    /// Stable across container restarts within one pod (the emptyDir
    /// state file), which is what makes self-recognition of our own
    /// lease safe and a replacement pod's takeover slow.
    pub holder_id: String,
    /// Set the moment this syncer is deposed. Every path checks it
    /// before touching the store, and the server stops answering reads
    /// too: a deposed server that kept serving `upload-pack` would
    /// serve stale refs indefinitely.
    pub fenced: Option<String>,
    /// Unix seconds of the last acknowledged push, for `/status`.
    pub last_push_unix: u64,
    pub started_unix: u64,
}

impl Syncer {
    pub fn new(store: Arc<dyn ObjectStore>, cfg: ForgeConfig, holder_id: String) -> Self {
        let git = gitcmd::Git::new(&cfg.repo);
        Syncer {
            store,
            cfg,
            git,
            lease: None,
            cell: None,
            holder_id,
            fenced: None,
            last_push_unix: 0,
            started_unix: now_unix(),
        }
    }

    /// The one gate every store-touching path takes first. A fence is
    /// terminal for the process: nothing clears it.
    pub fn check_fence(&self) -> ForgeResult<()> {
        match &self.fenced {
            Some(why) => Err(ForgeError::Fenced(why.clone())),
            None => Ok(()),
        }
    }

    pub fn fence(&mut self, why: impl Into<String>) -> ForgeError {
        let why = why.into();
        self.lease = None;
        self.fenced = Some(why.clone());
        ForgeError::Fenced(why)
    }

    pub fn lease(&self) -> ForgeResult<&EpochLease> {
        self.lease.as_ref().ok_or_else(|| ForgeError::State("no lease held".into()))
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
