//! Project id → share → "may I dial, and where".
//!
//! Pure: no client, no clock, no network. Everything the decision
//! depends on is in [`ShareView`], which is built from the CR once and
//! then reasoned about — so every phase the fleet can be in is a test
//! rather than a cluster.
//!
//! ## Read WHETHER before WHERE
//!
//! `status.apiEndpoint` is a stable formula, not a liveness signal
//! (`crd.rs`): a parked share still publishes one and the name simply
//! does not resolve, because a headless Service with no pods has no
//! EndpointSlice. So a gateway that dials the endpoint first gets a DNS
//! failure and has to guess whether that means "waking", "gone" or
//! "your cluster's DNS is broken". Reading `phase` first turns all
//! three into different answers, and turns the first one into an action.
//!
//! ## Why the CR and not the hub's /status
//!
//! Two reasons, and the second is the one that bites. The CR is watched
//! anyway, so reading it is free while a poll is a request. And **a
//! file-API call counts as activity on the share** — the hub's own
//! idle accounting is what the ladder suspends on, and a gateway that
//! polled the hub to find out whether it was idle would keep every
//! share it ever touched awake forever. Even a 304 counts.

use crate::lite_operator::crd::{FlintShare, Phase};
use crate::lite_operator::idle::ANN_REQUESTED_AT;
use kube::ResourceExt;

use super::derive::{Binding, NoBinding, ANN_TOKEN_VERSION};

/// The longest a project id may be.
///
/// A share is `{prefix}{project}` and must be a legal object name (253),
/// but the binding constraint is much tighter: the operator's API
/// Service is `{base[..50]}-api-{uid[0:8]}`, and `base` is derived from
/// the CR name. 63 is the DNS label limit; this leaves room for the
/// prefix an install chooses.
pub const MAX_PROJECT_ID: usize = 48;

/// Why a project id was refused. Rendered to the caller, so it says
/// what is wrong without echoing the input back into the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadProjectId {
    Empty,
    TooLong,
    /// Anything outside `[a-z0-9-]`, or a leading/trailing `-`.
    Charset,
}

impl BadProjectId {
    pub fn message(self) -> &'static str {
        match self {
            BadProjectId::Empty => "project id is empty",
            BadProjectId::TooLong => "project id is longer than 48 characters",
            BadProjectId::Charset => {
                "project id must match [a-z0-9]([-a-z0-9]*[a-z0-9])? — lowercase \
                 letters, digits and interior hyphens only"
            }
        }
    }
}

/// RFC 1035 label rules, applied to the caller's input BEFORE it is
/// concatenated into anything.
///
/// This is the only place a caller-supplied string becomes part of a
/// Kubernetes object name, so it is validated as a whole rather than
/// sanitised: a rejected id is a 400 and a valid one needs no escaping
/// downstream. Sanitising would map two different ids onto one share.
pub fn validate_project_id(id: &str) -> Result<(), BadProjectId> {
    if id.is_empty() {
        return Err(BadProjectId::Empty);
    }
    if id.len() > MAX_PROJECT_ID {
        return Err(BadProjectId::TooLong);
    }
    let bytes = id.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return Err(BadProjectId::Charset);
    }
    if !bytes.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-') {
        return Err(BadProjectId::Charset);
    }
    Ok(())
}

/// The FlintShare name a project id maps to.
///
/// Call only with a validated id.
pub fn share_name(prefix: &str, project: &str) -> String {
    format!("{prefix}{project}")
}

/// Everything the decision needs, lifted out of the CR.
#[derive(Debug, Clone, Default)]
pub struct ShareView {
    pub namespace: String,
    pub name: String,
    /// `metadata.deletionTimestamp` is set. The CR is still readable
    /// and still carries its last status, so this must be checked
    /// explicitly — a `GET` during finalization returns 200.
    pub deleting: bool,
    pub phase: Option<Phase>,
    /// The hub's own phase, when the operator observed one this pass.
    pub hub_phase: Option<String>,
    pub api_endpoint: Option<String>,
    /// `status.address` — the NFS door, `host:2049`. Withdrawn by the
    /// operator on `Failed` and `Terminating`, which is why it is read
    /// rather than derived.
    pub address: Option<String>,
    /// The hub's persisted NFS server identity. **A change here means
    /// every existing mount is stale**: it is stable across ordinary
    /// restarts, but a hibernate deletes the PVC, so a woken share
    /// comes back with a new one and the stateids clients still hold
    /// refer to a server generation that no longer exists.
    pub server_id: Option<String>,
    /// `ApiEndpointPublished`: (status, reason, message).
    pub api_condition: Option<(bool, String, String)>,
    /// Who won the bucket subtree, when this share lost it.
    pub conflict_with: Option<String>,
    /// True once the wake annotation is present — the gateway does not
    /// need to re-arm a share someone already asked for.
    pub wake_requested: bool,
    /// The binding for a derived token, or why there is none.
    pub endpoint_s3: String,
    pub bucket: Option<String>,
    pub key_prefix: Option<String>,
    pub token_version: u64,
    /// `chert.us/volume-id`, when the share carries one.
    pub volume_id: Option<String>,
    /// `spec.idle.suspendAfterSecs`. `None` = the ladder is OFF for
    /// this share and it will never be suspended for quiet.
    pub suspend_after_secs: Option<u64>,
    /// `spec.idle.suspendWithSessions`. **The protective value is
    /// `Some(false)`**, and it is opt-in: absent and `Some(true)` both
    /// mean the ladder suspends even while an NFS client holds a lease.
    pub suspend_with_sessions: Option<bool>,
    /// `spec.monitoring.fileApi.hydrateWaitSecs`, when the share sets
    /// one. How long the HUB will hold a download open waiting for an
    /// evicted file to come back from S3 before answering 503.
    ///
    /// The gateway has to know it, because its own header deadline
    /// races it: with both at the default 30s a cold read would fail as
    /// a GATEWAY timeout (502, no Retry-After) instead of the hub's
    /// 503 with one — turning a normal, retryable hydration into an
    /// error the caller cannot act on. See `proxy::header_deadline`.
    pub hydrate_wait_secs: Option<u64>,
}

