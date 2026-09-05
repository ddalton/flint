//! The `FlintRepo` reconciler.
//!
//! A slim controller rather than a trim of `lite_operator::reconcile`,
//! which is 4,000 lines because a share has a PVC to create, expand,
//! verify and delete; a hibernate that must prove a clean flush before
//! it destroys the only other copy; a reprovision path; and a
//! four-Service-type API surface. A repository has none of those. What
//! it does have, and what this file is, is: arbitration for the bucket
//! subtree, three children, one idle rung, a status document polled
//! from the server itself, and a phase.
//!
//! Two rules are worth stating before the code.
//!
//! **A failed `/status` poll is never "idle".** It arrives at the
//! ladder as `Err`, and the ladder Holds. An unreachable server is an
//! unknown server; treating it as quiet is how a repository gets scaled
//! to zero in the middle of a push.
//!
//! **The operator's arbitration is not the fence.** It refuses a second
//! CR over one subtree, which is the case an operator can see and fix.
//! The fence is the syncer's: the claim cell it reads before it serves
//! a byte, and the epoch lease it holds while it does. A CR the
//! operator has not judged yet still resolves to its spec, so the data
//! plane cannot depend on this having run.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Client, ResourceExt};

use crate::lite_operator::hubstatus::{self, HubSnapshot};

use super::crd::{FlintRepo, FlintRepoStatus, RepoLifecycle, RepoPhase, RepoStats};
use super::idle::{self, Decision, IdleState, ANN_IDLE_SINCE, ANN_IDLE_STATE};
use super::render::{self, RenderDefaults, STATUS_PORT};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
}

pub struct Defaults {
    pub render: RenderDefaults,
    /// How long to wait for the server's `/status`. Short: it is a
    /// loopback-speed request inside the cluster, and a slow one must
    /// not hold a reconcile pass open.
    pub status_timeout: Duration,
    /// Field manager for every apply this operator makes.
    pub manager: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            render: RenderDefaults::default(),
            status_timeout: Duration::from_secs(3),
            manager: "flint-forge-operator".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub phase: RepoPhase,
    /// Seconds until the next unconditional pass. Short while
    /// something is in motion, long when nothing is.
    pub requeue_secs: u64,
}

/// One thing a repository claims in a bucket.
///
/// The export is a claim too, and a separate one: it publishes a lean
/// workspace, so two repositories exporting to one prefix would be two
/// writers of one manifest — a collision the CRD's own CEL rule cannot
/// see, because it can only compare a CR against itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Claim {
    pub bucket: String,
    pub prefix: String,
    pub kind: &'static str,
}

pub fn claims(repo: &FlintRepo) -> Vec<Claim> {
    let mut v = vec![Claim {
        bucket: repo.spec.bucket.clone(),
        prefix: repo.spec.key_prefix.trim_end_matches('/').to_string(),
        kind: "repository",
    }];
    if let Some(e) = repo.spec.export.as_ref() {
        v.push(Claim {
            bucket: repo.spec.bucket.clone(),
            prefix: e.prefix.trim_end_matches('/').to_string(),
            kind: "export",
        });
    }
    v
}

/// Who owns a contested claim. The EARLIEST CR wins, ties broken by
/// uid so the answer is the same on every operator replica and across
/// restarts — an arbitration that could flip would hand the subtree
/// back and forth and never settle.
fn precedes(a: &FlintRepo, b: &FlintRepo) -> bool {
    let ta = a.creation_timestamp();
    let tb = b.creation_timestamp();
    match (ta, tb) {
        (Some(x), Some(y)) if x != y => x < y,
        _ => a.uid().unwrap_or_default() < b.uid().unwrap_or_default(),
    }
}

