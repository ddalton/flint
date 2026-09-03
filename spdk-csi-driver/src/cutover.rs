// cutover.rs — reassembly cutover orchestrator (incremental-rebuild phase 4,
// §6 "cutover opportunities").
//
// A warm standby (phase 3) only rejoins the raid at the next assembly —
// which `admit_standbys_at_stage` handles — but nothing guarantees an
// assembly ever happens: an RWO pod can run for months, an RWX NFS server
// pod likewise. This module creates the opportunity, deliberately and
// verifiably:
//
// - **RWX volumes**: bounce the volume's `flint-nfs-server` pod. It is a
//   bare pod (no controller recreates it), so the bounce is
//   delete → wait for the pod to be gone AND the synthetic PV's
//   VolumeAttachment to detach → recreate from the sanitized original spec.
//   The detach wait closes the §6 same-node race: recreating before kubelet
//   unstages can land the pod on the same node where the staged volume is
//   reused — no NodeStage, no reassembly, clients ate a restart for
//   nothing. Honest scoping (§6): the shipped NFS server holds NFSv4 state
//   in memory, so a bounce costs clients the 90s grace-window recovery;
//   stateless I/O rides through.
// - **RWO volumes**: opt-in only, via the PV annotation
//   `disk.chert.us/rejoin-bounce: "enabled"` — bouncing a workload
//   pod is an application restart and never the driver's call to make
//   unilaterally. The pods using the volume's claim are deleted; their
//   owning controller reschedules them.
//
// **Verification, not hope** (§6: "verify the outcome, don't assume it"):
// every bounce is tracked and judged on later cycles. Standbys that flipped
// to in_sync → `CutoverSucceeded`. Still standby after the cooldown →
// `CutoverIneffective` (same-node reuse, failed stage, or a deferred
// admission) and the volume becomes eligible for another attempt. The
// scheduling-hint escalation (cordon/anti-affinity) is deliberately not
// implemented here — an ineffective bounce is surfaced, not silently
// retried forever.
//
// A bounce is only planned when every standby is READY: lag ≤ max_lag
// epochs (chase converged), so the NodeStage final delta is small and the
// admission will not blow its budget. DEFAULT ON since wave 2 (contract
// R4; see CutoverConfig::enabled) — disable with FLINT_CUTOVER=disabled.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{PersistentVolume, Pod};
use k8s_openapi::api::storage::v1::VolumeAttachment;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use tracing::{debug, info, warn};

use crate::driver::SpdkCsiDriver;
use crate::freshness_gate::LegAvailability;
use crate::replica_sync::{self, epoch_seq, SyncState, VolumeSyncRecord};

pub type RpcError = Box<dyn std::error::Error + Send + Sync>;

/// PV annotation opting an RWO volume into workload-pod bounces.
pub const REJOIN_BOUNCE_ANNOTATION: &str = "disk.chert.us/rejoin-bounce";

/// PV annotation set by the node agent when a volume is ATTACHED to a node
/// but its raid bdev does not exist there — a dead data path the health
/// monitor cannot see (its stale predicate requires an online raid; phase-6
/// yield, bug 1). Value = the flagging node's name; only that node clears
/// it (raid reappeared, or the attachment left). Consumers: operators
/// (event + annotation), and the future in-place repair / bounce fallback.
pub const DATA_PATH_LOST_ANNOTATION: &str = "disk.chert.us/data-path-lost";

/// NoSchedule taint applied to the bounced workload's node for the duration
/// of a bounce (scheduling escalation, phase-6 follow-up): without it the
/// scheduler re-places the pod on the same node about half the time,
/// kubelet reuses the staged volume, no reassembly happens, and the bounce
/// bought a workload restart for nothing — observed defeating BOTH bounce
/// types live. The taint value encodes the application time (unix seconds)
/// so expiry survives controller restarts; the tick sweeps expired taints.
/// A taint chosen over cordon so operator cordon state is never touched,
/// and over pod anti-affinity because RWO replacements come from the
/// workload's own controller template, which flint cannot mutate. On a
/// cluster with no alternative node the taint still works: it outlives
/// kubelet's unstage, so even a same-node replacement must restage.
pub const BOUNCE_TAINT_KEY: &str = "disk.chert.us/bounce";

/// PV annotation claiming the recreate of a volume's NFS server pod for the
/// duration of a bounce. The value is the claim's EXPIRY (unix seconds), not
/// its application time.
///
/// Storing the deadline rather than the start is deliberate: the wait this
/// claim protects is `detach_timeout`, which is operator-configurable
/// (`FLINT_CUTOVER_DETACH_TIMEOUT_SECS`). With a fixed TTL on the reader's
/// side, raising that timeout past the TTL would silently expire the claim
/// while the bouncer was still waiting — and the double-creator race would
/// come back with no signal at all. The writer sizes the deadline from its own
/// timeout; the reader needs no knowledge of it.
///
/// This exists because the bare `flint-nfs-<vol>` pod has TWO independent
/// creators: `execute_cutover`'s recreate and `rwx_nfs.rs`'s liveness
/// reconciler, each on its own tokio task in this one process, under one
/// lease, with no mutual exclusion. The detach wait deliberately holds the
/// pod ABSENT with client attachments intact — precisely the reconciler's one
/// `Recreate` cell — so without this claim the reconciler rebuilds the pod
/// inside the wait, kubelet reuses the staged volume, and the bounce is
/// silently defeated (formal run `BouncePod`, eight states).
pub const BOUNCE_IN_FLIGHT_ANNOTATION: &str = "disk.chert.us/bounce-in-flight";

/// Margin added to `detach_timeout` when sizing a claim's deadline, covering
/// the delete and the recreate round trips around the wait itself.
pub const BOUNCE_CLAIM_MARGIN_SECS: u64 = 60;

/// The READER's cap on how far in the future a claim may sit. Boundedness is
/// enforced here, not by trusting the writer: a bouncer that dies mid-window
/// must not be able to disable the only other actor that can rebuild the
/// volume's server, and neither must a bug or a bad clock that writes a
/// far-future deadline. Comfortably above any sane
/// `detach_timeout + margin`, and far below any human response time.
pub const BOUNCE_CLAIM_MAX_HORIZON_SECS: u64 = 900;

/// Deadline for a claim protecting a wait of `detach_timeout`, CLAMPED to the
/// reader's horizon.
///
/// Without the clamp the two bounds fight each other and the belt loses
/// silently: a `detach_timeout` above `MAX_HORIZON - MARGIN` (840s) would
/// write a deadline the reader rejects as absurd, so the claim would be inert
/// for exactly the window it exists to protect — reintroducing the
/// double-creator race in the one configuration that most needs it. Clamping
/// keeps the claim live for as long as the reader will honour it; the residual
/// gap (a wait longer than the horizon) is reported by
/// `bounce_claim_covers_wait` so it is never silent.
pub fn bounce_claim_deadline(now_epoch_secs: u64, detach_timeout: Duration) -> u64 {
    let want = detach_timeout
        .as_secs()
        .saturating_add(BOUNCE_CLAIM_MARGIN_SECS)
        .min(BOUNCE_CLAIM_MAX_HORIZON_SECS);
    now_epoch_secs.saturating_add(want)
}

/// Whether a claim can cover a whole `detach_timeout` wait. False means the
/// configured timeout exceeds the reader's horizon, so the tail of the wait is
/// unprotected — worth one startup warning, never a silent hole.
pub fn bounce_claim_covers_wait(detach_timeout: Duration) -> bool {
    detach_timeout.as_secs().saturating_add(BOUNCE_CLAIM_MARGIN_SECS)
        <= BOUNCE_CLAIM_MAX_HORIZON_SECS
}

/// Is a recreate claim live? Absent, unparseable, already expired, or sitting
/// absurdly far in the future all read as "no claim" — never strand a volume's
/// server behind a stale or bogus marker (the same fail-open doctrine as
/// `expired_bounce_taints`).
pub fn bounce_claim_active(
    value: Option<&str>,
    now_epoch_secs: u64,
    max_horizon_secs: u64,
) -> bool {
    let Some(deadline) = value.and_then(|v| v.trim().parse::<u64>().ok()) else {
        return false;
    };
    deadline > now_epoch_secs && deadline.saturating_sub(now_epoch_secs) <= max_horizon_secs
}

/// PV annotation carrying this volume's consecutive-bounce bookkeeping,
/// `"<count>|<last_attempt_unix>"`.
///
/// `cutover.rs` shipped with NO attempt counter, no backoff and no negative
/// caching anywhere (formal canary `Inv_NoPointlessRebounce`, run
/// `BounceLoop`), which left three separately-sufficient churn doors: the
/// `Err` arm records nothing so the documented 900s minimum never applied on
/// ANY failure path; the `CutoverIneffective` verdict drops the attempt and
/// declares the volume eligible again immediately; and both live only in a
/// stack-local `HashMap`, so a controller restart forgets everything. One
/// persisted counter closes all three, and persisting it is also what makes
/// the backoff survive the restart the in-memory map could not.
pub const CUTOVER_ATTEMPTS_ANNOTATION: &str = "disk.chert.us/cutover-attempts";

/// Cap on the backoff multiplier: 1×, 2×, 4×, 8× the cooldown, then flat.
/// Bounded on purpose — an unbounded doubling becomes indistinguishable from
/// "never retry", and a volume whose bounces keep failing still deserves a
/// periodic attempt once an operator has fixed whatever was wrong.
pub const CUTOVER_BACKOFF_CAP_MULT: u32 = 8;

pub fn encode_attempts(count: u32, at_epoch_secs: u64) -> String {
    format!("{}|{}", count, at_epoch_secs)
}

/// Absent, malformed, or partially-parseable values read as "no history" —
/// bookkeeping must never be able to wedge a volume shut.
pub fn decode_attempts(value: &str) -> Option<(u32, u64)> {
    let (c, t) = value.trim().split_once('|')?;
    Some((c.trim().parse().ok()?, t.trim().parse().ok()?))
}

/// Exponential, capped: attempt 1 waits `base`, 2 waits 2×, 3 waits 4×,
/// 4+ waits `cap_mult`×.
pub fn attempt_backoff_secs(count: u32, base_secs: u64, cap_mult: u32) -> u64 {
    if count == 0 {
        return 0;
    }
    let mult = 1u32.checked_shl(count - 1).unwrap_or(cap_mult).min(cap_mult);
    base_secs.saturating_mul(mult as u64)
}

/// Whether a new bounce may be planned for this volume yet.
#[derive(Debug, Clone, PartialEq)]
pub enum AttemptGate {
    Allow,
    /// Within the backoff window for `count` prior consecutive attempts.
    Backoff { count: u32, remaining_secs: u64 },
}

/// The persisted half of the churn belt. Read from the PV the tick already
/// holds, so it costs no extra API call.
pub fn attempt_gate(
    annotation: Option<&str>,
    now_epoch_secs: u64,
    base_secs: u64,
    cap_mult: u32,
) -> AttemptGate {
    let Some((count, last)) = annotation.and_then(decode_attempts) else {
        return AttemptGate::Allow;
    };
    if count == 0 {
        return AttemptGate::Allow;
    }
    let wait = attempt_backoff_secs(count, base_secs, cap_mult);
    // A clock that moved backwards must not read as an eternal backoff.
    let elapsed = now_epoch_secs.saturating_sub(last);
    if elapsed >= wait {
        AttemptGate::Allow
    } else {
        AttemptGate::Backoff { count, remaining_secs: wait - elapsed }
    }
}

/// The node that wrote a `data-path-lost` flag. The value is
/// `"<node>|<since>"`; pre-R4 flags are a bare node name.
pub fn flag_owner_node(value: &str) -> &str {
    value.split_once('|').map(|(n, _)| n).unwrap_or(value).trim()
}

/// Bounce taints whose encoded application time is older than `ttl_secs`.
/// Unparseable values count as expired — never strand a node.
pub fn expired_bounce_taints(
    taints: &[(String, String)],
    now_epoch_secs: u64,
    ttl_secs: u64,
) -> Vec<String> {
    taints
        .iter()
        .filter(|(_, value)| {
            value
                .parse::<u64>()
                .map(|applied| now_epoch_secs.saturating_sub(applied) > ttl_secs)
                .unwrap_or(true)
        })
        .map(|(node, _)| node.clone())
        .collect()
}

/// One node-agent observation about one attached volume's data path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DataPathAction {
    /// Set the annotation + Warning event (confirmed lost).
    Flag,
    /// Remove this node's annotation + Normal event (healed or moved).
    Clear,
    /// Do nothing this tick.
    Hold,
}

/// Pure verdict for the node agent's data-path pass. `strikes_with_this`
/// counts consecutive raid-missing observations INCLUDING the current one;
/// `threshold` rides out an in-flight NodeStage, whose VA legitimately
/// precedes the raid by up to the stage-delta budget.
pub fn data_path_verdict(
    attached_here: bool,
    raid_present: bool,
    flagged_by_me: bool,
    strikes_with_this: u32,
    threshold: u32,
) -> DataPathAction {
    if !attached_here || raid_present {
        // Healed, or no longer this node's concern: clear our own flag.
        return if flagged_by_me { DataPathAction::Clear } else { DataPathAction::Hold };
    }
    if strikes_with_this >= threshold && !flagged_by_me {
        return DataPathAction::Flag;
    }
    DataPathAction::Hold
}

/// Is the IN-PLACE repair due this tick? Pure companion to
/// `data_path_verdict`, and deliberately a SEPARATE threshold from it.
///
/// The two actions cost different things, so they should not share a
/// confirmation count:
///
///   repair (this) — rebuild the raid chain in place. Idempotent, no consumer
///     disruption, serialised against NodeStage/NodeUnstage by the per-volume
///     lock, and refused outright unless kubelet still has the volume staged
///     on this node. Cheap enough to attempt one tick sooner.
///   flag (`data_path_verdict`) — hand the volume to the controller's
///     data-path arm, which can end in a pod bounce. Keeps the extra tick.
///
/// Not zero-threshold: at one strike this is the in-flight-stage race (the VA
/// legitimately precedes the raid). The lock makes that safe rather than
/// corrupting, but the repair would be pointless work, so hold one tick.
pub fn repair_due(
    attached_here: bool,
    raid_present: bool,
    strikes_with_this: u32,
    threshold: u32,
) -> bool {
    attached_here && !raid_present && strikes_with_this >= threshold
}

/// First-observation visibility for a total data-path collapse (7b-3 P1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CollapseEvent {
    /// Emit the Warning event NOW — a raid this agent has previously
    /// observed present just vanished under a live attachment.
    Lost,
    /// The raid returned; close the warned episode (Normal event).
    Restored,
    /// Nothing to say this tick.
    None,
}

