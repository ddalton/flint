//! The `.flint/` control namespace (boundary-verbs plan §2.0, D0/D11):
//! the agent's side of the file protocol.
//!
//! The app container ships UNCHANGED — no HTTP client, no CLI binary,
//! no library can be assumed in its image. The only interface every
//! agent is guaranteed to have is the shared workspace mount, so the
//! boundary verbs are *files*. This module owns everything the sidecar
//! writes there:
//!
//! - `capabilities.json` — the marker an agent MUST check before
//!   touching a sentinel. Absence ⇒ an old sidecar ⇒ sentinels are live
//!   ammunition (they would be scanned and published, §2.0).
//! - `remote.seq` — the news ticker (D5), fed from information the
//!   barrier already has: zero added bucket requests.
//! - the pre-existing-`.flint/` pre-flight (D0.4), which disables
//!   sentinel consumption for a workspace that was already using the
//!   namespace as data.
//!
//! **Trust statement (plan §1.2):** every file here is writable by
//! every process sharing the mount. Acks are advisory coordination
//! signals, NOT attestations — an in-pod process can forge one. The
//! authoritative durability signal is a remote read the pod cannot
//! forge (gateway `GET /status`, or the manifest itself).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{now_unix, LeanResult, Sidecar, SENTINEL_PROTOCOL};

pub const CAPABILITIES: &str = "capabilities.json";
pub const REMOTE_SEQ: &str = "remote.seq";
pub const PUBLISH: &str = "publish";
pub const PUBLISH_ACK: &str = "publish.ack";
pub const SYNC: &str = "sync";
pub const SYNC_ACK: &str = "sync.ack";

/// State-dir file recording the sticky pre-flight verdict. Sticky
/// matters: the marker's own absence is one of the pre-flight's inputs,
/// so a second startup would otherwise see the marker it just wrote and
/// silently re-enable the verbs it disabled.
const POSTURE: &str = "sentinels.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootStamp {
    pub holder_id: String,
    pub boot_unix: u64,
}

/// `.flint/capabilities.json` (D11). Written atomically at EVERY `run`
/// startup — after claim succeeds, before the first poll — and again
/// during first checkout, so it exists the instant the agent may start.
///
/// The startup write is load-bearing on the live-tree restart row:
/// `checkout()` returns at `marker_present()` without materializing
/// anything, so pinning the marker write inside checkout would upgrade
/// a fleet whose live workspaces never get it — sentinels dead on
/// exactly the pods the upgrade targeted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub protocol: u32,
    pub verbs: Vec<String>,
    pub boundary_mode: String,
    /// "live" | "fenced" (D2). A fenced sidecar advertises no verbs, so
    /// agents stop touching sentinels on a zombie.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub sentinel_min_interval_secs: u64,
    pub sentinel_hourly_budget: u64,
    pub sidecar_version: String,
    /// The rollback tell (D11): an agent that observes a sidecar
    /// restart while this stamp is UNCHANGED is looking at a stale
    /// marker left by a downgrade — the safety catch painted green.
    pub boot: BootStamp,
}

/// `.flint/remote.seq` — the news ticker (D5).
///
/// `observed_seq > integrated_seq` means "there is news you have not
/// integrated — consider dropping a `.flint/sync` at your next safe
/// point". The agent polls one LOCAL file: no network, no client, no
/// credentials, and N agents cost zero bucket requests rather than
/// N×HEAD.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteSeq {
    pub observed_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_etag: Option<String>,
    pub integrated_seq: u64,
    /// Refreshed on EVERY successful HEAD tick — a local rename, zero
    /// bucket cost. Without the heartbeat an agent cannot distinguish
    /// "no news" from "sidecar dead" on an idle-but-healthy workspace.
    /// Contract: older than 3×floor ⇒ sidecar or proxy problem.
    pub updated_unix: u64,
    /// Advisory news from the gateway (D14): a HITL/CI party asked this
    /// workspace to pull. The sidecar performs NO tree mutation on
    /// receipt — it moves the ticker and stops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_requested_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_requested_by: Option<String>,
    /// `integrated_seq` at the moment the request was carried. Once the
    /// workspace integrates past it the request is stale and clears
    /// itself — the agent pulled, whether or not it did so on account
    /// of the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_requested_at_seq: Option<u64>,
}

