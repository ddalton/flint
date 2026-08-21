//! The control loop: CR in, four objects out, status back.
//!
//! Every decision that can be made without the cluster is a pure
//! function below with its own test; the async half is the thin part
//! that reads and writes. That split is deliberate — the interesting
//! failures here (a shrinking PVC, a Retain claim deleted anyway, a
//! roll that never happens, an adoption that double-mounts) are
//! decision bugs, and a decision that needs a cluster to test is a
//! decision nobody tests.
//!
//! # Ownership, and the one child that is different
//!
//! ConfigMap, Service and Deployment carry an ownerReference to the CR
//! and die with it. **The PVC never does.** Kubernetes' garbage
//! collector does not know what `reclaim: Retain` means: an ownerRef'd
//! claim is collected the instant the CR goes, and for a tier-off
//! share that PVC is the only copy of the data. Retain therefore has
//! to be safe by CONSTRUCTION — there is no ownerReference for a
//! reconcile bug to mishandle — and `Delete` is an explicit action in
//! the finalizer, ordered PVC-delete-then-finalizer-removal so a crash
//! in between retries instead of orphaning.
//!
//! # Why the pod ever restarts
//!
//! The hub parses `--config` once, at boot, and has no reload path;
//! credentials ride `envFrom`, fixed at container start. So a settings
//! edit or a rotated Secret reaches a RUNNING hub only if something
//! rolls it. That something is the `checksum/config` /
//! `checksum/creds` pod-template annotations: they are the entire
//! mechanism, which is why `restartPolicy: Manual` is expressed by
//! holding the annotation back rather than by any cleverer means.


use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Secret, Service};
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event as KEvent, EventType, Recorder};
use kube::{Api, Client, Resource, ResourceExt};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::conflict::{self, Admission, Candidate};
use super::hubstatus;
use super::idle::{self, Decision, IdleState};
use super::persistence;
use super::crd::{FlintShare, FlintShareStatus, Lifecycle, Phase, Reclaim, RestartPolicy, ShareCondition};
use super::render::{self, RenderDefaults};

pub const FIELD_MANAGER: &str = "flint-lite-operator";
/// Finalizer name. Its only job is to give the operator a chance to
/// honor `reclaim` before the CR (and with it, owner GC) disappears.
pub const FINALIZER: &str = "flint.io/share-protection";

/// Steady-state re-check. Drift repair does not depend on this (SSA
/// re-applies on every event), so it is deliberately slow.
const REQUEUE_SETTLED: Duration = Duration::from_secs(300);
/// While a hub is still coming up, or blocked on something outside our
/// control, look again soon.
const REQUEUE_PROGRESS: Duration = Duration::from_secs(15);
const REQUEUE_BLOCKED: Duration = Duration::from_secs(30);
/// The floor for a share that is parked with nothing to count down to.
///
/// Measured at the design target before this existed: 3000 shares, 300
/// live, produced ~99 apiserver writes/s with NOTHING changing —
/// 11.8/s of status applies plus ~87/s of child-object applies. The
/// 2700 parked shares were most of it, each re-applying four identical
/// objects on a 300s beat to conclude nothing had happened.
const REQUEUE_PARKED: Duration = Duration::from_secs(1800);

/// How long a settled share may be left alone.
///
/// For a share with the idle ladder armed, this interval IS the
/// resolution of `suspendAfterSecs`: the ladder only ever decides
/// during a reconcile, and a `Hold` does not short-circuit, so the
/// share is left until the next timer fires. At the flat
/// `REQUEUE_SETTLED` a share configured to suspend after 20s is
/// recorded `Held` at "idle 0s" and then comes down up to five minutes
/// later — the knob means something other than what it says, and no
/// unit test can see it because `decide` is pure and correct.
///
/// Floored at `REQUEUE_PROGRESS` so a very small threshold cannot turn
/// into a hub poll per share per second, and capped at
/// `REQUEUE_SETTLED` so arming the ladder never makes a share cost
/// MORE to watch than leaving it off.
///
/// Each rung is looked at on its OWN knob: an up share is waiting to
/// suspend, a parked one is waiting to hibernate, and a hibernated one
/// is waiting for a wake — which arrives as a watch event, not on this
/// timer.
/// Stamped on the Deployment: what the operator last applied, and when
/// it last proved it. Both halves are needed — the hash alone cannot
/// tell a match that is one reconcile old from one that is a week old,
/// and a level-triggered operator must not let drift survive forever.
const ANN_RENDER_HASH: &str = "flint.io/render-hash";
const ANN_RENDER_VERIFIED: &str = "flint.io/render-verified-at";

/// How long a parked share may coast on a matching hash before the
/// operator re-asserts everything regardless. Ten parked requeues.
const FULL_APPLY_AFTER: i64 = 10 * 1800;

#[derive(Debug, PartialEq, Eq)]
enum GateState {
    /// Hash matches and the stamp is recent: the applies are provably
    /// no-ops and can be skipped.
    Fresh,
    /// Hash matches but the stamp is old, or the stamp is unparseable.
    /// Re-assert anyway — see FULL_APPLY_AFTER.
    Stale,
}

/// A fingerprint of everything a reconcile would apply.
///
/// Deliberately hashes the SERIALIZED objects rather than a few chosen
/// fields: a gate that hashes a subset silently stops noticing whatever
/// was left out, and the failure is a share that never converges with
/// nothing in the logs to say why.
fn render_fingerprint(r: &render::Rendered) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in [
        serde_json::to_string(&r.config_map).ok(),
        serde_json::to_string(&r.service).ok(),
        serde_json::to_string(&r.deployment).ok(),
        r.pvc.as_ref().and_then(|p| serde_json::to_string(p).ok()),
    ] {
        v.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// Read the gate off a live Deployment. `None` = no usable stamp (a
/// share this operator has not applied yet, or one whose render
/// changed), which always means apply.
fn apply_gate_state(dep: &Deployment, want: &str) -> Option<GateState> {
    let ann = dep.metadata.annotations.as_ref()?;
    if ann.get(ANN_RENDER_HASH).map(String::as_str) != Some(want) {
        return None;
    }
    let verified = ann.get(ANN_RENDER_VERIFIED)?;
    let age = chrono::DateTime::parse_from_rfc3339(verified)
        .ok()
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())?;
    // A stamp from the FUTURE is a clock problem, not freshness. Treat
    // it as stale rather than trusting it indefinitely.
    Some(if (0..FULL_APPLY_AFTER).contains(&age) { GateState::Fresh } else { GateState::Stale })
}

/// Ceiling for the failure backoff. A share that has been failing for
/// fifteen minutes is not going to be fixed by asking faster.
const RETRY_MAX: Duration = Duration::from_secs(900);
/// How long a share may sit un-Ready at full speed before the interval
/// starts stretching. Long enough to cover a normal cold start (the
/// startupProbe budget is 600s) without stretching a healthy boot.
const PROGRESS_PATIENCE_SECS: i64 = 600;

/// Exponential backoff for a share whose reconcile is FAILING.
///
/// The old policy was a flat 30s under a doc comment that claimed
/// "exponential-ish, bounded". At fleet scale that flat rate is the
/// term that makes a BROKEN fleet cost more than a healthy one: every
/// other path settles at 300s or 1800s, so a hundred shares failing for
/// one shared reason — a bad node, a missing Secret, an AZ blip — drive
/// the apiserver harder precisely when it is least able to take it, and
/// the pressure never decays.
///
/// Doubling from 30s to a 900s ceiling turns that into a decaying
/// signal. Cleared on any success, so a transient failure costs one
/// slow retry rather than a penalty box.
fn retry_backoff(consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.saturating_sub(1).min(16);
    let secs = 30u64.saturating_mul(1u64 << shift);
    Duration::from_secs(secs).min(RETRY_MAX)
}

/// How often to look at a share that is not Ready yet.
///
/// `Pending` and `Starting` used to requeue at a flat 15s FOREVER, with
/// no cap and no give-up. A share that can never start therefore never
/// stops asking — measured on a rig where 131 Pending and 67 Starting
/// shares drove the apiserver harder than the same fleet healthy.
///
/// A cold start is genuinely worth watching closely, so the first
/// `PROGRESS_PATIENCE_SECS` are unchanged. After that the interval
/// stretches toward the settled rate: still converging is still
/// progress, but it no longer costs the same as the first ten minutes.
fn progress_requeue(not_ready_for_secs: Option<i64>) -> Duration {
    match not_ready_for_secs {
        Some(age) if age > PROGRESS_PATIENCE_SECS => {
            // One doubling per patience window past the first.
            let steps = ((age - PROGRESS_PATIENCE_SECS) / PROGRESS_PATIENCE_SECS).min(8) as u32;
            let secs = REQUEUE_PROGRESS.as_secs().saturating_mul(1u64 << (steps + 1));
            Duration::from_secs(secs).min(REQUEUE_SETTLED)
        }
        _ => REQUEUE_PROGRESS,
    }
}

