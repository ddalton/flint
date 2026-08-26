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
pub mod control;
pub mod gated;
pub mod gateway;
pub mod gauges;
pub use gauges::{status_report, Gauges, StatusReport};
pub mod inbox;
pub mod lease;
pub mod manifest;
pub mod metrics;
pub mod scan;
pub(crate) mod safefs;
pub mod sentinel;
pub mod state;
pub mod sync;
pub mod uds;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use flint_store::ObjectStore;

/// Reserved control namespace inside the subtree prefix. Never scanned,
/// never part of a checkout, never GC'd by the barrier.
pub const LEAN_DIR: &str = ".flint/lean";

/// The sidecar's durable bookkeeping directory inside the workspace
/// (emptyDir-scoped: survives container restarts, dies with the pod).
pub const STATE_DIR: &str = ".flint-sync";

/// The workspace-local control namespace (boundary-verbs plan D0): the
/// agent's side of the file protocol. Reserved exactly as the gateway
/// reserves `.flint/` on the HTTP side (`gateway::path_ok`) — the scan
/// skips it, `classify` never makes it delete-eligible, and checkout
/// never materializes a `files/.flint/...` citation into it.
pub const CONTROL_DIR: &str = ".flint";

/// Protocol version advertised in `.flint/capabilities.json`.
pub const SENTINEL_PROTOCOL: u32 = 1;

/// This binary's version, echoed to the agent (`capabilities.json`) and
/// to the operator (the lease-heartbeat echo, §2.6). Both mixed-version
/// holes — agent↔sidecar and operator↔sidecar — are detected by
/// comparing what is RUNNING against what was asked for, and neither
/// comparison exists without a version on the running side.
pub const SIDECAR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default whole-object publish ceiling; larger files go through the
/// multipart compose path (`tier/flush.rs` uses the same 64 MiB split).
pub const WHOLE_PUT_MAX: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LeanError {
    #[error("store: {0}")]
    Store(#[from] flint_store::StoreError),
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

/// Citation policy (boundary-verbs plan D6). `hybrid` is the default:
/// the fused barrier runs at every floor tick AND at every consumed
/// publish sentinel, whichever comes first — citation never waits, so
/// published-view freshness is never later than cadence-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoundaryMode {
    /// Exactly pre-boundary behavior (the escape hatch).
    Cadence,
    /// Cadence ∪ sentinels. No trade; the default.
    Hybrid,
    /// Durability and visibility split: uploads land as uncited object
    /// versions every floor tick, citation happens at coherent points
    /// only. Opt-in per workspace; requires the versioning conformance
    /// probe and a `visibility_lag_bound_secs`.
    Gated,
}

impl BoundaryMode {
    pub fn parse(s: &str) -> Option<BoundaryMode> {
        match s {
            "cadence" => Some(BoundaryMode::Cadence),
            "hybrid" => Some(BoundaryMode::Hybrid),
            "gated" => Some(BoundaryMode::Gated),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            BoundaryMode::Cadence => "cadence",
            BoundaryMode::Hybrid => "hybrid",
            BoundaryMode::Gated => "gated",
        }
    }
}

/// Sentinel posture knob (D0.4). `auto` = verbs on unless the
/// pre-existing-`.flint/` pre-flight trips; `force` accepts consumption
/// of pre-existing sentinel-named files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SentinelMode {
    Auto,
    Off,
    Force,
}

impl SentinelMode {
    pub fn parse(s: &str) -> Option<SentinelMode> {
        match s {
            "auto" => Some(SentinelMode::Auto),
            "off" => Some(SentinelMode::Off),
            "force" => Some(SentinelMode::Force),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            SentinelMode::Auto => "auto",
            SentinelMode::Off => "off",
            SentinelMode::Force => "force",
        }
    }
}

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
    /// Bounded concurrency for uploads and checkout fetches. The 0b
    /// rig measured the sequential loops at 561-854 PUTs/s and
    /// 1,000-2,000 GETs/s; fan-out multiplies directly against those.
    pub fanout: usize,

    // --- boundary verbs (plan docs/plans/flint-lean-boundary-verbs-plan.md) ---
    /// Citation policy. Default `hybrid` ≡ today's behavior when the
    /// agent never touches a sentinel.
    pub boundary_mode: BoundaryMode,
    /// Sentinel posture (D0.4 pre-flight override).
    pub sentinel_mode: SentinelMode,
    /// A consumed sentinel arriving sooner than this after the previous
    /// sentinel-honoring barrier waits; touches inside the interval
    /// coalesce into the pending record.
    pub sentinel_min_interval_secs: u64,
    /// Work-metered hourly cap (D3.1): a honor charges
    /// `max(1, ceil(published_bytes / whole_put_max))` units, or 0 for
    /// a no-diff honor. Exhaustion defers honors to the floor tick —
    /// the workspace degrades to exactly cadence behavior.
    pub sentinel_hourly_budget: u64,
    /// Sentinel poll cadence (env-only, not a fleet contract).
    pub sentinel_poll_secs: u64,
    /// Gated: hard cap on citation staleness. Required iff gated.
    pub visibility_lag_bound_secs: Option<u64>,
    /// Gated: scan-to-scan stability window that counts as quiescence.
    pub quiesce_bound_secs: u64,
    /// Gated: forced-citation sources bounding the preStop drain.
    pub staged_backlog_cap_objects: u64,
    pub staged_backlog_cap_bytes: u64,
    /// Gated: the noncurrent-version retention the operator provisions
    /// on `<prefix>/files/` — the crash-window backstop BEHIND flint's
    /// exact per-citation version GC (D8).
    pub noncurrent_retention_days: u64,
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
            fanout: 16,
            boundary_mode: BoundaryMode::Hybrid,
            sentinel_mode: SentinelMode::Auto,
            sentinel_min_interval_secs: 5,
            sentinel_hourly_budget: 60,
            sentinel_poll_secs: 1,
            visibility_lag_bound_secs: None,
            quiesce_bound_secs: 30,
            staged_backlog_cap_objects: 5000,
            staged_backlog_cap_bytes: 2 * 1024 * 1024 * 1024,
            noncurrent_retention_days: 30,
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
    /// The workspace-local control namespace (D0).
    pub fn control_dir(&self) -> PathBuf {
        self.root.join(CONTROL_DIR)
    }
}

/// Everything a sidecar operation needs: store + config + durable state.
pub struct Sidecar {
    pub store: Arc<dyn ObjectStore>,
    pub cfg: LeanConfig,
    pub state: state::SidecarState,
    /// The held lease, once claimed. Barriers refuse to run without it.
    pub lease: Option<flint_store::EpochLease>,
    /// Standing conditions already written to `conflicts.jsonl` by THIS
    /// process, so a condition that persists across every poll tick is
    /// recorded once rather than per tick. Deliberately in-memory: a
    /// restart is a new incarnation and should re-record what it finds.
    pub(crate) noted_not_regular: std::collections::BTreeSet<String>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