/// The strike threshold above exists to ride out an in-flight NodeStage,
/// whose VA legitimately precedes the raid — but it also made a TOTAL
/// collapse (raid bdev unregistered under a mounted filesystem) silent
/// whenever layer-2 repair won the race to the third strike (drill C: a
/// 2-minute hard EIO outage produced no event at all). `previously_seen`
/// distinguishes the two: a raid this agent has observed present for this
/// volume cannot be "still staging" — its absence is a collapse, and the
/// operator hears about it on the FIRST strike. One event per episode
/// (`already_warned`); flagging/repair cadence is unchanged.
pub fn raid_collapse_verdict(
    attached_here: bool,
    raid_present: bool,
    previously_seen: bool,
    already_warned: bool,
) -> CollapseEvent {
    if attached_here && !raid_present && previously_seen && !already_warned {
        CollapseEvent::Lost
    } else if raid_present && already_warned {
        CollapseEvent::Restored
    } else {
        CollapseEvent::None
    }
}

#[derive(Debug, Clone)]
pub struct CutoverConfig {
    /// FLINT_CUTOVER — DEFAULT ON since wave 2 (contract R4): the bounce
    /// is the escalation ladder's terminal rung, and shipping it default-off
    /// while the repair loops that starve it ran default-on was exactly the
    /// F38(c) starvation. Disable with FLINT_CUTOVER=disabled.
    pub enabled: bool,
    /// Minimum wall clock between bounce attempts for one volume, and the
    /// window after which an unverified bounce is declared ineffective.
    /// FLINT_CUTOVER_COOLDOWN_SECS, default 900.
    pub cooldown: Duration,
    /// A standby may trail by at most this many epochs to be "ready" —
    /// beyond it the NodeStage final delta would likely blow its budget.
    /// FLINT_CUTOVER_MAX_LAG, default 1.
    pub max_lag: u64,
    /// How long the NFS bounce waits for the old pod to disappear and the
    /// synthetic PV to detach before recreating (closes the same-node
    /// reuse race). FLINT_CUTOVER_DETACH_TIMEOUT_SECS, default 120.
    pub detach_timeout: Duration,
    /// Scheduling escalation: taint the bounced workload's node
    /// (NoSchedule) so the replacement cannot reuse the staged volume.
    /// FLINT_CUTOVER_ESCALATION, default on; FLINT_CUTOVER_TAINT_SECS
    /// sets the taint's lifetime (default 120 — must outlive kubelet's
    /// unstage so even a same-node landing restages).
    pub escalation: bool,
    pub taint_ttl: Duration,
    /// S2: converged-standby ADMISSION on RWX volumes belongs to the
    /// in-place hot-rejoin window, not the bounce — this planner keeps
    /// only the data-path-lost (relocation) arm for them. Mirrors
    /// FLINT_RWX_INPLACE_ADMISSION (default ON); with the kill switch
    /// off, the admission bounce returns.
    pub rwx_inplace: bool,
    /// The freshness gate's thresholds, for the preflight's excusability line.
    ///
    /// NOTE what this does NOT buy: the gate runs in the NODE process
    /// (NodeStage → `freshness_gate::evaluate`) while this config is parsed in
    /// the CONTROLLER, and the chart sets `FLINT_F36C_*` on the DaemonSet only.
    /// Sharing a struct field therefore guarantees nothing across the process
    /// boundary — an operator who retunes the gate on the node side and not the
    /// controller side WILL drift the two. Closing that properly means plumbing
    /// the knobs into both templates; until then the shared DEFAULTS are what
    /// keeps them aligned, and this comment is the honest version of a claim
    /// that was previously overstated here.
    pub gate: crate::freshness_gate::GateConfig,
}

impl Default for CutoverConfig {
    fn default() -> Self {
        CutoverConfig {
            enabled: true,
            cooldown: Duration::from_secs(900),
            max_lag: 1,
            detach_timeout: Duration::from_secs(120),
            escalation: true,
            taint_ttl: Duration::from_secs(120),
            rwx_inplace: true,
            gate: crate::freshness_gate::GateConfig::default(),
        }
    }
}