impl ShareView {
    pub fn of(share: &FlintShare) -> Self {
        let st = share.status.as_ref();
        let api_condition = st.and_then(|s| {
            s.conditions
                .as_ref()?
                .iter()
                .find(|c| c.r#type == "ApiEndpointPublished")
                .map(|c| {
                    (
                        c.status == "True",
                        c.reason.clone(),
                        c.message.clone().unwrap_or_default(),
                    )
                })
        });
        ShareView {
            namespace: share.namespace().unwrap_or_default(),
            name: share.name_any(),
            deleting: share.metadata.deletion_timestamp.is_some(),
            phase: st.and_then(|s| s.phase.clone()),
            hub_phase: st.and_then(|s| s.hub_phase.clone()),
            api_endpoint: st.and_then(|s| s.api_endpoint.clone()),
            address: st.and_then(|s| s.address.clone()),
            server_id: st.and_then(|s| s.server_id.clone()),
            api_condition,
            conflict_with: st
                .and_then(|s| s.conflict_with.as_ref())
                .map(|c| format!("{}/{}", c.namespace, c.name)),
            wake_requested: share
                .metadata
                .annotations
                .as_ref()
                .is_some_and(|a| a.contains_key(ANN_REQUESTED_AT)),
            endpoint_s3: share.spec.endpoint.clone().unwrap_or_default(),
            bucket: share.spec.bucket.clone(),
            key_prefix: share.spec.key_prefix.clone(),
            token_version: share
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(ANN_TOKEN_VERSION))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v >= 1)
                .unwrap_or(1),
            volume_id: share
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_VOLUME_ID))
                .filter(|v| !v.is_empty())
                .cloned(),
            suspend_after_secs: share
                .spec
                .idle
                .as_ref()
                .and_then(|i| i.suspend_after_secs),
            suspend_with_sessions: share
                .spec
                .idle
                .as_ref()
                .and_then(|i| i.suspend_with_sessions),
            hydrate_wait_secs: share
                .spec
                .monitoring
                .as_ref()
                .and_then(|m| m.file_api.as_ref())
                .and_then(|a| a.hydrate_wait_secs)
                .and_then(|v| u64::try_from(v).ok()),
        }
    }

    /// The identity a derived token binds to.
    pub fn binding(&self) -> Result<Binding<'_>, NoBinding> {
        match (self.bucket.as_deref(), self.key_prefix.as_deref()) {
            (Some(bucket), Some(key_prefix)) if !bucket.is_empty() => Ok(Binding {
                endpoint: &self.endpoint_s3,
                bucket,
                key_prefix,
                version: self.token_version,
            }),
            _ => Err(NoBinding),
        }
    }
}

/// What the gateway does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Serving, and here is where.
    Dial(String),
    /// Parked and wakeable: arm `chert.us/requested-at`, then wait.
    Wake,
    /// Coming up already — do not arm anything, just wait.
    Wait,
    /// Do not dial, now or after waiting.
    Refuse(Refusal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub reason: &'static str,
    pub message: String,
    /// Seconds, when coming back later could plausibly work.
    pub retry_after: Option<u64>,
}

fn refuse(status: u16, reason: &'static str, message: impl Into<String>) -> Decision {
    Decision::Refuse(Refusal { status, reason, message: message.into(), retry_after: None })
}

fn refuse_later(
    status: u16,
    reason: &'static str,
    message: impl Into<String>,
    secs: u64,
) -> Decision {
    Decision::Refuse(Refusal {
        status,
        reason,
        message: message.into(),
        retry_after: Some(secs),
    })
}

/// The decision, from the CR alone.
///
/// Ordering is deliberate and each step is load-bearing:
///
/// 1. **Deletion first.** The finalizer keeps a deleted CR readable
///    with its last status intact, and owner GC has not run, so the
///    Deployment and Service are still up: a phase-first reader sees
///    `Ready`, dials, and succeeds — right up until the children are
///    collected under it. This is the one case where the CR's status is
///    actively misleading and only `deletionTimestamp` says so.
/// 2. **Then the phases that must never dial**, whatever the endpoint
///    says.
/// 3. **Then parked-but-wakeable.**
/// 4. **Then the endpoint**, which is the only step that can fail for a
///    share that is otherwise perfectly healthy.
pub fn decide(v: &ShareView) -> Decision {
    decide_for(v, Door::FileApi)
}

/// Which of the hub's two doors a caller is asking about.
///
/// The phase half of the decision is identical for both — a
/// `Terminating` share serves neither, a parked one wakes for either.
/// The doors differ only in the last step, and they differ in a way
/// that matters: **an NFS-only share has no `apiEndpoint` at all**.
///
/// That is not an edge case. `monitoring.fileApi` is off by default, so
/// a plain NFS share — which is the primary consumer shape — publishes
/// no file-API endpoint, and a wake request that insisted on one would
/// refuse the very shares it exists to bring up. The first cut of
/// `/wake` did exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    /// `status.apiEndpoint` — the HTTP file API.
    FileApi,
    /// `status.address` — `host:2049`, what a consumer mounts.
    Nfs,
}

pub fn decide_for(v: &ShareView, door: Door) -> Decision {
    if v.deleting || v.phase == Some(Phase::Terminating) {
        return refuse(
            410,
            "Terminating",
            "this project's share is being deleted; its files are no longer served",
        );
    }

    let Some(phase) = v.phase.clone() else {
        // No status at all: the operator has not reached this share
        // yet. Brand new, or no operator is running — which the caller
        // can tell apart by reading the operator's Lease, and the
        // gateway deliberately does not, because a gateway that decided
        // "no operator ⇒ give up" would turn a control-plane blip into
        // a data-plane outage.
        return refuse_later(
            503,
            "NotReconciledYet",
            "the operator has not reported on this share yet",
            5,
        );
    };

    match phase {
        Phase::Failed => {
            let msg = match &v.conflict_with {
                Some(winner) => format!(
                    "this share is refused: {winner} owns its bucket subtree. \
                     Serving it here would publish two hubs over one prefix."
                ),
                None => "this share is Failed — see its conditions".to_string(),
            };
            return refuse(409, "Failed", msg);
        }
        Phase::Suspended => {
            // An ADMIN decision, and the CRD is explicit that a wake
            // request does not override it. Arming the annotation here
            // would be a gateway quietly reversing an operator's call.
            return refuse(
                409,
                "AdminSuspended",
                "this share is administratively suspended; a wake request does not override it",
            );
        }
        // Already answered above. Spelled out rather than folded into a
        // wildcard so that ADDING a phase to the CRD breaks this match
        // instead of silently falling into whichever arm a `_` names —
        // a new phase must be a decision here, not a default.
        Phase::Terminating => unreachable!("handled before the match"),
        Phase::IdleSuspended => return Decision::Wake,
        Phase::Hibernated => return Decision::Wake,
        Phase::Pending | Phase::Starting | Phase::Reprovisioning => return Decision::Wait,
        Phase::Ready => {}
    }

    // Ready. Now the doors part.
    if door == Door::Nfs {
        // `status.address` is withdrawn by the operator on `Failed` and
        // `Terminating`, both already refused above — so an absent
        // address here means the Service has not published one yet,
        // which is a wait rather than a refusal.
        return match v.address.as_deref() {
            Some(a) if !a.is_empty() => Decision::Dial(a.to_string()),
            _ => refuse_later(
                503,
                "NoAddress",
                "the share is up but has published no NFS address yet",
                5,
            ),
        };
    }

    // The file API's endpoint is the last thing that can be missing, and
    // the operator already recorded WHY on the condition — so the
    // caller gets that reason rather than a bare "no endpoint".
    match v.api_endpoint.as_deref() {
        Some(ep) if !ep.is_empty() => Decision::Dial(ep.to_string()),
        _ => {
            let (reason, detail) = match &v.api_condition {
                Some((_, reason, message)) => (reason.as_str(), message.clone()),
                None => ("Unknown", "no ApiEndpointPublished condition".to_string()),
            };
            match reason {
                // Not a transient state: someone has to change the CR.
                "NotConfigured" => refuse(
                    501,
                    "FileApiDisabled",
                    format!("this share does not serve the file API ({detail})"),
                ),
                "NameCollision" => refuse(
                    409,
                    "ApiServiceCollision",
                    format!("refusing to route to this share's API: {detail}"),
                ),
                _ => refuse_later(
                    503,
                    "NoApiEndpoint",
                    format!("no file-API endpoint published for this share: {detail}"),
                    10,
                ),
            }
        }
    }
}

