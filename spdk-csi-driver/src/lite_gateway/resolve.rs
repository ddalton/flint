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
    /// Parked and wakeable: arm `flint.io/requested-at`, then wait.
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

    // Ready. The endpoint is the last thing that can be missing, and
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

/// The documented index from a project id to its share.
///
/// `docs/flint-lite-operator.md` tells a front door to derive the name
/// (`fs-<project-id>`) AND label it. Both are load-bearing and they fail
/// differently: the derived name is what makes an ensure-live create
/// idempotent (two replicas racing issue the same create, one gets 409),
/// while the label is what makes the mapping legible from the cluster
/// side — it is already a printer column on the CRD.
pub const LABEL_PROJECT_ID: &str = "flint.io/project-id";

pub fn project_id_of(share: &FlintShare) -> Option<String> {
    share.metadata.labels.as_ref()?.get(LABEL_PROJECT_ID).cloned()
}

/// The result of looking a project up in the fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    Found(T),
    NotFound,
    /// More than one share claims this project. Reported as `ns/name`
    /// pairs and REFUSED — see [`find`].
    Ambiguous(Vec<String>),
}

/// Find the share for a project id, label first and derived name second.
///
/// **Ambiguity is refused, never resolved.** Watching every namespace is
/// the fleet posture, and two namespaces can hold a `fs-proj-a` each —
/// one tenant's, one another's. Any tie-break (first in the store,
/// lowest namespace, most recently created) is a rule that silently
/// serves one tenant's files to a caller asking for the other's, and
/// the store's iteration order is not even stable across relists. So
/// this returns the candidates and the caller answers 409 naming them:
/// wrong is worse than unavailable here, and an operator can fix a
/// duplicate in a minute once they can see it.
///
/// The label wins over the name when BOTH match different shares,
/// because the label is the deliberate statement and the name is a
/// convention. A share matching by name is only consulted when no share
/// carries the label — which is what lets an install that predates the
/// labelling convention keep working.
pub fn find<T: AsRef<FlintShare> + Clone>(
    shares: &[T],
    prefix: &str,
    project: &str,
    namespace: Option<&str>,
) -> Lookup<T> {
    let in_scope = |s: &FlintShare| match namespace {
        Some(ns) => s.metadata.namespace.as_deref() == Some(ns),
        None => true,
    };
    let name = share_name(prefix, project);

    let by_label: Vec<&T> = shares
        .iter()
        .filter(|s| in_scope(s.as_ref()) && project_id_of(s.as_ref()).as_deref() == Some(project))
        .collect();
    let chosen = if by_label.is_empty() {
        shares
            .iter()
            .filter(|s| in_scope(s.as_ref()) && s.as_ref().metadata.name.as_deref() == Some(&name))
            .collect::<Vec<_>>()
    } else {
        by_label
    };

    match chosen.len() {
        0 => Lookup::NotFound,
        1 => Lookup::Found(chosen[0].clone()),
        _ => Lookup::Ambiguous(
            chosen
                .iter()
                .map(|s| {
                    let m = &s.as_ref().metadata;
                    format!(
                        "{}/{}",
                        m.namespace.as_deref().unwrap_or("?"),
                        m.name.as_deref().unwrap_or("?")
                    )
                })
                .collect(),
        ),
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
            "apiVersion": "flint.io/v1alpha1",
            "kind": "FlintShare",
            "metadata": {
                "name": "fs-proj-a",
                "namespace": "workspaces",
                "labels": { "flint.io/project-id": "proj-a" },
                "annotations": {
                    "flint.io/requested-at": "2026-08-21T00:00:00Z",
                    "flint.io/api-token-version": "4"
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
            "apiVersion": "flint.io/v1alpha1", "kind": "FlintShare",
            "metadata": {"name": "p", "namespace": "ns", "annotations": {}},
            "spec": {"persistence": {"size": "1Gi"}}
        });
        for ann in [serde_json::json!({}), serde_json::json!({"flint.io/api-token-version": "0"}),
                    serde_json::json!({"flint.io/api-token-version": "nonsense"}),
                    serde_json::json!({"flint.io/api-token-version": "-2"})] {
            let mut j = base.clone();
            j["metadata"]["annotations"] = ann.clone();
            let share: FlintShare = serde_json::from_value(j).unwrap();
            assert_eq!(ShareView::of(&share).token_version, 1, "{ann}");
        }
    }

    fn share(ns: &str, name: &str, label: Option<&str>) -> std::sync::Arc<FlintShare> {
        let mut labels = serde_json::Map::new();
        if let Some(l) = label {
            labels.insert("flint.io/project-id".into(), serde_json::json!(l));
        }
        std::sync::Arc::new(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "flint.io/v1alpha1", "kind": "FlintShare",
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
        match find(&fleet, "fs-", "proj-a", None) {
            Lookup::Found(s) => assert_eq!(s.metadata.name.as_deref(), Some("fs-proj-a")),
            l => panic!("{l:?}"),
        }
        // No label anywhere for proj-b: the derived name carries it.
        match find(&fleet, "fs-", "proj-b", None) {
            Lookup::Found(s) => assert_eq!(s.metadata.name.as_deref(), Some("fs-proj-b")),
            l => panic!("{l:?}"),
        }
        assert_eq!(find(&fleet, "fs-", "nope", None), Lookup::NotFound);
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
        match find(&fleet, "fs-", "proj-a", None) {
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
        match find(&fleet, "fs-", "proj-a", None) {
            Lookup::Ambiguous(mut who) => {
                who.sort();
                assert_eq!(who, vec!["tenant-a/fs-proj-a", "tenant-b/fs-proj-a"]);
            }
            l => panic!("picked one of two tenants: {l:?}"),
        }
        // Reversing the store's order must not change the answer.
        let reversed: Vec<_> = fleet.iter().rev().cloned().collect();
        assert!(matches!(find(&reversed, "fs-", "proj-a", None), Lookup::Ambiguous(_)));

        // Pinning the gateway to one namespace disambiguates it.
        match find(&fleet, "fs-", "proj-a", Some("tenant-b")) {
            Lookup::Found(s) => assert_eq!(s.metadata.namespace.as_deref(), Some("tenant-b")),
            l => panic!("{l:?}"),
        }
    }

    #[test]
    fn a_namespace_pinned_gateway_cannot_see_out_of_its_namespace() {
        let fleet = vec![share("tenant-a", "fs-proj-a", Some("proj-a"))];
        assert_eq!(find(&fleet, "fs-", "proj-a", Some("tenant-b")), Lookup::NotFound);
    }

    #[test]
    fn a_bucketless_share_has_no_binding() {
        let share: FlintShare = serde_json::from_value(serde_json::json!({
            "apiVersion": "flint.io/v1alpha1", "kind": "FlintShare",
            "metadata": {"name": "p", "namespace": "ns"},
            "spec": {"persistence": {"size": "1Gi"}}
        }))
        .unwrap();
        assert_eq!(ShareView::of(&share).binding(), Err(NoBinding));
    }
}
