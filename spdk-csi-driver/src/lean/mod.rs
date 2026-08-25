//! flint-lean: the checkout/publish sidecar core (plan of record:
//! docs/plans/flint-lean-plan.md v2).
//!
//! Lean is a SEPARATE front end from the hub: no NFS, no daemon in the
//! data path. An agent pod gets a full local checkout on an emptyDir
//! (real ext4/APFS POSIX), and a sidecar publishes snapshots to the
//! bucket through a barrier. Durability is RPO-shaped (the flush floor);
//! coherence is snapshot-level (the subtree manifest CAS is the atomic
//! commit point). This module reuses `tier::store` (ObjectStore,
//! GenerationStamps, conditional puts, the epoch cell) as a library and
//! deliberately does NOT reuse the hub's flush/arbitration machinery —
//! the plan pins that: the hub's 412 arbitration is LOCAL-WINS-overwrite
//! (`tier/flush.rs`), which is exactly wrong under HITL second writers.
//!
//! The protocol here is the machine checked by `lean/formal/
//! LeanSubtree.tla` (20-run gate). The load-bearing rules, each of
//! which a model mutation rediscovers when broken:
//!
//! - **Barrier order**: consume inbox → scan → intent/window → uploads
//!   → manifest CAS (merge) → deletes LAST as GC of keys the NEW
//!   manifest no longer references → baseline rewrite. (v1's
//!   upload→delete→CAS order dangles the manifest on a crash.)
//! - **HITL writes land as object + inbox entry, never direct manifest
//!   edits**; the barrier never runs against an unconsumed inbox.
//! - **Never If-Match-overwrite an ETag this sidecar did not itself
//!   publish or consume**: a foreign 412 parks the path and surfaces a
//!   conflict; own-crashed-PUT is recognized by flush_uuid and adopted.
//! - **GC deletes are HEAD-guarded** on the recognized ETag.
//! - **Takeover rotation**: a successor CAS-rewrites the manifest
//!   (seq++, content-identical) BEFORE serving, so a deposed
//!   straggler's manifest CAS 412s; every publish carries its epoch.
//! - **Restart matrix**: marker present ⇒ never re-materialize (a
//!   re-checkout would resurrect unpublished deletes); reload the
//!   persisted baseline, rescan, self-recognize the lease via the
//!   persisted incarnation id (emptyDir-scoped — a replacement pod gets
//!   a fresh identity and takes the takeover path).
//! - **Deletion basis**: a path is delete-eligible only if absent in
//!   two consecutive scans AND present in our own baseline (the
//!   rename-vs-walk guard).

pub mod barrier;
pub mod checkout;
pub mod inbox;
pub mod lease;
pub mod manifest;
pub mod scan;
pub mod state;
pub mod sync;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use crate::tier::store::ObjectStore;

/// Reserved control namespace inside the subtree prefix. Never scanned,
/// never part of a checkout, never GC'd by the barrier.
pub const LEAN_DIR: &str = ".flint/lean";

/// The sidecar's durable bookkeeping directory inside the workspace
/// (emptyDir-scoped: survives container restarts, dies with the pod).
pub const STATE_DIR: &str = ".flint-sync";

/// Default whole-object publish ceiling; larger files go through the
/// multipart compose path (`tier/flush.rs` uses the same 64 MiB split).
pub const WHOLE_PUT_MAX: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LeanError {
    #[error("store: {0}")]
    Store(#[from] crate::tier::store::StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("state: {0}")]
    State(String),
    /// The lease was lost or a higher epoch was observed: the caller
    /// must stop publishing immediately (self-fence).
    #[error("fenced: {0}")]
    Fenced(String),
    /// A budget refused the operation (checkout bytes/file-count).
    #[error("budget: {0}")]
    Budget(String),
}

pub type LeanResult<T> = Result<T, LeanError>;

#[derive(Clone)]
pub struct LeanConfig {
    /// Bucket key prefix for this subtree (project- or subtree-scoped;
    /// the proxy's tenancy boundary).
    pub prefix: String,
    /// Workspace root (the agent's tree).
    pub root: PathBuf,
    /// Publish cadence floor, seconds (the durability contract).
    pub floor_secs: u64,
    /// Whole-object ceiling; larger files use multipart compose.
    pub whole_put_max: u64,
    /// Checkout refusal budgets (0 = unlimited).
    pub max_bytes: u64,
    pub max_files: u64,
    /// Window deadline slack beyond the barrier start, seconds. A dead
    /// sidecar's window is ignorable past this deadline.
    pub window_slack_secs: u64,
}

impl LeanConfig {
    pub fn new(prefix: &str, root: impl Into<PathBuf>) -> Self {
        LeanConfig {
            prefix: prefix.trim_end_matches('/').to_string(),
            root: root.into(),
            floor_secs: 60,
            whole_put_max: WHOLE_PUT_MAX,
            max_bytes: 0,
            max_files: 0,
            window_slack_secs: 180,
        }
    }

    pub fn file_key(&self, path: &str) -> String {
        format!("{}/files/{}", self.prefix, path)
    }
    pub fn manifest_key(&self) -> String {
        format!("{}/{}/manifest", self.prefix, LEAN_DIR)
    }
    pub fn inbox_key(&self) -> String {
        format!("{}/{}/inbox", self.prefix, LEAN_DIR)
    }
    pub fn epoch_key(&self) -> String {
        format!("{}/{}/epoch", self.prefix, LEAN_DIR)
    }
    pub fn conflict_key(&self, uuid: &str, path: &str) -> String {
        format!("{}/{}/conflicts/{}/{}", self.prefix, LEAN_DIR, uuid, path)
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }
}

/// Everything a sidecar operation needs: store + config + durable state.
pub struct Sidecar {
    pub store: Arc<dyn ObjectStore>,
    pub cfg: LeanConfig,
    pub state: state::SidecarState,
    /// The held lease, once claimed. Barriers refuse to run without it.
    pub lease: Option<crate::tier::store::EpochLease>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