/// Whether the hub's own phase says a dial is worth making.
///
/// `hubPhase` is absent whenever the operator's poll did not land this
/// pass, and absent must NOT read as "not serving" — it is "not
/// observed". So this only ever DOWNGRADES a `Ready` share: an
/// observed non-serving phase is worth a 503 with the phase named,
/// because the alternative is dialling a hub that will answer 503
/// anyway and burning a round trip and a stream to learn it.
pub fn hub_phase_blocks(v: &ShareView) -> Option<Refusal> {
    let hp = v.hub_phase.as_deref()?;
    // The two the hub itself serves on (`fileapi::routes_gated`).
    if matches!(hp, "Serving" | "Sweeping") {
        return None;
    }
    Some(Refusal {
        status: 503,
        reason: "HubNotServing",
        message: format!("the hub for this share is {hp}, not yet serving"),
        // Draining is a shutdown, not a startup: coming back in two
        // seconds is right for an import and wrong for a hub on its way
        // out, which needs its replacement scheduled first.
        retry_after: Some(if hp == "Draining" { 15 } else { 5 }),
    })
}

/// The documented index from a project id to its share(s).
///
/// `docs/flint-lite-operator.md` tells a front door to derive the name
/// (`fs-<project-id>`) AND label it. Both are load-bearing and they fail
/// differently: the derived name is what makes an ensure-live create
/// idempotent (two replicas racing issue the same create, one gets 409),
/// while the label is what makes the mapping legible from the cluster
/// side — it is already a printer column on the CRD.
pub const LABEL_PROJECT_ID: &str = "chert.us/project-id";

/// Which of a project's volumes a share is.
///
/// **A project may have several hubs, and the operator has no opinion
/// about it.** `conflict::overlaps` keys fleet uniqueness on
/// `(endpoint, bucket, prefix-subtree)` and nothing in the operator
/// reads `chert.us/project-id` at all — so N shares on N different
/// prefixes, all labelled with one project id, is a legal and
/// unremarkable configuration. (One HUB serving several volumes is a
/// different thing entirely and is not implemented; see
/// `docs/plans/multi-volume-hub-design.md`. The model here is one
/// volume, one hub, N of them per project.)
///
/// Absent, the CR's own name is the volume id. That keeps a
/// single-volume project working with no labels at all, and gives a
/// multi-volume one a usable identifier before anyone thinks to add
/// this label.
pub const LABEL_VOLUME_ID: &str = "chert.us/volume-id";

pub fn project_id_of(share: &FlintShare) -> Option<String> {
    share.metadata.labels.as_ref()?.get(LABEL_PROJECT_ID).cloned()
}

pub fn volume_id_of(share: &FlintShare) -> String {
    share
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(LABEL_VOLUME_ID))
        .filter(|v| !v.is_empty())
        .cloned()
        .unwrap_or_else(|| share.metadata.name.clone().unwrap_or_default())
}

/// The result of looking a project up in the fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    Found(T),
    NotFound,
    /// The project has more than one volume and the request named
    /// none. Carries the volume ids — the caller CAN act on this, by
    /// asking again for one of them.
    NeedsVolume(Vec<String>),
    /// Two shares claim the same (project, volume). A misconfiguration
    /// the caller cannot fix; the `ns/name` pairs are for the log.
    Ambiguous(Vec<String>),
}

/// Every share belonging to a project, label first and derived name
/// second.
///
/// The label wins over the name when both match, because the label is
/// the deliberate statement and the name is a convention. A share
/// matching by name is only consulted when NO share carries the label —
/// which is what lets an install that predates the labelling convention
/// keep working, while a project that has adopted labels is not
/// polluted by whatever object happens to be called `fs-<id>`.
pub fn shares_of<T: AsRef<FlintShare> + Clone>(
    shares: &[T],
    prefix: &str,
    project: &str,
    namespace: Option<&str>,
) -> Vec<T> {
    let in_scope = |s: &FlintShare| match namespace {
        Some(ns) => s.metadata.namespace.as_deref() == Some(ns),
        None => true,
    };
    let by_label: Vec<T> = shares
        .iter()
        .filter(|s| in_scope(s.as_ref()) && project_id_of(s.as_ref()).as_deref() == Some(project))
        .cloned()
        .collect();
    if !by_label.is_empty() {
        return by_label;
    }
    let name = share_name(prefix, project);
    shares
        .iter()
        .filter(|s| in_scope(s.as_ref()) && s.as_ref().metadata.name.as_deref() == Some(&name))
        .cloned()
        .collect()
}

/// Narrow a project's shares to the one a request addressed.
///
/// `volume: None` is the single-volume shape — it serves when there is
/// exactly one and asks for a volume when there are several, rather
/// than picking. **Every tie-break is a rule that serves one volume's
/// files to a caller asking for another**, and with two volumes of one
/// project that is a caller reading `models/` when it asked for
/// `data/`. The reflector's iteration order is not even stable across
/// a watch reconnect, so a silent pick would not be consistently wrong
/// — it would be intermittently wrong, which is worse.
pub fn pick<T: AsRef<FlintShare> + Clone>(found: &[T], volume: Option<&str>) -> Lookup<T> {
    let refs = |set: &[T]| -> Vec<String> {
        set.iter()
            .map(|s| {
                let m = &s.as_ref().metadata;
                format!(
                    "{}/{}",
                    m.namespace.as_deref().unwrap_or("?"),
                    m.name.as_deref().unwrap_or("?")
                )
            })
            .collect()
    };

    let Some(want) = volume else {
        return match found.len() {
            0 => Lookup::NotFound,
            1 => Lookup::Found(found[0].clone()),
            _ => {
                let mut vols: Vec<String> =
                    found.iter().map(|s| volume_id_of(s.as_ref())).collect();
                vols.sort();
                vols.dedup();
                // Several shares that all resolve to ONE volume id is a
                // duplicate, not a choice — naming it as a choice would
                // send the caller round a loop that cannot terminate.
                if vols.len() == 1 {
                    return Lookup::Ambiguous(refs(found));
                }
                Lookup::NeedsVolume(vols)
            }
        };
    };

    let matched: Vec<T> = found
        .iter()
        .filter(|s| volume_id_of(s.as_ref()) == want)
        .cloned()
        .collect();
    match matched.len() {
        0 => Lookup::NotFound,
        1 => Lookup::Found(matched[0].clone()),
        _ => Lookup::Ambiguous(refs(&matched)),
    }
}