/// Spread a requeue so the fleet does not self-synchronise.
///
/// Without this, 3000 shares seeded together stay in lockstep forever:
/// every interval is a fixed constant, so they all come due in the same
/// second and the apiserver sees a 3000-wide spike on a 300s beat
/// instead of a flat 10/s. Deterministic per share — a hash, not a
/// random — so a requeue is reproducible and a share cannot be unlucky
/// twice for different reasons.
fn jittered(d: Duration, key: &str) -> Duration {
    use std::hash::{Hash, Hasher};
    if d <= REQUEUE_PROGRESS {
        return d; // too short to be worth spreading
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    // Spread CONTINUOUSLY across +/-25%, in milliseconds, not in
    // integer percent: percent granularity gives only 51 distinct
    // values, so at 3000 shares ~59 of them still come due in the same
    // millisecond. Caught by the spread test, which is why it counts
    // distinct intervals rather than just asserting "it moved".
    let base = d.as_millis() as u64;
    let span = base / 2; // the full +/-25% width
    let offset = if span == 0 { 0 } else { h.finish() % span };
    Duration::from_millis(base * 3 / 4 + offset)
}

fn settled_requeue(share: &FlintShare, state: IdleState) -> Duration {
    let idle = share.spec.idle.as_ref();
    match state {
        // Up, and the next rung down is a suspend.
        IdleState::Active => bounded(idle.and_then(|i| i.suspend_after_secs)),
        // Already parked, and the next rung down is a hibernate. A
        // parked share previously requeued at REQUEUE_PROGRESS forever,
        // re-applying four objects every 15s to decide nothing — the
        // cost falls on precisely the shares the ladder put away to
        // stop costing anything.
        // Already parked. The timer exists ONLY to notice the next
        // rung falling due, so re-check when that is actually near
        // rather than every 300s for hours.
        //
        // Clamping the raw threshold (what this did before) is wrong at
        // fleet scale: `bounded` caps at REQUEUE_SETTLED, so a share
        // with `hibernateAfterSecs: 86400` re-checked every 300s for a
        // day — 288 wakeups, 287 of which could only conclude "not
        // yet". Clamping the time REMAINING keeps the rung's resolution
        // exactly (the last re-check before it falls due is still
        // within REQUEUE_PROGRESS) while costing one wakeup, not
        // hundreds. `down_for` is read from an annotation with no I/O.
        //
        // No hibernate rung configured ⇒ nothing to count down to, so
        // the timer buys nothing at all and goes to the parked floor.
        IdleState::Suspended => match idle.and_then(|i| i.hibernate_after_secs) {
            Some(after) => {
                let down_for = idle::since(share)
                    .map(|t| (chrono::Utc::now() - t).num_seconds().max(0) as u64)
                    .unwrap_or(0);
                Duration::from_secs(after.saturating_sub(down_for))
                    .clamp(REQUEUE_PROGRESS, REQUEUE_PARKED)
            }
            None => REQUEUE_PARKED,
        },
        // Bottom of the ladder: there is no next rung, and a wake
        // arrives as a watch event rather than on this timer.
        IdleState::Hibernated => REQUEUE_PARKED,
        // Mid-verification is progress, not steady state. Unreachable
        // from here today (`verify_and_hibernate` short-circuits with
        // its own action) and cheap to keep right if that changes.
        IdleState::HibernateVerifying => REQUEUE_PROGRESS,
        // A disk rebuild in flight. Progress, never steady state — and
        // the one ladder position where a slow re-check is a share
        // sitting with no disk at all.
        IdleState::ReprovisionVerifying | IdleState::ReprovisionDraining => REQUEUE_PROGRESS,
    }
}

/// Clamp a ladder threshold into a sane re-check interval, or fall back
/// to the settled interval when that rung is not configured.
fn bounded(threshold_secs: Option<u64>) -> Duration {
    match threshold_secs {
        Some(secs) => Duration::from_secs(secs).clamp(REQUEUE_PROGRESS, REQUEUE_SETTLED),
        None => REQUEUE_SETTLED,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube api: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Ctx {
    pub client: Client,
    pub defaults: RenderDefaults,
    pub recorder: Recorder,
    /// Every share in the fleet, for conflict arbitration. A snapshot
    /// of the controller's own cache — the check must see other
    /// namespaces, which is exactly what admission control cannot.
    pub fleet: kube::runtime::reflector::Store<FlintShare>,
    /// The arbitration table, and a fingerprint of the fleet it was
    /// built from. See [`admit_table`] for why the fingerprint lives
    /// here rather than the table being rebuilt from a watch event.
    pub admit_cache: std::sync::Mutex<Option<(u64, Arc<conflict::AdmitTable>)>>,
    /// Consecutive reconcile failures per share, for [`error_policy`]'s
    /// backoff. Keyed `namespace/name`; cleared on any success.
    pub failures: dashmap::DashMap<String, u32>,
}

/// The arbitration table for the current fleet, rebuilt only when the
/// fleet actually changed.
///
/// # Why the fingerprint, and why it is checked HERE
///
/// `conflict::admit` is O(rank²) per call — measured 13.5 ms for the
/// median share and 52.6 ms for the newest at N=3000, so a full fleet
/// pass is ~47 SECONDS of CPU. The table answers the same question in
/// ~2.5 ms for the whole fleet. But a cache is only safe if it cannot
/// serve a stale answer, and a stale arbitration answer is not a slow
/// reconcile — it strands losers in `Failed` forever, or admits a
/// second hub onto a contended subtree.
///
/// The tempting place to rebuild is the FlintShare watch mapper. That
/// is WRONG: the mapper and the reflector `Store` are fed by two
/// INDEPENDENT watch connections, so the mapper can be behind the
/// store (or ahead of it) with nothing forcing them to agree. Instead
/// the LOOKUP validates itself — hash the fleet identity here, rebuild
/// synchronously on a miss. The hash is O(N) over fields we would have
/// had to read anyway; it costs microseconds against the milliseconds
/// it replaces, and it can never answer from a fleet that is not the
/// one the caller is reconciling against.
fn admit_table(ctx: &Ctx) -> (Vec<Arc<FlintShare>>, Arc<conflict::AdmitTable>) {
    use std::hash::{Hash, Hasher};

    let state = ctx.fleet.state();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    state.len().hash(&mut h);
    for s in &state {
        s.metadata.uid.hash(&mut h);
        s.metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.as_second())
            .hash(&mut h);
        s.spec.bucket.hash(&mut h);
        s.spec.prefix().hash(&mut h);
        s.spec.endpoint_key().hash(&mut h);
    }
    let fp = h.finish();

    if let Ok(guard) = ctx.admit_cache.lock() {
        if let Some((cached_fp, table)) = guard.as_ref() {
            if *cached_fp == fp {
                return (state, Arc::clone(table));
            }
        }
    }
    let fleet: Vec<Candidate> = state.iter().map(|s| Candidate::of(s)).collect();
    let table = Arc::new(conflict::AdmitTable::build(&fleet));
    if let Ok(mut guard) = ctx.admit_cache.lock() {
        *guard = Some((fp, Arc::clone(&table)));
    }
    (state, table)
}

// ---------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------

/// Parse a Kubernetes quantity into bytes. Only the storage-shaped
/// suffixes, because that is all a PVC size can be.
pub fn quantity_bytes(q: &str) -> Option<u128> {
    let q = q.trim();
    let (num, mult) = match q {
        _ if q.ends_with("Ki") => (&q[..q.len() - 2], 1024u128),
        _ if q.ends_with("Mi") => (&q[..q.len() - 2], 1024u128.pow(2)),
        _ if q.ends_with("Gi") => (&q[..q.len() - 2], 1024u128.pow(3)),
        _ if q.ends_with("Ti") => (&q[..q.len() - 2], 1024u128.pow(4)),
        _ if q.ends_with("Pi") => (&q[..q.len() - 2], 1024u128.pow(5)),
        _ if q.ends_with('k') || q.ends_with('K') => (&q[..q.len() - 1], 1000u128),
        _ if q.ends_with('M') => (&q[..q.len() - 1], 1000u128.pow(2)),
        _ if q.ends_with('G') => (&q[..q.len() - 1], 1000u128.pow(3)),
        _ if q.ends_with('T') => (&q[..q.len() - 1], 1000u128.pow(4)),
        _ if q.ends_with('P') => (&q[..q.len() - 1], 1000u128.pow(5)),
        _ => (q, 1u128),
    };
    num.parse::<u128>().ok().map(|n| n * mult)
}

/// What to do about the claim this reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimPlan {
    /// Apply it (create, or an in-place expansion the class may honor).
    Apply,
    /// Adopted, or unchanged in a way SSA would only churn.
    Skip,
    /// The share is HIBERNATED: its PVC was deliberately deleted and the
    /// bucket is the only copy. Re-applying it here would recreate an
    /// empty disk on the very next reconcile — which is not just wasted
    /// storage, it is the shape that makes a wake ambiguous, because
    /// the hub would find a fresh empty PVC and could not tell it from
    /// a DR restore that had already run.
    Hibernated,
    /// The CR asks for LESS than the claim already has. Kubernetes
    /// cannot shrink a PVC, and an apply would fail every reconcile
    /// forever with an error nobody reads — say so in status instead.
    ShrinkRefused { have: String, want: String },
}

pub fn claim_plan(
    existing: Option<&PersistentVolumeClaim>,
    want: &str,
    adopted: bool,
    state: IdleState,
) -> ClaimPlan {
    if state == IdleState::Hibernated {
        return ClaimPlan::Hibernated;
    }
    // A rebuild in flight owns this claim. Applying here would either
    // re-declare the old size over the new one or, worse, recreate the
    // claim the drain just deleted — and a share would come back on a
    // disk nobody asked for. The driver puts the state back to Active
    // when it is done, and the very next pass applies normally.
    if state.is_reprovisioning() {
        return ClaimPlan::Skip;
    }
    if adopted {
        return ClaimPlan::Skip;
    }
    let Some(have) = existing
        .and_then(|p| p.spec.as_ref())
        .and_then(|s| s.resources.as_ref())
        .and_then(|r| r.requests.as_ref())
        .and_then(|r| r.get("storage"))
        .map(|q| q.0.clone())
    else {
        return ClaimPlan::Apply;
    };
    match (quantity_bytes(&have), quantity_bytes(want)) {
        (Some(h), Some(w)) if w < h => ClaimPlan::ShrinkRefused {
            have,
            want: want.to_string(),
        },
        _ => ClaimPlan::Apply,
    }
}

/// Hash a Secret's contents so a rotation becomes a pod-template
/// change. Sorted keys: a map's iteration order must not decide
/// whether the fleet rolls.
/// The label a referenced credentials Secret must carry for the
/// operator to WATCH it.
///
/// The watch was `Api::<Secret>::all` with no selector, so every
/// Secret in the cluster — every service-account token, every
/// unrelated tenant's credentials — was resident in the operator's
/// memory for the sake of the handful it cares about.
///
/// Selecting on this label bounds that. The consequence of a missing
/// label is deliberately mild: the checksum is computed from a direct
/// `get`, not from the watch store (see `apply`), so an unlabelled
/// Secret still rotates the hub — on the next periodic reconcile
/// rather than instantly. The label buys immediacy, not correctness,
/// which is why adding it can be a warning rather than a failure.
pub const LABEL_CREDENTIALS: &str = "flint.io/credentials";

/// Whether a Secret carries the watch label, whatever its value.
pub fn is_watched_secret(secret: &Secret) -> bool {
    secret
        .metadata
        .labels
        .as_ref()
        .is_some_and(|l| l.contains_key(LABEL_CREDENTIALS))
}

pub fn creds_checksum(secret: &Secret) -> String {
    let mut h = Sha256::new();
    if let Some(data) = &secret.data {
        for (k, v) in data {
            h.update(k.as_bytes());
            h.update([0]);
            h.update(&v.0);
            h.update([0]);
        }
    }
    if let Some(data) = &secret.string_data {
        for (k, v) in data {
            h.update(k.as_bytes());
            h.update([0]);
            h.update(v.as_bytes());
            h.update([0]);
        }
    }
    format!("{:x}", h.finalize())
}

/// The checksum to WRITE into the pod template, and whether the
/// running pod is current.
///
/// `Manual` holds the old value so the config edit lands in the
/// ConfigMap without bouncing the share — the roll is then the
/// operator's `kubectl rollout restart`, and `ConfigCurrent=False`
/// says it is owed. (Note the honest caveat, documented in status: the
/// ConfigMap is already updated, so an unrelated restart also picks up
/// the new config.)
pub fn effective_checksum(
    existing: Option<&Deployment>,
    desired: &str,
    policy: RestartPolicy,
) -> (String, bool) {
    let running = existing
        .and_then(|d| d.spec.as_ref())
        .and_then(|s| s.template.metadata.as_ref())
        .and_then(|m| m.annotations.as_ref())
        .and_then(|a| a.get("checksum/config"))
        .cloned();
    match (policy, running) {
        (RestartPolicy::Manual, Some(old)) if old != desired => (old, false),
        _ => (desired.to_string(), true),
    }
}

/// Is a pod that mounts our claim one of ours?
///
/// "Ours" means it is managed by the Deployment we own or are adopting
/// — decided by that Deployment's own selector, so it works for a
/// chart-born Deployment (`app: flint-lite`) as well as ours. If the
/// Deployment does not exist yet, nothing mounting the claim is ours,
/// which is the whole point: that is the second writer.
pub fn pod_is_ours(dep: Option<&Deployment>, pod: &Pod) -> bool {
    let Some(sel) = dep
        .and_then(|d| d.spec.as_ref())
        .and_then(|s| s.selector.match_labels.as_ref())
    else {
        return false;
    };
    let labels = pod.metadata.labels.clone().unwrap_or_default();
    !sel.is_empty() && sel.iter().all(|(k, v)| labels.get(k) == Some(v))
}

pub fn pod_mounts_claim(pod: &Pod, claim: &str) -> bool {
    pod.spec
        .as_ref()
        .and_then(|s| s.volumes.as_ref())
        .is_some_and(|vols| {
            vols.iter().any(|v| {
                v.persistent_volume_claim
                    .as_ref()
                    .is_some_and(|p| p.claim_name == claim)
            })
        })
}

/// Why adoption is blocked, if it is.
///
/// The trap this closes: the chart's children have fixed,
/// release-unprefixed names, so a CR named anything else renders a
/// SECOND Deployment on the same claim. RWO is node-granular — with
/// WaitForFirstConsumer or a local PV both pods land on the same node
/// and both mount it — and the epoch cannot fence them, because both
/// read the same `state.db` and self-recognize as the same
/// `hub-{server_id}` holder. Two sqlite writers, no error, corrupted
/// state. So: no Deployment until the foreign pod is gone.
pub fn adoption_block(
    dep: Option<&Deployment>,
    pods: &[Pod],
    claim: &str,
    my_deployment: &str,
) -> Option<String> {
    let foreign: Vec<String> = pods
        .iter()
        .filter(|p| pod_mounts_claim(p, claim))
        .filter(|p| !pod_is_ours(dep, p))
        .filter_map(|p| p.metadata.name.clone())
        .collect();
    if foreign.is_empty() {
        return None;
    }
    Some(format!(
        "pod(s) {} already mount PVC {claim} and are not managed by Deployment {my_deployment}. \
         Two hubs on one state.db are two sqlite writers — and the epoch cannot fence them, \
         because both pods read the same state.db and recognize themselves as the holder. \
         Scale the old workload down (helm uninstall, or `kubectl scale deploy/... --replicas=0`) \
         and this share will adopt the claim.",
        foreign.join(", ")
    ))
}

/// Phase from what Kubernetes already knows.
///
/// The one that matters: a tiered hub is legitimately pre-listener for
/// minutes (epoch claim waiting out a dead holder's lease, DR import
/// walking the bucket). That is `Starting`, and an operator that calls
/// it failure kills takeovers.
pub fn phase_of(
    lifecycle: Lifecycle,
    dep: Option<&Deployment>,
    blocked: bool,
    idle_state: IdleState,
) -> Phase {
    if blocked {
        return Phase::Failed;
    }
    // An ADMIN's suspend outranks the ladder's, and reports
    // differently: a front door must be able to tell "will wake on
    // request" from "someone said no" without guessing.
    if lifecycle == Lifecycle::Suspended {
        return Phase::Suspended;
    }
    match idle_state {
        IdleState::Hibernated => return Phase::Hibernated,
        IdleState::Suspended => return Phase::IdleSuspended,
        // Reported for BOTH halves of the rebuild, including the one
        // where the pod is up: a consumer that reads Ready here would
        // mount a hub whose disk is about to be destroyed under it.
        IdleState::ReprovisionVerifying | IdleState::ReprovisionDraining => {
            return Phase::Reprovisioning
        }
        _ => {}
    }
    let status = dep.and_then(|d| d.status.as_ref());
    let available = status.and_then(|s| s.available_replicas).unwrap_or(0);
    let replicas = status.and_then(|s| s.replicas).unwrap_or(0);
    match (available, replicas) {
        (a, _) if a >= 1 => Phase::Ready,
        (_, r) if r >= 1 => Phase::Starting,
        _ => Phase::Pending,
    }
}

/// What consumers mount. A DNS name rather than a ClusterIP: it
/// survives the Service being recreated, and it is what goes in a PV's
/// `nfs.server` field.
/// What `status.address` should say — the thing a consumer mounts.
///
/// `advertise` (from `spec.service.advertiseAddress`) wins over
/// everything derived, and is returned VERBATIM. Explicit beats
/// inferred: the operator can see a Service object but it cannot see
/// the network the consumer lives on, and for every type except
/// LoadBalancer what it can derive is in-cluster-only — a ClusterIP no
/// other cluster can route to, or, for NodePort, the `.svc` DNS name
/// rather than the node address the consumer actually needs. Deriving
/// harder is not the answer; being told is.
pub fn address_of(svc: &Service, namespace: &str, advertise: Option<&str>) -> Option<String> {
    if let Some(a) = advertise.map(str::trim).filter(|a| !a.is_empty()) {
        return Some(a.to_string());
    }
    let spec = svc.spec.as_ref()?;
    let port = spec.ports.as_ref()?.first()?.port;
    let name = svc.metadata.name.as_ref()?;
    match spec.type_.as_deref() {
        Some("LoadBalancer") => {
            let ing = svc
                .status
                .as_ref()?
                .load_balancer
                .as_ref()?
                .ingress
                .as_ref()?
                .first()?
                .clone();
            let host = ing.hostname.or(ing.ip)?;
            Some(format!("{host}:{port}"))
        }
        _ => Some(format!("{name}.{namespace}.svc.cluster.local:{port}")),
    }
}

/// Upsert a condition, preserving `lastTransitionTime` unless the
/// status actually changed — so the timestamp means what it says
/// instead of "when we last reconciled".
pub fn set_condition(conds: &mut Vec<ShareCondition>, new: ShareCondition) {
    match conds.iter_mut().find(|c| c.r#type == new.r#type) {
        Some(old) => {
            let last = if old.status == new.status {
                old.last_transition_time.clone()
            } else {
                new.last_transition_time.clone()
            };
            *old = ShareCondition {
                last_transition_time: last,
                ..new
            };
        }
        None => conds.push(new),
    }
    conds.sort_by(|a, b| a.r#type.cmp(&b.r#type));
}

pub fn condition(
    r#type: &str,
    ok: bool,
    reason: &str,
    message: impl Into<Option<String>>,
    generation: Option<i64>,
) -> ShareCondition {
    ShareCondition {
        r#type: r#type.to_string(),
        status: if ok { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.into(),
        last_transition_time: now_rfc3339(),
        observed_generation: generation,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

pub async fn reconcile(share: Arc<FlintShare>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = share
        .metadata
        .namespace
        .clone()
        .ok_or_else(|| Error::Invalid("FlintShare has no namespace".into()))?;
    let api: Api<FlintShare> = Api::namespaced(ctx.client.clone(), &ns);

    kube::runtime::finalizer(&api, FINALIZER, share, |event| async {
        match event {
            kube::runtime::finalizer::Event::Apply(s) => apply(s, ctx.clone()).await,
            kube::runtime::finalizer::Event::Cleanup(s) => cleanup(s, ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::Invalid(format!("finalizer: {e}")))
}

async fn apply(share: Arc<FlintShare>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = share.namespace().unwrap_or_default();
    let name = share.name_any();
    let generation = share.metadata.generation;
    let mut conds: Vec<ShareCondition> = share
        .status
        .as_ref()
        .and_then(|s| s.conditions.clone())
        .unwrap_or_default();

    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    let existing_dep = get_opt(deployments.get(&render::names(&share).deployment)).await?;

    // --- 1. Does anyone else own this bucket subtree? ------------------
    // Before anything is created: a duplicate that never reconciles is
    // a duplicate that can never take over.
    let (_fleet_state, table) = admit_table(&ctx);
    if let Admission::Rejected { winner, message } = table.verdict(&Candidate::of(&share)) {
        warn!(share = %name, %winner, "refusing to reconcile: bucket subtree already owned");
        // An already-running loser must STOP. Skipping it would leave
        // exactly the hub that takes the prefix over when the winner
        // dies for a lease window.
        if existing_dep.is_some() {
            // A merge patch, not an apply: `force` is only legal on an
            // apply patch, and a full apply here would mean rendering
            // (and thereby endorsing) a spec we have just refused.
            deployments
                .patch(
                    &render::names(&share).deployment,
                    &PatchParams::default(),
                    &Patch::Merge(json!({"spec": {"replicas": 0}})),
                )
                .await?;
        }
        event(&ctx, &share, EventType::Warning, "Conflict", &message).await;
        set_condition(
            &mut conds,
            condition("Conflict", true, "SubtreeOwned", message.clone(), generation),
        );
        set_condition(
            &mut conds,
            condition("Ready", false, "Conflict", message, generation),
        );
        write_status(
            &ctx,
            &share,
            FlintShareStatus {
                phase: Some(Phase::Failed),
                address: None,
                observed_generation: generation,
                claim_name: None,
                server_id: carry_server_id(&share, None),
                conditions: Some(conds),
            },
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_SETTLED));
    }
    set_condition(
        &mut conds,
        condition("Conflict", false, "Unique", None, generation),
    );

    let names = render::names(&share);

    // --- 2. Adoption fence --------------------------------------------
    if names.claim_is_adopted {
        let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
        let all = pods.list(&ListParams::default()).await?.items;
        if let Some(why) =
            adoption_block(existing_dep.as_ref(), &all, &names.claim, &names.deployment)
        {
            warn!(share = %name, claim = %names.claim, "adoption blocked");
            event(&ctx, &share, EventType::Warning, "AdoptionBlocked", &why).await;
            set_condition(
                &mut conds,
                condition("AdoptionBlocked", true, "ForeignWriter", why.clone(), generation),
            );
            set_condition(
                &mut conds,
                condition("Ready", false, "AdoptionBlocked", why, generation),
            );
            write_status(
                &ctx,
                &share,
                FlintShareStatus {
                    phase: Some(Phase::Failed),
                    address: None,
                    observed_generation: generation,
                    claim_name: Some(names.claim.clone()),
                    server_id: carry_server_id(&share, None),
                    conditions: Some(conds),
                },
            )
            .await?;
            return Ok(Action::requeue(REQUEUE_BLOCKED));
        }
        set_condition(
            &mut conds,
            condition("AdoptionBlocked", false, "Adopted", None, generation),
        );
    }

    // --- 3. Credentials ------------------------------------------------
    // Hashing the Secret is what makes a rotation a deliberate rollout
    // instead of a heartbeat failure the operator cannot explain.
    let mut creds_sum = None;
    if let Some(secret_name) = share
        .spec
        .credentials_secret_ref
        .as_deref()
        .filter(|s| !s.is_empty() && share.spec.tiered())
    {
        let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
        match get_opt(secrets.get(secret_name)).await? {
            Some(s) => {
                creds_sum = Some(creds_checksum(&s));
                set_condition(
                    &mut conds,
                    condition("CredentialsFound", true, "Present", None, generation),
                );
                // Found, hashed, and the hub will roll on a rotation
                // either way — but only a labelled Secret gets a watch
                // event, so an unlabelled one rolls on the next
                // periodic pass instead of within seconds. Say so
                // rather than letting someone discover the latency
                // during a credential incident.
                if !is_watched_secret(&s) {
                    let msg = format!(
                        "Secret {secret_name} is missing the label {LABEL_CREDENTIALS} — \
                         rotations will be picked up on the next periodic reconcile rather \
                         than immediately. `kubectl label secret {secret_name} \
                         {LABEL_CREDENTIALS}=true` to close the gap."
                    );
                    set_condition(
                        &mut conds,
                        condition(
                            "CredentialsWatched",
                            false,
                            "Unlabelled",
                            Some(msg),
                            generation,
                        ),
                    );
                } else {
                    set_condition(
                        &mut conds,
                        condition("CredentialsWatched", true, "Labelled", None, generation),
                    );
                }
            }
            None => {
                let msg = format!(
                    "Secret {secret_name} does not exist; the hub container will not start until \
                     it does (envFrom is resolved at container start)"
                );
                event(&ctx, &share, EventType::Warning, "CredentialsMissing", &msg).await;
                set_condition(
                    &mut conds,
                    condition("CredentialsFound", false, "Missing", msg, generation),
                );
            }
        }
    }

    // --- 4. Render -----------------------------------------------------
    // A pre-existing Deployment keeps its selector: selectors are
    // immutable, and an adopted chart Deployment was born with
    // `app: flint-lite`.
    let selector_override = existing_dep
        .as_ref()
        .and_then(|d| d.spec.as_ref())
        .map(|s| s.selector.clone())
        .filter(|s| {
            s.match_labels.as_ref() != Some(&render::selector_labels(&share))
        });
    let mut rendered = render::render(
        &share,
        &ctx.defaults,
        creds_sum.as_deref(),
        selector_override,
    );

    let (write_sum, config_current) = effective_checksum(
        existing_dep.as_ref(),
        &rendered.config_checksum,
        share.spec.restart_policy.clone().unwrap_or_default(),
    );
    if write_sum != rendered.config_checksum {
        // restartPolicy: Manual — the new config is applied, the bounce
        // is owed.
        rendered.deployment = render::deployment(
            &share,
            &ctx.defaults,
            &write_sum,
            creds_sum.as_deref(),
            rendered.deployment.spec.as_ref().map(|s| s.selector.clone()),
        );
    }
    set_condition(
        &mut conds,
        condition(
            "ConfigCurrent",
            config_current,
            if config_current { "InSync" } else { "RestartOwed" },
            (!config_current).then(|| {
                format!(
                    "the rendered config changed but restartPolicy is Manual — the running hub \
                     keeps its old settings until it restarts (`kubectl rollout restart \
                     deploy/{}`)",
                    names.deployment
                )
            }),
            generation,
        ),
    );

    // --- 5. Apply -------------------------------------------------------
    let owner = share
        .controller_owner_ref(&())
        .ok_or_else(|| Error::Invalid("FlintShare has no uid".into()))?;
    let pp = PatchParams::apply(FIELD_MANAGER).force();

    // THE PARKED-SHARE APPLY GATE.
    //
    // Every reconcile re-applies four objects with no diff check. For a
    // share the ladder has already put away, none of them CAN have
    // changed — the operator is rewriting identical bytes on a timer.
    // Measured at the design target: 3000 shares with 300 live produced
    // ~99 apiserver writes/s with nothing changing, and the 2700 parked
    // shares were most of it.
    //
    // So: hash what we are about to apply, stamp it on the Deployment,
    // and for a PARKED share skip the applies while the stamp matches.
    //
    // Live shares are deliberately NOT gated. They are the minority, a
    // live hub is where drift actually costs something, and gating them
    // would trade a real property for a rounding error.
    //
    // THE FORCED PASS IS NOT OPTIONAL. This operator is level-triggered:
    // its correctness argument is that it re-asserts desired state
    // whether or not it believes anything changed. A hash gate is a
    // bet that the hash sees everything — and it does not see a
    // hand-edited ConfigMap, a stripped label, or anything else that
    // changes the CLUSTER without changing the RENDER. So the stamp
    // carries a timestamp and a stale one forces a full apply, which
    // bounds how long any drift can survive to FULL_APPLY_AFTER
    // regardless of what the hash thinks.
    let parked = idle::state_of(&share).is_down();
    let render_hash = render_fingerprint(&rendered);
    let gate = parked
        .then(|| existing_dep.as_ref().and_then(|d| apply_gate_state(d, &render_hash)))
        .flatten();
    let skip_applies = matches!(gate, Some(GateState::Fresh));

    let mut cm = rendered.config_map.clone();
    cm.metadata.owner_references = Some(vec![owner.clone()]);
    if !skip_applies {
        Api::<ConfigMap>::namespaced(ctx.client.clone(), &ns)
            .patch(&names.config_map, &pp, &Patch::Apply(&cm))
            .await?;
    }

    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
    let existing_pvc = get_opt(claims.get(&names.claim)).await?;
    // The EFFECTIVE size: `spec.persistence.size` unless auto-expand
    // has recorded a bigger target for that exact size. The operator
    // never writes spec, so the target rides an annotation — see
    // `lite_operator::persistence`.
    let want_size = persistence::effective_size(&share);
    match claim_plan(
        existing_pvc.as_ref(),
        &want_size,
        names.claim_is_adopted,
        idle::state_of(&share),
    ) {
        ClaimPlan::Apply => {
            if let Some(pvc) = rendered.pvc.clone() {
                // NO ownerReference, ever. See the module doc: owner GC
                // does not know what Retain means.
                match claims.patch(&names.claim, &pp, &Patch::Apply(&pvc)).await {
                    Ok(_) => {}
                    // A GROWTH the storage refuses. Overwhelmingly this
                    // is a StorageClass without `allowVolumeExpansion`,
                    // which no amount of retrying fixes — and failing
                    // the whole reconcile would take the rest of the
                    // share's convergence down with it, over a disk
                    // that is merely smaller than we wanted.
                    Err(kube::Error::Api(e)) if e.code == 422 || e.code == 403 => {
                        let msg = format!(
                            "the claim could not be resized to {want_size}: {}. The hub keeps                              its current disk; if this is a StorageClass without                              allowVolumeExpansion, no retry will help.",
                            e.message
                        );
                        warn!(share = %share.name_any(), "{msg}");
                        event(&ctx, &share, EventType::Warning, "ExpansionRefused", &msg).await;
                        set_condition(
                            &mut conds,
                            condition(
                                "PersistenceCurrent",
                                false,
                                "ExpansionRefused",
                                Some(msg),
                                generation,
                            ),
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        ClaimPlan::Skip => {}
        // Hibernated: the PVC was deliberately deleted and the bucket is
        // the only copy. It comes back at WAKE, not here — recreating it
        // now would leave an empty disk that a waking hub could not tell
        // from a restore that had already run.
        ClaimPlan::Hibernated => {}
        // Opted in, and eligible: rebuild the disk at the smaller
        // size instead of refusing forever. Only ever STARTED from
        // Active — `claim_plan` skips outright once a rebuild is in
        // flight, so this cannot re-trigger itself while running.
        ClaimPlan::ShrinkRefused { have, want }
            if shrink_reprovision_ok(&share, names.claim_is_adopted) =>
        {
            let msg = format!(
                "persistence.size {want} is smaller than the existing claim's {have}, and                  persistence.reprovisionOnShrink is on — verifying the bucket can rebuild this                  volume before destroying the disk. The share will come back on a NEW empty                  claim: expect a fresh serverId and a DR import."
            );
            warn!(share = %share.name_any(), "{msg}");
            event(&ctx, &share, EventType::Warning, "ReprovisionStarted", &msg).await;
            set_idle_state(&ctx, &share, &ns, IdleState::ReprovisionVerifying, false).await?;
            return Ok(Action::requeue(REQUEUE_PROGRESS));
        }
        ClaimPlan::ShrinkRefused { have, want } => {
            let hint = if auto_expand_would_undo_it(&share) {
                " persistence.autoExpand is on with a higher maxSize, so a rebuild at the \
                  smaller size would be grown straight back — an outage and a DR import that \
                  change nothing. Lower autoExpand.maxSize to the size you want, or turn \
                  autoExpand off."
            } else if names.claim_is_adopted {
                " The claim is adopted, so the operator will not rebuild it either."
            } else if share.spec.bucket.is_none() {
                " Without spec.bucket this PVC is the only copy, so it cannot be rebuilt from                   anywhere — reprovisionOnShrink is refused for tier-off shares."
            } else {
                " Set persistence.reprovisionOnShrink to rebuild the disk at the smaller size                   instead (verified against the bucket first, and it costs a wake)."
            };
            let msg = format!(
                "persistence.size {want} is smaller than the existing claim's {have}; Kubernetes \
                 cannot shrink a PVC. The hub keeps {have}.{hint}"
            );
            event(&ctx, &share, EventType::Warning, "ShrinkRefused", &msg).await;
            set_condition(
                &mut conds,
                condition("PersistenceCurrent", false, "ShrinkRefused", msg, generation),
            );
        }
    }

    let mut svc = rendered.service.clone();
    svc.metadata.owner_references = Some(vec![owner.clone()]);
    if !skip_applies {
        Api::<Service>::namespaced(ctx.client.clone(), &ns)
            .patch(&names.service, &pp, &Patch::Apply(&svc))
            .await?;
    }

    let dep = if skip_applies {
        // Nothing was applied, so reuse what we already read. The only
        // consumer below is the hibernate reclaim, which wants the LIVE
        // Deployment — which this is.
        existing_dep.clone().expect("gate only engages with an existing Deployment")
    } else {
        let mut dep = rendered.deployment.clone();
        dep.metadata.owner_references = Some(vec![owner]);
        // Stamp the gate on the object it gates on, so the next
        // reconcile can read both halves in the GET it already does.
        let ann = dep.metadata.annotations.get_or_insert_with(Default::default);
        ann.insert(ANN_RENDER_HASH.to_string(), render_hash.clone());
        ann.insert(
            ANN_RENDER_VERIFIED.to_string(),
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        deployments.patch(&names.deployment, &pp, &Patch::Apply(&dep)).await?
    };

    // A hibernated share's disk, once its pod has genuinely drained.
    // Driven from here rather than inside the decision because it has
    // to happen on a LATER reconcile: the hub needs its whole
    // termination grace period to flush and release the epoch.
    if idle::state_of(&share) == IdleState::Hibernated
        && reclaim_hibernated_disk(&ctx, &share, &ns, &names, Some(&dep)).await?
    {
        return Ok(Action::requeue(REQUEUE_SETTLED));
    }

    // --- 5b. The idle ladder --------------------------------------------
    // Runs AFTER everything is applied, so the poll below sees the hub
    // this reconcile actually produced, and any state change it writes
    // is picked up by the reconcile its own annotation patch triggers.
    let idle_outcome = drive_idle_ladder(&ctx, &share, &names, Some(&dep), &mut conds).await?;
    if let Some(action) = idle_outcome.short_circuit {
        write_status(
            &ctx,
            &share,
            FlintShareStatus {
                phase: Some(idle_outcome.phase),
                address: None,
                observed_generation: generation,
                claim_name: Some(names.claim.clone()),
                server_id: carry_server_id(&share, None),
                conditions: Some(conds),
            },
        )
        .await?;
        return Ok(action);
    }

    // --- 6. Status ------------------------------------------------------
    let svc_live = get_opt(
        Api::<Service>::namespaced(ctx.client.clone(), &ns).get(&names.service),
    )
    .await?;
    let lifecycle = share.spec.lifecycle.clone().unwrap_or_default();
    let phase = phase_of(lifecycle.clone(), Some(&dep), false, idle::state_of(&share));
    let ready = phase == Phase::Ready;
    set_condition(
        &mut conds,
        condition(
            "Ready",
            ready,
            match phase {
                Phase::Ready => "Serving",
                Phase::Suspended => "Suspended",
                Phase::Starting => "Starting",
                _ => "Pending",
            },
            match phase {
                // The window an operator must not misread. A tiered hub
                // claims its epoch and may import the whole bucket
                // BEFORE the listener binds; the startupProbe budgets
                // for it and liveness does not begin until it passes.
                Phase::Starting if share.spec.tiered() => Some(
                    "hub pod is pre-listener: claiming the volume epoch (which may wait out a \
                     dead holder's lease) and/or importing the bucket. This is progress."
                        .to_string(),
                ),
                _ => None,
            },
            generation,
        ),
    );

    write_status(
        &ctx,
        &share,
        FlintShareStatus {
            phase: Some(phase.clone()),
            address: svc_live.as_ref().and_then(|s| {
                address_of(
                    s,
                    &ns,
                    share
                        .spec
                        .service
                        .as_ref()
                        .and_then(|sv| sv.advertise_address.as_deref()),
                )
            }),
            observed_generation: generation,
            claim_name: Some(names.claim.clone()),
            server_id: carry_server_id(&share, idle_outcome.server_id.clone()),
            conditions: Some(conds.clone()),
        },
    )
    .await?;

    // Consume `wake-intent`, but only once the hub it was for is
    // actually serving.
    //
    // The ORDER is the whole subtlety. Clearing it in the same patch
    // that sets the share Active would clear it before the render that
    // reads it ever runs, so the intent would never reach the pod —
    // wired and inert, which is the state this was in to begin with.
    // Clearing it here means the ConfigMap flips back to the standing
    // setting on the next pass, and because `rollout_checksum` ignores
    // the boot-only knob, that flip does NOT roll the hub. Leaving it
    // set instead would make one front-door hint permanent.
    if phase == Phase::Ready
        && idle::state_of(&share) == idle::IdleState::Active
        && idle::wake_intent(&share).is_some()
    {
        let patch = serde_json::json!({
            "metadata": { "annotations": { idle::ANN_WAKE_INTENT: serde_json::Value::Null } }
        });
        Api::<FlintShare>::namespaced(ctx.client.clone(), &ns)
            .patch(
                &share.name_any(),
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Merge(&patch),
            )
            .await?;
    }

    // Reconciled without error: whatever was failing is not failing now,
    // so the next failure starts its backoff from the bottom.
    ctx.failures.remove(&format!("{ns}/{}", share.name_any()));

    let interval = match phase {
        // An admin's Suspended is inert: the ladder returns `Stay`
        // without so much as a poll, so there is nothing to look at
        // sooner.
        Phase::Suspended => REQUEUE_SETTLED,
        Phase::Ready | Phase::IdleSuspended | Phase::Hibernated => {
            settled_requeue(&share, idle::state_of(&share))
        }
        // Not Ready yet. Watch closely at first, then stretch — see
        // `progress_requeue`.
        _ => progress_requeue(
            conds.iter()
                .find(|c| c.r#type == "Ready" && c.status != "True")
                .and_then(|c| chrono::DateTime::parse_from_rfc3339(&c.last_transition_time).ok())
                .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds()),
        ),
    };
    Ok(Action::requeue(jittered(interval, &format!("{ns}/{}", share.name_any()))))
}

/// What the ladder did this pass.
struct IdleOutcome {
    phase: Phase,
    /// `Some` ⇒ the ladder changed something and this reconcile ends
    /// here; the change it made re-triggers the loop.
    short_circuit: Option<Action>,
    /// The hub's persisted server id, when this pass actually reached
    /// the hub. Carried out rather than polled for separately: the
    /// ladder already made that round trip, and a second one for a
    /// field that changes about once a week would be the wrong trade.
    server_id: Option<String>,
}

/// Poll the hub, decide, act.
///
/// Every state change is written as an ANNOTATION patch on the CR, not
/// to spec and not only to status. Spec belongs to the user. Status is
/// not read before the render, so a suspend recorded only there would
/// be undone by the very next reconcile — the renderer computes
/// `replicas` and server-side-applies it, seconds later, forever.
async fn drive_idle_ladder(
    ctx: &Arc<Ctx>,
    share: &Arc<FlintShare>,
    names: &render::Names,
    dep: Option<&Deployment>,
    conds: &mut Vec<ShareCondition>,
) -> Result<IdleOutcome> {
    let ns = share.namespace().unwrap_or_default();
    let generation = share.metadata.generation;
    let state = idle::state_of(share);
    let cfg = share.spec.idle.clone();
    let lifecycle = share.spec.lifecycle.clone().unwrap_or_default();

    // The phase the ladder implies, independent of what it decides to
    // do next. An admin's Suspended reports as `Suspended` and the
    // ladder's as `IdleSuspended`, deliberately: a front door has to be
    // able to tell "will wake on request" from "someone said no".
    let ladder_phase = match (lifecycle.clone(), state) {
        (Lifecycle::Suspended, _) => Some(Phase::Suspended),
        (_, IdleState::Suspended) => Some(Phase::IdleSuspended),
        (_, IdleState::Hibernated) => Some(Phase::Hibernated),
        _ => None,
    };

    // Nothing to do at all: no policy, never been touched by the
    // ladder, and no wake outstanding. The overwhelmingly common case,
    // and it must cost nothing — in particular NO status poll, which is
    // a network round trip per share per reconcile across the fleet.
    //
    // Auto-expand is the second reason to need that round trip: the
    // size it computes comes from the hub's manifest gauges and from
    // nowhere else. It is opt-in, so only the shares that asked for it
    // pay the poll — but forgetting it here is silent, and it was:
    // this branch is why the first drill run watched a 1Gi claim sit
    // at 1Gi under a 600 MiB project, with every unit test passing.
    if !needs_hub_poll(share, state) {
        return Ok(IdleOutcome {
            phase: ladder_phase.unwrap_or(Phase::Pending),
            short_circuit: None,
            // Deliberately not observed: this branch exists BECAUSE it
            // makes no round trip. `apply` carries the last known id
            // forward rather than blanking it.
            server_id: None,
        });
    }

    // Ask the hub, but only when there is a hub to ask. A share that is
    // already down has no pod, and a share still starting has no
    // listener — in both cases the poll would fail, and a failed poll
    // must never read as idleness.
    // Threaded alongside `hub_quiet` from the same poll: a hub that
    // could not be reached reports neither, and `None` here means
    // "unknown", never "nobody is mounted".
    let mut sessions_live = None;
    let mut server_id = None;
    let hub_quiet = if state.is_down() {
        Err("the hub is scaled to zero".to_string())
    } else {
        match poll_hub(ctx, share, &ns, names, dep).await {
            Ok(snap) => {
                sessions_live = snap.sessions_live();
                server_id = snap.server_id.clone();
                let after = cfg
                    .as_ref()
                    .and_then(|c| c.suspend_after_secs)
                    .unwrap_or(u64::MAX);
                let verdict = snap.suspendable(after);
                set_condition(
                    conds,
                    condition(
                        "HubReachable",
                        true,
                        "Polled",
                        // NO TICKING COUNTER HERE. `idle_secs` advances
                        // every second, so embedding it changed the
                        // condition message on nearly every reconcile,
                        // which changed `status`, which fired the
                        // operator's own FlintShare watch, which
                        // scheduled another reconcile. The gain is
                        // 1/(1 - min(1, d)) for a reconcile taking d
                        // seconds: invisible at the 4-share scale every
                        // drill has run at, and NON-TERMINATING once d
                        // reaches 1s — which it does as the fleet grows.
                        // Idle is published as a metric instead.
                        Some(format!(
                            "phase {:?}, rpoClean {:?}",
                            snap.phase, snap.rpo_clean
                        )),
                        generation,
                    ),
                );
                // A sizing fault the hub cannot work through: some
                // object in the bucket is bigger than this PVC can ever
                // hold, so every read of it answers NOSPC. Said here
                // because the hub's own log is the only other place it
                // appears, and nobody reads a healthy share's log.
                // Size the disk from what the bucket holds, if asked
                // to. Runs here because this is where the hub's
                // manifest numbers arrive; the annotation it writes is
                // read by the render on the NEXT pass, which is what
                // actually grows the claim.
                maybe_auto_expand(ctx, share, &ns, names, conds, &snap, generation).await?;

                let blocked = snap.gauges().map_or(0, |g| g.hydration_blocked);
                // Name the size to raise it TO. "Too small" without a
                // number sends a reader to the hub's log to find one,
                // and the hub already knows it from the manifest.
                let floor = snap
                    .gauges()
                    .and_then(|g| g.largest_object_bytes)
                    .map(|b| format!(" — the largest is {b} bytes"))
                    .unwrap_or_default();
                set_condition(
                    conds,
                    if blocked > 0 {
                        condition(
                            "HydrationUnblocked",
                            false,
                            "ObjectExceedsVolume",
                            Some(format!(
                                "{blocked} object(s) in the bucket are larger than this \
                                 volume minus its reserve and can never be read here{floor}; \
                                 raise spec.persistence.size past it"
                            )),
                            generation,
                        )
                    } else {
                        condition(
                            "HydrationUnblocked",
                            true,
                            "WithinVolume",
                            None,
                            generation,
                        )
                    },
                );
                verdict
            }
            Err(why) => {
                // Reported, not acted on. This is the condition an
                // operator looks at when a share will not suspend.
                set_condition(
                    conds,
                    condition("HubReachable", false, "PollFailed", Some(why.clone()), generation),
                );
                Err(why)
            }
        }
    };

    // The hibernate verification, which cannot live in the pure
    // decision function because it deletes a PVC.
    //
    // **Verify at DRAIN time, not at suspend time.** The drain's real
    // outcome is unobservable from here: the hub exits 0 whether or not
    // it flushed, scale-to-zero deletes the pod so no exit code
    // survives, and the operator has no bucket credentials to read the
    // released epoch mark itself. So the share is scaled back to ONE,
    // asked directly whether the bucket can rebuild it, and only then
    // is the disk reclaimed. A3's fast epoch re-claim makes that extra
    // wake cheap.
    if state == IdleState::HibernateVerifying {
        return verify_and_hibernate(ctx, share, names, dep, conds).await;
    }

    // A disk rebuild in flight. Runs BEFORE any idleness evaluation:
    // suspending or hibernating a share midway through would strand it
    // between two disks, and a wake request must not abort it either
    // (see `IdleState::ReprovisionVerifying`).
    if state.is_reprovisioning() {
        return drive_reprovision(ctx, share, names, dep, conds, state).await;
    }

    let now = chrono::Utc::now();

    // A request stamp from the future beyond any plausible skew is a
    // broken clock somewhere, and `decide` discards it rather than
    // letting it pin the share awake. Say so out loud: discarded
    // silently, the symptom is "this project never suspends" with
    // nothing anywhere naming the cause.
    if let Some(ahead) = idle::implausible_request(cfg.as_ref(), share, now) {
        warn!(
            share = %share.name_any(), ahead_secs = ahead,
            "{} is {}s in the FUTURE — ignoring it as a request signal; \
             check the clock on whatever writes it",
            idle::ANN_REQUESTED_AT, ahead,
        );
        event(
            ctx,
            share,
            EventType::Warning,
            "ImplausibleRequest",
            &format!(
                "{} is {}s in the future and is being ignored — the writer's clock is wrong",
                idle::ANN_REQUESTED_AT, ahead
            ),
        )
        .await;
    }

    let decision = idle::decide(
        cfg.as_ref(),
        idle::Inputs {
            share,
            now,
            hub_quiet,
            sessions_live,
        },
    );

    let (next, reason) = match &decision {
        Decision::Stay => {
            return Ok(IdleOutcome {
                phase: ladder_phase.unwrap_or(Phase::Pending),
                short_circuit: None,
                server_id: server_id.clone(),
            })
        }
        Decision::Hold(why) => {
            set_condition(
                conds,
                condition("IdleEligible", false, "Held", Some(why.clone()), generation),
            );
            return Ok(IdleOutcome {
                phase: ladder_phase.unwrap_or(Phase::Pending),
                short_circuit: None,
                server_id: server_id.clone(),
            });
        }
        Decision::Suspend => (IdleState::Suspended, "idle".to_string()),
        Decision::Wake => (IdleState::Active, "wake requested".to_string()),
        Decision::BeginHibernate => (
            IdleState::HibernateVerifying,
            "verifying the flush before reclaiming the disk".to_string(),
        ),
    };

    // Patch the annotations. This is the durable carrier; the render
    // reads it on the reconcile this patch triggers.
    let mut ann = serde_json::Map::new();
    ann.insert(
        idle::ANN_IDLE_STATE.to_string(),
        serde_json::Value::String(next.as_str().to_string()),
    );
    ann.insert(
        idle::ANN_IDLE_SINCE.to_string(),
        serde_json::Value::String(now_rfc3339()),
    );
    if next == IdleState::Active {
        // The request has been honoured; clearing it means the NEXT
        // idle window starts from the hub's own activity clock rather
        // than from a stale heartbeat. A null value removes the key.
        ann.insert(idle::ANN_REQUESTED_AT.to_string(), serde_json::Value::Null);
    }
    let patch = serde_json::json!({ "metadata": { "annotations": ann } });
    Api::<FlintShare>::namespaced(ctx.client.clone(), &ns)
        .patch(&share.name_any(), &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
        .await?;

    let (ev_reason, note) = match decision {
        Decision::Suspend => (
            "IdleSuspended",
            format!("no activity for the configured window ({reason}) — scaled to zero; the PVC is kept and a wake is a pod start"),
        ),
        Decision::Wake => ("Woken", format!("{reason} — scaling back up")),
        Decision::BeginHibernate => (
            "HibernateStarted",
            format!("{reason}; the PVC is deleted only after the hub reports a clean flush"),
        ),
        _ => unreachable!("Stay/Hold returned above"),
    };
    info!(share = %share.name_any(), state = next.as_str(), "idle ladder: {}", note);
    event(ctx, share, EventType::Normal, ev_reason, &note).await;
    set_condition(
        conds,
        condition("IdleEligible", true, ev_reason, Some(note), generation),
    );

    let phase = match next {
        IdleState::Suspended => Phase::IdleSuspended,
        IdleState::Hibernated => Phase::Hibernated,
        IdleState::HibernateVerifying => Phase::Ready,
        IdleState::ReprovisionVerifying | IdleState::ReprovisionDraining => Phase::Reprovisioning,
        IdleState::Active => Phase::Starting,
    };
    Ok(IdleOutcome {
        server_id: None,
        phase,
        // The annotation patch re-triggers the loop, which re-renders
        // with the new state. Requeue anyway rather than relying on the
        // watch: a dropped event must not strand a share half-way.
        short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)),
    })
}

/// The hibernate rung: scale to one, prove the bucket can rebuild the
/// volume, drain cleanly, and only then delete the PVC.
///
/// Everything about this is written to be safe when interrupted. The
/// state annotation is durable, so an operator restart mid-verification
/// resumes verifying rather than deleting unverified or waking for
/// good. And a wake request arriving DURING verification aborts it: the
/// share was wanted, which is a better reason to be up than the ladder
/// had to bring it down.
async fn verify_and_hibernate(
    ctx: &Arc<Ctx>,
    share: &Arc<FlintShare>,
    names: &render::Names,
    dep: Option<&Deployment>,
    conds: &mut Vec<ShareCondition>,
) -> Result<IdleOutcome> {
    let ns = share.namespace().unwrap_or_default();
    let generation = share.metadata.generation;

    // Someone wants it. Abandon the hibernate — it is already up.
    if idle::requested_at(share).is_some() {
        set_idle_state(ctx, share, &ns, IdleState::Active, true).await?;
        let note = "wake requested during hibernate verification — kept the PVC".to_string();
        info!(share = %share.name_any(), "{note}");
        event(ctx, share, EventType::Normal, "HibernateAborted", &note).await;
        set_condition(
            conds,
            condition("IdleEligible", false, "WokenDuringVerify", Some(note), generation),
        );
        return Ok(IdleOutcome { phase: Phase::Starting, short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)), server_id: None });
    }

    let snap = match poll_hub(ctx, share, &ns, names, dep).await {
        Ok(s) => s,
        Err(why) => {
            // The hub is still coming back up, or unreachable. WAIT.
            // The one thing this must never do is delete a PVC it could
            // not ask about.
            set_condition(
                conds,
                condition("HubReachable", false, "PollFailed", Some(why.clone()), generation),
            );
            return Ok(IdleOutcome { phase: Phase::Starting, short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)), server_id: None });
        }
    };

    if let Err(why) = snap.hibernatable() {
        // Not clean. Stay up and keep flushing; the next pass asks
        // again. This is the arm that stands between a bug and a
        // deleted project, so it reports loudly rather than retrying
        // silently forever.
        warn!(share = %share.name_any(), "hibernate deferred: {why}");
        event(
            ctx,
            share,
            EventType::Warning,
            "HibernateDeferred",
            &format!("not reclaiming the disk: {why}"),
        )
        .await;
        set_condition(
            conds,
            condition("IdleEligible", false, "NotRecoverable", Some(why), generation),
        );
        return Ok(IdleOutcome { phase: Phase::Ready, short_circuit: Some(Action::requeue(REQUEUE_BLOCKED)), server_id: None });
    }

    // Clean. Record Hibernated FIRST, so the render scales to zero and
    // the pod drains — the drain flushes and releases the epoch. The
    // PVC deletion waits for the pod to actually be gone: deleting a
    // claim a pod still mounts leaves it Terminating until the pod
    // exits anyway, and doing it in that order means an interrupted
    // operator could not tell a finished drain from an aborted one.
    set_idle_state(ctx, share, &ns, IdleState::Hibernated, false).await?;
    let note = format!(
        "the bucket can rebuild this volume (rpoClean, epoch {}); scaling to zero, then \
         reclaiming the disk. The CR and the bucket are untouched.",
        snap.epoch.as_ref().and_then(|e| e.number).unwrap_or(0)
    );
    info!(share = %share.name_any(), "{note}");
    event(ctx, share, EventType::Normal, "HibernateVerified", &note).await;
    set_condition(
        conds,
        condition("IdleEligible", true, "Hibernating", Some(note), generation),
    );
    Ok(IdleOutcome { phase: Phase::Hibernated, short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)), server_id: None })
}

/// Rebuild a share's disk at a smaller size.
///
/// The same verify-then-delete the hibernate rung uses, and for the
/// same reason: the operator holds no bucket credentials, so it cannot
/// check for itself that the tree is recoverable — it has to ask the
/// hub. Two durable steps, because the hub must be UP to answer and
/// DOWN to release the claim.
///
/// Deliberately NOT abortable by `requested-at`. Hibernation aborts on
/// a wake because it came down for want of interest, so interest is a
/// real reason to stop. This was asked for explicitly, and the front
/// door's own keepalive is not a change of mind — aborting on it would
/// make the feature unusable on exactly the shares someone is using.
async fn drive_reprovision(
    ctx: &Arc<Ctx>,
    share: &Arc<FlintShare>,
    names: &render::Names,
    dep: Option<&Deployment>,
    conds: &mut Vec<ShareCondition>,
    state: IdleState,
) -> Result<IdleOutcome> {
    let ns = share.namespace().unwrap_or_default();
    let generation = share.metadata.generation;

    if state == IdleState::ReprovisionVerifying {
        let snap = match poll_hub(ctx, share, &ns, names, dep).await {
            Ok(s) => s,
            Err(why) => {
                // Still coming up, or unreachable. WAIT — never destroy
                // a disk we could not ask about.
                set_condition(
                    conds,
                    condition("HubReachable", false, "PollFailed", Some(why), generation),
                );
                return Ok(IdleOutcome {
                    phase: Phase::Reprovisioning,
                    short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)),
                    server_id: None,
                });
            }
        };
        if let Err(why) = snap.hibernatable() {
            // Not recoverable yet. Stay up and keep flushing. This arm
            // stands between a resize and a lost project, so it says so
            // rather than retrying in silence.
            warn!(share = %share.name_any(), "reprovision deferred: {why}");
            event(
                ctx,
                share,
                EventType::Warning,
                "ReprovisionDeferred",
                &format!("not rebuilding the disk: {why}"),
            )
            .await;
            set_condition(
                conds,
                condition("PersistenceCurrent", false, "NotRecoverable", Some(why), generation),
            );
            return Ok(IdleOutcome {
                phase: Phase::Reprovisioning,
                short_circuit: Some(Action::requeue(REQUEUE_BLOCKED)),
                server_id: None,
            });
        }
        set_idle_state(ctx, share, &ns, IdleState::ReprovisionDraining, false).await?;
        let note = format!(
            "the bucket can rebuild this volume (rpoClean, epoch {}); scaling to zero to \
             release the claim, then recreating it at {}",
            snap.epoch.as_ref().and_then(|e| e.number).unwrap_or(0),
            share.spec.persistence.size,
        );
        info!(share = %share.name_any(), "{note}");
        event(ctx, share, EventType::Normal, "ReprovisionVerified", &note).await;
        set_condition(
            conds,
            condition("PersistenceCurrent", false, "Reprovisioning", Some(note), generation),
        );
        return Ok(IdleOutcome {
            phase: Phase::Reprovisioning,
            short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)),
            server_id: None,
        });
    }

    // ReprovisionDraining: the render has us at zero replicas. Wait for
    // the pod to be genuinely gone before touching the claim — deleting
    // one a pod still mounts just parks it in Terminating, where an
    // interrupted operator cannot tell a finished drain from an
    // aborted one. Same rule as the hibernate reclaim.
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let still_running = pods
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .any(|p| pod_is_ours(dep, p) && pod_mounts_claim(p, &names.claim));
    if still_running {
        return Ok(IdleOutcome {
            phase: Phase::Reprovisioning,
            short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)),
            server_id: None,
        });
    }

    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
    if get_opt(claims.get(&names.claim)).await?.is_some() {
        claims.delete(&names.claim, &Default::default()).await?;
        let note = format!(
            "PVC {} deleted — recreating it at {}. The bucket is the only copy until the \
             import finishes.",
            names.claim, share.spec.persistence.size,
        );
        warn!(share = %share.name_any(), "{note}");
        event(ctx, share, EventType::Normal, "DiskReclaimed", &note).await;
        // Not Active yet: the claim may linger in Terminating, and
        // returning to Active while it does would re-apply the OLD
        // object and resurrect the old size. Come back and check.
        return Ok(IdleOutcome {
            phase: Phase::Reprovisioning,
            short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)),
            server_id: None,
        });
    }

    // Gone. Back to Active — the next pass renders a fresh claim at the
    // new size and starts the hub, which imports from the bucket.
    set_idle_state(ctx, share, &ns, IdleState::Active, false).await?;
    let note = format!(
        "disk rebuilt at {} — the hub is starting and will import from the bucket. Every \
         client must remount: the serverId is new.",
        share.spec.persistence.size,
    );
    info!(share = %share.name_any(), "{note}");
    event(ctx, share, EventType::Normal, "Reprovisioned", &note).await;
    set_condition(
        conds,
        condition("PersistenceCurrent", true, "Reprovisioned", Some(note), generation),
    );
    Ok(IdleOutcome {
        phase: Phase::Starting,
        short_circuit: Some(Action::requeue(REQUEUE_PROGRESS)),
        server_id: None,
    })
}

