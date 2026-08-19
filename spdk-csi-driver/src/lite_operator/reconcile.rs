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
    /// The CR asks for LESS than the claim already has. Kubernetes
    /// cannot shrink a PVC, and an apply would fail every reconcile
    /// forever with an error nobody reads — say so in status instead.
    ShrinkRefused { have: String, want: String },
}

pub fn claim_plan(existing: Option<&PersistentVolumeClaim>, want: &str, adopted: bool) -> ClaimPlan {
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
pub fn phase_of(lifecycle: Lifecycle, dep: Option<&Deployment>, blocked: bool) -> Phase {
    if blocked {
        return Phase::Failed;
    }
    if lifecycle == Lifecycle::Suspended {
        return Phase::Suspended;
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
pub fn address_of(svc: &Service, namespace: &str) -> Option<String> {
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
    let fleet: Vec<Candidate> = ctx
        .fleet
        .state()
        .iter()
        .map(|s| Candidate::of(s))
        .collect();
    if let Admission::Rejected { winner, message } = conflict::admit(&fleet, &Candidate::of(&share))
    {
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

    let mut cm = rendered.config_map.clone();
    cm.metadata.owner_references = Some(vec![owner.clone()]);
    Api::<ConfigMap>::namespaced(ctx.client.clone(), &ns)
        .patch(&names.config_map, &pp, &Patch::Apply(&cm))
        .await?;

    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
    let existing_pvc = get_opt(claims.get(&names.claim)).await?;
    match claim_plan(
        existing_pvc.as_ref(),
        &share.spec.persistence.size,
        names.claim_is_adopted,
    ) {
        ClaimPlan::Apply => {
            if let Some(pvc) = rendered.pvc.clone() {
                // NO ownerReference, ever. See the module doc: owner GC
                // does not know what Retain means.
                claims.patch(&names.claim, &pp, &Patch::Apply(&pvc)).await?;
            }
        }
        ClaimPlan::Skip => {}
        ClaimPlan::ShrinkRefused { have, want } => {
            let msg = format!(
                "persistence.size {want} is smaller than the existing claim's {have}; Kubernetes \
                 cannot shrink a PVC. The hub keeps {have}."
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
    Api::<Service>::namespaced(ctx.client.clone(), &ns)
        .patch(&names.service, &pp, &Patch::Apply(&svc))
        .await?;

    let mut dep = rendered.deployment.clone();
    dep.metadata.owner_references = Some(vec![owner]);
    let dep = deployments.patch(&names.deployment, &pp, &Patch::Apply(&dep)).await?;

    // --- 6. Status ------------------------------------------------------
    let svc_live = get_opt(
        Api::<Service>::namespaced(ctx.client.clone(), &ns).get(&names.service),
    )
    .await?;
    let lifecycle = share.spec.lifecycle.clone().unwrap_or_default();
    let phase = phase_of(lifecycle.clone(), Some(&dep), false);
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
            address: svc_live.as_ref().and_then(|s| address_of(s, &ns)),
            observed_generation: generation,
            claim_name: Some(names.claim.clone()),
            conditions: Some(conds),
        },
    )
    .await?;

    Ok(Action::requeue(match phase {
        Phase::Ready | Phase::Suspended => REQUEUE_SETTLED,
        _ => REQUEUE_PROGRESS,
    }))
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
                          adopted = names.claim_is_adopted,
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
pub fn error_policy(share: Arc<FlintShare>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    warn!(share = %share.name_any(), "reconcile failed: {err}");
    Action::requeue(Duration::from_secs(30))
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
    use crate::lite_operator::crd::{FlintShareSpec, PersistenceSpec};
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
        assert_eq!(claim_plan(None, "20Gi", false), ClaimPlan::Apply);
        assert_eq!(claim_plan(Some(&pvc_of("20Gi")), "20Gi", false), ClaimPlan::Apply);
        assert_eq!(claim_plan(Some(&pvc_of("20Gi")), "100Gi", false), ClaimPlan::Apply);
        assert_eq!(
            claim_plan(Some(&pvc_of("100Gi")), "20Gi", false),
            ClaimPlan::ShrinkRefused {
                have: "100Gi".into(),
                want: "20Gi".into()
            }
        );
        // An adopted claim is someone else's declaration; we bind to
        // it, we do not re-declare it.
        assert_eq!(claim_plan(Some(&pvc_of("100Gi")), "20Gi", true), ClaimPlan::Skip);
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
        assert_eq!(phase_of(Lifecycle::Active, Some(&dep), false), Phase::Starting);

        dep.status = Some(DeploymentStatus {
            replicas: Some(1),
            available_replicas: Some(1),
            ..Default::default()
        });
        assert_eq!(phase_of(Lifecycle::Active, Some(&dep), false), Phase::Ready);
        assert_eq!(
            phase_of(Lifecycle::Suspended, Some(&dep), false),
            Phase::Suspended
        );
        assert_eq!(phase_of(Lifecycle::Active, None, false), Phase::Pending);
        assert_eq!(phase_of(Lifecycle::Active, Some(&dep), true), Phase::Failed);
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
            address_of(&svc("ClusterIP", None), "ws").as_deref(),
            Some("tenant-a.ws.svc.cluster.local:2049")
        );
        // A LoadBalancer with no ingress yet has no address to report —
        // reporting the ClusterIP would be a lie a consumer cannot reach.
        assert_eq!(address_of(&svc("LoadBalancer", None), "ws"), None);
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
            address_of(&lb, "ws").as_deref(),
            Some("a.elb.amazonaws.com:2049")
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
}