impl CutoverConfig {
    pub fn from_env() -> Self {
        let d = CutoverConfig::default();
        CutoverConfig {
            enabled: std::env::var("FLINT_CUTOVER")
                .map(|v| {
                    !(v.eq_ignore_ascii_case("disabled")
                        || v.eq_ignore_ascii_case("false")
                        || v == "0")
                })
                .unwrap_or(d.enabled),
            cooldown: std::env::var("FLINT_CUTOVER_COOLDOWN_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.cooldown),
            max_lag: std::env::var("FLINT_CUTOVER_MAX_LAG")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(d.max_lag),
            detach_timeout: std::env::var("FLINT_CUTOVER_DETACH_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.detach_timeout),
            escalation: std::env::var("FLINT_CUTOVER_ESCALATION")
                .map(|v| !(v.eq_ignore_ascii_case("disabled") || v == "0" || v.eq_ignore_ascii_case("false")))
                .unwrap_or(d.escalation),
            taint_ttl: std::env::var("FLINT_CUTOVER_TAINT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.taint_ttl),
            // One env var, one parse — both planners must read the same
            // switch or admissions race (both are Resolver-class claims,
            // first-come between them).
            rwx_inplace: crate::hot_rejoin::rwx_inplace_admission_enabled(),
            gate: crate::freshness_gate::GateConfig::from_env(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NfsPodRef {
    pub namespace: String,
    pub name: String,
    /// Only a PVC-backed NFS pod stages the volume — an emptyDir one has no
    /// raid to reassemble.
    pub pvc_backed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PodRef {
    pub namespace: String,
    pub name: String,
}

/// Everything the planner needs about one volume, gathered by the tick.
#[derive(Debug, Clone)]
pub struct VolumeCutoverView {
    pub volume_id: String,
    pub record: VolumeSyncRecord,
    /// Node consuming the volume per its VolumeAttachment (RWO path).
    pub consumer: Option<String>,
    /// The volume's `flint-nfs-{vol}` server pod, if any (RWX path).
    pub nfs_pod: Option<NfsPodRef>,
    /// PV annotation `disk.chert.us/rejoin-bounce` == "enabled".
    pub rwo_bounce_enabled: bool,
    /// Workload pods mounting the volume's claim.
    pub workload_pods: Vec<PodRef>,
    /// The data-path-lost annotation is set (layer-1 detection flagged a
    /// dead consumer data path AND the layer-2 in-place repair failed —
    /// ublk frontend, aborted filesystem, or an unrecoverable export).
    /// Debounced by the loop before it reaches the planner.
    pub data_path_lost: bool,
    /// Node the volume's NFS server pod runs on — where its raid assembles,
    /// under the BACKING handle (RWX only).
    pub nfs_server_node: Option<String>,
    /// A preflight refusal has been standing longer than the refusal bound.
    /// The tick owns the clock; the belt only reads the verdict. See
    /// `bounce_preflight` for why an unbounded belt is a liveness bug.
    pub refusal_expired: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CutoverDecision {
    /// Delete + recreate the bare NFS server pod (RWX).
    BounceNfsPod,
    /// Delete the workload pods using the claim (RWO, opt-in).
    BounceWorkloadPods,
    /// Nothing to do; the reason is for operator-facing logs.
    Wait(&'static str),
}

/// Decide whether this volume gets a bounce now. Pure — the §6 conditions:
/// a standby exists, every standby has converged (lag ≤ max_lag, so the
/// NodeStage final delta is small), and the volume is actually consumed
/// (otherwise the next natural stage admits the standby for free).
pub fn plan_cutover(view: &VolumeCutoverView, cfg: &CutoverConfig) -> CutoverDecision {
    let vol = &view.volume_id;

    // Layer 3 (phase-6): a dead data path the in-place repair could not
    // fix. The bounce IS the remediation — a restage rebuilds the raid
    // from the in-sync replicas — so the standby/lag gates below do not
    // apply (there is nothing to admit, only a data path to rebuild).
    if view.data_path_lost {
        if let Some(nfs) = &view.nfs_pod {
            if nfs.pvc_backed {
                return CutoverDecision::BounceNfsPod;
            }
            return CutoverDecision::Wait("data path lost but NFS pod is not PVC-backed");
        }
        if view.consumer.is_some() {
            if !view.rwo_bounce_enabled {
                return CutoverDecision::Wait(
                    "data path lost and in-place repair failing; rejoin-bounce not enabled — \
                     operator must bounce the workload (or enable the annotation)",
                );
            }
            if view.workload_pods.is_empty() {
                return CutoverDecision::Wait("data path lost but no workload pods found");
            }
            return CutoverDecision::BounceWorkloadPods;
        }
        return CutoverDecision::Wait(
            "data path lost but volume not attached — the next stage rebuilds it",
        );
    }

    let standbys: Vec<&_> = view
        .record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::Standby)
        .collect();
    if standbys.is_empty() {
        return CutoverDecision::Wait("no standby replica");
    }
    let latest = view.record.latest_epoch_seq(vol);
    if latest == 0 {
        return CutoverDecision::Wait("no epoch history");
    }
    for rec in &standbys {
        let Some(seq) = rec.last_epoch.as_deref().and_then(|e| epoch_seq(vol, e)) else {
            return CutoverDecision::Wait("standby mark unreadable — not ready");
        };
        if latest.saturating_sub(seq) > cfg.max_lag {
            return CutoverDecision::Wait("standby lag above threshold — chase has not converged");
        }
    }

    if let Some(nfs) = &view.nfs_pod {
        if !nfs.pvc_backed {
            return CutoverDecision::Wait("NFS pod is not PVC-backed — nothing to reassemble");
        }
        // S2: admission of a converged standby is the in-place window's
        // job (hot_rejoin, on the NFS server's node) — the bounce here
        // survives only for relocation (the data_path_lost arm above)
        // and as the kill-switch fallback.
        if cfg.rwx_inplace {
            return CutoverDecision::Wait(
                "converged standby on an RWX volume — in-place admission owns it (FLINT_RWX_INPLACE_ADMISSION)",
            );
        }
        return CutoverDecision::BounceNfsPod;
    }
    if view.consumer.is_some() {
        if !view.rwo_bounce_enabled {
            return CutoverDecision::Wait(
                "volume attached; rejoin-bounce annotation not enabled — waiting for a natural reassembly",
            );
        }
        if view.workload_pods.is_empty() {
            return CutoverDecision::Wait("no workload pods found using the claim");
        }
        return CutoverDecision::BounceWorkloadPods;
    }
    CutoverDecision::Wait("volume not attached — the next stage admits the standby naturally")
}

/// What `execute_cutover` did. Replaces a bare `bool` so the TICK owns the
/// operator-facing surface: a refusal repeats every 60s for as long as a writer
/// stays unavailable, and `emit_pv_event` creates a NEW Event object per call
/// (nanosecond-unique name, no aggregation), so emitting from inside would put
/// one event per volume per minute into etcd — drowning exactly the signal F60
/// §4 complains is missing. The tick dedupes on the reason instead.
#[derive(Debug, Clone, PartialEq)]
pub enum CutoverOutcome {
    /// A teardown happened: start the verification clock and charge an attempt.
    Issued,
    /// A belt refused before tearing anything down. Not an attempt.
    Refused(String),
}

/// Verdict of the commit-time bounce preflight.
#[derive(Debug, Clone, PartialEq)]
pub enum PreflightVerdict {
    /// Every recorded writer is answering, or verifiably gone — a teardown can
    /// be expected to come back whole.
    Go,
    /// At least one recorded writer is neither: tearing down now would
    /// manufacture an outage the failure alone would not have caused.
    Refuse(String),
}

/// What the preflight knows about one recorded writer at commit time.
#[derive(Debug, Clone, PartialEq)]
pub struct WriterEvidence {
    pub uuid: String,
    pub node: String,
    /// GROUND TRUTH from the serving raid: is this writer's base actually
    /// configured? `None` = the raid could not be probed (already torn down,
    /// not online, or mid-transition), which is a different question — see
    /// `bounce_preflight`.
    pub base_configured: Option<bool>,
    /// The writer's node condition, or `None` if the API was unreadable.
    pub availability: Option<LegAvailability>,
}

/// THE COMMIT-TIME PREFLIGHT (F60 §1, formal `BouncePreflight`).
///
/// `plan_cutover` decides to tear down a serving data path reading only
/// `sync_state` and `last_epoch`; `VolumeCutoverView` carries no leg health at
/// all. The model's `BounceRisk` run turns that into a counterexample with ONE
/// bouncer, the lease fully honoured and no race: a stale data-path flag fires
/// a bounce while a second writer is transiently gone, and the reassembly can
/// only return by excusing an acked tail that was recoverable all along — the
/// controller manufactures both the outage and the hollow risk marker.
///
/// **The evidence is the RAID, not the API server.** The model's guard is
/// `Responsive(w)`, which is ground truth about the LEG's data path — and a leg
/// dies while its node stays Ready all the time here: F33's documented "Ready
/// does not guarantee a live tgt", F42's dead remote target, the csi-node roll
/// landmine. A node-condition-only belt therefore passes its own
/// counterexample whenever the leg dies leg-locally, which is the common case.
/// So this reads `base_configured` from the live raid, exactly as `drain_leg`
/// probes before committing its record round, and uses node conditions only for
/// the second disjunct (`deemedDead`).
///
/// Node conditions decide only whether an ABSENT writer is honestly excusable:
/// `NodeGone`, or NotReady past the gate's own `node_gone_secs`, is evidence the
/// gate will accept as permanent, so serving without that writer surfaces an
/// honest risk rather than a hollow one. Anything else is the hazard window.
///
/// `refusal_expired` is the bound. `freshness_gate::evaluate` is deliberately
/// deadline-bounded ("Never hang — the 2.4 obligation"); a belt with no such
/// bound would block the escalation ladder's TERMINAL rung indefinitely, and on
/// the data-path arm the volume is ALREADY down, so refusing lengthens an
/// outage instead of preventing one. A flapping kubelet keeps resetting
/// `lastTransitionTime`, so `node_gone_secs` alone never trips. Once the bound
/// passes, the bounce proceeds and says so.
pub fn bounce_preflight(
    writers: &[WriterEvidence],
    gate: &crate::freshness_gate::GateConfig,
    refusal_expired: bool,
) -> PreflightVerdict {
    if refusal_expired {
        return PreflightVerdict::Go;
    }
    // Honest excusability, per the gate's OWN threshold so the two cannot
    // drift: past it the gate stops calling a NotReady node recoverable.
    let verifiably_gone = |a: &Option<LegAvailability>| match a {
        Some(LegAvailability::NodeGone) => true,
        Some(LegAvailability::NodeNotReady { not_ready_secs }) => {
            *not_ready_secs >= gate.node_gone_secs
        }
        _ => false,
    };
    for w in writers {
        // Answering in the live raid: this leg survives a restage.
        if w.base_configured == Some(true) {
            continue;
        }
        // Absent but verifiably gone: the replacement machinery owns it and the
        // gate will excuse it soundly. Refusing here would strand the volume
        // behind a node that is never coming back.
        if verifiably_gone(&w.availability) {
            continue;
        }
        let why = match (w.base_configured, &w.availability) {
            (Some(false), Some(LegAvailability::NodeReady)) => {
                "its base is NOT configured in the serving raid although its node reads Ready \
                 (a dead tgt, a rolled node, or a fault-out the monitor has not marked yet)"
                    .to_string()
            }
            (Some(false), Some(LegAvailability::NodeNotReady { not_ready_secs })) => format!(
                "its base is not configured and its node has been NotReady only {}s, inside the \
                 {}s recoverable window",
                not_ready_secs, gate.node_gone_secs
            ),
            (Some(false), _) => {
                "its base is not configured and its node condition is unreadable".to_string()
            }
            (None, _) => "the serving raid could not be probed, so its data path is unverified"
                .to_string(),
            (Some(true), _) => unreachable!("configured writers were skipped above"),
        };
        return PreflightVerdict::Refuse(format!(
            "recorded writer {} on {} is neither answering nor verifiably gone: {} — a bounce now \
             would tear down a volume whose reassembly must then defer or excuse a recoverable \
             writer",
            w.uuid, w.node, why
        ));
    }
    PreflightVerdict::Go
}

/// The set the preflight must clear: the UNION of `record.writer_uuids()` and
/// the replicas the record calls in_sync.
///
/// Neither alone is right. `writer_uuids()` is what the freshness gate demands
/// back after a teardown — and it is NOT the in-sync set: the below-2-base
/// forced-stale fallback stamps Stale legs into the writer set wholesale
/// without marking them in_sync, so a belt keyed on in_sync would skip exactly
/// the legs the gate then waits for. But `writer_uuids()` is only populated by
/// `set_writer_set` AT ASSEMBLY, so a record that has not assembled since it
/// was created has an EMPTY one — and a belt keyed on that alone would return
/// "all clear" on zero evidence, vacuously passing precisely when the record is
/// most degraded. The union closes both holes and is the conservative reading:
/// tearing down while a member of either set is transiently absent is the
/// hazard.
pub fn recorded_writers(record: &VolumeSyncRecord) -> Vec<(String, String)> {
    let gate_writers = record.writer_uuids();
    record
        .replicas
        .iter()
        .filter(|r| {
            r.sync_state == SyncState::InSync || gate_writers.iter().any(|w| w == &r.lvol_uuid)
        })
        .map(|r| (r.lvol_uuid.clone(), r.node_name.clone()))
        .collect()
}

/// Strip the server-populated fields so a fetched pod can be re-created.
/// `node_name` is cleared on purpose: the scheduler must place the
/// replacement fresh (the spec's affinity still steers it to replica
/// nodes); a pinned node guarantees the same-node staged-volume reuse race.
pub fn sanitized_for_recreate(mut pod: Pod) -> Pod {
    pod.metadata.resource_version = None;
    pod.metadata.uid = None;
    pod.metadata.creation_timestamp = None;
    pod.metadata.deletion_timestamp = None;
    pod.metadata.deletion_grace_period_seconds = None;
    pod.metadata.managed_fields = None;
    pod.metadata.owner_references = None;
    pod.status = None;
    if let Some(spec) = pod.spec.as_mut() {
        spec.node_name = None;
    }
    pod
}

/// The subset of `uuids` whose replica is still a standby in `record` — a
/// bounce attempt is resolved once none remain (they were admitted in_sync,
/// or fell back to stale and need the catch-up again first).
pub fn standbys_still_pending(record: &VolumeSyncRecord, uuids: &[String]) -> Vec<String> {
    uuids
        .iter()
        .filter(|u| {
            record
                .get(u)
                .map(|r| r.sync_state == SyncState::Standby)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Cluster effects of a bounce, faked in unit tests.
#[async_trait]
pub trait CutoverOps: Sync {
    async fn get_pod(&self, namespace: &str, name: &str) -> Result<Option<Pod>, RpcError>;
    /// Delete a pod; absent is success (idempotent).
    async fn delete_pod(&self, namespace: &str, name: &str) -> Result<(), RpcError>;
    /// Wait (bounded) until the pod is gone and the PV's VolumeAttachment
    /// is detached. False on timeout.
    async fn await_detached(
        &self,
        namespace: &str,
        pod_name: &str,
        pv_name: &str,
        timeout: Duration,
    ) -> bool;
    async fn recreate_pod(&self, pod: Pod) -> Result<(), RpcError>;
    async fn emit(&self, volume_id: &str, event_type: &str, reason: &str, message: &str);
    /// Apply the bounce NoSchedule taint to `node`; `value` encodes the
    /// application time (unix seconds) for crash-safe expiry.
    async fn taint_node(&self, node: &str, value: &str) -> Result<(), RpcError>;
    /// Remove the bounce taint from `node` (absent is success).
    async fn untaint_node(&self, node: &str) -> Result<(), RpcError>;
    /// Nodes currently carrying the bounce taint, with their values.
    async fn list_bounce_taints(&self) -> Result<Vec<(String, String)>, RpcError>;
    /// Availability evidence for one leg's node, for the commit-time
    /// preflight. `None` = could not determine, which the preflight treats
    /// as UNSAFE (a bounce is destructive; blind must mean refuse — the
    /// opposite of the freshness gate's blind default).
    async fn node_availability(&self, node: &str) -> Option<LegAvailability>;
    /// Claim (`true`) or release (`false`) the recreate of this volume's NFS
    /// server pod, so the liveness reconciler does not rebuild it inside the
    /// bounce's detach wait. Written as a timestamped PV annotation, so it is
    /// crash-safe AND self-expiring — the same doctrine as the bounce taint.
    /// `deadline_epoch_secs` is `Some(expiry)` to claim, `None` to release.
    async fn set_bounce_in_flight(
        &self,
        volume_id: &str,
        deadline_epoch_secs: Option<u64>,
    ) -> Result<(), RpcError>;
    /// Persist (or clear) this volume's consecutive-bounce bookkeeping.
    async fn set_attempts(&self, volume_id: &str, value: Option<&str>) -> Result<(), RpcError>;
    /// Clear a `data-path-lost` flag from the PV that carries it (the volume's
    /// own PV for RWO, the backing PV for RWX).
    async fn clear_data_path_flag(&self, pv_name: &str) -> Result<(), RpcError>;
    /// GROUND TRUTH for the preflight: recorded writers whose base is NOT
    /// configured in the volume's serving raid on `consumer`. `None` = the raid
    /// could not be probed (absent, not online, or unreachable), which the
    /// preflight treats as "data path unverified" rather than as health.
    /// Delegates to `replica_sync::replicas_missing_from_raid`, the SAME
    /// matcher the freshness gate uses, so the belt and the gate can never
    /// disagree about which writers are absent.
    /// `raid_name` is resolved by the CALLER, because which handle stages a
    /// volume's raid depends on the arm: an RWX volume assembles under the
    /// BACKING handle on the NFS server's node, an RWO one under the user
    /// handle on its consumer (`identity::raid_name`'s typed parameter exists
    /// to force exactly this decision).
    async fn writers_absent_from_raid(
        &self,
        raid_name: &str,
        volume_id: &str,
        consumer: &str,
        record: &VolumeSyncRecord,
    ) -> Option<Vec<String>>;
}

/// Execute a planned bounce. Returns whether a bounce was actually issued
/// (the caller starts the verification clock on true).
pub async fn execute_cutover(
    ops: &dyn CutoverOps,
    view: &VolumeCutoverView,
    decision: &CutoverDecision,
    cfg: &CutoverConfig,
) -> Result<CutoverOutcome, RpcError> {
    if let CutoverDecision::Wait(reason) = decision {
        return Ok(CutoverOutcome::Refused((*reason).to_string()));
    }
    // THE COMMIT-TIME PREFLIGHT (F60 §1).  Gather evidence HERE, as late as
    // possible, rather than trusting the tick's gather snapshot: `is_leader()`
    // was checked once at the top of a tick whose per-volume work runs to
    // `detach_timeout`, and there is no CAS anywhere in this module to attach a
    // guard to.  Refusing is not a failure — no attempt is charged, so the
    // volume becomes eligible again the moment its writers answer or are
    // verifiably gone, or when the refusal bound expires.
    let writers = recorded_writers(&view.record);
    // Which node stages this volume's raid, and therefore under which handle:
    // the NFS server's node under the BACKING handle for RWX, the consumer
    // under the user handle for RWO.
    let sid = crate::identity::StorageId::of_handle(&view.volume_id);
    let (raid_name, raid_node) = match &view.nfs_pod {
        Some(_) => (
            crate::identity::raid_name(&crate::identity::StagedHandle::backing_for(&sid)),
            view.nfs_server_node.clone(),
        ),
        None => (
            crate::identity::raid_name(&crate::identity::StagedHandle::user(&sid)),
            view.consumer.clone(),
        ),
    };
    // GROUND TRUTH: which recorded writers are absent from the live raid. None
    // = unprobeable (no raid, not online, unreachable, or no node to ask).
    let absent = match raid_node.as_deref() {
        Some(node) => {
            ops.writers_absent_from_raid(&raid_name, &view.volume_id, node, &view.record)
                .await
        }
        None => None,
    };
    let mut evidence = Vec::with_capacity(writers.len());
    for (uuid, node) in &writers {
        evidence.push(WriterEvidence {
            uuid: uuid.clone(),
            node: node.clone(),
            base_configured: absent.as_ref().map(|a| !a.iter().any(|u| u == uuid)),
            availability: ops.node_availability(node).await,
        });
    }
    if let PreflightVerdict::Refuse(why) =
        bounce_preflight(&evidence, &cfg.gate, view.refusal_expired)
    {
        info!(volume_id = %view.volume_id, reason = %why, "[CUTOVER] Bounce refused by preflight");
        return Ok(CutoverOutcome::Refused(why));
    }
    if view.refusal_expired {
        // The bound fired: proceeding is deliberate, and the operator hears
        // about it exactly once (this path is reachable only while a refusal
        // has been standing longer than the gate's own defer bound).
        warn!(volume_id = %view.volume_id,
              "[CUTOVER] Preflight refusal bound expired — proceeding with the bounce anyway");
        ops.emit(
            &view.volume_id,
            "Warning",
            "CutoverPreflightOverridden",
            "A recorded writer stayed neither answering nor verifiably gone for longer than the \
             refusal bound; bouncing anyway rather than blocking the remediation indefinitely \
             (the reassembly may have to surface an acked-tail risk)",
        )
        .await;
    }
    match decision {
        CutoverDecision::Wait(reason) => Ok(CutoverOutcome::Refused((*reason).to_string())),
        CutoverDecision::BounceNfsPod => {
            let nfs = view
                .nfs_pod
                .as_ref()
                .ok_or("planned an NFS bounce without an NFS pod")?;
            // The NFS server is a bare pod — capture its spec BEFORE the
            // delete; nothing else can recreate it.  A pre-mutation READ
            // failure tears nothing down, so it is a REFUSAL, not an attempt:
            // charging it would back the volume off for a bounce it never got.
            let pod = match ops.get_pod(&nfs.namespace, &nfs.name).await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Ok(CutoverOutcome::Refused(
                        "the NFS server pod disappeared before the bounce could start".to_string(),
                    ))
                }
                Err(e) => {
                    return Ok(CutoverOutcome::Refused(format!(
                        "could not read the NFS server pod ({e}) — nothing torn down"
                    )))
                }
            };
            let pod_node = pod.spec.as_ref().and_then(|s| s.node_name.clone());
            let replacement = sanitized_for_recreate(pod);
            // HALF A of the double-creator fix (F60 §3, formal
            // `ReconcilerBelt`): claim the recreate before deleting, so the
            // liveness reconciler — a SECOND, independent creator of this
            // same bare pod name, on its own 30s tick in this same process —
            // does not rebuild it inside the detach wait.  BOUNDED on
            // purpose: the marker carries an EXPIRY sized from this very
            // timeout, and the reader rejects expired, unparseable and
            // absurdly-far-future values alike, so a bouncer that dies
            // mid-window cannot disable the only actor able to bring the
            // volume's server back.  (The model cannot check that:
            // `WF(BounceRecreate)` assumes the bouncer completes — hence a
            // bounded claim rather than an unconditional hold.)
            //
            // Ordered BEFORE the escalation taint deliberately: a refusal must
            // leave no trace, and a failed claim used to abandon a NoSchedule
            // taint on a healthy node with no bounce to justify it.
            let deadline = bounce_claim_deadline(epoch_secs_now(), cfg.detach_timeout);
            if let Err(e) = ops.set_bounce_in_flight(&view.volume_id, Some(deadline)).await {
                warn!(volume_id = %view.volume_id, error = %e,
                      "[CUTOVER] Could not claim the recreate — refusing the bounce rather than \
                       racing the reconciler");
                return Ok(CutoverOutcome::Refused(format!(
                    "could not claim the server-pod recreate ({e}) — refusing rather than racing \
                     the liveness reconciler for the pod name"
                )));
            }
            // Scheduling escalation: taint the pod's node so the replacement
            // cannot reuse the staged volume (best-effort — a failed taint
            // degrades to the pre-escalation coin flip).
            if cfg.escalation {
                if let Some(node) = pod_node.as_deref() {
                    let value = epoch_secs_now().to_string();
                    if let Err(e) = ops.taint_node(node, &value).await {
                        warn!(volume_id = %view.volume_id, node, error = %e, "[CUTOVER] Bounce taint failed (continuing without escalation)");
                    }
                }
            }
            // A failed delete means no bounce happened, so the claim must not
            // outlive it: releasing here keeps the reconciler's hands free
            // instead of blocking it until the expiry for nothing.
            if let Err(e) = ops.delete_pod(&nfs.namespace, &nfs.name).await {
                if let Err(e2) = ops.set_bounce_in_flight(&view.volume_id, None).await {
                    warn!(volume_id = %view.volume_id, error = %e2,
                          "[CUTOVER] Could not release the recreate claim after a failed delete \
                           (it expires on its own)");
                }
                return Err(e);
            }

            let pv_name = crate::identity::backing_pv_name(&view.volume_id);
            if !ops
                .await_detached(&nfs.namespace, &nfs.name, &pv_name, cfg.detach_timeout)
                .await
            {
                // HALF B (formal `DetachWaitHonored`): do NOT recreate into a
                // still-staged volume.  The old code warned and recreated
                // anyway, which defeats the bouncer's OWN wait — the
                // `BounceTimeout` run finds the same silent-defeat violation
                // through this door with the reconciler already belted, which
                // is why fixing the reconciler alone is not enough.  Hand the
                // recreate to the reconciler by releasing the claim: it
                // rebuilds from the publish-side ensure machinery on its next
                // tick, by which time the unstage has had longer to land and
                // the bounce taint still repels the old node.  The teardown
                // DID happen, so this returns Ok(true): the attempt is
                // recorded and judged like any other.
                // THE HAND-OFF NEEDS A RECEIVER.  With FLINT_NFS_RECONCILER
                // =disabled nothing is spawned to rebuild the pod
                // (main.rs:389-403), so handing off would leave the RWX volume
                // with NO server until the next ControllerPublish — the very
                // outage §0 of the F60 doc says the reconciler exists to
                // prevent.  In that configuration the pre-fix behaviour is the
                // lesser evil: recreate, accept the same-node reuse risk, and
                // let the judge call it ineffective.
                if !crate::rwx_nfs::nfs_reconciler_enabled() {
                    warn!(
                        volume_id = %view.volume_id,
                        "[CUTOVER] Detach timed out AND the liveness reconciler is disabled — \
                         recreating here despite the staged-volume reuse risk, because nothing \
                         else would ever rebuild this bare pod"
                    );
                    let recreated = ops.recreate_pod(replacement).await;
                    if let Err(e) = ops.set_bounce_in_flight(&view.volume_id, None).await {
                        warn!(volume_id = %view.volume_id, error = %e,
                              "[CUTOVER] Could not release the recreate claim");
                    }
                    recreated?;
                    ops.emit(
                        &view.volume_id,
                        "Warning",
                        "CutoverDetachTimeout",
                        &format!(
                            "Backing PV did not detach within {}s and FLINT_NFS_RECONCILER is \
                             disabled — recreated the server pod anyway; a same-node staged-volume \
                             reuse will surface as CutoverIneffective",
                            cfg.detach_timeout.as_secs()
                        ),
                    )
                    .await;
                    return Ok(CutoverOutcome::Issued);
                }
                warn!(
                    volume_id = %view.volume_id,
                    "[CUTOVER] Synthetic PV did not detach within the timeout — NOT recreating \
                     (a recreate now would reuse the staged volume and defeat the bounce); \
                     handing the recreate to the liveness reconciler"
                );
                if let Err(e) = ops.set_bounce_in_flight(&view.volume_id, None).await {
                    warn!(volume_id = %view.volume_id, error = %e,
                          "[CUTOVER] Could not release the recreate claim — the reconciler will \
                           rebuild once the marker expires");
                }
                ops.emit(
                    &view.volume_id,
                    "Warning",
                    "CutoverDetachTimeout",
                    &format!(
                        "Backing PV did not detach within {}s — the server pod was not recreated \
                         here; the liveness reconciler owns it now (recreating into a staged \
                         volume would silently defeat the bounce)",
                        cfg.detach_timeout.as_secs()
                    ),
                )
                .await;
                return Ok(CutoverOutcome::Issued);
            }
            let recreated = ops.recreate_pod(replacement).await;
            // Release the claim on BOTH paths: a failed recreate must fall
            // through to the reconciler, not strand the volume.
            if let Err(e) = ops.set_bounce_in_flight(&view.volume_id, None).await {
                warn!(volume_id = %view.volume_id, error = %e,
                      "[CUTOVER] Could not release the recreate claim (it expires on its own)");
            }
            recreated?;
            ops.emit(
                &view.volume_id,
                "Normal",
                "CutoverStarted",
                &format!(
                    "NFS server pod {} bounced so the next stage reassembles the raid with the \
                     caught-up standby (NFSv4 clients recover via the grace window)",
                    nfs.name
                ),
            )
            .await;
            Ok(CutoverOutcome::Issued)
        }
        CutoverDecision::BounceWorkloadPods => {
            // Scheduling escalation: taint the consumer node so the
            // workload controller's replacement cannot reuse the staged
            // volume (RWO replacements come from the workload's own
            // template — a taint is the only steering flint has).
            if cfg.escalation {
                if let Some(node) = view.consumer.as_deref() {
                    let value = epoch_secs_now().to_string();
                    if let Err(e) = ops.taint_node(node, &value).await {
                        warn!(volume_id = %view.volume_id, node, error = %e, "[CUTOVER] Bounce taint failed (continuing without escalation)");
                    }
                }
            }
            for pod in &view.workload_pods {
                ops.delete_pod(&pod.namespace, &pod.name).await?;
            }
            ops.emit(
                &view.volume_id,
                "Normal",
                "CutoverStarted",
                &format!(
                    "{} workload pod(s) bounced (rejoin-bounce annotation) so the reschedule \
                     reassembles the raid with the caught-up standby",
                    view.workload_pods.len()
                ),
            )
            .await;
            Ok(CutoverOutcome::Issued)
        }
    }
}

// ---------------------------------------------------------------------------
// Kubernetes-backed ops + orchestrator loop (controller role)
// ---------------------------------------------------------------------------

pub struct KubeCutoverOps {
    pub client: kube::Client,
    /// The driver doubles as the SPDK RPC handle (`impl CatchupRpc for
    /// SpdkCsiDriver`), which is what lets the preflight probe the live raid
    /// the way `drain_leg` does instead of trusting node conditions.
    pub driver: Arc<SpdkCsiDriver>,
}

#[async_trait]
impl CutoverOps for KubeCutoverOps {
    async fn get_pod(&self, namespace: &str, name: &str) -> Result<Option<Pod>, RpcError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        match pods.get(name).await {
            Ok(pod) => Ok(Some(pod)),
            Err(e) if e.to_string().contains("NotFound") => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_pod(&self, namespace: &str, name: &str) -> Result<(), RpcError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        match pods.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("NotFound") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn await_detached(
        &self,
        namespace: &str,
        pod_name: &str,
        pv_name: &str,
        timeout: Duration,
    ) -> bool {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let vas: Api<VolumeAttachment> = Api::all(self.client.clone());
        let deadline = Instant::now() + timeout;
        loop {
            let pod_gone = match pods.get(pod_name).await {
                Ok(_) => false,
                Err(e) => e.to_string().contains("NotFound"),
            };
            let attached = vas
                .list(&ListParams::default())
                .await
                .map(|l| {
                    l.items.iter().any(|va| {
                        va.spec.source.persistent_volume_name.as_deref() == Some(pv_name)
                            && va.status.as_ref().map(|s| s.attached).unwrap_or(false)
                    })
                })
                .unwrap_or(true); // can't tell → keep waiting
            if pod_gone && !attached {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Same evidence the freshness gate reads, with the OPPOSITE blind
    /// default: an unreadable API is `None` here (⇒ the preflight refuses),
    /// because a bounce tears a serving volume down and blind must mean
    /// refuse. `driver.rs::node_availability` maps the same blip to
    /// NodeReady on purpose — deferring an assembly while blind is safe;
    /// bouncing while blind is not.
    async fn node_availability(&self, node: &str) -> Option<LegAvailability> {
        use k8s_openapi::api::core::v1::Node as k8sNode;
        let nodes: Api<k8sNode> = Api::all(self.client.clone());
        match nodes.get_opt(node).await {
            Err(_) => None,
            Ok(None) => Some(LegAvailability::NodeGone),
            Ok(Some(n)) => {
                let ready = n
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .and_then(|cs| cs.iter().find(|c| c.type_ == "Ready"));
                Some(match ready {
                    Some(c) if c.status == "True" => LegAvailability::NodeReady,
                    Some(c) => {
                        let not_ready_secs = c
                            .last_transition_time
                            .as_ref()
                            .map(|t| {
                                (chrono::Utc::now().timestamp() - t.0.as_second()).max(0) as u64
                            })
                            .unwrap_or(0);
                        LegAvailability::NodeNotReady { not_ready_secs }
                    }
                    None => LegAvailability::NodeNotReady { not_ready_secs: 0 },
                })
            }
        }
    }

    /// Errors are RETURNED, not swallowed (unlike `set_pv_annotation`):
    /// failing to CLAIM the recreate must refuse the bounce rather than race
    /// the reconciler for the pod name.
    async fn set_bounce_in_flight(
        &self,
        volume_id: &str,
        deadline_epoch_secs: Option<u64>,
    ) -> Result<(), RpcError> {
        use kube::api::{Patch, PatchParams};
        let pvs: Api<PersistentVolume> = Api::all(self.client.clone());
        let value = deadline_epoch_secs.map(|d| d.to_string());
        let patch = serde_json::json!({
            "metadata": { "annotations": { BOUNCE_IN_FLIGHT_ANNOTATION: value } }
        });
        pvs.patch(volume_id, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    async fn set_attempts(&self, volume_id: &str, value: Option<&str>) -> Result<(), RpcError> {
        use kube::api::{Patch, PatchParams};
        let pvs: Api<PersistentVolume> = Api::all(self.client.clone());
        let patch = serde_json::json!({
            "metadata": { "annotations": { CUTOVER_ATTEMPTS_ANNOTATION: value } }
        });
        pvs.patch(volume_id, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    async fn writers_absent_from_raid(
        &self,
        raid_name: &str,
        volume_id: &str,
        consumer: &str,
        record: &VolumeSyncRecord,
    ) -> Option<Vec<String>> {
        let raids = crate::catchup::get_raids(&*self.driver, consumer).await.ok()?;
        let raid = raids
            .iter()
            .find(|r| r.get("name").and_then(|n| n.as_str()) == Some(raid_name))?;
        replica_sync::replicas_missing_from_raid(raid, volume_id, record)
    }

    async fn clear_data_path_flag(&self, pv_name: &str) -> Result<(), RpcError> {
        use kube::api::{Patch, PatchParams};
        let pvs: Api<PersistentVolume> = Api::all(self.client.clone());
        let patch = serde_json::json!({
            "metadata": { "annotations": { DATA_PATH_LOST_ANNOTATION: null } }
        });
        pvs.patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    async fn recreate_pod(&self, pod: Pod) -> Result<(), RpcError> {
        let namespace = pod
            .metadata
            .namespace
            .clone()
            .ok_or("recreated pod has no namespace")?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
        pods.create(&PostParams::default(), &pod).await?;
        Ok(())
    }

    async fn emit(&self, volume_id: &str, event_type: &str, reason: &str, message: &str) {
        replica_sync::emit_pv_event(
            &self.client,
            "cutover-orchestrator",
            volume_id,
            event_type,
            reason,
            message,
        )
        .await;
    }

    async fn taint_node(&self, node: &str, value: &str) -> Result<(), RpcError> {
        use k8s_openapi::api::core::v1::{Node, Taint};
        let nodes: Api<Node> = Api::all(self.client.clone());
        let current = nodes.get(node).await?;
        let mut taints = current
            .spec
            .as_ref()
            .and_then(|s| s.taints.clone())
            .unwrap_or_default();
        taints.retain(|t| t.key != BOUNCE_TAINT_KEY);
        taints.push(Taint {
            key: BOUNCE_TAINT_KEY.to_string(),
            value: Some(value.to_string()),
            effect: "NoSchedule".to_string(),
            time_added: None,
        });
        let patch = serde_json::json!({ "spec": { "taints": taints } });
        nodes
            .patch(node, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    async fn untaint_node(&self, node: &str) -> Result<(), RpcError> {
        use k8s_openapi::api::core::v1::Node;
        let nodes: Api<Node> = Api::all(self.client.clone());
        let current = match nodes.get(node).await {
            Ok(n) => n,
            Err(e) if e.to_string().contains("NotFound") => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut taints = current
            .spec
            .as_ref()
            .and_then(|s| s.taints.clone())
            .unwrap_or_default();
        let before = taints.len();
        taints.retain(|t| t.key != BOUNCE_TAINT_KEY);
        if taints.len() == before {
            return Ok(());
        }
        let patch = serde_json::json!({ "spec": { "taints": taints } });
        nodes
            .patch(node, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    async fn list_bounce_taints(&self) -> Result<Vec<(String, String)>, RpcError> {
        use k8s_openapi::api::core::v1::Node;
        let nodes: Api<Node> = Api::all(self.client.clone());
        let mut out = Vec::new();
        for node in nodes.list(&ListParams::default()).await?.items {
            let Some(name) = node.metadata.name.clone() else { continue };
            for t in node.spec.as_ref().and_then(|s| s.taints.as_ref()).into_iter().flatten() {
                if t.key == BOUNCE_TAINT_KEY {
                    out.push((name.clone(), t.value.clone().unwrap_or_default()));
                }
            }
        }
        Ok(out)
    }
}

struct BounceAttempt {
    at: Instant,
    standbys: Vec<String>,
    /// Bounce issued for a dead data path (layer 3) rather than standby
    /// admission — judged by the data-path-lost annotation clearing, not
    /// by standby state.
    data_path: bool,
}

/// Background cutover loop (controller role, default-disabled).
pub async fn run_cutover_orchestrator(driver: Arc<SpdkCsiDriver>, cfg: CutoverConfig) {
    info!(
        cooldown_secs = cfg.cooldown.as_secs(),
        max_lag = cfg.max_lag,
        "[CUTOVER] Reassembly cutover orchestrator started"
    );
    if !bounce_claim_covers_wait(cfg.detach_timeout) {
        warn!(
            detach_timeout_secs = cfg.detach_timeout.as_secs(),
            horizon_secs = BOUNCE_CLAIM_MAX_HORIZON_SECS,
            "[CUTOVER] detach_timeout exceeds the recreate-claim horizon — the tail of each \
             detach wait is unprotected against the liveness reconciler recreating the server \
             pod early (which silently defeats the bounce). Lower \
             FLINT_CUTOVER_DETACH_TIMEOUT_SECS or raise the horizon."
        );
    }
    let ops = KubeCutoverOps {
        client: driver.kube_client.clone(),
        driver: driver.clone(),
    };
    let mut bounces: HashMap<String, BounceAttempt> = HashMap::new();
    // First-seen times for data-path-lost annotations: a 90s debounce so
    // a transient repair failure (replica node briefly down) doesn't cost
    // a workload bounce the next repair tick would have avoided.
    let mut data_path_seen: HashMap<String, Instant> = HashMap::new();
    // Standing preflight refusals: volume -> (FIRST refusal, last reason).
    // Two jobs. (1) The BOUND: the first instant is deliberately not reset when
    // the reason changes, so a flapping node that keeps producing a different
    // reason cannot postpone the bound forever. (2) Event cadence: the reason
    // is what dedupes the operator-facing event, because emit_pv_event creates
    // a new Event object every call.
    let mut refusals: HashMap<String, (Instant, String)> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if !crate::orchestrator_lease::is_leader() {
            continue; // standing by — the orchestrator lease is held elsewhere
        }
        if let Err(e) = cutover_tick(
            &driver,
            &ops,
            &cfg,
            &mut bounces,
            &mut data_path_seen,
            &mut refusals,
        )
        .await
        {
            warn!(error = %e, "[CUTOVER] Tick failed (non-fatal)");
        }
    }
}

/// Seconds since the unix epoch (taint-value clock).
fn epoch_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn cutover_tick(
    driver: &Arc<SpdkCsiDriver>,
    ops: &KubeCutoverOps,
    cfg: &CutoverConfig,
    bounces: &mut HashMap<String, BounceAttempt>,
    data_path_seen: &mut HashMap<String, Instant>,
    refusals: &mut HashMap<String, (Instant, String)>,
) -> Result<(), RpcError> {
    // Sweep expired bounce taints first (crash-safe: the application time
    // lives in the taint value, so a restarted controller still cleans
    // up). Runs even with escalation disabled — leftovers must not
    // strand a node.
    match ops.list_bounce_taints().await {
        Ok(taints) if !taints.is_empty() => {
            for node in
                expired_bounce_taints(&taints, epoch_secs_now(), cfg.taint_ttl.as_secs())
            {
                match ops.untaint_node(&node).await {
                    Ok(()) => info!(node, "[CUTOVER] Bounce taint expired — removed"),
                    Err(e) => warn!(node, error = %e, "[CUTOVER] Failed to remove expired bounce taint"),
                }
            }
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "[CUTOVER] Could not list bounce taints"),
    }

    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let vas: Api<VolumeAttachment> = Api::all(driver.kube_client.clone());

    let consumers: HashMap<String, String> = vas
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|va| va.status.as_ref().map(|s| s.attached).unwrap_or(false))
        .filter_map(|va| {
            va.spec
                .source
                .persistent_volume_name
                .map(|pv| (pv, va.spec.node_name))
        })
        .collect();
    let nfs_cfg = crate::rwx_nfs::NfsConfig::from_env();

    let pv_items = pvs.list(&ListParams::default()).await?.items;

    // For RWX volumes the data-path flag lands on the synthetic NFS backing
    // PV (its volumeHandle names the real raid under the NFS server pod),
    // while the sync record and the NFS pod hang off the parent RWX PV.
    // Fold the backing PV's flag into the parent's view; the backing PV
    // itself runs no cutover stream. Verification works on the same folded
    // flag: a successful restage puts the raid back under the backing
    // handle, the node agent clears that flag, and the bounce is judged
    // succeeded.
    // parent volume_id -> (PV carrying the flag, flag value).  The VALUE is
    // kept now, not just presence: the orphaned-flag sweep below needs the
    // node that wrote it, and clearing needs the object that holds it.
    let alias_flags: std::collections::HashMap<String, (String, String)> = pv_items
        .iter()
        .filter_map(|pv| {
            let value = pv
                .metadata
                .annotations
                .as_ref()?
                .get(DATA_PATH_LOST_ANNOTATION)?
                .clone();
            let parent = replica_sync::nfs_backing_parent(pv)?;
            Some((parent, (pv.metadata.name.clone()?, value)))
        })
        .collect();

    for pv in pv_items {
        let Some(volume_id) = pv.metadata.name.clone() else { continue };
        let is_flint = pv
            .spec
            .as_ref()
            .and_then(|s| s.csi.as_ref())
            .map(|c| c.driver == "disk.csi.chert.us")
            .unwrap_or(false);
        if !is_flint {
            continue;
        }
        if replica_sync::nfs_backing_parent(&pv).is_some() {
            continue; // synthetic backing PV — folded into the parent above
        }
        if !matches!(replica_sync::replicas_from_pv(&pv), Ok(Some(_))) {
            continue; // single replica (or unreadable)
        }
        let Some(record) = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(replica_sync::SYNC_STATE_ANNOTATION))
            .and_then(|s| VolumeSyncRecord::from_annotation(s).ok())
        else {
            continue;
        };

        let flag_value = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(DATA_PATH_LOST_ANNOTATION))
            .cloned();
        let own_flag = flag_value.is_some();
        let is_rwx = replica_sync::is_rwx_pv(&pv);
        if is_rwx && own_flag {
            // Pre-fix agents flagged RWX PVs (the workload-attachment false
            // positive); current agents skip RWX PVs entirely, so nothing
            // ever clears such a flag and it holds bounce verification open
            // forever. Clear it here — the live RWX signal is the backing
            // PV's flag, folded in below.
            use kube::api::{Patch, PatchParams};
            let patch = serde_json::json!({
                "metadata": { "annotations": { DATA_PATH_LOST_ANNOTATION: null } }
            });
            match pvs.patch(&volume_id, &PatchParams::default(), &Patch::Merge(&patch)).await {
                Ok(_) => info!(
                    volume_id,
                    "[CUTOVER] Cleared stale data-path flag on RWX PV (pre-fix residue)"
                ),
                Err(e) => warn!(volume_id, error = %e, "[CUTOVER] Failed to clear stale RWX data-path flag"),
            }
        }
        let mut data_path_flagged =
            (own_flag && !is_rwx) || alias_flags.contains_key(&volume_id);

        // ORPHANED-FLAG SWEEP (F60 §4, churn door 3).  A data-path-lost flag
        // is clearable ONLY by the node that wrote it (node_agent.rs,
        // `flagged_by_me`).  Once that node is GONE the verification predicate
        // is permanently unsatisfiable, so the ineffective verdict re-arms
        // eligibility every cooldown forever — an unbounded bounce series on a
        // volume whose data path may be perfectly healthy.  Nobody else can
        // clear it, so the controller must.
        if data_path_flagged {
            let carrier = if own_flag && !is_rwx {
                flag_value.as_deref().map(|v| (volume_id.clone(), v.to_string()))
            } else {
                alias_flags.get(&volume_id).cloned()
            };
            if let Some((pv_name, value)) = carrier {
                let owner = flag_owner_node(&value);
                if !owner.is_empty()
                    && ops.node_availability(owner).await
                        == Some(crate::freshness_gate::LegAvailability::NodeGone)
                {
                    match ops.clear_data_path_flag(&pv_name).await {
                        Ok(()) => {
                            info!(volume_id, node = owner, pv = %pv_name,
                                  "[CUTOVER] Cleared orphaned data-path flag (its flagging node is gone — nothing else could ever clear it)");
                            ops.emit(
                                &volume_id,
                                "Normal",
                                "DataPathFlagOrphaned",
                                &format!(
                                    "Cleared the data-path-lost flag written by node {}: that node \
                                     no longer exists, and only its own agent could have cleared \
                                     it — leaving it would re-bounce this volume forever",
                                    owner
                                ),
                            )
                            .await;
                            data_path_flagged = false;
                            data_path_seen.remove(&volume_id);
                            bounces.remove(&volume_id);
                            refusals.remove(&volume_id);
                            // Every bounce charged against this volume was
                            // charged for a flag nothing could ever clear, so
                            // the backoff it accrued is forgiven rather than
                            // left to throttle a volume whose reason to bounce
                            // has just been withdrawn.
                            if let Err(e) = ops.set_attempts(&volume_id, None).await {
                                warn!(volume_id, error = %e,
                                      "[CUTOVER] Could not clear bookkeeping after an orphaned flag");
                            }
                        }
                        Err(e) => warn!(volume_id, error = %e,
                                        "[CUTOVER] Failed to clear orphaned data-path flag"),
                    }
                }
            }
        }

        // Judge a pending bounce before planning a new one.
        if let Some(attempt) = bounces.get(&volume_id) {
            // Data-path bounces are judged by the annotation: the node
            // agent clears it once the restage put the raid back.
            if attempt.data_path {
                if !data_path_flagged {
                    ops.emit(
                        &volume_id,
                        "Normal",
                        "CutoverSucceeded",
                        "Data path restored after the bounce (restage rebuilt the raid)",
                    )
                    .await;
                    bounces.remove(&volume_id);
                    data_path_seen.remove(&volume_id);
                    // Progress resets the backoff — the counter measures
                    // CONSECUTIVE attempts that accomplished nothing.
                    if let Err(e) = ops.set_attempts(&volume_id, None).await {
                        warn!(volume_id, error = %e, "[CUTOVER] Could not clear bounce bookkeeping");
                    }
                } else if attempt.at.elapsed() >= cfg.cooldown {
                    ops.emit(
                        &volume_id,
                        "Warning",
                        "CutoverIneffective",
                        &format!(
                            "Bounce did not restore the data path within {}s — the restage may \
                             be failing (check NodeStage errors); eligible to retry",
                            cfg.cooldown.as_secs()
                        ),
                    )
                    .await;
                    bounces.remove(&volume_id);
                    continue;
                } else {
                    continue; // verification window still open
                }
                continue;
            }
            let pending = standbys_still_pending(&record, &attempt.standbys);
            if pending.is_empty() {
                let admitted = attempt
                    .standbys
                    .iter()
                    .filter(|u| {
                        record
                            .get(u)
                            .map(|r| r.sync_state == SyncState::InSync)
                            .unwrap_or(false)
                    })
                    .count();
                if admitted > 0 {
                    ops.emit(
                        &volume_id,
                        "Normal",
                        "CutoverSucceeded",
                        &format!(
                            "Reassembly after the bounce admitted {} standby replica(s) in_sync",
                            admitted
                        ),
                    )
                    .await;
                    // Progress resets the backoff (consecutive-failure count).
                    if let Err(e) = ops.set_attempts(&volume_id, None).await {
                        warn!(volume_id, error = %e, "[CUTOVER] Could not clear bounce bookkeeping");
                    }
                }
                bounces.remove(&volume_id);
            } else if attempt.at.elapsed() >= cfg.cooldown {
                ops.emit(
                    &volume_id,
                    "Warning",
                    "CutoverIneffective",
                    &format!(
                        "Bounce did not flip standby replica(s) {:?} to in_sync within {}s — \
                         same-node staged-volume reuse, a failed stage, or a deferred admission; \
                         eligible to retry",
                        pending,
                        cfg.cooldown.as_secs()
                    ),
                )
                .await;
                bounces.remove(&volume_id);
                continue; // retry no earlier than the next tick
            } else {
                continue; // verification window still open
            }
        }

        // Assemble the planner's view.
        // One GET, two facts: the pod ref for the planner and the node its raid
        // assembles on (under the BACKING handle) for the preflight probe.
        let mut nfs_server_node: Option<String> = None;
        let nfs_pod = match &nfs_cfg {
            Some(c) => {
                let fetched = ops
                    .get_pod(&c.namespace, &format!("flint-nfs-{}", volume_id))
                    .await
                    .ok()
                    .flatten();
                nfs_server_node = fetched
                    .as_ref()
                    .and_then(|p| p.spec.as_ref())
                    .and_then(|s| s.node_name.clone());
                fetched.map(|p| NfsPodRef {
                    namespace: c.namespace.clone(),
                    name: format!("flint-nfs-{}", volume_id),
                    pvc_backed: p
                        .spec
                        .as_ref()
                        .and_then(|s| s.volumes.as_ref())
                        .map(|vols| vols.iter().any(|v| v.persistent_volume_claim.is_some()))
                        .unwrap_or(false),
                })
            }
            None => None,
        };
        let rwo_bounce_enabled = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(REJOIN_BOUNCE_ANNOTATION))
            .map(|v| v.eq_ignore_ascii_case("enabled") || v == "true" || v == "1")
            .unwrap_or(false);
        let consumer = consumers.get(&volume_id).cloned();
        let workload_pods = if rwo_bounce_enabled && consumer.is_some() && nfs_pod.is_none() {
            match pv
                .spec
                .as_ref()
                .and_then(|s| s.claim_ref.as_ref())
                .and_then(|c| c.namespace.clone().zip(c.name.clone()))
            {
                Some((ns, claim)) => pods_using_claim(&driver.kube_client, &ns, &claim).await,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        // Debounce the data-path flag: 90s of continuous presence before
        // the planner sees it (a transient repair failure clears itself).
        // R4 episode: prefer the wall-clock `since` embedded in the flag
        // ("node|since") — it survives controller restarts, so a restart
        // mid-episode can no longer reset the clock and re-starve the
        // bounce. Old-format flags (bare node) fall back to in-memory.
        let data_path_lost = if data_path_flagged {
            let embedded_since = flag_value
                .as_deref()
                .and_then(|v| v.split_once('|'))
                .and_then(|(_, ts)| chrono::DateTime::parse_from_rfc3339(ts).ok());
            match embedded_since {
                Some(since) => {
                    (chrono::Utc::now().timestamp() - since.timestamp()) >= 90
                }
                None => {
                    let first =
                        data_path_seen.entry(volume_id.clone()).or_insert_with(Instant::now);
                    first.elapsed() >= Duration::from_secs(90)
                }
            }
        } else {
            data_path_seen.remove(&volume_id);
            false
        };

        let view = VolumeCutoverView {
            volume_id: volume_id.clone(),
            record,
            consumer,
            nfs_pod,
            rwo_bounce_enabled,
            workload_pods,
            data_path_lost,
            nfs_server_node,
            // The bound: a refusal may stand no longer than the gate's own
            // defer bound. Symmetric by intent — the belt may delay a bounce
            // for at most as long as the gate may delay an assembly, and
            // "Never hang" applies to both.
            refusal_expired: refusals
                .get(&volume_id)
                .map(|(first, _)| first.elapsed().as_secs() >= cfg.gate.defer_secs)
                .unwrap_or(false),
        };
        // THE PERSISTED CHURN BELT (F60 §4, churn doors 1+2).  Gate BEFORE
        // planning, on bookkeeping that lives on the PV rather than in a
        // stack-local map — so it survives the controller restart that used
        // to forget every cooldown, and so the FAILURE paths arm it too.
        let attempts_annotation = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(CUTOVER_ATTEMPTS_ANNOTATION))
            .cloned();
        if !matches!(plan_cutover(&view, cfg), CutoverDecision::Wait(_)) {
            if let AttemptGate::Backoff { count, remaining_secs } = attempt_gate(
                attempts_annotation.as_deref(),
                epoch_secs_now(),
                cfg.cooldown.as_secs(),
                CUTOVER_BACKOFF_CAP_MULT,
            ) {
                info!(
                    volume_id,
                    count,
                    remaining_secs,
                    "[CUTOVER] In bounce backoff — not planning (consecutive attempts without progress)"
                );
                continue;
            }
        }
        match plan_cutover(&view, cfg) {
            CutoverDecision::Wait(reason) => {
                debug!(volume_id, reason, "[CUTOVER] Waiting");
                // F48: same reservation-release rule as hot-rejoin — a
                // reservation posted while a bounce was needed must not
                // outlive the need; idle reservations starve maintainers
                // until the TTL lapse.
                crate::volume_claims::global()
                    .release_reservation(&volume_id, crate::volume_claims::OP_CUTOVER);
            }
            decision => {
                // Shared per-volume claim (Tier-2 design item 4): never
                // bounce a volume mid-catch-up or mid-hot-rejoin — the
                // restage would waste their work. Held only for the bounce
                // itself; verification is passive.
                let Some(_claim) = crate::volume_claims::global()
                    .try_claim(&volume_id, crate::volume_claims::OP_CUTOVER)
                else {
                    // F39: a wedged catch-up claim silently starved the
                    // BOUNCE — the terminal escalation — for the whole
                    // incident. Never below info again.
                    crate::volume_claims::log_claim_skip(
                        &volume_id,
                        crate::volume_claims::OP_CUTOVER,
                        crate::volume_claims::global(),
                    );
                    continue;
                };
                let standbys: Vec<String> = view
                    .record
                    .replicas
                    .iter()
                    .filter(|r| r.sync_state == SyncState::Standby)
                    .map(|r| r.lvol_uuid.clone())
                    .collect();
                info!(volume_id, ?decision, "[CUTOVER] Bouncing for reassembly");
                let outcome = execute_cutover(ops, &view, &decision, cfg).await;
                // Charge an attempt only when something was actually torn
                // down. `Issued` and `Err` both mean the delete may have
                // landed; a `Refused` tore nothing down, so charging it would
                // back off a volume that never got a bounce. The old code
                // recorded nothing on the Err arm at all, which is why the
                // documented 900s minimum never applied to any failure path —
                // including the 409 the liveness reconciler used to cause.
                let charged = matches!(outcome, Ok(CutoverOutcome::Issued) | Err(_));
                if charged {
                    let (prior, _) = attempts_annotation
                        .as_deref()
                        .and_then(decode_attempts)
                        .unwrap_or((0, 0));
                    let next = encode_attempts(prior.saturating_add(1), epoch_secs_now());
                    if let Err(e) = ops.set_attempts(&volume_id, Some(&next)).await {
                        warn!(volume_id, error = %e,
                              "[CUTOVER] Could not persist bounce bookkeeping — backoff will not \
                               survive a restart for this volume");
                    }
                }
                match outcome {
                    Ok(CutoverOutcome::Issued) => {
                        refusals.remove(&volume_id);
                        bounces.insert(
                            volume_id.clone(),
                            BounceAttempt {
                                at: Instant::now(),
                                standbys,
                                data_path: view.data_path_lost,
                            },
                        );
                    }
                    Ok(CutoverOutcome::Refused(why)) => {
                        // Emit ONCE per distinct reason, and keep the FIRST
                        // refusal instant so the bound cannot be postponed by
                        // a reason that keeps changing.
                        let first_or_changed = match refusals.get(&volume_id) {
                            Some((_, prev)) => prev != &why,
                            None => true,
                        };
                        let since = refusals
                            .get(&volume_id)
                            .map(|(t, _)| *t)
                            .unwrap_or_else(Instant::now);
                        refusals.insert(volume_id.clone(), (since, why.clone()));
                        if first_or_changed {
                            ops.emit(
                                &volume_id,
                                "Warning",
                                "CutoverRefused",
                                &format!("Bounce refused: {}", why),
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        warn!(volume_id, error = %e, "[CUTOVER] Bounce failed");
                        ops.emit(
                            &volume_id,
                            "Warning",
                            "CutoverFailed",
                            &format!("Cutover bounce failed: {}", e),
                        )
                        .await;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Pods in `namespace` mounting `claim` — the RWO bounce targets.
async fn pods_using_claim(client: &kube::Client, namespace: &str, claim: &str) -> Vec<PodRef> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    match pods.list(&ListParams::default()).await {
        Ok(list) => list
            .items
            .into_iter()
            .filter(|p| {
                p.spec
                    .as_ref()
                    .and_then(|s| s.volumes.as_ref())
                    .map(|vols| {
                        vols.iter().any(|v| {
                            v.persistent_volume_claim
                                .as_ref()
                                .map(|c| c.claim_name == claim)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .filter_map(|p| {
                p.metadata.name.map(|name| PodRef {
                    namespace: namespace.to_string(),
                    name,
                })
            })
            .collect(),
        Err(e) => {
            warn!(namespace, claim, error = %e, "[CUTOVER] Pod listing failed");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minimal_models::ReplicaInfo;
    use crate::replica_sync::epoch_name;
    use std::sync::Mutex;

    #[test]
    fn data_path_verdict_flags_after_threshold_only() {
        // In-flight stage (strikes below threshold): hold.
        assert_eq!(data_path_verdict(true, false, false, 1, 3), DataPathAction::Hold);
        assert_eq!(data_path_verdict(true, false, false, 2, 3), DataPathAction::Hold);
        // Third consecutive miss: flag.
        assert_eq!(data_path_verdict(true, false, false, 3, 3), DataPathAction::Flag);
        // Already flagged by us: nothing to re-do.
        assert_eq!(data_path_verdict(true, false, true, 5, 3), DataPathAction::Hold);
    }

    #[test]
    fn repair_fires_a_tick_before_the_flag() {
        // THE ASYMMETRY, pinned (2026-07-30). The in-place repair and the
        // controller-side flag must NOT share a confirmation count: the repair
        // is idempotent, lock-serialised and staged-here-gated, while the flag
        // can end in a pod bounce. Shipped thresholds are 2 and 3.
        const REPAIR: u32 = 2;
        const FLAG: u32 = 3;
        assert!(REPAIR < FLAG, "the cheap action must not wait on the expensive one");

        // One strike is the in-flight-stage race — neither acts.
        assert!(!repair_due(true, false, 1, REPAIR));
        assert_eq!(data_path_verdict(true, false, false, 1, FLAG), DataPathAction::Hold);

        // Two strikes: repair, and DO NOT flag. This is the whole point — the
        // volume gets its raid back without the controller bouncing anything.
        assert!(repair_due(true, false, 2, REPAIR));
        assert_eq!(data_path_verdict(true, false, false, 2, FLAG), DataPathAction::Hold);

        // Three: the repair evidently did not stick, so escalate as well.
        assert!(repair_due(true, false, 3, REPAIR));
        assert_eq!(data_path_verdict(true, false, false, 3, FLAG), DataPathAction::Flag);

        // Healed, or not ours: never repair.
        assert!(!repair_due(true, true, 9, REPAIR), "raid present — nothing to rebuild");
        assert!(!repair_due(false, false, 9, REPAIR), "not attached here — not ours");
    }

    #[test]
    fn data_path_lost_bounces_without_any_standby() {
        // Layer 3: the bounce is the remediation for a dead data path —
        // no standby is required (all replicas may be in_sync).
        let record = VolumeSyncRecord::initial(&[
            replica("node-a", "uuid-a"),
            replica("node-b", "uuid-b"),
        ]);
        let mut v = rwo_view(record.clone());
        v.data_path_lost = true;
        assert_eq!(plan_cutover(&v, &CutoverConfig::default()), CutoverDecision::BounceWorkloadPods);

        let mut n = nfs_view(record.clone());
        n.data_path_lost = true;
        assert_eq!(plan_cutover(&n, &CutoverConfig::default()), CutoverDecision::BounceNfsPod);

        // RWO without the opt-in: surfaced, not bounced.
        let mut v2 = rwo_view(record.clone());
        v2.data_path_lost = true;
        v2.rwo_bounce_enabled = false;
        assert!(matches!(
            plan_cutover(&v2, &CutoverConfig::default()),
            CutoverDecision::Wait(r) if r.contains("rejoin-bounce")
        ));

        // Not attached: nothing to bounce; the next stage rebuilds.
        let mut v3 = rwo_view(record);
        v3.data_path_lost = true;
        v3.consumer = None;
        v3.nfs_pod = None;
        assert!(matches!(
            plan_cutover(&v3, &CutoverConfig::default()),
            CutoverDecision::Wait(r) if r.contains("not attached")
        ));
    }

    #[test]
    fn data_path_verdict_clears_only_its_own_flag() {
        // Raid back: clear ours, hold if not ours.
        assert_eq!(data_path_verdict(true, true, true, 0, 3), DataPathAction::Clear);
        assert_eq!(data_path_verdict(true, true, false, 0, 3), DataPathAction::Hold);
        // Attachment left this node: same rule.
        assert_eq!(data_path_verdict(false, false, true, 0, 3), DataPathAction::Clear);
        assert_eq!(data_path_verdict(false, false, false, 0, 3), DataPathAction::Hold);
        // Healthy steady state: hold.
        assert_eq!(data_path_verdict(true, true, false, 0, 3), DataPathAction::Hold);
    }

    #[test]
    fn raid_collapse_verdict_first_strike_visibility() {
        use CollapseEvent::*;
        // A previously-seen raid vanishing under a live attachment is a
        // collapse: warn on the FIRST observation (7b-3 P1).
        assert_eq!(raid_collapse_verdict(true, false, true, false), Lost);
        // …but only once per episode.
        assert_eq!(raid_collapse_verdict(true, false, true, true), None);
        // Never seen present = in-flight NodeStage: stay silent (the strike
        // threshold owns that case).
        assert_eq!(raid_collapse_verdict(true, false, false, false), None);
        // Raid back after a warning: close the episode.
        assert_eq!(raid_collapse_verdict(true, true, true, true), Restored);
        // Healthy steady state / detached: nothing to say.
        assert_eq!(raid_collapse_verdict(true, true, true, false), None);
        assert_eq!(raid_collapse_verdict(false, false, true, false), None);
    }

    fn replica(node: &str, uuid: &str) -> ReplicaInfo {
        ReplicaInfo {
            node_name: node.to_string(),
            node_uid: format!("uid-{}", node),
            disk_pci_address: "0000:00:1e.0".to_string(),
            lvol_uuid: uuid.to_string(),
            lvol_name: format!("lvol-{}", uuid),
            lvs_name: "lvs0".to_string(),
            nqn: None,
            target_ip: None,
            target_port: None,
            health: "online".to_string(),
        }
    }

    /// vol1 with epochs 1..=5 and replica b a standby caught up through 5.
    fn ready_record() -> VolumeSyncRecord {
        let mut record = VolumeSyncRecord::initial(&[
            replica("node-a", "uuid-a"),
            replica("node-b", "uuid-b"),
            replica("node-c", "uuid-c"),
        ]);
        let all = vec!["uuid-a".to_string(), "uuid-b".to_string(), "uuid-c".to_string()];
        for seq in 1..=5 {
            record.apply_epoch_cut(&epoch_name("vol1", seq), &all, "t");
        }
        record.mark_stale("uuid-b", "leg failed", "t");
        record.mark_standby("uuid-b", &epoch_name("vol1", 5), "caught up", "t");
        record
    }

    fn cfg() -> CutoverConfig {
        CutoverConfig {
            enabled: true,
            cooldown: Duration::from_secs(900),
            max_lag: 1,
            detach_timeout: Duration::from_secs(120),
            escalation: true,
            taint_ttl: Duration::from_secs(120),
            rwx_inplace: true,
            gate: gate_cfg(),
        }
    }

    /// The gate config the preflight shares. Defaults: node_gone_secs 360.
    fn gate_cfg() -> crate::freshness_gate::GateConfig {
        crate::freshness_gate::GateConfig::default()
    }

    #[test]
    fn expired_bounce_taints_honors_ttl_and_garbage() {
        let taints = vec![
            ("node-a".to_string(), "1000".to_string()),   // 500s old
            ("node-b".to_string(), "1400".to_string()),   // 100s old
            ("node-c".to_string(), "garbage".to_string()), // unparseable
        ];
        let expired = expired_bounce_taints(&taints, 1500, 120);
        assert_eq!(expired, vec!["node-a".to_string(), "node-c".to_string()]);
        // Nothing expired within the ttl.
        assert!(expired_bounce_taints(&taints[1..2], 1500, 120).is_empty());
    }

    fn nfs_view(record: VolumeSyncRecord) -> VolumeCutoverView {
        VolumeCutoverView {
            volume_id: "vol1".to_string(),
            record,
            consumer: None,
            nfs_pod: Some(NfsPodRef {
                namespace: "flint-system".to_string(),
                name: "flint-nfs-vol1".to_string(),
                pvc_backed: true,
            }),
            rwo_bounce_enabled: false,
            workload_pods: vec![],
            data_path_lost: false,
            nfs_server_node: Some("node-a".to_string()),
            refusal_expired: false,
        }
    }

    fn rwo_view(record: VolumeSyncRecord) -> VolumeCutoverView {
        VolumeCutoverView {
            volume_id: "vol1".to_string(),
            record,
            consumer: Some("node-a".to_string()),
            nfs_pod: None,
            rwo_bounce_enabled: true,
            workload_pods: vec![PodRef { namespace: "default".to_string(), name: "app-0".to_string() }],
            data_path_lost: false,
            nfs_server_node: None,
            refusal_expired: false,
        }
    }

    // ---- planner ----------------------------------------------------------

    #[test]
    fn plan_requires_a_ready_standby() {
        // No standby at all.
        let mut record = ready_record();
        record.mark_in_sync("uuid-b", &epoch_name("vol1", 5), "x", "t");
        assert_eq!(
            plan_cutover(&nfs_view(record), &cfg()),
            CutoverDecision::Wait("no standby replica")
        );

        // Standby lagging beyond max_lag: the chase has not converged.
        let mut record = ready_record();
        record.mark_standby("uuid-b", &epoch_name("vol1", 3), "behind", "t");
        assert!(matches!(
            plan_cutover(&nfs_view(record), &cfg()),
            CutoverDecision::Wait(r) if r.contains("lag")
        ));

        // Unreadable mark: not ready.
        let mut record = ready_record();
        record.replicas[1].last_epoch = Some("garbage".to_string());
        assert!(matches!(
            plan_cutover(&nfs_view(record), &cfg()),
            CutoverDecision::Wait(r) if r.contains("unreadable")
        ));
    }

    #[test]
    fn plan_defers_rwx_admission_to_the_inplace_window() {
        // S2: a converged standby on an RWX volume is the in-place
        // hot-rejoin window's job — this planner keeps only the
        // data-path-lost (relocation) arm and the kill-switch fallback.
        assert!(matches!(
            plan_cutover(&nfs_view(ready_record()), &cfg()),
            CutoverDecision::Wait(r) if r.contains("in-place admission")
        ));
    }

    #[test]
    fn plan_bounces_pvc_backed_nfs_pod_when_inplace_disabled() {
        // The FLINT_RWX_INPLACE_ADMISSION kill switch restores the
        // pre-S2 Tier-1 admission bounce.
        let mut c = cfg();
        c.rwx_inplace = false;
        assert_eq!(plan_cutover(&nfs_view(ready_record()), &c), CutoverDecision::BounceNfsPod);

        // emptyDir-backed NFS pod has no raid to reassemble.
        let mut view = nfs_view(ready_record());
        view.nfs_pod.as_mut().unwrap().pvc_backed = false;
        assert!(matches!(plan_cutover(&view, &c), CutoverDecision::Wait(_)));
    }

    #[test]
    fn plan_rwo_bounce_is_strictly_opt_in() {
        assert_eq!(
            plan_cutover(&rwo_view(ready_record()), &cfg()),
            CutoverDecision::BounceWorkloadPods
        );

        // Knob off: never bounce a workload uninvited.
        let mut view = rwo_view(ready_record());
        view.rwo_bounce_enabled = false;
        assert!(matches!(
            plan_cutover(&view, &cfg()),
            CutoverDecision::Wait(r) if r.contains("rejoin-bounce")
        ));

        // Detached volume: the next natural stage admits the standby free.
        let mut view = rwo_view(ready_record());
        view.consumer = None;
        view.workload_pods.clear();
        assert!(matches!(
            plan_cutover(&view, &cfg()),
            CutoverDecision::Wait(r) if r.contains("not attached")
        ));
    }

    // ---- bounce execution -------------------------------------------------

    struct FakeOps {
        pod: Mutex<Option<Pod>>,
        detached: bool,
        log: Mutex<Vec<String>>,
        recreated: Mutex<Option<Pod>>,
        events: Mutex<Vec<(String, String)>>,
        /// What every writer's node looks like to the preflight. Default
        /// Ready — the tests that care override it.
        availability: Mutex<Option<LegAvailability>>,
        /// Whether the recreate-claim patch succeeds.
        claim_ok: bool,
        /// Ground truth: recorded writers absent from the serving raid.
        /// `Some(vec![])` = every writer answering; `None` = raid unprobeable.
        absent: Mutex<Option<Vec<String>>>,
    }

    impl FakeOps {
        fn with_nfs_pod() -> Self {
            let mut pod = Pod::default();
            pod.metadata.name = Some("flint-nfs-vol1".to_string());
            pod.metadata.namespace = Some("flint-system".to_string());
            pod.metadata.resource_version = Some("12345".to_string());
            pod.metadata.uid = Some("uid-xyz".to_string());
            pod.spec = Some(k8s_openapi::api::core::v1::PodSpec {
                node_name: Some("node-a".to_string()),
                ..Default::default()
            });
            pod.status = Some(Default::default());
            FakeOps {
                pod: Mutex::new(Some(pod)),
                detached: true,
                log: Mutex::new(Vec::new()),
                recreated: Mutex::new(None),
                events: Mutex::new(Vec::new()),
                availability: Mutex::new(Some(LegAvailability::NodeReady)),
                claim_ok: true,
                absent: Mutex::new(Some(Vec::new())),
            }
        }

        fn reasons(&self) -> Vec<String> {
            self.events.lock().unwrap().iter().map(|(r, _)| r.clone()).collect()
        }
    }

    #[async_trait]
    impl CutoverOps for FakeOps {
        async fn get_pod(&self, _ns: &str, name: &str) -> Result<Option<Pod>, RpcError> {
            self.log.lock().unwrap().push(format!("get:{}", name));
            Ok(self.pod.lock().unwrap().clone())
        }
        async fn delete_pod(&self, _ns: &str, name: &str) -> Result<(), RpcError> {
            self.log.lock().unwrap().push(format!("delete:{}", name));
            *self.pod.lock().unwrap() = None;
            Ok(())
        }
        async fn await_detached(
            &self,
            _ns: &str,
            pod: &str,
            pv: &str,
            _timeout: Duration,
        ) -> bool {
            self.log.lock().unwrap().push(format!("await:{}:{}", pod, pv));
            self.detached
        }
        async fn recreate_pod(&self, pod: Pod) -> Result<(), RpcError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("recreate:{}", pod.metadata.name.as_deref().unwrap_or("?")));
            *self.recreated.lock().unwrap() = Some(pod);
            Ok(())
        }
        async fn emit(&self, _volume_id: &str, event_type: &str, reason: &str, _message: &str) {
            self.events
                .lock()
                .unwrap()
                .push((reason.to_string(), event_type.to_string()));
        }
        async fn taint_node(&self, node: &str, _value: &str) -> Result<(), RpcError> {
            self.log.lock().unwrap().push(format!("taint:{}", node));
            Ok(())
        }
        async fn untaint_node(&self, node: &str) -> Result<(), RpcError> {
            self.log.lock().unwrap().push(format!("untaint:{}", node));
            Ok(())
        }
        async fn list_bounce_taints(&self) -> Result<Vec<(String, String)>, RpcError> {
            Ok(vec![])
        }
        async fn node_availability(&self, node: &str) -> Option<LegAvailability> {
            self.log.lock().unwrap().push(format!("avail:{}", node));
            self.availability.lock().unwrap().clone()
        }
        async fn set_bounce_in_flight(
            &self,
            _volume_id: &str,
            deadline: Option<u64>,
        ) -> Result<(), RpcError> {
            self.log.lock().unwrap().push(format!("claim:{}", deadline.is_some()));
            if self.claim_ok {
                Ok(())
            } else {
                Err("patch rejected".into())
            }
        }
        async fn set_attempts(&self, _volume_id: &str, value: Option<&str>) -> Result<(), RpcError> {
            self.log.lock().unwrap().push(format!("attempts:{:?}", value));
            Ok(())
        }
        async fn clear_data_path_flag(&self, pv_name: &str) -> Result<(), RpcError> {
            self.log.lock().unwrap().push(format!("clearflag:{}", pv_name));
            Ok(())
        }
        async fn writers_absent_from_raid(
            &self,
            _raid: &str,
            _vol: &str,
            _node: &str,
            _record: &VolumeSyncRecord,
        ) -> Option<Vec<String>> {
            self.absent.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn nfs_bounce_captures_deletes_waits_and_recreates() {
        let ops = FakeOps::with_nfs_pod();
        let view = nfs_view(ready_record());
        let outcome = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        assert_eq!(outcome, CutoverOutcome::Issued);
        // Spec captured before the delete; the bounce taint lands BEFORE
        // the delete (scheduling escalation — the replacement must not
        // reuse the staged volume); detach awaited; recreation last.
        // The commit-time preflight runs FIRST, over every recorded writer
        // (uuid-b is the standby, so only a and c are checked). The recreate
        // claim is taken before the TAINT — not merely before the delete — so a
        // refusal leaves no trace on a healthy node; and it is released after
        // the recreate.
        assert_eq!(
            ops.log.lock().unwrap().clone(),
            vec![
                "avail:node-a",
                "avail:node-c",
                "get:flint-nfs-vol1",
                "claim:true",
                "taint:node-a",
                "delete:flint-nfs-vol1",
                "await:flint-nfs-vol1:flint-nfs-pv-vol1",
                "recreate:flint-nfs-vol1",
                "claim:false",
            ]
        );
        // The replacement is sanitized: no server fields, no pinned node.
        let recreated = ops.recreated.lock().unwrap().clone().unwrap();
        assert_eq!(recreated.metadata.resource_version, None);
        assert_eq!(recreated.metadata.uid, None);
        assert!(recreated.status.is_none());
        assert_eq!(recreated.spec.as_ref().unwrap().node_name, None);
        assert_eq!(
            ops.events.lock().unwrap().clone(),
            vec![("CutoverStarted".to_string(), "Normal".to_string())]
        );
    }

    /// F60 §3 half B — THIS TEST'S CONTRACT IS THE INVERSE OF ITS PREVIOUS
    /// ONE, deliberately.  It used to assert "proceeds when detach times out":
    /// recreate anyway, on the reasoning that an ineffective bounce is caught
    /// by verification while a missing NFS pod is an outage.  The first half
    /// of that is false — recreating into a still-staged volume makes kubelet
    /// reuse it, so NO reassembly happens, the standby stays parked, and the
    /// attempt is not even recorded when the create then 409s.  The formal
    /// `BounceTimeout` run reaches the silent-defeat violation through exactly
    /// this door with the reconciler already belted.  The second half is
    /// answered by the liveness reconciler, which rebuilds an Absent server
    /// within about one 30s tick — so handing off is safe, and that is what
    /// releasing the claim does.
    #[tokio::test]
    async fn nfs_bounce_hands_off_rather_than_recreating_into_a_staged_volume() {
        let mut ops = FakeOps::with_nfs_pod();
        ops.detached = false;
        let view = nfs_view(ready_record());
        let outcome = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        // The teardown DID happen, so the attempt must be recorded and judged.
        assert_eq!(outcome, CutoverOutcome::Issued);
        // ...but this bouncer does NOT recreate.
        assert!(ops.recreated.lock().unwrap().is_none());
        // The claim is released so the reconciler can own the rebuild, and the
        // operator is told why the pod is not back yet.
        let log = ops.log.lock().unwrap().clone();
        assert_eq!(log.last().unwrap(), "claim:false");
        assert!(!log.iter().any(|s| s.starts_with("recreate:")));
        assert!(ops.reasons().contains(&"CutoverDetachTimeout".to_string()));
    }

    #[tokio::test]
    async fn workload_bounce_deletes_every_claim_pod() {
        let ops = FakeOps::with_nfs_pod();
        let mut view = rwo_view(ready_record());
        view.workload_pods.push(PodRef {
            namespace: "default".to_string(),
            name: "app-1".to_string(),
        });
        let outcome = execute_cutover(&ops, &view, &CutoverDecision::BounceWorkloadPods, &cfg())
            .await
            .unwrap();
        assert_eq!(outcome, CutoverOutcome::Issued);
        // Escalation taints the consumer node before any delete.
        assert_eq!(
            ops.log.lock().unwrap().clone(),
            vec![
                "avail:node-a",
                "avail:node-c",
                "taint:node-a",
                "delete:app-0",
                "delete:app-1",
            ]
        );
        assert_eq!(
            ops.events.lock().unwrap().clone(),
            vec![("CutoverStarted".to_string(), "Normal".to_string())]
        );
    }

    // ---- the commit-time preflight (F60 §1) -------------------------------

    /// The headline belt.  `plan_cutover` reads no leg health at all, so
    /// without this the controller tears a serving volume down while a
    /// recorded writer is transiently gone, and the reassembly can only
    /// return by excusing an acked tail that was recoverable — the
    /// manufactured outage plus a hollow risk marker (formal `BounceRisk`).
    #[tokio::test]
    async fn preflight_refuses_while_a_writer_is_transiently_gone() {
        let ops = FakeOps::with_nfs_pod();
        *ops.absent.lock().unwrap() = Some(vec!["uuid-a".to_string()]);
        *ops.availability.lock().unwrap() =
            Some(LegAvailability::NodeNotReady { not_ready_secs: 12 });
        let view = nfs_view(ready_record());
        let outcome = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        assert!(matches!(outcome, CutoverOutcome::Refused(_)), "a bounce must not be issued");
        // NOTHING was mutated: the whole writer set is probed in one pass
        // (a complete picture for the operator message), and then the belt
        // refuses before the taint, the claim, or the delete.
        let log = ops.log.lock().unwrap().clone();
        assert_eq!(log, vec!["avail:node-a", "avail:node-c"]);
        // The REASON is returned, not emitted: the tick owns the operator
        // surface so a refusal repeating every 60s does not create one Event
        // object per minute (emit_pv_event never aggregates).
        match outcome {
            CutoverOutcome::Refused(why) => assert!(
                why.contains("not configured") && why.contains("NotReady"),
                "the reason must name the ground truth AND the node evidence: {}",
                why
            ),
            other => panic!("expected a refusal, got {:?}", other),
        }
    }

    /// A verifiably-gone writer is NOT a reason to refuse: the replacement
    /// machinery owns it and the gate will excuse it honestly, not hollowly.
    /// Refusing here would park the volume behind a dead node forever.
    #[tokio::test]
    async fn preflight_allows_a_verifiably_gone_writer() {
        let ops = FakeOps::with_nfs_pod();
        *ops.availability.lock().unwrap() = Some(LegAvailability::NodeGone);
        let view = nfs_view(ready_record());
        let outcome = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        assert_eq!(outcome, CutoverOutcome::Issued);
    }

    /// THE POLARITY TEST.  `driver.rs::node_availability` maps an unreadable
    /// API to NodeReady, because deferring an assembly while blind is the
    /// bounded-safe direction.  A bounce is destructive, so blind must mean
    /// REFUSE.  Same evidence, opposite safe direction — if this ever flips,
    /// the belt silently stops belting during an API blip.
    #[tokio::test]
    async fn preflight_refuses_when_blind() {
        let ops = FakeOps::with_nfs_pod();
        // The raid says this writer is absent, and the API cannot say whether
        // its node is gone — the belt must not guess in the destructive
        // direction.
        *ops.absent.lock().unwrap() = Some(vec!["uuid-a".to_string()]);
        *ops.availability.lock().unwrap() = None;
        let view = nfs_view(ready_record());
        let outcome = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        match outcome {
            CutoverOutcome::Refused(why) => assert!(
                why.contains("unreadable"),
                "the reason must say we were blind: {}",
                why
            ),
            other => panic!("expected a refusal, got {:?}", other),
        }
    }

    fn ev(uuid: &str, configured: Option<bool>, a: Option<LegAvailability>) -> WriterEvidence {
        WriterEvidence {
            uuid: uuid.to_string(),
            node: format!("node-of-{}", uuid),
            base_configured: configured,
            availability: a,
        }
    }

    /// THE CORE CORRECTION: node conditions are NOT leg health. A leg dies
    /// while its node stays Ready all the time here (F33's "Ready does not
    /// guarantee a live tgt", F42's dead remote target, the csi-node roll
    /// landmine). A belt that only read node conditions passed its OWN
    /// counterexample in exactly that case, so the evidence is the raid.
    #[test]
    fn preflight_refuses_a_faulted_out_leg_whose_node_reads_ready() {
        let w = vec![ev("uuid-a", Some(false), Some(LegAvailability::NodeReady))];
        assert!(matches!(
            bounce_preflight(&w, &gate_cfg(), false),
            PreflightVerdict::Refuse(_)
        ));
        // ...and a leg answering in the raid is safe even if we would otherwise
        // have nothing else to go on.
        let ok = vec![ev("uuid-a", Some(true), None)];
        assert_eq!(bounce_preflight(&ok, &gate_cfg(), false), PreflightVerdict::Go);
    }

    /// An unprobeable raid is not evidence of health. Refuse — the same
    /// discipline `drain_leg` applies ("ground truth is unprobeable — refuse
    /// rather than mutate the record on a view we cannot verify").
    #[test]
    fn preflight_refuses_when_the_raid_cannot_be_probed() {
        let w = vec![ev("uuid-a", None, Some(LegAvailability::NodeReady))];
        assert!(matches!(
            bounce_preflight(&w, &gate_cfg(), false),
            PreflightVerdict::Refuse(_)
        ));
    }

    /// THE THRESHOLD. An absent writer is safe to bounce through only when the
    /// gate would call it PERMANENT — past that line, excusing it is honest
    /// rather than hollow, and refusing forever would strand the volume behind
    /// a node that is never coming back.
    #[test]
    fn preflight_waits_inside_the_recoverable_window_and_proceeds_past_it() {
        let gate = gate_cfg();
        let at = |secs| {
            vec![ev(
                "uuid-a",
                Some(false),
                Some(LegAvailability::NodeNotReady { not_ready_secs: secs }),
            )]
        };
        assert!(matches!(bounce_preflight(&at(0), &gate, false), PreflightVerdict::Refuse(_)));
        assert!(matches!(
            bounce_preflight(&at(gate.node_gone_secs - 1), &gate, false),
            PreflightVerdict::Refuse(_)
        ));
        assert_eq!(bounce_preflight(&at(gate.node_gone_secs), &gate, false), PreflightVerdict::Go);
        // A verifiably-gone node is always safe: the replacement machinery owns
        // it and the gate excuses it soundly.
        let gone = vec![ev("uuid-a", Some(false), Some(LegAvailability::NodeGone))];
        assert_eq!(bounce_preflight(&gone, &gate, false), PreflightVerdict::Go);
    }

    /// THE BOUND. `freshness_gate::evaluate` is deliberately deadline-bounded
    /// ("Never hang — the 2.4 obligation"); an unbounded belt would block the
    /// escalation ladder's TERMINAL rung forever, and on the data-path arm the
    /// volume is ALREADY down, so refusing lengthens an outage. A flapping
    /// kubelet keeps resetting lastTransitionTime, so node_gone_secs alone
    /// never trips — this is the escape hatch that makes that survivable.
    #[test]
    fn preflight_refusal_is_bounded() {
        let gate = gate_cfg();
        let hazard = vec![ev("uuid-a", Some(false), Some(LegAvailability::NodeReady))];
        assert!(matches!(
            bounce_preflight(&hazard, &gate, false),
            PreflightVerdict::Refuse(_)
        ));
        assert_eq!(
            bounce_preflight(&hazard, &gate, true),
            PreflightVerdict::Go,
            "a standing refusal must eventually yield rather than block forever"
        );
    }

    /// A claim-blocked attach says nothing about whether the leg returns.
    #[test]
    fn preflight_rejects_attach_signals_as_health_evidence() {
        let w = vec![ev("uuid-a", Some(false), Some(LegAvailability::ClaimBlocked))];
        assert!(matches!(
            bounce_preflight(&w, &gate_cfg(), false),
            PreflightVerdict::Refuse(_)
        ));
    }

    /// The writer set is `writer_uuids()`, NOT the in-sync replicas: the two
    /// diverge on the shipped forced-stale path, where stale legs are stamped
    /// into the writer set wholesale without being marked in_sync. A belt keyed
    /// on InSync would skip exactly the legs the gate then waits for.
    #[test]
    fn recorded_writers_is_the_union_of_the_gate_set_and_the_insync_set() {
        let record = ready_record();
        let got: Vec<String> = recorded_writers(&record).into_iter().map(|(u, _)| u).collect();
        // This fixture has never assembled, so writer_uuids() is EMPTY —
        // exactly the state in which keying on it alone would return "all
        // clear" on zero evidence. The in-sync half covers it.
        assert!(record.writer_uuids().is_empty(), "fixture has not assembled");
        assert_eq!(got, vec!["uuid-a".to_string(), "uuid-c".to_string()]);
        // ...and a leg in the gate's writer set but NOT in_sync (the
        // forced-stale fallback's residue) is still covered.
        let mut forced = ready_record();
        forced.set_writer_set(&["uuid-a".to_string(), "uuid-b".to_string()], "t");
        let got2: Vec<String> =
            recorded_writers(&forced).into_iter().map(|(u, _)| u).collect();
        assert!(
            got2.contains(&"uuid-b".to_string()),
            "a writer-set member that is not in_sync must still be checked: {:?}",
            got2
        );
    }

    /// An EMPTY writer set must not read as "all healthy": with no evidence at
    /// all, a vacuous Go would pass precisely when the record is most degraded.
    #[test]
    fn preflight_on_an_empty_writer_set_is_vacuous_and_must_be_guarded_upstream() {
        // Documents the shape: the pure function has nothing to refuse on, so
        // the CALLER must not reach it with an empty set (the planner cannot:
        // every arm requires either a standby or a live data-path flag, and a
        // volume with no writers at all has no serving path to tear down).
        assert_eq!(bounce_preflight(&[], &gate_cfg(), false), PreflightVerdict::Go);
    }

    // ---- the recreate claim (F60 §3 half A) -------------------------------

    /// EVERY exit path after the claim is taken must release it. A claim that
    /// outlives its bounce blocks the only other actor able to rebuild the
    /// pod, for nothing. This pins the failed-delete path, where an early
    /// `?` used to skip the release entirely.
    #[tokio::test]
    async fn a_failed_delete_releases_the_recreate_claim() {
        struct DeleteFails(FakeOps);
        #[async_trait]
        impl CutoverOps for DeleteFails {
            async fn get_pod(&self, ns: &str, n: &str) -> Result<Option<Pod>, RpcError> {
                self.0.get_pod(ns, n).await
            }
            async fn delete_pod(&self, _ns: &str, _n: &str) -> Result<(), RpcError> {
                self.0.log.lock().unwrap().push("delete:ERR".to_string());
                Err("apiserver said no".into())
            }
            async fn await_detached(&self, _n: &str, _p: &str, _v: &str, _t: Duration) -> bool {
                unreachable!("must not wait after a failed delete")
            }
            async fn recreate_pod(&self, _pod: Pod) -> Result<(), RpcError> {
                unreachable!("must not recreate after a failed delete")
            }
            async fn emit(&self, v: &str, t: &str, r: &str, m: &str) {
                self.0.emit(v, t, r, m).await
            }
            async fn taint_node(&self, n: &str, v: &str) -> Result<(), RpcError> {
                self.0.taint_node(n, v).await
            }
            async fn untaint_node(&self, n: &str) -> Result<(), RpcError> {
                self.0.untaint_node(n).await
            }
            async fn list_bounce_taints(&self) -> Result<Vec<(String, String)>, RpcError> {
                Ok(vec![])
            }
            async fn node_availability(&self, n: &str) -> Option<LegAvailability> {
                self.0.node_availability(n).await
            }
            async fn set_bounce_in_flight(
                &self,
                v: &str,
                d: Option<u64>,
            ) -> Result<(), RpcError> {
                self.0.set_bounce_in_flight(v, d).await
            }
            async fn set_attempts(&self, v: &str, val: Option<&str>) -> Result<(), RpcError> {
                self.0.set_attempts(v, val).await
            }
            async fn clear_data_path_flag(&self, p: &str) -> Result<(), RpcError> {
                self.0.clear_data_path_flag(p).await
            }
            async fn writers_absent_from_raid(
                &self,
                r: &str,
                v: &str,
                n: &str,
                rec: &VolumeSyncRecord,
            ) -> Option<Vec<String>> {
                self.0.writers_absent_from_raid(r, v, n, rec).await
            }
        }
        let ops = DeleteFails(FakeOps::with_nfs_pod());
        let view = nfs_view(ready_record());
        let result = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg()).await;
        assert!(result.is_err(), "the delete failure must propagate");
        let log = ops.0.log.lock().unwrap().clone();
        assert_eq!(
            log.iter().filter(|s| s.starts_with("claim:")).collect::<Vec<_>>(),
            vec!["claim:true", "claim:false"],
            "the claim must be taken and then RELEASED: {:?}",
            log
        );
    }

    /// A claim that cannot be written must REFUSE the bounce rather than
    /// proceed unprotected: racing the reconciler for the pod name is how the
    /// bounce gets silently defeated.
    #[tokio::test]
    async fn unwritable_claim_refuses_the_bounce() {
        let mut ops = FakeOps::with_nfs_pod();
        ops.claim_ok = false;
        let view = nfs_view(ready_record());
        let outcome = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        assert!(matches!(outcome, CutoverOutcome::Refused(_)));
        let log = ops.log.lock().unwrap().clone();
        assert!(
            !log.iter().any(|s| s.starts_with("delete:")),
            "the pod must NOT be deleted without the claim: {:?}",
            log
        );
    }

    /// The claim is BOUNDED — the one hazard the formal `BouncePodFixed` run
    /// cannot see, because `WF(BounceRecreate)` assumes the bouncer completes.
    /// A bouncer that dies mid-window must not disable the reconciler for
    /// good, so an expired or unreadable claim reads as absent (fail-open),
    /// exactly like an expired bounce taint.
    #[test]
    fn recreate_claim_is_bounded_and_fails_open() {
        let cap = BOUNCE_CLAIM_MAX_HORIZON_SECS;
        // Live claim: honoured right up to its deadline.
        assert!(bounce_claim_active(Some("1100"), 1000, cap));
        assert!(bounce_claim_active(Some("1001"), 1000, cap));
        // AT the deadline and past it: the bouncer is presumed dead.
        assert!(!bounce_claim_active(Some("1000"), 1000, cap));
        assert!(!bounce_claim_active(Some("999"), 1000, cap));
        // Absent / garbage / empty: never strand a volume's server.
        assert!(!bounce_claim_active(None, 5000, cap));
        assert!(!bounce_claim_active(Some("not-a-number"), 5000, cap));
        assert!(!bounce_claim_active(Some(""), 5000, cap));
        // BOUNDEDNESS IS ENFORCED BY THE READER: a bug, a bad clock, or a
        // hostile writer cannot disable the reconciler indefinitely with a
        // far-future deadline.
        assert!(bounce_claim_active(Some(&(1000 + cap).to_string()), 1000, cap));
        assert!(!bounce_claim_active(Some(&(1000 + cap + 1).to_string()), 1000, cap));
        assert!(!bounce_claim_active(Some("99999999999"), 1000, cap));
    }

    /// THE DEFECT THIS DESIGN EXISTS TO PREVENT: `detach_timeout` is operator-
    /// configurable, so a claim sized by a FIXED constant would silently
    /// expire while the bouncer was still waiting — reopening the
    /// double-creator race with no signal at all. The deadline is therefore
    /// derived from the configured timeout, and must outlive it.
    #[test]
    fn claim_deadline_outlives_even_a_raised_detach_timeout() {
        let now = 10_000;
        for secs in [1u64, 30, 120, 300, 600] {
            let timeout = Duration::from_secs(secs);
            let deadline = bounce_claim_deadline(now, timeout);
            assert!(
                deadline > now + secs,
                "claim must outlive a {}s detach wait, got {}",
                secs,
                deadline - now
            );
            // ...and still be honoured by the reader at the moment the wait
            // would time out.
            assert!(
                bounce_claim_active(
                    Some(&deadline.to_string()),
                    now + secs,
                    BOUNCE_CLAIM_MAX_HORIZON_SECS
                ),
                "a {}s wait must still hold its claim when it times out",
                secs
            );
        }
        // AN ABSURD CONFIGURATION MUST DEGRADE, NOT INVERT.  Before the clamp,
        // a timeout past the reader's horizon wrote a deadline the reader
        // rejected as absurd — so the claim was INERT for exactly the window it
        // protects, silently reopening the double-creator race in the
        // configuration that most needs it. Now the deadline is clamped to the
        // horizon: still live, covering as much of the wait as the reader will
        // honour, with the shortfall reported rather than hidden.
        let absurd = bounce_claim_deadline(now, Duration::from_secs(100_000));
        assert_eq!(absurd, now + BOUNCE_CLAIM_MAX_HORIZON_SECS, "clamped to the horizon");
        assert!(
            bounce_claim_active(Some(&absurd.to_string()), now, BOUNCE_CLAIM_MAX_HORIZON_SECS),
            "a clamped claim must still be honoured — an inert claim is the bug"
        );
        // ...and the shortfall is detectable, which is what the startup warning
        // reports.
        assert!(bounce_claim_covers_wait(Duration::from_secs(120)));
        assert!(bounce_claim_covers_wait(Duration::from_secs(
            BOUNCE_CLAIM_MAX_HORIZON_SECS - BOUNCE_CLAIM_MARGIN_SECS
        )));
        assert!(!bounce_claim_covers_wait(Duration::from_secs(
            BOUNCE_CLAIM_MAX_HORIZON_SECS - BOUNCE_CLAIM_MARGIN_SECS + 1
        )));
        assert!(!bounce_claim_covers_wait(Duration::from_secs(100_000)));
        // A hand-written far-future value (bug, bad clock, hostile writer) is
        // still rejected — boundedness stays reader-enforced.
        assert!(!bounce_claim_active(Some("99999999999"), now, BOUNCE_CLAIM_MAX_HORIZON_SECS));
    }

    // ---- the persisted churn belt (F60 §4) --------------------------------

    /// The three shipped churn doors, as a unit.  `BounceLoop` proves the
    /// safety belt does NOT close churn; this is the belt that does.
    #[test]
    fn attempt_gate_arms_a_capped_exponential_backoff() {
        let base = 900;
        let cap = CUTOVER_BACKOFF_CAP_MULT;
        // No history at all: allowed.
        assert_eq!(attempt_gate(None, 10_000, base, cap), AttemptGate::Allow);
        // One prior attempt, 100s ago: still inside the 900s window. THIS is
        // the door the Err arm used to leave open — it recorded nothing, so
        // the next 60s tick re-bounced immediately.
        assert!(matches!(
            attempt_gate(Some("1|9900"), 10_000, base, cap),
            AttemptGate::Backoff { count: 1, .. }
        ));
        // ...and once the window passes, allowed again (never "never").
        assert_eq!(attempt_gate(Some("1|9000"), 10_000, base, cap), AttemptGate::Allow);
        // Escalation: 1×, 2×, 4×, then capped at 8×.
        assert_eq!(attempt_backoff_secs(1, base, cap), 900);
        assert_eq!(attempt_backoff_secs(2, base, cap), 1_800);
        assert_eq!(attempt_backoff_secs(3, base, cap), 3_600);
        assert_eq!(attempt_backoff_secs(4, base, cap), 7_200);
        assert_eq!(attempt_backoff_secs(9, base, cap), 900 * 8, "capped, not doubling forever");
        assert_eq!(attempt_backoff_secs(64, base, cap), 900 * 8, "no shift overflow");
        assert_eq!(attempt_backoff_secs(0, base, cap), 0);
    }

    /// Bookkeeping must never be able to wedge a volume shut: anything
    /// unreadable reads as "no history".
    #[test]
    fn attempt_bookkeeping_fails_open() {
        let base = 900;
        let cap = CUTOVER_BACKOFF_CAP_MULT;
        for bad in ["", "garbage", "1", "|", "x|y", "1|", "|900", "-1|900"] {
            assert_eq!(
                attempt_gate(Some(bad), 10_000, base, cap),
                AttemptGate::Allow,
                "malformed bookkeeping {:?} must not wedge the volume",
                bad
            );
        }
        // A count of zero is history-free too.
        assert_eq!(attempt_gate(Some("0|9999"), 10_000, base, cap), AttemptGate::Allow);
        // A clock that went backwards must not read as an eternal backoff.
        assert_eq!(attempt_gate(Some("1|99999"), 10_000, base, cap).clone(),
                   AttemptGate::Backoff { count: 1, remaining_secs: 900 });
        // Round-trip.
        assert_eq!(decode_attempts(&encode_attempts(3, 12_345)), Some((3, 12_345)));
    }

    /// The flag's owner is the only actor that can clear it, so the sweep
    /// needs the node out of both the current and the pre-R4 formats.
    #[test]
    fn flag_owner_parses_both_formats() {
        assert_eq!(flag_owner_node("node-a|2026-07-29T00:00:00Z"), "node-a");
        assert_eq!(flag_owner_node("node-a"), "node-a", "pre-R4 bare-node flags");
        assert_eq!(flag_owner_node(" node-a |x"), "node-a");
        assert_eq!(flag_owner_node(""), "");
    }

    // ---- verification helpers ---------------------------------------------

    #[test]
    fn pending_standbys_resolve_on_admission_or_derail() {
        let uuids = vec!["uuid-b".to_string()];
        let record = ready_record();
        assert_eq!(standbys_still_pending(&record, &uuids), vec!["uuid-b".to_string()]);

        // Admitted in_sync: resolved.
        let mut record = ready_record();
        record.mark_in_sync("uuid-b", &epoch_name("vol1", 6), "admitted", "t");
        assert!(standbys_still_pending(&record, &uuids).is_empty());

        // Fell back to stale (failed again): also resolved — the catch-up
        // must run before another bounce makes sense.
        let mut record = ready_record();
        record.mark_stale("uuid-b", "lost again", "t");
        assert!(standbys_still_pending(&record, &uuids).is_empty());
    }

    #[test]
    fn sanitize_clears_all_server_populated_fields() {
        let mut pod = Pod::default();
        pod.metadata.name = Some("p".to_string());
        pod.metadata.resource_version = Some("1".to_string());
        pod.metadata.uid = Some("u".to_string());
        pod.metadata.owner_references = Some(vec![]);
        pod.spec = Some(k8s_openapi::api::core::v1::PodSpec {
            node_name: Some("node-x".to_string()),
            ..Default::default()
        });
        pod.status = Some(Default::default());
        let clean = sanitized_for_recreate(pod);
        assert_eq!(clean.metadata.name.as_deref(), Some("p"));
        assert_eq!(clean.metadata.resource_version, None);
        assert_eq!(clean.metadata.uid, None);
        assert_eq!(clean.metadata.owner_references, None);
        assert_eq!(clean.spec.unwrap().node_name, None);
        assert!(clean.status.is_none());
    }
}