/// `Some(message)` when this repository must NOT serve: another CR owns
/// a bucket subtree it claims.
///
/// Nesting is deliberately not a collision here, and that is a property
/// of the layout rather than an omission. Everything a repository owns
/// lives under `<prefix>/git/`, so a repository at `a/` and one at
/// `a/proj/` own `a/git/**` and `a/proj/git/**`, which are disjoint.
/// Only an EXACT prefix match is two servers over one subtree.
pub fn arbitrate(me: &FlintRepo, all: &[FlintRepo]) -> Option<String> {
    let mine = claims(me);
    for other in all {
        if other.uid() == me.uid() || other.metadata.deletion_timestamp.is_some() {
            continue;
        }
        for their in claims(other) {
            for my in &mine {
                if my.bucket == their.bucket && my.prefix == their.prefix && !precedes(me, other) {
                    return Some(format!(
                        "{} owns {}/{} (as its {}); this repository claims the same subtree as its \
                         {} and will not be served — two servers over one prefix is a state the \
                         snapshot CAS cannot arbitrate",
                        render::slug(other),
                        their.bucket,
                        their.prefix,
                        their.kind,
                        my.kind
                    ));
                }
            }
        }
    }
    None
}

/// The phase, from everything observed this pass.
///
/// Ordering is load-bearing: refusal outranks the ladder, the ladder
/// outranks readiness, and readiness needs BOTH a ready pod and the
/// server's own word. A pod that is Running while the syncer is still
/// restoring is `Starting`, not `Ready` — and the door reads this,
/// so calling it Ready would send a clone into a server with no refs.
pub fn phase_of(
    repo: &FlintRepo,
    refused: Option<&str>,
    state: IdleState,
    ready_replicas: i32,
    snapshot: Option<&HubSnapshot>,
) -> RepoPhase {
    if repo.metadata.deletion_timestamp.is_some() {
        return RepoPhase::Terminating;
    }
    if refused.is_some() {
        return RepoPhase::Failed;
    }
    if repo.spec.lifecycle.unwrap_or_default() == RepoLifecycle::Suspended {
        return RepoPhase::Suspended;
    }
    if state == IdleState::Suspended {
        return RepoPhase::IdleSuspended;
    }
    match snapshot {
        Some(s) if ready_replicas >= 1 && s.phase.is_quiescible() => RepoPhase::Ready,
        _ if ready_replicas >= 1 => RepoPhase::Starting,
        _ => RepoPhase::Pending,
    }
}

/// How many replicas this phase wants.
pub fn replicas_for(phase: RepoPhase, decision: &Decision) -> i32 {
    match phase {
        // A refused repository runs nothing. Leaving its pod up would
        // be the second writer the refusal exists to prevent.
        RepoPhase::Failed | RepoPhase::Terminating => 0,
        RepoPhase::Suspended => 0,
        _ => match decision {
            Decision::Suspend => 0,
            Decision::Wake => 1,
            _ if matches!(phase, RepoPhase::IdleSuspended) => 0,
            _ => 1,
        },
    }
}

async fn apply<K>(api: &Api<K>, name: &str, obj: &K, manager: &str) -> Result<(), Error>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    api.patch(name, &PatchParams::apply(manager).force(), &Patch::Apply(obj)).await?;
    Ok(())
}