/// The sticky pre-flight verdict (D0.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelPosture {
    pub enabled: bool,
    /// Fleet-visible reason, mirrored into `capabilities.json` and the
    /// `SentinelVerbsActive` condition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> LeanResult<()> {
    // `.flint/` is app-writable by construction — the agent drops its
    // sentinels there — so both the parent and the temp name are
    // attacker-reachable, and `remote.seq` is rewritten every tick.
    super::safefs::check_parent(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    ));
    super::safefs::write_via_tmp(path, &tmp, bytes, None)
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> LeanResult<()> {
    let bytes = serde_json::to_vec_pretty(v)
        .map_err(|e| super::LeanError::State(format!("{}: {e}", path.display())))?;
    write_atomic(path, &bytes)
}

impl Sidecar {
    pub fn control_path(&self, name: &str) -> PathBuf {
        self.cfg.control_dir().join(name)
    }

    fn posture_path(&self) -> PathBuf {
        self.cfg.state_dir().join(POSTURE)
    }

    pub fn load_posture(&self) -> LeanResult<Option<SentinelPosture>> {
        let p = self.posture_path();
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| super::LeanError::State(format!("posture: {e}")))
    }

    /// The pre-existing-`.flint/` pre-flight (D0.4).
    ///
    /// Reserving `.flint/` is a BREAKING change for a workspace already
    /// using it as data: its writes silently stop publishing and —
    /// worse — files literally named `.flint/publish` or `.flint/sync`
    /// would be *consumed* (renamed away) by the sentinel poll: a data
    /// grab from a non-participating workspace. When either signature
    /// is present, sentinel consumption is disabled for the workspace:
    /// the poll arm never arms, `capabilities.json` reports no verbs
    /// with the reason, and the operator surfaces it as
    /// `SentinelVerbsActive=False`.
    ///
    /// The verdict is STICKY once written — see `POSTURE`.
    pub fn sentinel_preflight(&self) -> LeanResult<SentinelPosture> {
        if self.cfg.sentinel_mode == super::SentinelMode::Off {
            let posture =
                SentinelPosture { enabled: false, reason: Some("sentinels-off".into()) };
            write_json(&self.posture_path(), &posture)?;
            return Ok(posture);
        }
        if let Some(prior) = self.load_posture()? {
            if !prior.enabled && self.cfg.sentinel_mode != super::SentinelMode::Force {
                return Ok(prior);
            }
        }

        // (a) the baseline cites a path under the reserved namespace.
        let baseline = self.state.load_baseline()?;
        let legacy_cited = baseline.entries.keys().any(|p| super::scan::is_control_path(p))
            || baseline.inst_base.keys().any(|p| super::scan::is_control_path(p));

        // (b) a sentinel-named file exists before this tree has ever
        //     been given a capability marker (i.e. before any sidecar
        //     that understands the protocol ran here).
        let never_marked = !self.control_path(CAPABILITIES).exists();
        let sentinel_named = self.control_path(PUBLISH).exists() || self.control_path(SYNC).exists();
        let preexisting = legacy_cited || (never_marked && sentinel_named);

        let posture = if preexisting && self.cfg.sentinel_mode != super::SentinelMode::Force {
            SentinelPosture { enabled: false, reason: Some("preexisting-flint-paths".into()) }
        } else if preexisting {
            // `force` requires the operator of record to have accepted
            // the consumption behavior explicitly.
            SentinelPosture { enabled: true, reason: Some("forced-over-preexisting".into()) }
        } else {
            SentinelPosture { enabled: true, reason: None }
        };
        write_json(&self.posture_path(), &posture)?;
        Ok(posture)
    }

    /// Write `.flint/capabilities.json` for the current posture.
    pub fn write_capabilities(&self, posture: &SentinelPosture, fenced: bool) -> LeanResult<()> {
        let verbs: Vec<String> = if fenced || !posture.enabled {
            vec![]
        } else {
            ["publish", "sync", "remote-seq"].iter().map(|s| s.to_string()).collect()
        };
        let holder_id = self
            .lease
            .as_ref()
            .map(|l| l.holder_id.clone())
            .or_else(|| {
                self.state.load_incarnation().ok().flatten().map(|i| i.holder_id)
            })
            .unwrap_or_else(|| "unclaimed".into());
        let caps = Capabilities {
            protocol: SENTINEL_PROTOCOL,
            verbs,
            boundary_mode: self.cfg.boundary_mode.as_str().to_string(),
            state: if fenced { "fenced".into() } else { "live".into() },
            reason: posture.reason.clone(),
            sentinel_min_interval_secs: self.cfg.sentinel_min_interval_secs,
            sentinel_hourly_budget: self.cfg.sentinel_hourly_budget,
            sidecar_version: super::SIDECAR_VERSION.to_string(),
            boot: BootStamp { holder_id, boot_unix: now_unix() },
        };
        write_json(&self.control_path(CAPABILITIES), &caps)
    }

    pub fn read_capabilities(&self) -> Option<Capabilities> {
        let bytes = std::fs::read(self.control_path(CAPABILITIES)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn load_remote_seq(&self) -> RemoteSeq {
        std::fs::read(self.control_path(REMOTE_SEQ))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Move the ticker (D5). `observed` is what the barrier's existing
    /// manifest HEAD (or its completed install) already told us — this
    /// never issues a request of its own.
    ///
    /// `updated_unix` refreshes on every call (the liveness heartbeat);
    /// `observed_seq`/`observed_etag` change only when they change.
    pub fn touch_remote_seq(
        &self,
        observed_seq: Option<u64>,
        observed_etag: Option<String>,
        integrated_seq: u64,
    ) -> LeanResult<()> {
        let mut t = self.load_remote_seq();
        if let Some(seq) = observed_seq {
            t.observed_seq = seq;
        }
        if observed_etag.is_some() {
            t.observed_etag = observed_etag;
        }
        t.integrated_seq = integrated_seq;
        if t.observed_seq < integrated_seq {
            // Our own install is the newest news we know of.
            t.observed_seq = integrated_seq;
        }
        // A request the workspace has already integrated past is stale
        // news and self-clears (D14).
        if t.sync_requested_at_seq.map(|at| integrated_seq > at).unwrap_or(false) {
            t.sync_requested_unix = None;
            t.sync_requested_by = None;
            t.sync_requested_at_seq = None;
        }
        t.updated_unix = now_unix();
        write_json(&self.control_path(REMOTE_SEQ), &t)
    }

    /// Carry an advisory sync request into the ticker (D14). The
    /// asymmetry with `boundary_request` is about blast radius, not
    /// principle: a boundary publishes what is already on disk and
    /// touches no local file, whereas `sync` re-derives the tree
    /// against the current remote manifest and DELETES local files for
    /// remotely-deleted paths. Performing that on a remote's say-so
    /// would upgrade what a leaked gateway bearer can do from "publish,
    /// plus hand over these N named objects" to "rewrite and delete
    /// across a running agent's tree, at my timing, under a scope I
    /// choose".
    pub fn carry_sync_request(&self, requested_unix: u64, requestor: &str) -> LeanResult<()> {
        let mut t = self.load_remote_seq();
        let newer = t.sync_requested_unix.map(|p| requested_unix > p).unwrap_or(true);
        if !newer {
            return Ok(());
        }
        t.sync_requested_unix = Some(requested_unix);
        t.sync_requested_by = Some(requestor.to_string());
        t.sync_requested_at_seq = Some(t.integrated_seq);
        t.updated_unix = now_unix();
        write_json(&self.control_path(REMOTE_SEQ), &t)
    }
}