/// Find one share: a project's shares, narrowed by volume.
pub fn find<T: AsRef<FlintShare> + Clone>(
    shares: &[T],
    prefix: &str,
    project: &str,
    volume: Option<&str>,
    namespace: Option<&str>,
) -> Lookup<T> {
    pick(&shares_of(shares, prefix, project, namespace), volume)
}

/// Whether a share can be scaled to zero out from under a live NFS
/// mount, and what to do about it.
///
/// **This is the sharp edge for a consumer that mounts.** The ladder
/// suspends when two signals agree: `chert.us/requested-at` is stale
/// AND the hub's own activity clock is quiet. A consumer doing file
/// I/O is held up by the second for free. A consumer that holds a
/// mount and does no I/O — an agent computing in memory, which is the
/// case `idle::decide` names explicitly — has only the first, and
/// nothing in the system stamps it on the consumer's behalf.
///
/// What happens if it does suspend is a stall rather than data loss:
/// with a `hard` mount, in-flight I/O blocks in uninterruptible sleep,
/// and `state.db` on the PVC keeps `serverId` stable so clients
/// reclaim. The problem is that **nothing wakes it** — wake is
/// level-triggered on an annotation, and an NFS client cannot write a
/// Kubernetes annotation. The mount hangs until something else asks.
///
/// So a caller that is about to mount gets told, rather than finding
/// out at 3am.
pub fn mount_hazard(v: &ShareView) -> Option<String> {
    let after = v.suspend_after_secs?;
    let every = (after / 2).max(1);
    if v.suspend_with_sessions == Some(false) {
        // This used to return None, on the reasoning that the caller had
        // "opted into the lease guard: the ladder will hold while any
        // client holds a lease". That is not what the guard does, and the
        // CRD says so two files away: leases EXPIRE, so a long enough
        // partition drops the count to zero on its own and the guard stops
        // guarding (crd.rs, `suspend_with_sessions`). It narrows the
        // window; it does not close it.
        //
        // Measured on a real cluster across an `iptables -j DROP`: the
        // guard held from t=49 to t=99 reporting "a client still holds a
        // lease", the lease then expired 1 -> 0 at t=99, and the share
        // SUSPENDED at t=111 under a client that was still there. A
        // control share — connected, quiet, 3.4x the threshold — never
        // suspended, which is what makes that attributable to the
        // partition.
        //
        // So the one configuration this function called safe is the
        // configuration the drill proved unsafe, and it told a mounting
        // consumer nothing at all. Warn in both branches; the keepalive is
        // the remedy that works, because it crosses a different network
        // path from the mount and so survives a partition of the mount's.
        return Some(format!(
            "this share holds off suspending while an NFS client holds a lease \
(spec.idle.suspendWithSessions: false), but a lease EXPIRES: a client that is \
partitioned rather than gone stops renewing, the count reaches zero on its own, and \
the share then suspends after {after}s of quiet with the mount still held. The guard \
narrows that window, it does not close it. POST this endpoint every {every}s while \
the mount is held — that call crosses a different path from the mount, so it still \
arrives when the mount's path is cut."
        ));
    }
    Some(format!(
        "this share suspends after {after}s of quiet even while an NFS client holds a \
lease (spec.idle.suspendWithSessions is not false). A mount held open without file \
I/O will be scaled to zero underneath it, and nothing will wake it, because an NFS \
client cannot write the wake annotation. POST this endpoint again every {every}s \
while the mount is held. Setting spec.idle.suspendWithSessions: false narrows the \
window but does not close it — a partitioned client's lease expires and the guard \
stops guarding — so it is not a substitute for the keepalive."
    ))
}

/// How often a mounting consumer should call back to stay alive.
///
/// Half the suspend budget, so one missed call is survivable. `None`
/// when the ladder is off for this share — there is nothing to keep
/// alive against.
pub fn keepalive_secs(v: &ShareView) -> Option<u64> {
    v.suspend_after_secs.map(|a| (a / 2).max(1))
}

/// The volume id for an already-lifted view.
///
/// `ShareView` keeps the CR's own name, and the volume label falls back
/// to exactly that — so this is the same answer [`volume_id_of`] gives,
/// without carrying the whole object forward.
pub fn volume_id_of_view(v: &ShareView) -> String {
    v.volume_id.clone().unwrap_or_else(|| v.name.clone())
}

/// One row of `GET /v1/projects/{id}/shares`.
///
/// What a UI needs to render a project that has more than one volume,
/// without holding a Kubernetes client: which volumes exist, whether
/// each is servable right now, and what backs it.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VolumeRow {
    pub volume: String,
    /// `Ready`, `IdleSuspended`, … — `null` when the operator has not
    /// reported on this share yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// The hub's own phase, when observed this pass. Absent means NOT
    /// OBSERVED, never "not serving".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    /// True when a request for this volume would be proxied right now.
    /// False is not an error — a parked volume wakes on first use.
    pub serving: bool,
}