/// Poll the server's own `/status`.
///
/// Dials the POD, not the Service: a headless Service publishes DNS
/// only for ready pods, and this is precisely the question "is it
/// ready" — resolving through it would answer only when the answer was
/// already known. `Err` is returned rather than swallowed, because the
/// ladder must be able to tell "quiet" from "could not ask".
/// The forge-specific half of the server's status document. Parsed
/// from the same body as the `HubSnapshot`, so the ladder's predicate
/// and the repository's counters cost ONE round trip between them.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgeStatusDoc {
    #[serde(default)]
    pub repo: Option<RepoDoc>,
    #[serde(default)]
    activity: Option<ActivityDoc>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoDoc {
    #[serde(default)]
    pub refs: u64,
    #[serde(default)]
    pub packs: u64,
    #[serde(default)]
    pub snapshot_seq: u64,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityDoc {
    #[serde(default)]
    last_activity_unix: u64,
}

pub async fn poll_status(
    client: &Client,
    repo: &FlintRepo,
    d: &Defaults,
) -> Result<(HubSnapshot, ForgeStatusDoc), String> {
    let ns = repo.namespace().unwrap_or_default();
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let selector = render::selector_labels(repo)
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    let list = pods
        .list(&ListParams::default().labels(&selector))
        .await
        .map_err(|e| format!("listing the server pod: {e}"))?;
    let ip = list
        .items
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .find_map(|p| p.status.as_ref().and_then(|s| s.pod_ip.clone()))
        .ok_or_else(|| "no server pod with an address yet".to_string())?;
    let (body, url) = hubstatus::poll_raw(&ip, STATUS_PORT, d.status_timeout).await?;
    let snap: HubSnapshot =
        serde_json::from_str(&body).map_err(|e| format!("parsing {url}: {e}"))?;
    // The forge half is best effort: a server too old to report its
    // counters is still a server whose ladder predicate is readable,
    // and refusing the whole poll over a missing block would make an
    // upgrade look like an outage.
    let forge: ForgeStatusDoc = serde_json::from_str(&body).unwrap_or_default();
    Ok((snap, forge))
}

/// One reconcile pass.
pub async fn full_pass(
    client: &Client,
    repo: &FlintRepo,
    all: &[FlintRepo],
    d: &Defaults,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Outcome, Error> {
    let ns = repo.namespace().unwrap_or_default();
    let name = repo.name_any();
    let repos: Api<FlintRepo> = Api::namespaced(client.clone(), &ns);

    let refused = arbitrate(repo, all);

    // Children first, and unconditionally — including for a refused
    // repository, whose Deployment is applied at zero replicas rather
    // than left at whatever it was.
    if refused.is_none() {
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
        let svcs: Api<Service> = Api::namespaced(client.clone(), &ns);
        let n = render::names(repo);
        apply(&cms, &n.config_map, &render::config_map(repo), &d.manager).await?;
        apply(&svcs, &n.service, &render::service(repo), &d.manager).await?;
        // The trust boundary that makes `X-Remote-User` mean anything.
        // Rendered only when the operator was told where the door runs:
        // a policy naming the wrong door breaks every clone, and a
        // cluster whose CNI does not enforce NetworkPolicy gets nothing
        // from one either. See `render::network_policy`.
        if let Some(door) = d.render.door.as_ref() {
            let nps: Api<k8s_openapi::api::networking::v1::NetworkPolicy> =
                Api::namespaced(client.clone(), &ns);
            apply(
                &nps,
                &n.network_policy,
                &render::network_policy(repo, door),
                &d.manager,
            )
            .await?;
        }
    }

    // The server's own word. A failure is carried as `Err` all the way
    // into the ladder rather than being turned into an absence.
    let polled = poll_status(client, repo, d).await;
    let after = repo.spec.idle.as_ref().and_then(|i| i.suspend_after_secs);
    let server_quiet = match (&polled, after) {
        (Ok((s, _)), Some(after)) => idle::server_quiet(s, after),
        (Ok(_), None) => Ok(()),
        (Err(e), _) => Err(format!("could not read the server's status: {e}")),
    };
    let snapshot = polled.as_ref().ok().map(|(s, _)| s);
    let forge_doc = polled.as_ref().ok().map(|(_, f)| f);

    let state = idle::state_of(repo.annotations());
    let decision = idle::decide(idle::Inputs { repo, now, server_quiet });

    let deploys: Api<Deployment> = Api::namespaced(client.clone(), &ns);
    let n = render::names(repo);
    let ready_replicas = deploys
        .get_opt(&n.deployment)
        .await?
        .and_then(|dep| dep.status.and_then(|s| s.ready_replicas))
        .unwrap_or(0);

    let phase = phase_of(repo, refused.as_deref(), state, ready_replicas, snapshot);
    let replicas = replicas_for(phase, &decision);
    apply(
        &deploys,
        &n.deployment,
        &render::deployment(repo, &d.render, replicas),
        &d.manager,
    )
    .await?;

    // Record the ladder's new position, if it moved. The wake stamp is
    // deliberately NOT cleared: it is the door's heartbeat, and
    // consuming it would make the next staleness test read "never
    // asked" one pass after someone asked.
    let want_state = match decision {
        Decision::Suspend => Some(IdleState::Suspended),
        Decision::Wake => Some(IdleState::Active),
        _ => None,
    };
    if let Some(want) = want_state.filter(|w| *w != state) {
        let patch = serde_json::json!({
            "metadata": { "annotations": {
                ANN_IDLE_STATE: want.as_str(),
                ANN_IDLE_SINCE: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            }}
        });
        repos.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }

    let status = FlintRepoStatus {
        phase: Some(phase),
        // Withdrawn on the phases that must not advertise a door at
        // all, the way lite withdraws `status.address`: a door that
        // dialled a refused repository would be routing to the second
        // writer the refusal exists to prevent.
        git_endpoint: match phase {
            RepoPhase::Failed | RepoPhase::Terminating => None,
            _ => render::git_endpoint(repo),
        },
        observed_generation: repo.metadata.generation,
        server_id: snapshot.and_then(|s| s.server_id.clone()),
        // The DEBUG spelling, so `hub_phase_blocks` at the door — which
        // compares against `Serving` and `Sweeping` — reads it. The
        // wire's camelCase would silently never match, taking every
        // repository out of rotation at the door while the CR looked
        // healthy.
        server_phase: snapshot.map(|s| format!("{:?}", s.phase)),
        repo: forge_doc.and_then(|f| f.repo.as_ref()).map(|r| RepoStats {
            refs: r.refs,
            packs: r.packs,
            snapshot_seq: r.snapshot_seq,
            last_push_unix: forge_doc.and_then(|f| f.activity.as_ref()).map(|a| a.last_activity_unix),
        }),
        refused: refused.clone(),
        conditions: None,
    };
    let patch = serde_json::json!({ "status": status });
    repos
        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;

    Ok(Outcome {
        phase,
        requeue_secs: match phase {
            // Something is in motion: look again soon.
            RepoPhase::Pending | RepoPhase::Starting => 5,
            // Serving: the ladder's resolution is what sets the floor,
            // and a repository with no ladder needs no clock at all.
            RepoPhase::Ready => after.map(|a| (a / 4).clamp(15, 300)).unwrap_or(300),
            // Down or refused: nothing changes without an event, and an
            // event requeues immediately.
            _ => 300,
        },
    })
}

/// The annotations a fresh CR should carry. Exposed for tests and for
/// the binary's first pass.
pub fn initial_annotations() -> BTreeMap<String, String> {
    BTreeMap::from([(ANN_IDLE_STATE.to_string(), IdleState::Active.as_str().to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge_operator::crd::{ExportSpec, FlintRepoSpec};

    fn repo(name: &str, prefix: &str, uid: &str, created: &str) -> FlintRepo {
        let mut r = FlintRepo::new(
            name,
            FlintRepoSpec {
                project_id: name.into(),
                bucket: "bkt".into(),
                key_prefix: prefix.into(),
                endpoint: None,
                credentials_secret_ref: None,
                default_branch: None,
                consumers: None,
                branches: None,
                idle: None,
                export: None,
                fleet: None,
                log_level: None,
                lifecycle: None,
            },
        );
        r.metadata.namespace = Some("tenant".into());
        r.metadata.uid = Some(uid.into());
        r.metadata.creation_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                created.parse().expect("a timestamp"),
            ));
        r
    }

    /// Two CRs over one subtree is two servers over one prefix, which
    /// the snapshot CAS cannot arbitrate because they were never
    /// supposed to meet. The earliest wins, and the answer is the same
    /// on every replica.
    #[test]
    fn the_earlier_repository_owns_a_contested_subtree() {
        let first = repo("a", "tenant/x/", "uid-a", "2026-09-01T00:00:00Z");
        let second = repo("b", "tenant/x/", "uid-b", "2026-09-02T00:00:00Z");
        let all = vec![first.clone(), second.clone()];
        assert_eq!(arbitrate(&first, &all), None, "the earlier CR serves");
        let why = arbitrate(&second, &all).expect("the later CR is refused");
        assert!(why.contains("tenant/a"), "{why}");
        assert!(why.contains("tenant/x"), "{why}");
    }

    /// A tie has to break the same way everywhere, or the subtree is
    /// handed back and forth and never settles.
    #[test]
    fn a_tie_breaks_on_uid_and_is_stable() {
        let a = repo("a", "tenant/x/", "uid-a", "2026-09-01T00:00:00Z");
        let b = repo("b", "tenant/x/", "uid-b", "2026-09-01T00:00:00Z");
        let all = vec![a.clone(), b.clone()];
        assert_eq!(arbitrate(&a, &all), None);
        assert!(arbitrate(&b, &all).is_some());
        // …and the same with the list in the other order.
        let flipped = vec![b.clone(), a.clone()];
        assert_eq!(arbitrate(&a, &flipped), None);
        assert!(arbitrate(&b, &flipped).is_some());
    }

    /// Nesting is not a collision, and that is a property of the
    /// layout: everything a repository owns is under `<prefix>/git/`,
    /// so `a/` and `a/proj/` own disjoint trees.
    #[test]
    fn a_nested_prefix_is_not_a_collision() {
        let outer = repo("a", "tenant/", "uid-a", "2026-09-01T00:00:00Z");
        let inner = repo("b", "tenant/proj/", "uid-b", "2026-09-02T00:00:00Z");
        let all = vec![outer.clone(), inner.clone()];
        assert_eq!(arbitrate(&outer, &all), None);
        assert_eq!(arbitrate(&inner, &all), None);
    }

    /// An export claims a subtree too, and two repositories exporting
    /// to one prefix are two writers of one lean manifest — a
    /// collision the CRD's CEL rule cannot see.
    #[test]
    fn two_repositories_may_not_export_to_one_prefix() {
        let mut a = repo("a", "tenant/a/", "uid-a", "2026-09-01T00:00:00Z");
        let mut b = repo("b", "tenant/b/", "uid-b", "2026-09-02T00:00:00Z");
        let ex = |p: &str| ExportSpec { refs: vec!["main".into()], prefix: p.into(), every_secs: None };
        a.spec.export = Some(ex("tenant/shared/"));
        b.spec.export = Some(ex("tenant/shared/"));
        let all = vec![a.clone(), b.clone()];
        assert_eq!(arbitrate(&a, &all), None);
        let why = arbitrate(&b, &all).expect("the later export is refused");
        assert!(why.contains("export"), "{why}");
    }

    /// An export must not land on another repository's own prefix
    /// either: the export would write a lean manifest beside that
    /// repository's `git/`.
    #[test]
    fn an_export_may_not_land_on_another_repositorys_prefix() {
        let owner = repo("a", "tenant/x/", "uid-a", "2026-09-01T00:00:00Z");
        let mut squatter = repo("b", "tenant/y/", "uid-b", "2026-09-02T00:00:00Z");
        squatter.spec.export = Some(ExportSpec {
            refs: vec!["main".into()],
            prefix: "tenant/x/".into(),
            every_secs: None,
        });
        let all = vec![owner.clone(), squatter.clone()];
        assert_eq!(arbitrate(&owner, &all), None);
        assert!(arbitrate(&squatter, &all).is_some());
    }

    /// A CR being deleted has stopped claiming anything: its children
    /// are on their way out, and holding the subtree against its
    /// successor is how a recreate deadlocks.
    #[test]
    fn a_deleting_repository_stops_claiming_its_subtree() {
        let mut old = repo("a", "tenant/x/", "uid-a", "2026-09-01T00:00:00Z");
        old.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                "2026-09-03T00:00:00Z".parse().expect("a timestamp"),
            ));
        let new = repo("b", "tenant/x/", "uid-b", "2026-09-02T00:00:00Z");
        assert_eq!(arbitrate(&new, &[old, new.clone()]), None);
    }

    fn snapshot(phase: &str, idle: u64) -> HubSnapshot {
        serde_json::from_str(&format!(
            r#"{{"phase":"{phase}","rpoClean":true,"activity":{{"idleSecs":{idle}}}}}"#
        ))
        .expect("the syncer's own document must parse as a hub snapshot")
    }

    /// A pod that is Running while the syncer is still restoring is
    /// `Starting`, not `Ready` — the door reads this, so calling it
    /// Ready would send a clone into a server with no refs.
    #[test]
    fn readiness_needs_the_pod_and_the_servers_own_word() {
        let r = repo("a", "tenant/x/", "uid-a", "2026-09-01T00:00:00Z");
        assert_eq!(
            phase_of(&r, None, IdleState::Active, 1, Some(&snapshot("serving", 0))),
            RepoPhase::Ready
        );
        assert_eq!(
            phase_of(&r, None, IdleState::Active, 1, Some(&snapshot("importing", 0))),
            RepoPhase::Starting
        );
        assert_eq!(
            phase_of(&r, None, IdleState::Active, 1, None),
            RepoPhase::Starting,
            "a pod we cannot ask is starting, never ready"
        );
        assert_eq!(phase_of(&r, None, IdleState::Active, 0, None), RepoPhase::Pending);
    }

    /// Refusal outranks the ladder, the ladder outranks readiness, and
    /// a refused repository runs nothing — leaving its pod up would be
    /// the second writer the refusal exists to prevent.
    #[test]
    fn a_refused_repository_runs_nothing() {
        let r = repo("a", "tenant/x/", "uid-a", "2026-09-01T00:00:00Z");
        let phase = phase_of(&r, Some("b owns it"), IdleState::Active, 1, Some(&snapshot("serving", 0)));
        assert_eq!(phase, RepoPhase::Failed);
        assert_eq!(replicas_for(phase, &Decision::Stay), 0);
    }

    #[test]
    fn the_ladder_drives_the_replica_count() {
        assert_eq!(replicas_for(RepoPhase::Ready, &Decision::Stay), 1);
        assert_eq!(replicas_for(RepoPhase::Ready, &Decision::Suspend), 0);
        assert_eq!(replicas_for(RepoPhase::IdleSuspended, &Decision::Wake), 1);
        assert_eq!(
            replicas_for(RepoPhase::IdleSuspended, &Decision::Hold("quiet".into())),
            0
        );
        assert_eq!(replicas_for(RepoPhase::Suspended, &Decision::Wake), 0, "an admin outranks a wake");
    }

    /// The syncer's `/status` is deliberately in the hub's shape, so
    /// the ladder's predicate is shared rather than reimplemented. This
    /// pins that: a real document from `flint_forge::status` must feed
    /// `suspendable`.
    #[test]
    fn the_syncers_own_document_answers_the_ladders_question() {
        let doc = r#"{
            "phase": "serving",
            "uptimeSecs": 900,
            "serverId": "forge-1",
            "activity": { "lastActivityUnix": 1700000000, "idleSecs": 900 },
            "rpoClean": true,
            "epoch": { "held": true, "number": 3 },
            "repo": { "refs": 4, "packs": 1, "snapshotSeq": 12 },
            "syncerVersion": "0.1.0",
            "fenced": null
        }"#;
        let snap: HubSnapshot = serde_json::from_str(doc).expect("parses as a hub snapshot");
        assert!(snap.suspendable(600).is_ok(), "quiet past the threshold");
        assert!(snap.suspendable(1200).is_err(), "not quiet long enough yet");

        let forge: ForgeStatusDoc = serde_json::from_str(doc).expect("parses as a forge document");
        let stats = forge.repo.expect("the repo block");
        assert_eq!((stats.refs, stats.packs, stats.snapshot_seq), (4, 1, 12));
        assert_eq!(forge.activity.map(|a| a.last_activity_unix), Some(1700000000));
    }
}