/// Does this share need its hub asked for `/status` this pass?
///
/// A poll is a network round trip per share per reconcile, so the
/// default answer is no and the exceptions are enumerated. There are
/// exactly three, and the third is the one that is easy to forget:
///
/// 1. an idle policy — the ladder's whole input is the hub's activity;
/// 2. a ladder position other than Active — something is in flight;
/// 3. **auto-expand** — the size it computes comes from the hub's
///    manifest gauges and from nowhere else.
///
/// Pinned by a test because the failure is silent. The first drill run
/// of auto-expand watched a 1Gi claim sit at 1Gi under a 600 MiB
/// project, with every unit test green, purely because this function's
/// third arm did not exist.
pub fn needs_hub_poll(share: &FlintShare, state: IdleState) -> bool {
    if share.spec.idle.is_some() || state != IdleState::Active {
        return true;
    }
    share
        .spec
        .persistence
        .auto_expand
        .as_ref()
        .is_some_and(|a| a.enabled.unwrap_or(false))
}

/// Grow the claim to fit the project, when `autoExpand` says to.
///
/// Writes a TARGET, never the claim directly and never `spec`. The
/// render reads that target on the next pass and applies it as an
/// ordinary size — so expansion travels the same path as any other
/// size change, and there is exactly one place that decides how big a
/// claim should be.
///
/// Every refusal is reported rather than retried in silence, because
/// the two that matter most are invisible otherwise: a StorageClass
/// without `allowVolumeExpansion` rejects the patch, and a claim at
/// `maxSize` stops growing while the project keeps growing.
async fn maybe_auto_expand(
    ctx: &Arc<Ctx>,
    share: &Arc<FlintShare>,
    ns: &str,
    names: &render::Names,
    conds: &mut Vec<ShareCondition>,
    snap: &hubstatus::HubSnapshot,
    generation: Option<i64>,
) -> Result<()> {
    let Some(ae) = share.spec.persistence.auto_expand.as_ref() else {
        return Ok(());
    };
    if !ae.enabled.unwrap_or(false) || names.claim_is_adopted {
        return Ok(());
    }
    // Mid-rebuild the claim belongs to the reprovision driver.
    if idle::state_of(share).is_reprovisioning() {
        return Ok(());
    }
    // The hub has not read a manifest yet. `None` is not zero: sizing a
    // disk against "I do not know" is how a project gets a 1Gi claim.
    let (Some(logical), Some(largest)) = (
        snap.gauges().and_then(|g| g.logical_bytes),
        snap.gauges().and_then(|g| g.largest_object_bytes),
    ) else {
        return Ok(());
    };
    let Some(max_bytes) = ae.max_size.as_deref().and_then(quantity_bytes) else {
        return Ok(()); // CEL requires it; nothing to do without one
    };

    // Measure against what is PROVISIONED, not what spec asks for —
    // that is what an expansion has to beat to be worth an API write.
    let current = persistence::effective_size(share);
    let Some(current_bytes) = quantity_bytes(&current) else { return Ok(()) };

    let inv = persistence::Inventory { logical_bytes: logical, largest_object_bytes: largest };
    let buffer = ae.buffer_percent.unwrap_or(persistence::DEFAULT_BUFFER_PCT);

    let Some(target_bytes) = persistence::expand_to(inv, buffer, current_bytes, max_bytes) else {
        // Nothing to do — but say so when the reason is the ceiling
        // rather than "big enough", or a project silently stops being
        // able to cache itself.
        let wanted = persistence::wanted_bytes(inv, buffer);
        if wanted > max_bytes {
            let msg = format!(
                "autoExpand is capped: this project wants {} but maxSize is {}. The disk stays \
                 at {} and the tier evicts more to fit.",
                persistence::as_gi(wanted),
                ae.max_size.clone().unwrap_or_default(),
                current,
            );
            set_condition(
                conds,
                condition("PersistenceCurrent", true, "AtMaxSize", Some(msg), generation),
            );
        }
        return Ok(());
    };

    let target = persistence::as_gi(target_bytes);
    let basis = share.spec.persistence.size.clone();
    let patch = serde_json::json!({ "metadata": { "annotations": {
        persistence::ANN_SIZE_TARGET: persistence::format_target(&basis, &target)
    }}});
    Api::<FlintShare>::namespaced(ctx.client.clone(), ns)
        .patch(&share.name_any(), &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
        .await?;

    let msg = format!(
        "growing the disk {current} → {target}: the project holds {} with a {buffer}% buffer \
         (largest object {}). spec.persistence.size is unchanged at {basis}.",
        persistence::as_gi(logical as u128),
        persistence::as_gi(largest as u128),
    );
    info!(share = %share.name_any(), "{msg}");
    event(ctx, share, EventType::Normal, "AutoExpanding", &msg).await;
    set_condition(
        conds,
        condition("PersistenceCurrent", false, "Expanding", Some(msg), generation),
    );
    Ok(())
}

/// May a shrink rebuild this share's disk?
///
/// Three independent refusals, and each is a data-safety statement
/// rather than a policy preference:
///
/// - **Not opted in.** Destroying a volume is not something to infer
///   from an edit to a size field.
/// - **No bucket.** A tier-off share's PVC is the only copy of its
///   data. There is nothing to rebuild it from, so this would be a
///   delete dressed up as a resize.
/// - **Adopted claim.** The operator did not create it and does not get
///   to delete it — the same rule the hibernate reclaim follows.
///
/// Started only from `Active`: a rebuild already in flight must not
/// restart itself, and one must never begin under an admin's
/// `lifecycle: Suspended`, where the hub is down and cannot be asked
/// whether the bucket is current.
pub fn shrink_reprovision_ok(share: &FlintShare, adopted: bool) -> bool {
    share.spec.persistence.reprovision_on_shrink.unwrap_or(false)
        && share.spec.bucket.is_some()
        && !adopted
        && idle::state_of(share) == IdleState::Active
        && share.spec.lifecycle.clone().unwrap_or_default() == Lifecycle::Active
        && !auto_expand_would_undo_it(share)
}

/// Would auto-expand simply grow this shrink back?
///
/// The two features pull in opposite directions and, left alone, they
/// LOOP: the user lowers `size`, the rebuild destroys the disk and
/// imports the project onto a smaller one, auto-expand then measures
/// the same project and grows straight back to where it started. The
/// net effect is an outage and a DR import that change nothing — and it
/// repeats every time the user tries again.
///
/// So the ceiling has to come down with the size. `maxSize` clamps what
/// auto-expand may ever ask for, which makes this decidable from spec
/// alone — no inventory, no poll, no ordering question about which
/// controller ran first. Lower the ceiling to the size you want and the
/// shrink goes through; leave it high and the share says why it will
/// not.
pub fn auto_expand_would_undo_it(share: &FlintShare) -> bool {
    let Some(ae) = share.spec.persistence.auto_expand.as_ref() else {
        return false;
    };
    if !ae.enabled.unwrap_or(false) {
        return false;
    }
    // No ceiling means unbounded growth, so it would certainly undo it.
    // (CEL requires one when enabled; this is the belt.)
    let Some(max) = ae.max_size.as_deref().and_then(quantity_bytes) else {
        return true;
    };
    match quantity_bytes(&share.spec.persistence.size) {
        Some(want) => max > want,
        None => true,
    }
}

/// Patch the ladder's durable position onto the CR.
async fn set_idle_state(
    ctx: &Arc<Ctx>,
    share: &FlintShare,
    ns: &str,
    next: IdleState,
    clear_request: bool,
) -> Result<()> {
    let mut ann = serde_json::Map::new();
    ann.insert(
        idle::ANN_IDLE_STATE.to_string(),
        serde_json::Value::String(next.as_str().to_string()),
    );
    ann.insert(idle::ANN_IDLE_SINCE.to_string(), serde_json::Value::String(now_rfc3339()));
    if clear_request {
        ann.insert(idle::ANN_REQUESTED_AT.to_string(), serde_json::Value::Null);
    }
    let patch = serde_json::json!({ "metadata": { "annotations": ann } });
    Api::<FlintShare>::namespaced(ctx.client.clone(), ns)
        .patch(&share.name_any(), &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Delete a hibernated share's PVC, once its pod is genuinely gone.
///
/// Separate from the decision above and driven from the main apply
/// path, because it has to happen on a LATER reconcile: the hub needs
/// its full termination grace period to drain, flush and release the
/// epoch, and deleting the claim while the pod still mounts it just
/// parks the claim in Terminating — where an interrupted operator
/// cannot tell a finished drain from an aborted one.
async fn reclaim_hibernated_disk(
    ctx: &Arc<Ctx>,
    share: &FlintShare,
    ns: &str,
    names: &render::Names,
    dep: Option<&Deployment>,
) -> Result<bool> {
    // Adopted claims are the user's. The operator did not create it and
    // does not get to delete it.
    if names.claim_is_adopted {
        return Ok(false);
    }
    // A wake landed between the hibernate decision and this reclaim.
    // The data is in the bucket either way — the verification proved
    // that before anything was scaled down — so deleting here would
    // only turn a pod-start wake into a full DR import. Let the ladder
    // process the request instead.
    if idle::requested_at(share).is_some() {
        return Ok(false);
    }
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let all = pods.list(&ListParams::default()).await?.items;
    let still_running = all
        .iter()
        .any(|p| pod_is_ours(dep, p) && pod_mounts_claim(p, &names.claim));
    if still_running {
        // Draining. The grace period is sized for a real flush.
        return Ok(false);
    }
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), ns);
    if get_opt(claims.get(&names.claim)).await?.is_none() {
        return Ok(false); // already reclaimed
    }
    claims.delete(&names.claim, &Default::default()).await?;
    let note = format!(
        "PVC {} deleted — the bucket is now the only copy, and waking this share is a DR \
         import. The CR and the bucket itself are untouched.",
        names.claim
    );
    warn!(share = %share.name_any(), "{note}");
    event(ctx, share, EventType::Normal, "DiskReclaimed", &note).await;
    Ok(true)
}

/// Find this share's hub pod and ask it for `/status`.
///
/// The POD IP, never the Service — the Service carries NFS and may be a
/// LoadBalancer, and the hub's monitoring port also serves a read-write
/// file API.
async fn poll_hub(
    ctx: &Arc<Ctx>,
    share: &FlintShare,
    ns: &str,
    names: &render::Names,
    dep: Option<&Deployment>,
) -> std::result::Result<hubstatus::HubSnapshot, String> {
    let Some(m) = share.spec.monitoring.as_ref().filter(|m| m.enabled.unwrap_or(false)) else {
        return Err(
            "spec.monitoring is off, so the hub publishes no status — the idle ladder cannot \
             tell whether anyone is using this share"
                .to_string(),
        );
    };
    let port = m.port.unwrap_or(render::HEALTH_PORT);

    // SELECTED, not the whole namespace. This runs once per poll per
    // share, so at fleet scale it is the dominant API-server term — and
    // in a namespace holding many shares, each poll was paging in every
    // other share's pods to discard them client-side.
    //
    // The selector comes from the Deployment rather than from
    // `render::labels`, because those two are not the same thing for an
    // ADOPTED share: its Deployment was born with the chart's selector
    // and a Deployment's selector is immutable. Asking the object what
    // it selects is the only version that is right in both cases, and
    // `pod_is_ours` still re-checks it below.
    let lp = dep
        .and_then(|d| d.spec.as_ref())
        .and_then(|sp| sp.selector.match_labels.as_ref())
        .filter(|m| !m.is_empty())
        .map(|m| {
            ListParams::default().labels(
                &m.iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
            )
        })
        .unwrap_or_default();
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let all = pods
        .list(&lp)
        .await
        .map_err(|e| format!("listing pods: {e}"))?
        .items;
    let ip = all
        .iter()
        .filter(|p| pod_is_ours(dep, p) && pod_mounts_claim(p, &names.claim))
        // A terminating pod still has an IP and still answers, and its
        // answer is about a hub that is going away. Never poll one.
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .find_map(|p| p.status.as_ref()?.pod_ip.clone())
        .ok_or_else(|| "no running hub pod with an IP".to_string())?;

    hubstatus::poll(&ip, port, std::time::Duration::from_secs(3)).await
}

/// Why an adopted claim outlives its share. One string, because the
/// same rule is applied from two places and they must not drift.
pub const ADOPTED_CLAIM_NOT_DELETED: &str =
    "the claim is adopted (spec.existingClaim) and the operator does not delete volumes it \
     did not create";

/// The server id to publish this pass.
///
/// A reconcile that did not reach the hub — the ladder is off, the
/// share is scaled to zero, the poll failed — must not blank a value
/// it simply did not look for. `write_status` replaces the whole
/// status, so "unobserved" has to be spelled as "keep what was there".
fn carry_server_id(share: &FlintShare, observed: Option<String>) -> Option<String> {
    observed.or_else(|| share.status.as_ref().and_then(|s| s.server_id.clone()))
}

/// Whether the operator may delete this share's claim.
///
/// **Adopted claims are never deleted, whatever `reclaim` says.** The
/// user pointed `spec.existingClaim` at a PVC they made; `reclaim:
/// Delete` means "delete the PVC I created for this share", and here
/// the operator created nothing. The hibernate path has always refused
/// on these grounds (`hibernate_reclaim`); CR deletion did not, so one
/// field meant two different things depending on the route that
/// reached it.
///
/// The asymmetry of being wrong settles it. Adoption is the documented
/// migration path off a helm release, so an adopted claim is evidence
/// that something else still believes it owns that data — deleting it
/// destroys a volume whose real owner expects it to be there.
/// Refusing leaks a PVC, which is visible in `kubectl get pvc` and
/// removable with one command.
pub fn may_delete_claim(reclaim: &Reclaim, claim_is_adopted: bool) -> bool {
    matches!(reclaim, Reclaim::Delete) && !claim_is_adopted
}

/// CR deletion. Owner GC takes the ConfigMap, Service and Deployment;
/// only the claim needs a decision.
async fn cleanup(share: Arc<FlintShare>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = share.namespace().unwrap_or_default();
    let names = render::names(&share);
    let reclaim = share.spec.reclaim.clone().unwrap_or_default();

    match reclaim {
        Reclaim::Retain => {
            info!(
                share = %share.name_any(), claim = %names.claim,
                "reclaim: Retain — keeping the PVC (the bucket is never touched either)"
            );
            event(
                &ctx,
                &share,
                EventType::Normal,
                "Retained",
                &format!(
                    "PVC {} kept (reclaim: Retain). Delete it by hand when you are sure.",
                    names.claim
                ),
            )
            .await;
        }
        // `may_delete_claim` is the rule; this arm is what happens when
        // it says no. Guarding on the function rather than repeating
        // `claim_is_adopted` is what keeps the two in step.
        Reclaim::Delete if !may_delete_claim(&reclaim, names.claim_is_adopted) => {
            let why = ADOPTED_CLAIM_NOT_DELETED;
            warn!(
                share = %share.name_any(), claim = %names.claim,
                "reclaim: Delete — REFUSED: {why}"
            );
            event(
                &ctx,
                &share,
                EventType::Warning,
                "ReclaimRefused",
                &format!("PVC {} was NOT deleted despite reclaim: Delete — {why}", names.claim),
            )
            .await;
        }
        Reclaim::Delete => {
            // Order matters and is the whole reason this runs before
            // the finalizer is removed: the delete is issued FIRST, and
            // deletionTimestamp is durable, so an operator crash before
            // the finalizer comes off simply retries. The reverse order
            // orphans the claim forever.
            //
            // The PVC will sit in Terminating until the pod releases it
            // — which happens once the CR is gone and owner GC removes
            // the Deployment. That is expected, not a hang.
            let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
            match claims.delete(&names.claim, &DeleteParams::default()).await {
                Ok(_) => {
                    warn!(share = %share.name_any(), claim = %names.claim,
                          "reclaim: Delete — deleting the PVC");
                    event(
                        &ctx,
                        &share,
                        EventType::Warning,
                        "Deleting",
                        &format!("deleting PVC {} (reclaim: Delete)", names.claim),
                    )
                    .await;
                }
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(Action::await_change())
}

/// A `get` that treats 404 as "not there" rather than an error.
async fn get_opt<T>(fut: impl std::future::Future<Output = kube::Result<T>>) -> Result<Option<T>> {
    match fut.await {
        Ok(v) => Ok(Some(v)),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn write_status(ctx: &Ctx, share: &FlintShare, status: FlintShareStatus) -> Result<()> {
    let ns = share.namespace().unwrap_or_default();
    let api: Api<FlintShare> = Api::namespaced(ctx.client.clone(), &ns);
    let patch = json!({
        "apiVersion": "flint.io/v1alpha1",
        "kind": "FlintShare",
        "status": status,
    });
    api.patch_status(
        &share.name_any(),
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&patch),
    )
    .await?;
    Ok(())
}

async fn event(ctx: &Ctx, share: &FlintShare, ty: EventType, reason: &str, note: &str) {
    // Events are best-effort by design: losing one must never fail a
    // reconcile that otherwise converged.
    if let Err(e) = ctx
        .recorder
        .publish(
            &KEvent {
                type_: ty,
                reason: reason.to_string(),
                note: Some(note.to_string()),
                action: "Reconcile".to_string(),
                secondary: None,
            },
            &share.object_ref(&()),
        )
        .await
    {
        warn!("could not publish event {reason}: {e}");
    }
}

/// Requeue policy on error: exponential-ish, bounded. A failing API
/// call is usually transient; a failing render is not, and neither
/// deserves a hot loop.
pub fn error_policy(share: Arc<FlintShare>, err: &Error, ctx: Arc<Ctx>) -> Action {
    let key = format!(
        "{}/{}",
        share.metadata.namespace.clone().unwrap_or_default(),
        share.name_any()
    );
    let n = {
        let mut e = ctx.failures.entry(key.clone()).or_insert(0);
        *e = e.saturating_add(1);
        *e
    };
    let wait = jittered(retry_backoff(n), &key);
    warn!(
        share = %share.name_any(), failures = n, retry_in_secs = wait.as_secs(),
        "reconcile failed: {err}"
    );
    Action::requeue(wait)
}

/// Map a child object (Secret, PVC, Pod) back to the shares that care
/// about it. Used by the controller's `watches()`.
pub fn shares_referencing_secret(
    shares: &[Arc<FlintShare>],
    secret_ns: &str,
    secret_name: &str,
) -> Vec<(String, String)> {
    shares
        .iter()
        .filter(|s| s.namespace().as_deref() == Some(secret_ns))
        .filter(|s| {
            s.spec.credentials_secret_ref.as_deref() == Some(secret_name) && s.spec.tiered()
        })
        .map(|s| (s.namespace().unwrap_or_default(), s.name_any()))
        .collect()
}

/// Map a claim back to the shares bound to it — the PVC's route home,
/// since it carries no ownerReference to travel by.
pub fn shares_using_claim(
    shares: &[Arc<FlintShare>],
    claim_ns: &str,
    claim_name: &str,
) -> Vec<(String, String)> {
    shares
        .iter()
        .filter(|s| s.namespace().as_deref() == Some(claim_ns))
        .filter(|s| render::names(s).claim == claim_name)
        .map(|s| (s.namespace().unwrap_or_default(), s.name_any()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lite_operator::crd::{FlintShareSpec, IdleSpec, PersistenceSpec};
    use std::collections::BTreeMap;
    use k8s_openapi::api::apps::v1::{DeploymentSpec, DeploymentStatus};
    use k8s_openapi::api::core::v1::{
        PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec,
        ServicePort, ServiceSpec as K8sServiceSpec, ServiceStatus, Volume, VolumeResourceRequirements,
    };
    use k8s_openapi::api::core::v1::{LoadBalancerIngress, LoadBalancerStatus};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

    fn share_named(name: &str) -> FlintShare {
        let mut s = FlintShare::new(
            name,
            FlintShareSpec {
                bucket: None,
                key_prefix: None,
                endpoint: None,
                region: None,
                credentials_secret_ref: None,
                import_on_start: None,
                persistence: PersistenceSpec {
                    size: "20Gi".into(),
                    storage_class_name: None,

                    reprovision_on_shrink: None,
                    auto_expand: None,
                },
                service: None,
                image: None,
                log_level: None,
                resources: None,
                node_selector: None,
                settings: None,
                lifecycle: None,
                reclaim: None,
                existing_claim: None,
                restart_policy: None,
                startup_failure_threshold: None,
            termination_grace_period_seconds: None,
            monitoring: None,
            idle: None,
            },
        );
        s.metadata.namespace = Some("ws".into());
        s
    }

    #[test]
    fn quantities_parse_the_way_kubernetes_writes_them() {
        assert_eq!(quantity_bytes("20Gi"), Some(20 * 1024u128.pow(3)));
        assert_eq!(quantity_bytes("1Ti"), Some(1024u128.pow(4)));
        assert_eq!(quantity_bytes("500M"), Some(500_000_000));
        assert_eq!(quantity_bytes("1024"), Some(1024));
        assert_eq!(quantity_bytes("garbage"), None);
        assert!(quantity_bytes("100Gi") > quantity_bytes("20Gi"));
        // Binary vs decimal actually differ, and a shrink guard that
        // conflated them would refuse a legitimate growth.
        assert!(quantity_bytes("1Gi") > quantity_bytes("1G"));
    }

    fn shrinkable(opt_in: bool, bucket: bool) -> FlintShare {
        let mut s = share_named("sh");
        s.spec.persistence.size = "5Gi".into();
        s.spec.persistence.reprovision_on_shrink = Some(opt_in);
        if bucket {
            s.spec.bucket = Some("b".into());
        }
        s
    }

    fn at_state(mut s: FlintShare, st: IdleState) -> FlintShare {
        s.metadata.annotations = Some(BTreeMap::from([(
            idle::ANN_IDLE_STATE.to_string(),
            st.as_str().to_string(),
        )]));
        s
    }

    /// The two-step, and getting it backwards is the bug: the hub has
    /// to be UP to be asked whether the bucket is current, and DOWN
    /// before anything may take its claim away.
    #[test]
    fn only_the_draining_half_of_a_reprovision_scales_to_zero() {
        assert!(!IdleState::ReprovisionVerifying.is_down(), "must stay up to be polled");
        assert!(IdleState::ReprovisionDraining.is_down(), "must go down to release the claim");
        assert!(IdleState::ReprovisionVerifying.is_reprovisioning());
        assert!(IdleState::ReprovisionDraining.is_reprovisioning());
        assert!(!IdleState::HibernateVerifying.is_reprovisioning());
        assert!(!IdleState::Active.is_reprovisioning());
    }

    /// The ladder's position is carried in an annotation, so a state
    /// that does not survive a write/read round trip is one an operator
    /// restart forgets — mid-rebuild, with a share between two disks.
    #[test]
    fn the_reprovision_states_survive_the_annotation_round_trip() {
        for st in [IdleState::ReprovisionVerifying, IdleState::ReprovisionDraining] {
            let s = at_state(share_named("rt"), st);
            assert_eq!(idle::state_of(&s), st, "{} did not round-trip", st.as_str());
        }
    }

    /// While a rebuild is in flight the claim belongs to the driver.
    /// An Apply here would re-declare the OLD size over the new one, or
    /// recreate the very claim the drain just deleted — either way the
    /// share comes back on a disk nobody asked for.
    #[test]
    fn a_reprovision_in_flight_keeps_the_apply_path_off_the_claim() {
        for st in [IdleState::ReprovisionVerifying, IdleState::ReprovisionDraining] {
            assert_eq!(
                claim_plan(Some(&pvc_of("100Gi")), "5Gi", false, st),
                ClaimPlan::Skip,
                "{} must not touch the claim",
                st.as_str()
            );
            // The post-delete moment: no claim, still mid-rebuild.
            assert_eq!(claim_plan(None, "5Gi", false, st), ClaimPlan::Skip);
        }
        // ...and the instant it finishes, the fresh claim is applied at
        // the SMALLER size. This is the whole point of the feature.
        assert_eq!(claim_plan(None, "5Gi", false, IdleState::Active), ClaimPlan::Apply);
    }

    fn with_auto_expand(mut s: FlintShare, max: &str) -> FlintShare {
        s.spec.persistence.auto_expand = Some(crate::lite_operator::crd::AutoExpandSpec {
            enabled: Some(true),
            buffer_percent: None,
            max_size: Some(max.into()),
        });
        s
    }

    /// Auto-expand reads the hub's manifest gauges and nothing else,
    /// so a share that never gets polled never grows — silently, with
    /// every other test still green. That is exactly what the first
    /// drill run of this feature did.
    #[test]
    fn an_auto_expand_share_is_polled_even_with_no_idle_policy() {
        let plain = share_named("p");
        assert!(
            !needs_hub_poll(&plain, IdleState::Active),
            "the common case must still cost nothing"
        );

        let ae = with_auto_expand(share_named("p"), "50Gi");
        assert!(
            needs_hub_poll(&ae, IdleState::Active),
            "auto-expand needs the gauges, so it needs the poll"
        );

        // Off means off: no poll bought by merely mentioning the block.
        let mut off = with_auto_expand(share_named("p"), "50Gi");
        off.spec.persistence.auto_expand.as_mut().unwrap().enabled = Some(false);
        assert!(!needs_hub_poll(&off, IdleState::Active));

        // The other two reasons still stand on their own.
        assert!(needs_hub_poll(&plain, IdleState::Suspended));
        let mut idle_cfg = share_named("p");
        idle_cfg.spec.idle = Some(crate::lite_operator::crd::IdleSpec {
            suspend_after_secs: Some(900),
            hibernate_after_secs: None,
            suspend_with_sessions: None,
        });
        assert!(needs_hub_poll(&idle_cfg, IdleState::Active));
    }

    /// The interaction that loops if nobody guards it: the user lowers
    /// `size`, the rebuild destroys the disk and imports onto a smaller
    /// one, and auto-expand measures the same project and grows right
    /// back. An outage and a DR import that change nothing — repeatable
    /// every time the user tries again.
    ///
    /// The ceiling has to come down with the size, which makes it
    /// decidable from spec alone: no inventory, no poll, no question
    /// about which ran first.
    #[test]
    fn a_shrink_is_refused_when_auto_expand_would_grow_it_straight_back() {
        // size 5Gi, ceiling 50Gi: the rebuild would be undone.
        let looped = with_auto_expand(shrinkable(true, true), "50Gi");
        assert!(auto_expand_would_undo_it(&looped));
        assert!(
            !shrink_reprovision_ok(&looped, false),
            "a rebuild that auto-expand will undo must not run"
        );

        // Ceiling lowered to the requested size: the shrink sticks, so
        // it is allowed. `shrinkable` asks for 5Gi.
        let agreed = with_auto_expand(shrinkable(true, true), "5Gi");
        assert!(!auto_expand_would_undo_it(&agreed));
        assert!(shrink_reprovision_ok(&agreed, false), "ceiling agrees — let it through");

        // A ceiling BELOW the size cannot grow it back either.
        let under = with_auto_expand(shrinkable(true, true), "2Gi");
        assert!(!auto_expand_would_undo_it(&under));

        // Auto-expand off: the guard must not fire at all.
        let mut off = with_auto_expand(shrinkable(true, true), "50Gi");
        off.spec.persistence.auto_expand.as_mut().unwrap().enabled = Some(false);
        assert!(!auto_expand_would_undo_it(&off));
        assert!(shrink_reprovision_ok(&off, false));
    }

    /// `spec` stays the user's. The target rides an annotation, and the
    /// BASIS is what makes "the operator grew past spec" distinguishable
    /// from "the user wants something smaller" — without it both are
    /// just `size < target` and the user could never shrink.
    #[test]
    fn an_edit_to_size_always_beats_a_recorded_target() {
        let mut s = share_named("ae");
        s.spec.persistence.size = "5Gi".into();
        assert_eq!(persistence::effective_size(&s), "5Gi", "no target yet");

        // The operator grew it, recording the size it grew FROM.
        s.metadata.annotations = Some(BTreeMap::from([(
            persistence::ANN_SIZE_TARGET.to_string(),
            persistence::format_target("5Gi", "40Gi"),
        )]));
        assert_eq!(persistence::effective_size(&s), "40Gi", "the target is in force");

        // The user edits size. The basis no longer matches, so their
        // number wins — this is what lets a shrink happen at all.
        s.spec.persistence.size = "8Gi".into();
        assert_eq!(
            persistence::effective_size(&s),
            "8Gi",
            "a stale basis must discard the target, or the user can never shrink"
        );

        // A target SMALLER than spec is ignored rather than shrinking.
        s.spec.persistence.size = "80Gi".into();
        s.metadata.annotations = Some(BTreeMap::from([(
            persistence::ANN_SIZE_TARGET.to_string(),
            persistence::format_target("80Gi", "40Gi"),
        )]));
        assert_eq!(persistence::effective_size(&s), "80Gi", "a target never shrinks a claim");

        // Garbage in the annotation falls back to spec instead of panicking.
        s.metadata.annotations = Some(BTreeMap::from([(
            persistence::ANN_SIZE_TARGET.to_string(),
            "nonsense".to_string(),
        )]));
        assert_eq!(persistence::effective_size(&s), "80Gi");
    }

    /// Three independent refusals, each a data-safety statement. Every
    /// one is asserted alone against an otherwise-eligible share, so a
    /// predicate that dropped any single conjunct fails here.
    #[test]
    fn a_shrink_rebuild_is_refused_unless_every_guard_agrees() {
        assert!(
            shrink_reprovision_ok(&shrinkable(true, true), false),
            "opted in, tiered, own claim, Active — the one eligible shape"
        );
        assert!(
            !shrink_reprovision_ok(&shrinkable(false, true), false),
            "not opted in: destroying a volume is not inferred from a size edit"
        );
        assert!(
            !shrink_reprovision_ok(&shrinkable(true, false), false),
            "no bucket: the PVC is the only copy, so this would be a delete, not a resize"
        );
        assert!(
            !shrink_reprovision_ok(&shrinkable(true, true), true),
            "adopted claim: the operator did not create it and does not delete it"
        );
    }

    /// A rebuild starts from Active and nowhere else. Re-entering from
    /// its own states would restart it forever, and starting under an
    /// admin's Suspended would ask a hub that is not running.
    #[test]
    fn a_shrink_rebuild_starts_only_from_a_running_share() {
        for st in [
            IdleState::ReprovisionVerifying,
            IdleState::ReprovisionDraining,
            IdleState::Suspended,
            IdleState::Hibernated,
            IdleState::HibernateVerifying,
        ] {
            assert!(
                !shrink_reprovision_ok(&at_state(shrinkable(true, true), st), false),
                "must not (re)start from {}",
                st.as_str()
            );
        }
        let mut admin_down = shrinkable(true, true);
        admin_down.spec.lifecycle = Some(Lifecycle::Suspended);
        assert!(
            !shrink_reprovision_ok(&admin_down, false),
            "an admin's Suspended means the hub is down and cannot be asked"
        );
    }

    fn pvc_of(size: &str) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            spec: Some(PersistentVolumeClaimSpec {
                resources: Some(VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".to_string(),
                        Quantity(size.to_string()),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A shrink must be REFUSED in status, not attempted: the apply
    /// would fail on every reconcile forever, and the operator would
    /// look broken for a spec the user can simply correct.
    #[test]
    fn a_smaller_size_is_refused_rather_than_retried_forever() {
        assert_eq!(claim_plan(None, "20Gi", false, IdleState::Active), ClaimPlan::Apply);
        assert_eq!(claim_plan(Some(&pvc_of("20Gi")), "20Gi", false, IdleState::Active), ClaimPlan::Apply);
        assert_eq!(claim_plan(Some(&pvc_of("20Gi")), "100Gi", false, IdleState::Active), ClaimPlan::Apply);
        assert_eq!(
            claim_plan(Some(&pvc_of("100Gi")), "20Gi", false, IdleState::Active),
            ClaimPlan::ShrinkRefused {
                have: "100Gi".into(),
                want: "20Gi".into()
            }
        );
        // An adopted claim is someone else's declaration; we bind to
        // it, we do not re-declare it.
        assert_eq!(claim_plan(Some(&pvc_of("100Gi")), "20Gi", true, IdleState::Active), ClaimPlan::Skip);
    }

    #[test]
    fn a_rotated_secret_changes_its_checksum() {
        let s = |v: &str| Secret {
            data: Some(BTreeMap::from([(
                "AWS_SECRET_ACCESS_KEY".to_string(),
                k8s_openapi::ByteString(v.as_bytes().to_vec()),
            )])),
            ..Default::default()
        };
        assert_eq!(creds_checksum(&s("a")), creds_checksum(&s("a")));
        assert_ne!(creds_checksum(&s("a")), creds_checksum(&s("b")));
    }

    fn dep_with_checksum(sum: Option<&str>, selector: Option<BTreeMap<String, String>>) -> Deployment {
        Deployment {
            spec: Some(DeploymentSpec {
                selector: LabelSelector {
                    match_labels: selector,
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        annotations: sum.map(|s| {
                            BTreeMap::from([("checksum/config".to_string(), s.to_string())])
                        }),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec::default()),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Immediate rolls; Manual applies the config but holds the pod
    /// template's annotation back, so the bounce is the operator's
    /// call — and status says it is owed.
    #[test]
    fn manual_restart_policy_withholds_the_roll() {
        let existing = dep_with_checksum(Some("old"), None);
        assert_eq!(
            effective_checksum(Some(&existing), "new", RestartPolicy::Immediate),
            ("new".to_string(), true)
        );
        assert_eq!(
            effective_checksum(Some(&existing), "new", RestartPolicy::Manual),
            ("old".to_string(), false)
        );
        // Nothing pending ⇒ current under either policy.
        assert_eq!(
            effective_checksum(Some(&existing), "old", RestartPolicy::Manual),
            ("old".to_string(), true)
        );
        // A first create has nothing to hold back.
        assert_eq!(
            effective_checksum(None, "new", RestartPolicy::Manual),
            ("new".to_string(), true)
        );
    }

    fn pod_on(name: &str, claim: &str, labels: &[(&str, &str)]) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(
                    labels
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                ..Default::default()
            },
            spec: Some(PodSpec {
                volumes: Some(vec![Volume {
                    name: "data".to_string(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: claim.to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The migration trap, closed: a chart-installed hub still mounting
    /// the claim blocks adoption, because a second Deployment on one
    /// RWO claim can land on the SAME NODE (RWO is node-granular) and
    /// the epoch cannot fence two pods that read the same state.db.
    #[test]
    fn a_foreign_pod_on_the_claim_blocks_adoption() {
        let chart_pod = pod_on("flint-lite-abc", "flint-lite-data", &[("app", "flint-lite")]);

        // Our Deployment does not exist yet: the chart's pod is the
        // second writer we would be creating.
        let why = adoption_block(None, &[chart_pod.clone()], "flint-lite-data", "tenant-a")
            .expect("must block");
        assert!(why.contains("flint-lite-abc"), "{why}");
        assert!(why.contains("state.db"), "the message must say WHY: {why}");

        // Adopting IN PLACE (same name as the chart's Deployment, whose
        // selector matches the pod) is not blocked — there is only ever
        // one Deployment, so there is no second writer.
        let chart_dep = dep_with_checksum(
            None,
            Some(BTreeMap::from([("app".to_string(), "flint-lite".to_string())])),
        );
        assert!(adoption_block(
            Some(&chart_dep),
            &[chart_pod.clone()],
            "flint-lite-data",
            "flint-lite"
        )
        .is_none());

        // A pod on a DIFFERENT claim is not our problem.
        let other = pod_on("other-1", "someone-else", &[("app", "x")]);
        assert!(adoption_block(None, &[other], "flint-lite-data", "tenant-a").is_none());
    }

    #[test]
    fn pod_ownership_is_decided_by_the_deployments_own_selector() {
        let pod = pod_on("p", "c", &[("app", "flint-lite"), ("extra", "1")]);
        let chart_dep = dep_with_checksum(
            None,
            Some(BTreeMap::from([("app".to_string(), "flint-lite".to_string())])),
        );
        assert!(pod_is_ours(Some(&chart_dep), &pod));

        let ours = dep_with_checksum(
            None,
            Some(BTreeMap::from([(
                "flint.io/share".to_string(),
                "tenant-a".to_string(),
            )])),
        );
        assert!(!pod_is_ours(Some(&ours), &pod));
        assert!(!pod_is_ours(None, &pod), "no Deployment ⇒ nothing is ours");
    }

    /// Pre-listener is PROGRESS. An operator that reports Failed here
    /// invites someone to "fix" a takeover or a DR import by deleting
    /// it.
    #[test]
    fn a_pre_listener_hub_reports_starting_not_failed() {
        let mut dep = dep_with_checksum(None, None);
        dep.status = Some(DeploymentStatus {
            replicas: Some(1),
            available_replicas: Some(0),
            ..Default::default()
        });
        assert_eq!(phase_of(Lifecycle::Active, Some(&dep), false, IdleState::Active), Phase::Starting);

        dep.status = Some(DeploymentStatus {
            replicas: Some(1),
            available_replicas: Some(1),
            ..Default::default()
        });
        assert_eq!(phase_of(Lifecycle::Active, Some(&dep), false, IdleState::Active), Phase::Ready);
        assert_eq!(
            phase_of(Lifecycle::Suspended, Some(&dep), false, IdleState::Active),
            Phase::Suspended
        );
        assert_eq!(phase_of(Lifecycle::Active, None, false, IdleState::Active), Phase::Pending);
        assert_eq!(phase_of(Lifecycle::Active, Some(&dep), true, IdleState::Active), Phase::Failed);
    }

    #[test]
    fn the_address_is_what_a_consumer_would_mount() {
        let svc = |ty: &str, status: Option<ServiceStatus>| Service {
            metadata: ObjectMeta {
                name: Some("tenant-a".into()),
                ..Default::default()
            },
            spec: Some(K8sServiceSpec {
                type_: Some(ty.into()),
                ports: Some(vec![ServicePort {
                    port: 2049,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status,
        };
        assert_eq!(
            address_of(&svc("ClusterIP", None), "ws", None).as_deref(),
            Some("tenant-a.ws.svc.cluster.local:2049")
        );
        // A LoadBalancer with no ingress yet has no address to report —
        // reporting the ClusterIP would be a lie a consumer cannot reach.
        assert_eq!(address_of(&svc("LoadBalancer", None), "ws", None), None);
        let lb = svc(
            "LoadBalancer",
            Some(ServiceStatus {
                load_balancer: Some(LoadBalancerStatus {
                    ingress: Some(vec![LoadBalancerIngress {
                        hostname: Some("a.elb.amazonaws.com".into()),
                        ..Default::default()
                    }]),
                }),
                ..Default::default()
            }),
        );
        assert_eq!(
            address_of(&lb, "ws", None).as_deref(),
            Some("a.elb.amazonaws.com:2049")
        );
    }

    fn armed(secs: u64) -> IdleSpec {
        IdleSpec {
            suspend_after_secs: Some(secs),
            hibernate_after_secs: None,
            suspend_with_sessions: None,
        }
    }

    /// The idle ladder only decides during a reconcile, so the settled
    /// requeue is the resolution of `suspendAfterSecs`. Found in a
    /// cluster, not here: the share sat `Held` at "idle 0s" with a
    /// 20s threshold and the next look was 300s away.
    #[test]
    fn an_armed_ladder_is_looked_at_on_its_own_threshold() {
        use IdleState::*;
        let mut share = share_named("s");
        // Ladder off: nothing to be timely about.
        assert_eq!(settled_requeue(&share, Active), REQUEUE_SETTLED);

        // Armed: the look-again interval tracks the knob, so a share
        // that goes quiet is not held past its own threshold.
        share.spec.idle = Some(armed(20));
        assert_eq!(settled_requeue(&share, Active), Duration::from_secs(20));

        // ... but never faster than REQUEUE_PROGRESS: a 1s threshold
        // must not become a hub poll per share per second.
        share.spec.idle = Some(armed(1));
        assert_eq!(settled_requeue(&share, Active), REQUEUE_PROGRESS);

        // ... and never slower than the unarmed case, so arming the
        // ladder cannot make a share cost more to watch.
        share.spec.idle = Some(armed(9_999));
        assert_eq!(settled_requeue(&share, Active), REQUEUE_SETTLED);

        // A share already parked is waiting on the HIBERNATE knob, not
        // the suspend one. With hibernation OFF there is no next rung
        // to count down to, so the timer buys nothing at all and the
        // share drops to the parked floor.
        share.spec.idle = Some(armed(20));
        assert_eq!(settled_requeue(&share, Suspended), REQUEUE_PARKED);

        // With hibernation ON, the interval is the time REMAINING to
        // that rung, not the raw threshold. This is the fleet-scale
        // point: clamping the threshold capped at 300s, so a share with
        // `hibernateAfterSecs: 86400` re-checked 288 times in a day to
        // conclude "not yet" 287 times. No `idle-since` annotation
        // here, so `down_for` is 0 and the whole threshold remains.
        share.spec.idle = Some(IdleSpec {
            suspend_after_secs: Some(20),
            hibernate_after_secs: Some(120),
            suspend_with_sessions: None,
        });
        assert_eq!(settled_requeue(&share, Suspended), Duration::from_secs(120));

        // A DAY-long hibernate threshold must not become 288 wakeups.
        share.spec.idle = Some(IdleSpec {
            suspend_after_secs: Some(20),
            hibernate_after_secs: Some(86_400),
            suspend_with_sessions: None,
        });
        assert_eq!(
            settled_requeue(&share, Suspended),
            REQUEUE_PARKED,
            "a far-off rung must clamp to the parked floor, not to REQUEUE_SETTLED"
        );

        // And once the rung is NEAR, resolution comes back: a share
        // that went down 86,340s ago is 60s from hibernating and must
        // be looked at then, not in half an hour.
        share.metadata.annotations.get_or_insert_with(Default::default).insert(
            idle::ANN_IDLE_SINCE.to_string(),
            (chrono::Utc::now() - chrono::Duration::seconds(86_340))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        let near = settled_requeue(&share, Suspended);
        assert!(
            near <= Duration::from_secs(75) && near >= REQUEUE_PROGRESS,
            "a rung 60s away must be re-checked in ~60s, got {near:?} — clamping on the \
             raw threshold instead of the REMAINING time loses the rung's resolution"
        );

        // Bottom of the ladder: nothing below it, so the floor again.
        assert_eq!(settled_requeue(&share, Hibernated), REQUEUE_PARKED);
    }

    // ---------------------------------------------------------------
    // Failure and boot must SHED load, not add it. Every other requeue
    // path settles at 300s or 1800s; these two governed a fleet that is
    // NOT working, and they were the only ones that got faster the
    // worse things got. Measured on a rig: 131 Pending + 67 Starting
    // shares drove the apiserver harder than the same fleet healthy.
    // ---------------------------------------------------------------

    /// The backoff must actually back off, and must stop somewhere.
    #[test]
    fn a_failing_share_is_asked_less_and_less_often() {
        assert_eq!(retry_backoff(1), Duration::from_secs(30), "the first retry is prompt");
        assert_eq!(retry_backoff(2), Duration::from_secs(60));
        assert_eq!(retry_backoff(3), Duration::from_secs(120));

        // Strictly increasing until the ceiling, then pinned there.
        let mut prev = Duration::ZERO;
        for n in 1..=40u32 {
            let d = retry_backoff(n);
            assert!(d >= prev, "backoff went BACKWARDS at {n}: {prev:?} -> {d:?}");
            assert!(d <= RETRY_MAX, "backoff blew past the ceiling at {n}: {d:?}");
            prev = d;
        }
        assert_eq!(retry_backoff(40), RETRY_MAX, "it must reach and hold the ceiling");

        // The whole point, stated as a comparison: a share failing for a
        // while must cost LESS than a healthy one, not more.
        assert!(
            retry_backoff(10) > REQUEUE_SETTLED,
            "a persistently failing share must be cheaper to hold than a settled one, \
             or a broken fleet outruns a healthy fleet — which is what it used to do"
        );
        // And it must not overflow into a tiny value on a huge count.
        assert_eq!(retry_backoff(u32::MAX), RETRY_MAX);
    }

    /// A cold start is worth watching; a share that has been Starting
    /// for an hour is not worth watching at the same rate.
    #[test]
    fn a_share_that_never_starts_stops_being_asked_every_15s() {
        // Inside the patience window: unchanged, because a real cold
        // start (epoch claim + DR import) lives here.
        assert_eq!(progress_requeue(None), REQUEUE_PROGRESS);
        assert_eq!(progress_requeue(Some(0)), REQUEUE_PROGRESS);
        assert_eq!(progress_requeue(Some(PROGRESS_PATIENCE_SECS)), REQUEUE_PROGRESS);

        // Past it, it stretches, monotonically, to the settled rate.
        let mut prev = REQUEUE_PROGRESS;
        for mult in 1..=12i64 {
            let d = progress_requeue(Some(PROGRESS_PATIENCE_SECS * (mult + 1)));
            assert!(d >= prev, "progress interval went backwards at {mult}x");
            assert!(d <= REQUEUE_SETTLED, "it must never exceed the settled rate");
            prev = d;
        }
        assert!(
            progress_requeue(Some(86_400)) >= REQUEUE_SETTLED,
            "a share stuck for a day must cost no more than a settled one"
        );
        assert!(
            progress_requeue(Some(PROGRESS_PATIENCE_SECS * 3)) > REQUEUE_PROGRESS,
            "past the patience window it MUST have stretched — otherwise this is the \
             flat-15s-forever behaviour with extra steps"
        );
    }

    /// Without jitter a fleet seeded together stays in lockstep: every
    /// interval is a constant, so 3000 shares come due in the same
    /// second and the apiserver sees a 3000-wide spike on a 300s beat
    /// rather than a flat rate.
    #[test]
    fn requeues_are_spread_so_the_fleet_does_not_beat_in_unison() {
        let spread: std::collections::HashSet<u64> = (0..200)
            .map(|i| jittered(REQUEUE_SETTLED, &format!("ns/s{i}")).as_millis() as u64)
            .collect();
        assert!(
            spread.len() > 150,
            "200 shares produced only {} distinct intervals — that is still a herd. \
             Integer-percent jitter gives at most 51 buckets, which at 3000 shares \
             leaves ~59 coming due in the same millisecond.",
            spread.len()
        );

        // Bounded: jitter must not turn a 300s interval into 15s or an
        // hour. +/-25%.
        for i in 0..500 {
            let d = jittered(REQUEUE_SETTLED, &format!("ns/s{i}"));
            assert!(
                d >= REQUEUE_SETTLED.mul_f64(0.74) && d <= REQUEUE_SETTLED.mul_f64(1.26),
                "jitter escaped +/-25% for s{i}: {d:?}"
            );
        }

        // Deterministic: the same share must not be re-rolled every
        // reconcile, or "spread" becomes "random walk".
        assert_eq!(jittered(REQUEUE_SETTLED, "ns/a"), jittered(REQUEUE_SETTLED, "ns/a"));

        // And a short interval is left alone — spreading 15s buys
        // nothing and blurs the progress signal.
        assert_eq!(jittered(REQUEUE_PROGRESS, "ns/a"), REQUEUE_PROGRESS);
    }

    // ---------------------------------------------------------------
    // The parked-share apply gate. Measured at the design target before
    // it existed: 3000 shares / 300 live produced ~99 apiserver
    // writes/s with NOTHING changing, most of it 2700 parked shares
    // re-applying four identical objects on a timer.
    // ---------------------------------------------------------------

    fn dep_with(hash: Option<&str>, verified: Option<chrono::DateTime<chrono::Utc>>) -> Deployment {
        let mut d = Deployment::default();
        let mut ann = std::collections::BTreeMap::new();
        if let Some(h) = hash {
            ann.insert(ANN_RENDER_HASH.to_string(), h.to_string());
        }
        if let Some(v) = verified {
            ann.insert(
                ANN_RENDER_VERIFIED.to_string(),
                v.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
        }
        d.metadata.annotations = Some(ann);
        d
    }

    /// The gate only engages on an exact match with a recent stamp.
    #[test]
    fn the_apply_gate_engages_only_on_a_fresh_exact_match() {
        let now = chrono::Utc::now();
        assert_eq!(
            apply_gate_state(&dep_with(Some("abc"), Some(now)), "abc"),
            Some(GateState::Fresh)
        );
        assert_eq!(
            apply_gate_state(&dep_with(Some("abc"), Some(now)), "different"),
            None,
            "a changed render must always apply"
        );
        assert_eq!(
            apply_gate_state(&dep_with(None, Some(now)), "abc"),
            None,
            "a Deployment this operator has never stamped must always apply"
        );
        assert_eq!(
            apply_gate_state(&dep_with(Some("abc"), None), "abc"),
            None,
            "a hash with no timestamp cannot be aged, so it cannot be trusted"
        );
    }

    /// THE PROPERTY THAT KEEPS THIS OPERATOR LEVEL-TRIGGERED.
    ///
    /// The whole correctness argument is that desired state is
    /// re-asserted whether or not anything is believed to have changed.
    /// A hash gate is a bet that the hash sees everything — and it does
    /// NOT see a hand-edited ConfigMap, a stripped label, or anything
    /// else that changes the CLUSTER without changing the RENDER. So a
    /// stale stamp must fall out of the gate, which bounds how long any
    /// such drift can survive no matter what the hash says.
    #[test]
    fn a_stale_stamp_forces_a_full_apply_however_well_the_hash_matches() {
        let old = chrono::Utc::now() - chrono::Duration::seconds(FULL_APPLY_AFTER + 60);
        assert_eq!(
            apply_gate_state(&dep_with(Some("abc"), Some(old)), "abc"),
            Some(GateState::Stale),
            "drift that the render cannot see must not be able to survive forever"
        );

        // A stamp from the FUTURE is a clock problem, not freshness —
        // trusting it would extend the gate indefinitely.
        let future = chrono::Utc::now() + chrono::Duration::seconds(3600);
        assert_eq!(
            apply_gate_state(&dep_with(Some("abc"), Some(future)), "abc"),
            Some(GateState::Stale)
        );

        // Unparseable is not fresh either.
        let mut d = dep_with(Some("abc"), None);
        d.metadata.annotations.as_mut().unwrap()
            .insert(ANN_RENDER_VERIFIED.to_string(), "not-a-time".to_string());
        assert_eq!(apply_gate_state(&d, "abc"), None);
    }

    /// The fingerprint must move when ANY of the four applied objects
    /// moves. A gate that hashes a subset silently stops noticing
    /// whatever was left out, and the symptom is a share that never
    /// converges with nothing in the logs to say why.
    #[test]
    fn the_render_fingerprint_moves_when_any_applied_object_moves() {
        let share = share_named("fp");
        let d = render::RenderDefaults::default();
        let base = render::render(&share, &d, None, None);
        let h0 = render_fingerprint(&base);

        let mut only_cm = render::render(&share, &d, None, None);
        only_cm.config_map.data.get_or_insert_with(Default::default)
            .insert("x".into(), "y".into());
        assert_ne!(render_fingerprint(&only_cm), h0, "a ConfigMap change must move it");

        let mut only_svc = render::render(&share, &d, None, None);
        only_svc.service.metadata.labels.get_or_insert_with(Default::default)
            .insert("x".into(), "y".into());
        assert_ne!(render_fingerprint(&only_svc), h0, "a Service change must move it");

        let mut only_dep = render::render(&share, &d, None, None);
        only_dep.deployment.spec.as_mut().unwrap().replicas = Some(7);
        assert_ne!(render_fingerprint(&only_dep), h0, "a Deployment change must move it");

        // And it must be STABLE: two renders of the same input agree,
        // or the gate never engages and buys nothing.
        assert_eq!(render_fingerprint(&render::render(&share, &d, None, None)), h0);
    }

    /// `reclaim: Delete` and `spec.existingClaim` are both the user's
    /// words, and they contradict each other. The hibernate path has
    /// always resolved that by refusing; CR deletion resolved it by
    /// deleting and logging `adopted=true`, so the same field meant
    /// two different things depending on which route reached it.
    #[test]
    fn an_adopted_claim_is_never_deleted_whatever_reclaim_says() {
        // The operator's own claim: `reclaim` is the whole decision.
        assert!(may_delete_claim(&Reclaim::Delete, false));
        assert!(!may_delete_claim(&Reclaim::Retain, false));

        // Adopted: refused in BOTH directions. Retain would have kept
        // it anyway; Delete is the arm that used to destroy a volume
        // the operator never created.
        assert!(!may_delete_claim(&Reclaim::Delete, true));
        assert!(!may_delete_claim(&Reclaim::Retain, true));

        // And it agrees with the hibernate path, which is the point —
        // `hibernate_reclaim` returns early on exactly this condition.
        for reclaim in [Reclaim::Retain, Reclaim::Delete] {
            assert!(
                !may_delete_claim(&reclaim, true),
                "hibernate refuses an adopted claim; deletion must not disagree"
            );
        }
    }

    /// **The only way a consumer in ANOTHER cluster gets a mountable
    /// address.** Everything derived is in-cluster-only except a
    /// LoadBalancer's ingress, and the NodePort case is the trap: it
    /// resolves to the `.svc` DNS name rather than a node address, so a
    /// foreign client reads `status.address`, tries to mount it, and
    /// fails on a name it cannot resolve.
    #[test]
    fn an_advertised_address_is_what_a_foreign_consumer_mounts() {
        let svc = |ty: &str| Service {
            metadata: ObjectMeta {
                name: Some("tenant-a".into()),
                ..Default::default()
            },
            spec: Some(K8sServiceSpec {
                type_: Some(ty.into()),
                ports: Some(vec![ServicePort {
                    port: 2049,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        };
        let cluster_ip = svc("ClusterIP");

        // Baseline: derived, and in-cluster only.
        assert_eq!(
            address_of(&cluster_ip, "ws", None).as_deref(),
            Some("tenant-a.ws.svc.cluster.local:2049")
        );

        // Advertised: verbatim, and it WINS over the derived value.
        assert_eq!(
            address_of(&cluster_ip, "ws", Some("hub-a.corp.internal:2149")).as_deref(),
            Some("hub-a.corp.internal:2149")
        );

        // It wins over a LoadBalancer's ingress too — explicit beats
        // inferred, and the operator cannot see the consumer's network.
        let mut lb = svc("LoadBalancer");
        lb.status = Some(ServiceStatus {
            load_balancer: Some(LoadBalancerStatus {
                ingress: Some(vec![LoadBalancerIngress {
                    hostname: Some("a.elb.amazonaws.com".into()),
                    ..Default::default()
                }]),
            }),
            ..Default::default()
        });
        assert_eq!(
            address_of(&lb, "ws", Some("10.0.4.7:2049")).as_deref(),
            Some("10.0.4.7:2049")
        );

        // IPv6 survives verbatim — the brackets are the reason the CEL
        // rule demands them, so the last colon is unambiguously the port.
        assert_eq!(
            address_of(&cluster_ip, "ws", Some("[2001:db8::1]:2049")).as_deref(),
            Some("[2001:db8::1]:2049")
        );

        // Absent and empty both fall through rather than advertising "".
        // An empty string is what a chart renders for an unset value,
        // and publishing it would blank status.address.
        assert_eq!(
            address_of(&cluster_ip, "ws", Some("")).as_deref(),
            Some("tenant-a.ws.svc.cluster.local:2049")
        );
        assert_eq!(
            address_of(&cluster_ip, "ws", Some("   ")).as_deref(),
            Some("tenant-a.ws.svc.cluster.local:2049")
        );
    }

    /// `lastTransitionTime` must mean "when this changed", not "when we
    /// last looped" — otherwise "how long has it been unready?" is
    /// unanswerable.
    #[test]
    fn conditions_keep_their_transition_time_until_the_status_changes() {
        let mut conds = Vec::new();
        let mut first = condition("Ready", false, "Pending", None, Some(1));
        first.last_transition_time = "2020-01-01T00:00:00Z".into();
        set_condition(&mut conds, first);

        let mut again = condition("Ready", false, "Starting", None, Some(2));
        again.last_transition_time = "2026-01-01T00:00:00Z".into();
        set_condition(&mut conds, again);
        assert_eq!(conds[0].last_transition_time, "2020-01-01T00:00:00Z");
        assert_eq!(conds[0].reason, "Starting", "the reason still updates");
        assert_eq!(conds[0].observed_generation, Some(2));

        let mut flipped = condition("Ready", true, "Serving", None, Some(2));
        flipped.last_transition_time = "2026-06-01T00:00:00Z".into();
        set_condition(&mut conds, flipped);
        assert_eq!(conds[0].last_transition_time, "2026-06-01T00:00:00Z");
        assert_eq!(conds.len(), 1, "one condition per type");
    }

    /// A rotated Secret changes nothing in any CR, so the only way the
    /// operator hears about it is a watch — and the only way the watch
    /// helps is this mapping.
    #[test]
    fn secret_and_claim_events_find_their_shares() {
        let mut a = share_named("a");
        a.spec.bucket = Some("b".into());
        a.spec.credentials_secret_ref = Some("flint-s3".into());
        let mut b = share_named("b");
        b.spec.bucket = Some("b".into());
        b.spec.credentials_secret_ref = Some("other".into());
        // Tier off ⇒ no envFrom ⇒ a Secret change cannot affect it.
        let mut c = share_named("c");
        c.spec.credentials_secret_ref = Some("flint-s3".into());

        let fleet = vec![Arc::new(a), Arc::new(b), Arc::new(c)];
        assert_eq!(
            shares_referencing_secret(&fleet, "ws", "flint-s3"),
            vec![("ws".to_string(), "a".to_string())]
        );
        assert!(shares_referencing_secret(&fleet, "other-ns", "flint-s3").is_empty());

        assert_eq!(
            shares_using_claim(&fleet, "ws", "a-data"),
            vec![("ws".to_string(), "a".to_string())]
        );

        // Adoption: the claim keeps its old name, and the mapping must
        // follow the CR's binding rather than the naming convention.
        let mut adopted = share_named("tenant-a");
        adopted.spec.existing_claim = Some("flint-lite-data".into());
        let fleet = vec![Arc::new(adopted)];
        assert_eq!(
            shares_using_claim(&fleet, "ws", "flint-lite-data"),
            vec![("ws".to_string(), "tenant-a".to_string())]
        );
        assert!(shares_using_claim(&fleet, "ws", "tenant-a-data").is_empty());
    }

    /// **Without a durable carrier the ladder cannot work at all.**
    ///
    /// The reconciler is level-triggered and server-side-applies what
    /// it renders. `render` computes `replicas` and `claim_plan`
    /// re-applies a missing PVC — so a suspend held only in status, or
    /// only in the controller's memory, is undone by the very next
    /// reconcile, within seconds, forever. These two assertions are the
    /// coupling that makes the annotation carrier load-bearing rather
    /// than decorative.
    #[test]
    fn the_render_obeys_the_idle_annotation() {
        use crate::lite_operator::idle;

        let mut share = share_named("tenant-a");
        // Active, no ladder: one replica.
        let r = render::render(&share, &RenderDefaults::default(), None, None);
        assert_eq!(r.deployment.spec.as_ref().unwrap().replicas, Some(1));

        // The ladder says down. Nothing in SPEC changed — only metadata.
        share.metadata.annotations = Some(
            [(idle::ANN_IDLE_STATE.to_string(), "Suspended".to_string())]
                .into_iter()
                .collect(),
        );
        let r = render::render(&share, &RenderDefaults::default(), None, None);
        assert_eq!(
            r.deployment.spec.as_ref().unwrap().replicas,
            Some(0),
            "a suspend the renderer does not read is undone on the next reconcile"
        );

        // And the PVC is still rendered — suspend KEEPS the disk.
        assert!(r.pvc.is_some(), "suspend must not touch the claim");
    }

    /// A hibernated share's PVC was deliberately deleted, and the
    /// bucket is the only copy. Re-applying it would leave an empty
    /// disk that a waking hub cannot distinguish from a restore that
    /// already ran.
    #[test]
    fn a_hibernated_share_does_not_get_its_pvc_recreated() {
        assert_eq!(
            claim_plan(None, "20Gi", false, IdleState::Hibernated),
            ClaimPlan::Hibernated,
            "a hibernated share must not have its claim re-applied"
        );
        // And the ordinary path is untouched.
        assert_eq!(claim_plan(None, "20Gi", false, IdleState::Active), ClaimPlan::Apply);
    }

    /// The front door has to be able to tell "will wake if I ask" from
    /// "an admin said no". One phase for both makes that impossible,
    /// and a front door that cannot tell retries forever against a
    /// share that is never coming back.
    #[test]
    fn an_admin_suspend_and_an_idle_suspend_report_different_phases() {
        let dep = dep_with_checksum(None, None);
        assert_eq!(
            phase_of(Lifecycle::Suspended, Some(&dep), false, IdleState::Active),
            Phase::Suspended
        );
        assert_eq!(
            phase_of(Lifecycle::Active, Some(&dep), false, IdleState::Suspended),
            Phase::IdleSuspended
        );
        assert_eq!(
            phase_of(Lifecycle::Active, Some(&dep), false, IdleState::Hibernated),
            Phase::Hibernated
        );
        // An admin's decision outranks the ladder's.
        assert_eq!(
            phase_of(Lifecycle::Suspended, Some(&dep), false, IdleState::Suspended),
            Phase::Suspended,
            "spec.lifecycle wins, and reports as such"
        );
        // And a conflict loser is Failed regardless of either.
        assert_eq!(
            phase_of(Lifecycle::Active, Some(&dep), true, IdleState::Suspended),
            Phase::Failed
        );
    }
}