pub fn volume_row(share: &FlintShare) -> VolumeRow {
    let v = ShareView::of(share);
    VolumeRow {
        volume: volume_id_of(share),
        phase: v.phase.as_ref().map(|p| format!("{p:?}")),
        hub_phase: v.hub_phase.clone(),
        bucket: v.bucket.clone(),
        key_prefix: v.key_prefix.clone(),
        serving: matches!(decide(&v), Decision::Dial(_)) && hub_phase_blocks(&v).is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(phase: Phase) -> ShareView {
        ShareView {
            namespace: "workspaces".into(),
            name: "proj-a".into(),
            phase: Some(phase),
            api_endpoint: Some("http://proj-a-api-abcd1234.workspaces.svc.cluster.local:8080".into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_ready_share_with_an_endpoint_is_the_only_thing_that_dials() {
        let mut dialled = Vec::new();
        for p in [
            Phase::Pending,
            Phase::Starting,
            Phase::Ready,
            Phase::Suspended,
            Phase::IdleSuspended,
            Phase::Hibernated,
            Phase::Reprovisioning,
            Phase::Failed,
        ] {
            if let Decision::Dial(_) = decide(&view(p.clone())) {
                dialled.push(p);
            }
        }
        assert_eq!(dialled, vec![Phase::Ready], "only Ready may dial");
    }

    /// The trap the CRD documents: a finalized share answers 200 to a
    /// GET, keeps its last status, and its Deployment and Service are
    /// still up because owner GC has not run. A phase-first reader
    /// dials it successfully and then watches the children vanish.
    #[test]
    fn a_terminating_share_is_refused_even_though_its_status_still_says_ready() {
        let v = ShareView { deleting: true, ..view(Phase::Ready) };
        match decide(&v) {
            Decision::Refuse(r) => {
                assert_eq!(r.status, 410);
                assert_eq!(r.reason, "Terminating");
                assert_eq!(r.retry_after, None, "it is not coming back");
            }
            d => panic!("dialled a share under deletion: {d:?}"),
        }
    }

    #[test]
    fn the_two_parked_phases_wake_and_the_admin_one_does_not() {
        assert_eq!(decide(&view(Phase::IdleSuspended)), Decision::Wake);
        assert_eq!(decide(&view(Phase::Hibernated)), Decision::Wake);
        match decide(&view(Phase::Suspended)) {
            Decision::Refuse(r) => assert_eq!(r.reason, "AdminSuspended"),
            d => panic!("a gateway must not reverse an admin suspend: {d:?}"),
        }
    }

    /// Waking a share that is already on its way up would rewrite the
    /// wake annotation on every request — a write per request against
    /// the API server, for a share that needs none of them.
    #[test]
    fn a_share_already_coming_up_waits_without_arming_anything() {
        assert_eq!(decide(&view(Phase::Pending)), Decision::Wait);
        assert_eq!(decide(&view(Phase::Starting)), Decision::Wait);
        assert_eq!(decide(&view(Phase::Reprovisioning)), Decision::Wait);
    }

    #[test]
    fn a_conflict_loser_names_the_winner_rather_than_saying_failed() {
        let mut v = view(Phase::Failed);
        v.conflict_with = Some("other-ns/winner".into());
        match decide(&v) {
            Decision::Refuse(r) => {
                assert_eq!(r.status, 409);
                assert!(r.message.contains("other-ns/winner"), "{}", r.message);
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn a_missing_endpoint_reports_the_operators_own_reason() {
        let cases = [
            ("NotConfigured", 501u16, "FileApiDisabled"),
            ("NameCollision", 409, "ApiServiceCollision"),
            ("ServiceMissing", 503, "NoApiEndpoint"),
            ("TokenUnresolved", 503, "NoApiEndpoint"),
            ("NoUid", 503, "NoApiEndpoint"),
        ];
        for (reason, status, gw_reason) in cases {
            let mut v = view(Phase::Ready);
            v.api_endpoint = None;
            v.api_condition = Some((false, reason.into(), format!("detail for {reason}")));
            match decide(&v) {
                Decision::Refuse(r) => {
                    assert_eq!(r.status, status, "{reason}");
                    assert_eq!(r.reason, gw_reason, "{reason}");
                    assert!(r.message.contains(&format!("detail for {reason}")), "{}", r.message);
                }
                d => panic!("{reason}: {d:?}"),
            }
        }
    }

    #[test]
    fn no_status_at_all_is_a_retryable_503_not_a_404() {
        // A share created a second ago is not a missing project.
        let v = ShareView { phase: None, ..view(Phase::Ready) };
        match decide(&v) {
            Decision::Refuse(r) => {
                assert_eq!(r.status, 503);
                assert_eq!(r.retry_after, Some(5));
            }
            d => panic!("{d:?}"),
        }
    }

    /// Absent is "not observed", never "not serving". Treating it as a
    /// refusal would make the gateway unusable against any hub the
    /// operator failed to poll once.
    #[test]
    fn an_unobserved_hub_phase_never_blocks() {
        let v = view(Phase::Ready);
        assert!(v.hub_phase.is_none());
        assert!(hub_phase_blocks(&v).is_none());
    }

    #[test]
    fn an_observed_hub_phase_blocks_unless_it_is_one_of_the_two_that_serve() {
        for hp in ["Serving", "Sweeping"] {
            let v = ShareView { hub_phase: Some(hp.into()), ..view(Phase::Ready) };
            assert!(hub_phase_blocks(&v).is_none(), "{hp} serves file routes");
        }
        for hp in ["Starting", "ClaimingEpoch", "Importing", "Reconciling", "Released", "Unknown"] {
            let v = ShareView { hub_phase: Some(hp.into()), ..view(Phase::Ready) };
            let r = hub_phase_blocks(&v).unwrap_or_else(|| panic!("{hp} must block"));
            assert_eq!(r.retry_after, Some(5));
            assert!(r.message.contains(hp));
        }
        let v = ShareView { hub_phase: Some("Draining".into()), ..view(Phase::Ready) };
        assert_eq!(hub_phase_blocks(&v).unwrap().retry_after, Some(15));
    }

    #[test]
    fn project_ids_that_could_become_a_different_object_are_refused() {
        for bad in [
            "", "-lead", "trail-", "Upper", "has_underscore", "has.dot", "a/b", "../x",
            "a b", "sh%61re", "\u{0}", "café",
        ] {
            assert!(validate_project_id(bad).is_err(), "accepted {bad:?}");
        }
        assert_eq!(validate_project_id(&"a".repeat(49)), Err(BadProjectId::TooLong));
        for good in ["a", "proj-a", "p1", "a-b-c-1", &"a".repeat(48)] {
            assert!(validate_project_id(good).is_ok(), "refused {good:?}");
        }
    }

    /// The point of validating rather than sanitising: two different
    /// project ids must never name one share. A sanitiser that stripped
    /// `/` would map `a/b` and `ab` onto the same project's files.
    #[test]
    fn validation_never_rewrites_the_id_into_another_projects_share() {
        assert!(validate_project_id("a/b").is_err());
        assert_eq!(share_name("fs-", "proj-a"), "fs-proj-a");
        assert_eq!(share_name("", "proj-a"), "proj-a");
    }

    /// Built from JSON rather than from the structs, on purpose: this
    /// pins the WIRE shape the operator writes (`camelCase`, and the
    /// condition list `ApiEndpointPublished` lives in). Constructing
    /// `FlintShareStatus` directly would still compile after a
    /// `#[serde(rename)]` change that made every share in production
    /// read as unreconciled.
    #[test]
    fn the_view_reads_the_shape_the_operator_actually_writes() {
        let share: FlintShare = serde_json::from_value(serde_json::json!({
            "apiVersion": "chert.us/v1alpha1",
            "kind": "FlintShare",
            "metadata": {
                "name": "fs-proj-a",
                "namespace": "workspaces",
                "labels": { "chert.us/project-id": "proj-a" },
                "annotations": {
                    "chert.us/requested-at": "2026-08-21T00:00:00Z",
                    "chert.us/api-token-version": "4"
                }
            },
            "spec": {
                "bucket": "tenant-bucket",
                "keyPrefix": "proj-a/",
                "persistence": { "size": "20Gi" }
            },
            "status": {
                "phase": "Ready",
                "hubPhase": "Serving",
                "apiEndpoint": "http://fs-proj-a-api-abcd1234.workspaces.svc.cluster.local:8080",
                "conflictWith": {
                    "namespace": "ns2", "name": "winner",
                    "prefix": "proj-a/", "relation": "Same"
                },
                "conditions": [{
                    "type": "Ready", "status": "True", "reason": "Serving",
                    "lastTransitionTime": "2026-08-21T00:00:00Z"
                }, {
                    "type": "ApiEndpointPublished", "status": "True", "reason": "InCluster",
                    "message": "http endpoint on Service fs-proj-a-api-abcd1234",
                    "lastTransitionTime": "2026-08-21T00:00:00Z"
                }]
            }
        }))
        .expect("the CR shape the operator writes must parse");

        let v = ShareView::of(&share);
        assert_eq!(v.namespace, "workspaces");
        assert_eq!(v.name, "fs-proj-a");
        assert_eq!(v.phase, Some(Phase::Ready));
        assert_eq!(v.hub_phase.as_deref(), Some("Serving"));
        assert!(v.api_endpoint.as_deref().unwrap().ends_with(":8080"));
        assert_eq!(v.conflict_with.as_deref(), Some("ns2/winner"));
        assert!(v.wake_requested, "the wake annotation must be seen");
        assert_eq!(v.token_version, 4, "the version annotation drives revocation");
        // Picked out of a LIST, not read positionally: `Ready` is first.
        assert_eq!(
            v.api_condition.as_ref().map(|(ok, r, _)| (*ok, r.as_str())),
            Some((true, "InCluster"))
        );
        assert_eq!(v.binding().unwrap().key_prefix, "proj-a/");
        assert_eq!(v.binding().unwrap().version, 4);
        assert!(!v.deleting);
        assert_eq!(project_id_of(&share).as_deref(), Some("proj-a"));
        assert_eq!(decide(&v), Decision::Dial(v.api_endpoint.clone().unwrap()));
    }

    #[test]
    fn a_missing_or_junk_version_annotation_is_version_one_not_zero() {
        // Version 0 would make `previous()` produce a token no hub has
        // ever held, so every rotation retry would present garbage.
        let base = serde_json::json!({
            "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
            "metadata": {"name": "p", "namespace": "ns", "annotations": {}},
            "spec": {"persistence": {"size": "1Gi"}}
        });
        for ann in [serde_json::json!({}), serde_json::json!({"chert.us/api-token-version": "0"}),
                    serde_json::json!({"chert.us/api-token-version": "nonsense"}),
                    serde_json::json!({"chert.us/api-token-version": "-2"})] {
            let mut j = base.clone();
            j["metadata"]["annotations"] = ann.clone();
            let share: FlintShare = serde_json::from_value(j).unwrap();
            assert_eq!(ShareView::of(&share).token_version, 1, "{ann}");
        }
    }

    fn share(ns: &str, name: &str, label: Option<&str>) -> std::sync::Arc<FlintShare> {
        let mut labels = serde_json::Map::new();
        if let Some(l) = label {
            labels.insert("chert.us/project-id".into(), serde_json::json!(l));
        }
        std::sync::Arc::new(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
                "metadata": {"name": name, "namespace": ns, "labels": labels},
                "spec": {"persistence": {"size": "1Gi"}}
            }))
            .unwrap(),
        )
    }

    #[test]
    fn the_label_is_the_index_and_the_derived_name_is_the_fallback() {
        let fleet = vec![
            share("workspaces", "fs-proj-a", Some("proj-a")),
            share("workspaces", "fs-proj-b", None),
            share("other", "unrelated", Some("proj-z")),
        ];
        match find(&fleet, "fs-", "proj-a", None, None) {
            Lookup::Found(s) => assert_eq!(s.metadata.name.as_deref(), Some("fs-proj-a")),
            l => panic!("{l:?}"),
        }
        // No label anywhere for proj-b: the derived name carries it.
        match find(&fleet, "fs-", "proj-b", None, None) {
            Lookup::Found(s) => assert_eq!(s.metadata.name.as_deref(), Some("fs-proj-b")),
            l => panic!("{l:?}"),
        }
        assert_eq!(find(&fleet, "fs-", "nope", None, None), Lookup::NotFound);
    }

    /// A deliberate label beats an accidental name match, so relabelling
    /// a share actually moves the project rather than being shadowed by
    /// whatever object happens to be called `fs-<id>`.
    #[test]
    fn a_labelled_share_wins_over_a_name_collision() {
        let fleet = vec![
            share("workspaces", "fs-proj-a", None),          // name match only
            share("workspaces", "renamed-later", Some("proj-a")), // the label
        ];
        match find(&fleet, "fs-", "proj-a", None, None) {
            Lookup::Found(s) => assert_eq!(s.metadata.name.as_deref(), Some("renamed-later")),
            l => panic!("{l:?}"),
        }
    }

    /// THE ONE THAT PROTECTS A TENANT.
    ///
    /// Watching all namespaces is the fleet posture, so two tenants can
    /// each hold a share for "proj-a". Every tie-break is a rule that
    /// serves one tenant's files to someone asking for the other's, and
    /// the reflector's iteration order is not stable across relists —
    /// so the SAME request could resolve differently after a watch
    /// reconnect. Refuse, and name both.
    #[test]
    fn two_shares_claiming_one_project_are_refused_rather_than_tie_broken() {
        let fleet = vec![
            share("tenant-a", "fs-proj-a", Some("proj-a")),
            share("tenant-b", "fs-proj-a", Some("proj-a")),
        ];
        match find(&fleet, "fs-", "proj-a", None, None) {
            Lookup::Ambiguous(mut who) => {
                who.sort();
                assert_eq!(who, vec!["tenant-a/fs-proj-a", "tenant-b/fs-proj-a"]);
            }
            l => panic!("picked one of two tenants: {l:?}"),
        }
        // Reversing the store's order must not change the answer.
        let reversed: Vec<_> = fleet.iter().rev().cloned().collect();
        assert!(matches!(find(&reversed, "fs-", "proj-a", None, None), Lookup::Ambiguous(_)));

        // Pinning the gateway to one namespace disambiguates it.
        match find(&fleet, "fs-", "proj-a", None, Some("tenant-b")) {
            Lookup::Found(s) => assert_eq!(s.metadata.namespace.as_deref(), Some("tenant-b")),
            l => panic!("{l:?}"),
        }
    }

    fn vshare(ns: &str, name: &str, project: &str, volume: Option<&str>) -> std::sync::Arc<FlintShare> {
        let mut labels = serde_json::Map::new();
        labels.insert("chert.us/project-id".into(), serde_json::json!(project));
        if let Some(v) = volume {
            labels.insert("chert.us/volume-id".into(), serde_json::json!(v));
        }
        std::sync::Arc::new(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
                "metadata": {"name": name, "namespace": ns, "labels": labels},
                "spec": {"persistence": {"size": "1Gi"}}
            }))
            .unwrap(),
        )
    }

    /// A project may legally have several hubs: the operator keys
    /// uniqueness on the bucket prefix subtree and never reads
    /// `chert.us/project-id`. So N shares on N prefixes with one project
    /// label is ordinary, and the lookup has to address them.
    #[test]
    fn a_project_may_have_several_volumes_and_each_is_addressable() {
        let fleet = vec![
            vshare("workspaces", "fs-p-data", "p", Some("data")),
            vshare("workspaces", "fs-p-models", "p", Some("models")),
        ];
        assert_eq!(shares_of(&fleet, "fs-", "p", None).len(), 2);
        for want in ["data", "models"] {
            match find(&fleet, "fs-", "p", Some(want), None) {
                Lookup::Found(s) => assert_eq!(volume_id_of(&s), want),
                l => panic!("{want}: {l:?}"),
            }
        }
    }

    /// Under-specified, not wrong: the caller gets the choice, sorted
    /// and deduplicated, rather than a share picked for it.
    #[test]
    fn a_bare_lookup_on_a_multi_volume_project_asks_which_one() {
        let fleet = vec![
            vshare("workspaces", "fs-p-models", "p", Some("models")),
            vshare("workspaces", "fs-p-data", "p", Some("data")),
        ];
        match find(&fleet, "fs-", "p", None, None) {
            Lookup::NeedsVolume(mut v) => {
                v.sort();
                assert_eq!(v, vec!["data", "models"]);
            }
            l => panic!("picked one of two volumes: {l:?}"),
        }
        // And the order it was stored in must not change the answer.
        let rev: Vec<_> = fleet.iter().rev().cloned().collect();
        assert!(matches!(find(&rev, "fs-", "p", None, None), Lookup::NeedsVolume(_)));
    }

    /// The distinction that matters: "pick one of these" is actionable,
    /// "two shares are the same volume" is a misconfiguration. Reporting
    /// the second as the first would send a caller round a loop that
    /// cannot terminate — it would ask for the volume it was offered and
    /// be offered it again.
    #[test]
    fn a_duplicate_volume_is_ambiguous_rather_than_a_choice() {
        let dup = vec![
            vshare("tenant-a", "fs-p-data", "p", Some("data")),
            vshare("tenant-b", "fs-p-data", "p", Some("data")),
        ];
        assert!(matches!(find(&dup, "fs-", "p", None, None), Lookup::Ambiguous(_)));
        assert!(matches!(find(&dup, "fs-", "p", Some("data"), None), Lookup::Ambiguous(_)));
    }

    #[test]
    fn an_unlabelled_share_uses_its_own_name_as_the_volume_id() {
        // So a project that never adopted the volume label still has a
        // usable identifier for every one of its shares.
        let fleet = vec![
            vshare("workspaces", "fs-p-data", "p", None),
            vshare("workspaces", "fs-p-models", "p", None),
        ];
        match find(&fleet, "fs-", "p", None, None) {
            Lookup::NeedsVolume(mut v) => {
                v.sort();
                assert_eq!(v, vec!["fs-p-data", "fs-p-models"]);
            }
            l => panic!("{l:?}"),
        }
        assert!(matches!(find(&fleet, "fs-", "p", Some("fs-p-data"), None), Lookup::Found(_)));
    }

    /// Never a fallback. Asking for a volume that does not exist must
    /// not serve one that does.
    #[test]
    fn an_unknown_volume_never_falls_back_to_a_sibling() {
        let fleet = vec![vshare("workspaces", "fs-p-data", "p", Some("data"))];
        assert_eq!(find(&fleet, "fs-", "p", Some("nope"), None), Lookup::NotFound);
        // The control: the sibling that DOES exist is still servable,
        // so the NotFound above is the volume filter and not an empty
        // fleet.
        assert!(matches!(find(&fleet, "fs-", "p", Some("data"), None), Lookup::Found(_)));
    }

    fn with_idle(after: Option<u64>, with_sessions: Option<bool>) -> ShareView {
        let mut idle = serde_json::Map::new();
        if let Some(a) = after {
            idle.insert("suspendAfterSecs".into(), serde_json::json!(a));
        }
        if let Some(w) = with_sessions {
            idle.insert("suspendWithSessions".into(), serde_json::json!(w));
        }
        let mut spec = serde_json::json!({"persistence": {"size": "1Gi"}});
        if !idle.is_empty() {
            spec["idle"] = serde_json::Value::Object(idle);
        }
        let share: FlintShare = serde_json::from_value(serde_json::json!({
            "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
            "metadata": {"name": "fs-p", "namespace": "ws"},
            "spec": spec
        }))
        .unwrap();
        ShareView::of(&share)
    }

    /// THE ONE AN AGENT NEEDS.
    ///
    /// agent-mounts-NFS is the primary consumer shape, and the hazard
    /// is specific: a mount held open with no file I/O is invisible to
    /// the hub's activity clock, so only the wake annotation keeps it
    /// alive — and an NFS client cannot write one. If it suspends, the
    /// mount hangs with no path back.
    ///
    /// So the caller is TOLD, at the moment it asks for an address.
    /// THE NFS-ONLY SHARE.
    ///
    /// `monitoring.fileApi` is OFF by default, so a plain NFS share —
    /// the primary consumer shape — publishes no `apiEndpoint` at all.
    /// The first cut of `/wake` judged every request at the file-API
    /// door, so it refused those shares with `FileApiDisabled`: the
    /// wake endpoint could not wake the exact shares it exists for.
    #[test]
    fn an_nfs_only_share_is_wakeable_even_though_it_has_no_api_endpoint() {
        let mut v = ShareView {
            namespace: "ws".into(),
            name: "fs-p".into(),
            phase: Some(Phase::Ready),
            address: Some("10.96.1.7:2049".into()),
            // No file API: no endpoint, and the operator says why.
            api_endpoint: None,
            api_condition: Some((
                false,
                "NotConfigured".into(),
                "spec.monitoring.fileApi is not enabled".into(),
            )),
            ..Default::default()
        };

        // The file-API door refuses it, and should.
        match decide_for(&v, Door::FileApi) {
            Decision::Refuse(r) => assert_eq!(r.reason, "FileApiDisabled"),
            d => panic!("the file API door must refuse a share with no file API: {d:?}"),
        }
        // The NFS door serves it.
        assert_eq!(
            decide_for(&v, Door::Nfs),
            Decision::Dial("10.96.1.7:2049".into()),
            "an NFS-only share must be reachable at the NFS door"
        );

        // And the phase half is shared: both doors agree on parked,
        // admin-suspended and terminating.
        for (phase, want) in [
            (Phase::IdleSuspended, Decision::Wake),
            (Phase::Hibernated, Decision::Wake),
            (Phase::Starting, Decision::Wait),
        ] {
            v.phase = Some(phase.clone());
            assert_eq!(decide_for(&v, Door::Nfs), want, "{phase:?} at the NFS door");
            assert_eq!(decide_for(&v, Door::FileApi), want, "{phase:?} at the API door");
        }
        v.phase = Some(Phase::Suspended);
        for door in [Door::Nfs, Door::FileApi] {
            match decide_for(&v, door) {
                Decision::Refuse(r) => assert_eq!(r.reason, "AdminSuspended", "{door:?}"),
                d => panic!("{door:?}: {d:?}"),
            }
        }
        v.phase = Some(Phase::Ready);
        v.deleting = true;
        for door in [Door::Nfs, Door::FileApi] {
            match decide_for(&v, door) {
                Decision::Refuse(r) => assert_eq!(r.status, 410, "{door:?}"),
                d => panic!("{door:?}: {d:?}"),
            }
        }
    }

    /// Ready but the Service has not published an address yet is a
    /// WAIT, not a refusal — `status.address` is withdrawn only on
    /// Failed and Terminating, and both are already refused above.
    #[test]
    fn a_ready_share_with_no_address_yet_is_retryable_at_the_nfs_door() {
        let v = ShareView {
            phase: Some(Phase::Ready),
            address: None,
            api_endpoint: Some("http://x:8080".into()),
            ..Default::default()
        };
        match decide_for(&v, Door::Nfs) {
            Decision::Refuse(r) => {
                assert_eq!(r.status, 503);
                assert_eq!(r.reason, "NoAddress");
                assert_eq!(r.retry_after, Some(5));
            }
            d => panic!("{d:?}"),
        }
        // The file API door is unaffected — the two doors are independent.
        assert!(matches!(decide_for(&v, Door::FileApi), Decision::Dial(_)));
    }

    #[test]
    fn a_share_that_can_be_suspended_under_a_mount_says_so() {
        // The ladder is off: nothing to warn about.
        assert_eq!(mount_hazard(&with_idle(None, None)), None);
        assert_eq!(keepalive_secs(&with_idle(None, None)), None);
        assert_eq!(mount_hazard(&with_idle(None, Some(true))), None,
            "suspendWithSessions is meaningless while the ladder is off");

        // The ladder is ON and the lease guard is NOT opted into —
        // absent and `true` are the same answer, which is the part
        // people get backwards.
        for w in [None, Some(true)] {
            let v = with_idle(Some(600), w);
            let why = mount_hazard(&v)
                .unwrap_or_else(|| panic!("suspendWithSessions={w:?} must warn"));
            assert!(why.contains("600s"), "{why}");
            assert!(why.contains("suspendWithSessions"), "{why}");
            assert_eq!(keepalive_secs(&v), Some(300), "half the budget, so one miss survives");
        }

        // OPTED IN, AND STILL A HAZARD. This assertion used to read
        // `== None`, on the belief that "the ladder holds while a lease is
        // live" — pinning the very behaviour that made the gateway silent
        // in the one configuration a real cluster proved unsafe.
        //
        // A lease EXPIRES. A client that is partitioned rather than gone
        // stops renewing, the count reaches zero on its own, and the guard
        // stops guarding: measured across an `iptables -j DROP` as guard
        // holding t=49-99, lease 1 -> 0 at t=99, SUSPENDED at t=111 with
        // the mount still held. The CRD's own doc says the same thing
        // ("it narrows the window; it does not close it").
        let v = with_idle(Some(600), Some(false));
        let why = mount_hazard(&v)
            .expect("suspendWithSessions: false narrows the window, it does not close it — \
                     a consumer about to mount must still be told");
        assert!(why.contains("EXPIRES"), "the warning must say WHY the guard lapses: {why}");
        assert!(why.contains("600s"), "{why}");
        // The remedy has to be the one that survives a partition of the
        // mount's path, which is the keepalive on the OTHER path — not
        // the flag the caller has already set.
        assert!(why.contains("every 300s"), "the warning must name the keepalive: {why}");
        assert_eq!(keepalive_secs(&v), Some(300));

        // ANTI-VACUITY: the two branches must still say DIFFERENT things,
        // or a warning that fires for everyone tells a caller nothing.
        let not_opted = mount_hazard(&with_idle(Some(600), None)).unwrap();
        assert_ne!(why, not_opted, "the opted-in warning must be specific to the lease-guard case");
    }

    #[test]
    fn a_tiny_suspend_budget_never_yields_a_zero_second_keepalive() {
        // A zero interval is a busy loop against the API server.
        assert_eq!(keepalive_secs(&with_idle(Some(1), None)), Some(1));
        assert_eq!(keepalive_secs(&with_idle(Some(0), None)), Some(1));
        assert!(mount_hazard(&with_idle(Some(1), None)).unwrap().contains("every 1s"));
    }

    #[test]
    fn a_volume_row_reports_serving_only_when_a_request_would_be_proxied() {
        let ready: FlintShare = serde_json::from_value(serde_json::json!({
            "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
            "metadata": {"name": "fs-p-data", "namespace": "ws",
                         "labels": {"chert.us/project-id": "p", "chert.us/volume-id": "data"}},
            "spec": {"bucket": "b", "keyPrefix": "p/data/", "persistence": {"size": "1Gi"}},
            "status": {"phase": "Ready", "apiEndpoint": "http://x:8080",
                       "conditions": [{"type": "ApiEndpointPublished", "status": "True",
                                       "reason": "InCluster",
                                       "lastTransitionTime": "2026-08-21T00:00:00Z"}]}
        })).unwrap();
        let row = volume_row(&ready);
        assert_eq!(row.volume, "data");
        assert_eq!(row.key_prefix.as_deref(), Some("p/data/"));
        assert!(row.serving);

        // Parked: legible, and honestly not serving. `serving: false` is
        // not an error — it wakes on first use.
        let mut parked = ready.clone();
        parked.status.as_mut().unwrap().phase = Some(Phase::IdleSuspended);
        let row = volume_row(&parked);
        assert!(!row.serving);
        assert_eq!(row.phase.as_deref(), Some("IdleSuspended"));

        // Ready by the operator, but the hub said otherwise.
        let mut importing = ready.clone();
        importing.status.as_mut().unwrap().hub_phase = Some("Importing".into());
        assert!(!volume_row(&importing).serving);
    }

    #[test]
    fn a_namespace_pinned_gateway_cannot_see_out_of_its_namespace() {
        let fleet = vec![share("tenant-a", "fs-proj-a", Some("proj-a"))];
        assert_eq!(find(&fleet, "fs-", "proj-a", None, Some("tenant-b")), Lookup::NotFound);
    }

    #[test]
    fn a_bucketless_share_has_no_binding() {
        let share: FlintShare = serde_json::from_value(serde_json::json!({
            "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
            "metadata": {"name": "p", "namespace": "ns"},
            "spec": {"persistence": {"size": "1Gi"}}
        }))
        .unwrap();
        assert_eq!(ShareView::of(&share).binding(), Err(NoBinding));
    }
}
