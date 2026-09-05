//! The `FlintRepo` CRD.
//!
//! One CR = one git repository: a durable project identity, a bucket
//! prefix, who may reach it, and who may move which ref. The server
//! that serves it is a Deployment of one pod with an `emptyDir` cache
//! (design `docs/plans/flint-forge-design.md` §2), so unlike a
//! `FlintShare` there is no PVC in its wake and no hibernate rung — a
//! suspended repo's local disk is already gone, and waking it is a
//! restore from the bucket either way.
//!
//! ## Why the phases are forge's own
//!
//! `lite_operator::crd::Phase` carries `Hibernated` and
//! `Reprovisioning`, which describe a PVC forge does not have. Rather
//! than publish phases that can never occur, this has its own set —
//! and projects onto lite's when the gateway builds a view, because
//! the DECISION (terminating serves nothing, parked wakes, refused
//! stays refused) is genuinely the same and is worth having one
//! implementation of. [`RepoPhase::as_share_phase`] is that
//! projection, and it is total by construction.
//!
//! ## Why the branch policy is a type here and a document there
//!
//! The enforcers — `pre-receive` and the syncer — read a rendered JSON
//! document, and they live in the `flint-forge` crate. The CRD needs a
//! `JsonSchema` type with CEL validation, which that crate has no
//! business carrying. So the shapes are separate and
//! [`BranchPolicy::render`] converts, which means a field added on
//! either side that nobody maps is a COMPILE error rather than a rule
//! that silently stops being enforced.

use std::collections::BTreeMap;

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::lite_operator::crd::Phase as SharePhase;
use crate::s3csi::policy::Consumers;

#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "chert.us",
    version = "v1alpha1",
    kind = "FlintRepo",
    plural = "flintrepos",
    singular = "flintrepo",
    shortname = "fr",
    namespaced,
    status = "FlintRepoStatus",
    derive = "PartialEq",
    doc = "A git repository served by one flint-forge server, with S3 as its only durable state",
    printcolumn = r#"{"name":"PHASE","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"BUCKET","type":"string","jsonPath":".spec.bucket"}"#,
    printcolumn = r#"{"name":"PREFIX","type":"string","jsonPath":".spec.keyPrefix"}"#,
    printcolumn = r#"{"name":"REFS","type":"integer","jsonPath":".status.repo.refs"}"#,
    printcolumn = r#"{"name":"AGE","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
// Identity immutability, for the reason lite has it: a repository that
// re-points its prefix is two servers over one bucket subtree, and the
// snapshot CAS cannot arbitrate between writers that were never
// supposed to meet.
#[x_kube(validation = Rule::new("self.projectId == oldSelf.projectId")
    .message("spec.projectId is immutable — create a new FlintRepo instead"))]
#[x_kube(validation = Rule::new("self.bucket == oldSelf.bucket")
    .message("spec.bucket is immutable — create a new FlintRepo instead"))]
#[x_kube(validation = Rule::new("self.keyPrefix == oldSelf.keyPrefix")
    .message("spec.keyPrefix is immutable — create a new FlintRepo instead"))]
// The same prefix-syntax refusal lite makes, and for the same reason: a
// prefix that does not end in "/" also matches its siblings, so
// "tenant-a" would share a subtree with "tenant-agency/".
#[x_kube(validation = Rule::new("self.keyPrefix.endsWith('/')")
    .message("spec.keyPrefix must end with '/' — a prefix without it also matches sibling names"))]
#[x_kube(validation = Rule::new("!self.keyPrefix.startsWith('/')")
    .message("spec.keyPrefix must not start with '/'"))]
// The export publishes a lean workspace, and a lean workspace's control
// objects live under `<prefix>/.flint/`. Pointing an export at the
// repository's own prefix would put a second writer's manifest beside
// `git/`; pointing it at another repo's export prefix would have two
// servers publishing one workspace.
#[x_kube(validation = Rule::new("!has(self.export) || self.export.prefix != self.keyPrefix")
    .message("spec.export.prefix must differ from spec.keyPrefix — the export is a separate lean workspace"))]
// ONE ref per export prefix. A lean workspace is one tree, so two refs
// published into one prefix would be two writers of one manifest, each
// deleting what the other just wrote. Refused at admission rather than
// discovered as a workspace that flickers between two histories.
#[x_kube(validation = Rule::new("!has(self.export) || size(self.export.refs) == 1")
    .message("spec.export.refs must name exactly one ref — a lean workspace is one tree, and two refs in one prefix would be two writers of one manifest"))]
pub struct FlintRepoSpec {
    /// The durable, user-declared identity the claim cell carries —
    /// stable across CR delete/recreate, and NEVER the CR UID. A
    /// standing claim naming another project is refused on the DATA
    /// plane by the syncer, not merely by the operator.
    pub project_id: String,

    pub bucket: String,
    /// Everything this repository owns lives under it: `git/` is the
    /// bare repository, and `git/snapshot` is its pointer.
    pub key_prefix: String,

    /// S3 endpoint override (a deployment proxy, or a MinIO rig).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Secret with the SYNCER's S3 credentials, keys `AWS_*` verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_secret_ref: Option<String>,

    /// `HEAD` in a repository nobody has pushed to. Default `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,

    /// Which ServiceAccounts the door will let through at all. Empty or
    /// absent = nobody, which is the safe direction for a surface whose
    /// whole point is that a pod's own token is the credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumers: Option<Consumers>,

    /// Who may move which ref, once through the door.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<BranchPolicy>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<RepoIdle>,

    /// The legible export (design §9): `git archive` of a ref,
    /// published as a lean workspace by the shipped `flint-sync`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<ExportSpec>,

    /// Fleet levers: clone bundles, and pruning merged agent branches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetSpec>,

    /// Git LFS: large binaries at `<keyPrefix>/lfs/objects/<oid>`,
    /// with the pointer files staying small in git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lfs: Option<LfsSpec>,

    /// `RUST_LOG` for the server pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,

    /// An admin scale-down. A wake request does NOT override it — the
    /// door would otherwise be quietly reversing an operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<RepoLifecycle>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum RepoLifecycle {
    #[default]
    Running,
    Suspended,
}

impl FlintRepoSpec {
    pub fn log_level_or(&self, default: &str) -> String {
        self.log_level.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| default.to_string())
    }
}

/// Who may move what. Rendered into the repository's state directory
/// and read by `pre-receive` AND by the syncer — one document, two
/// enforcers, because hooks can be misconfigured and the writer cannot.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BranchPolicy {
    /// Refs no push moves directly unless `pushers` names the
    /// principal, and that no push DELETES at all. Bare branch names
    /// (`main`, `release/*`); `*` matches any run of characters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected: Vec<String>,

    /// Ref pattern -> principals allowed to push it directly, as
    /// `system:serviceaccount:<ns>:<sa>`. `*` in the list means anyone
    /// the door let through.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pushers: BTreeMap<String, Vec<String>>,

    /// Merge target -> principals allowed to push `refs/for/<target>`.
    /// A protected target with no entry is CLOSED: otherwise
    /// `refs/for/` would be the way around the protection it exists to
    /// serve.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub merge_into: BTreeMap<String, Vec<String>>,

    /// The shape of ref an otherwise unlisted principal may create and
    /// push, e.g. `agent/*`.
    ///
    /// It bounds the NAME, not the owner. The principal a pod presents
    /// is its ServiceAccount and many pods share one, so nothing here
    /// stops one agent pushing another's branch; that needs a per-pod
    /// principal, which the door does not mint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pattern: Option<String>,

    /// Refs a non-fast-forward push may move. Default: none. An agent
    /// rebasing its own branch is the case that wants it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_non_fast_forward: Vec<String>,
}

impl BranchPolicy {
    /// The document the enforcers read.
    ///
    /// Field-by-field on purpose: a field added to either side that
    /// nobody maps here fails to compile, which is the only way two
    /// crates keep one rule.
    pub fn render(&self) -> flint_forge::policy::Policy {
        flint_forge::policy::Policy {
            protected: self.protected.clone(),
            pushers: self.pushers.clone(),
            merge_into: self.merge_into.clone(),
            agent_pattern: self.agent_pattern.clone(),
            allow_non_fast_forward: self.allow_non_fast_forward.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoIdle {
    /// Scale the server to zero after this many seconds with no git
    /// traffic. The door wakes it on the next request and holds that
    /// request while it restores.
    ///
    /// ONE rung, not lite's ladder: the cache is an `emptyDir`, so
    /// there is no PVC to delete and `Hibernated` would name the state
    /// this one already is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_after_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExportSpec {
    /// Refs whose trees are published, e.g. `["main"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    /// Where the exported workspace lives. A separate prefix from the
    /// repository's own — it is a lean workspace with lean's control
    /// objects, not a second directory in the bare repo.
    pub prefix: String,
    /// A floor, not a schedule: the export runs after a batch that
    /// moved an exported ref, no more often than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
}

/// The levers that decide what a thousand agents cost (design §8).
///
/// There is deliberately NO `wipSnapshots` here. An RPO on the agent's
/// working tree is a real want — git's contract is that uncommitted
/// work is not durable — but forge does not own agent pods and injects
/// nothing into them, so a spec field asking for it would be a field
/// the operator silently ignores. It ships as a script the agent's own
/// pod runs (`spdk-csi-driver/docker/forge/wip-snapshot.sh`), documented in
/// `docs/flint-forge-for-agents.md`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundles: Option<BundleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune_agent_branches: Option<PruneSpec>,
}

/// Clone bundles: the storm lever. **Inert unless the AGENT image also
/// sets `transfer.bundleURI=true`** — the client default is false, so a
/// stock git ignores the advertisement entirely.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleSpec {
    #[serde(default)]
    pub enabled: bool,
    /// A floor between cuts. A bundle is a full copy of the
    /// repository, so cutting one per push spends more than the storm
    /// it saves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    /// How long a presigned URL is good for. It is a bearer token for
    /// that object, handed to every agent that asks for a clone, so it
    /// is short and re-signed rather than S3's seven-day maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_ttl_secs: Option<u64>,
}

/// Pruning agent branches, which every clone pays for: a thousand
/// one-commit branches cost 0.54 CPU-s per clone instead of 0.13, and
/// a 74 KB advertisement on every request.
///
/// **Age alone is never the rule.** A branch is taken only when it is
/// already contained in the default branch — so nothing is lost that
/// `main` does not have — AND it has been quiet longer than
/// `afterSecs`, so a merge that just landed does not delete the branch
/// out from under the agent still pushing to it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PruneSpec {
    /// Which refs are eligible, e.g. `agent/*`. Nothing outside it is
    /// ever considered.
    pub pattern: String,
    /// How long a MERGED branch must have been quiet.
    pub after_secs: u64,
    /// How often the pass runs (default daily).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlintRepoStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RepoPhase>,

    /// Where the repository's git server answers, as an absolute URL.
    ///
    /// **This says WHERE. `phase` says WHETHER — read it first.** A
    /// parked repo has no pod, so the name does not resolve; the field
    /// is a stable formula, not a liveness signal. It is an in-cluster
    /// headless Service name, deliberately: git carries the repository
    /// in the request path, so the door routes to the pod and forge
    /// does not spend a ClusterIP per repository (design §2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_endpoint: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// The syncer's holder id, as last observed. A change means a new
    /// server took the repository over — every in-flight push against
    /// the old one failed, which is what a client already saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,

    /// The SYNCER's own phase, as the operator's last `/status` poll
    /// saw it — the Debug spelling of the parsed value, so `Serving`
    /// and `Sweeping` rather than the wire's camelCase, which is what
    /// `lite_gateway::resolve::hub_phase_blocks` compares against.
    ///
    /// `None` is "not observed this pass", NEVER "not serving": a
    /// missed poll must not take a live repository out of rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_phase: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoStats>,

    /// Set ⇒ the server refused to serve and no restart will change it
    /// (a foreign claim, a snapshot naming a pack the bucket lacks, a
    /// git below the floor). The operator reads the syncer's exit code
    /// rather than guessing from a crash loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<RepoCondition>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoStats {
    #[serde(default)]
    pub refs: u64,
    #[serde(default)]
    pub packs: u64,
    #[serde(default)]
    pub snapshot_seq: u64,
    /// Unix seconds of the last acknowledged push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_push_unix: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoCondition {
    pub r#type: String,
    pub status: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}

/// Git LFS — the multi-modal case.
///
/// A pack is delta-compressed and rewritten WHOLE by `repack -a`, so
/// images, audio, video and model weights committed as ordinary blobs
/// make every clone, every repack and every restore pay for them
/// again. With LFS on, the bytes live beside the packs as immutable
/// content-named objects and the client transfers them straight to and
/// from the object store — they never cross the repository server.
///
/// **Nothing collects them.** An LFS object is referenced by a pointer
/// file inside some tree of some commit, and deciding one is
/// unreferenced means walking every reachable tree; an unreferenced
/// object costs storage and nothing else, so forge leaves it rather
/// than shipping a reaper that is right most of the time.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LfsSpec {
    #[serde(default)]
    pub enabled: bool,
    /// How long a transfer URL is good for. Long enough for a
    /// multi-gigabyte object on a slow link; short enough that a leaked
    /// one is not a standing grant, because it is a bearer token for
    /// that object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// The CRD object, for `crdgen` and for the operator's own apply at
/// startup. One artifact, so the chart's bootstrap copy and the
/// compiled-in one cannot drift.
pub fn crd() -> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition
{
    use kube::CustomResourceExt;
    FlintRepo::crd()
}

/// Forge's lifecycle phases — its own, because it has no PVC and
/// therefore no `Hibernated` and no `Reprovisioning`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum RepoPhase {
    /// Children applied, no server pod running yet.
    #[default]
    Pending,
    /// The pod is up but pre-listener: claiming the lease (possibly
    /// waiting out a dead holder) and restoring from the snapshot.
    /// This is PROGRESS, not failure.
    Starting,
    /// Serving clones, fetches and pushes.
    Ready,
    /// `idle.suspendAfterSecs` elapsed: scaled to zero, wakeable by the
    /// door on the next request.
    IdleSuspended,
    /// An ADMIN scaled it down. A wake request does not override it —
    /// the door would otherwise be quietly reversing an operator.
    Suspended,
    /// Refused: another project claims this prefix, the snapshot cannot
    /// be restored, or git is below the floor. See `refused`.
    Failed,
    Terminating,
}

impl RepoPhase {
    /// The projection the gateway's decision runs on.
    ///
    /// Total by construction, and each arm is the phase whose DECISION
    /// is the same — not the one whose name is closest. `Failed` maps
    /// to `Failed` because both mean "do not dial, now or later";
    /// `IdleSuspended` maps to `IdleSuspended` because both mean "arm
    /// the wake and hold the request".
    pub fn as_share_phase(self) -> SharePhase {
        match self {
            RepoPhase::Pending => SharePhase::Pending,
            RepoPhase::Starting => SharePhase::Starting,
            RepoPhase::Ready => SharePhase::Ready,
            RepoPhase::IdleSuspended => SharePhase::IdleSuspended,
            RepoPhase::Suspended => SharePhase::Suspended,
            RepoPhase::Failed => SharePhase::Failed,
            RepoPhase::Terminating => SharePhase::Terminating,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn schema() -> Value {
        let crd = crd();
        serde_json::to_value(&crd).expect("the CRD serializes")["spec"]["versions"][0]["schema"]
            ["openAPIV3Schema"]
            .clone()
    }

    /// The check a cluster would otherwise make at install time, with
    /// an error about junctors — and it takes the WHOLE CRD down, not
    /// the offending field, so every knob would vanish together.
    /// schemars emits `anyOf: [<typed branch>, {null}]` for an
    /// `Option<T>` whose `T` carries its own doc comment, and this CRD
    /// has five such fields.
    #[test]
    fn the_schema_carries_no_junctors() {
        fn walk(v: &Value, path: &str, found: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    for k in ["anyOf", "oneOf", "allOf", "not"] {
                        if m.contains_key(k) {
                            found.push(format!("{path}.{k}"));
                        }
                    }
                    for (k, val) in m {
                        walk(val, &format!("{path}.{k}"), found);
                    }
                }
                Value::Array(a) => {
                    for (i, val) in a.iter().enumerate() {
                        walk(val, &format!("{path}[{i}]"), found);
                    }
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        walk(&schema(), "schema", &mut found);
        assert!(
            found.is_empty(),
            "Kubernetes refuses a structural schema with these, and refuses the WHOLE CRD: {found:?}"
        );
    }

    /// Identity immutability is what stops a repository re-pointing its
    /// prefix into another's subtree — two servers over one prefix is a
    /// state the snapshot CAS cannot arbitrate, because they were never
    /// supposed to meet. Admission is the only place that can refuse it
    /// before any bytes move.
    #[test]
    fn identity_is_immutable_at_admission() {
        let rules = serde_json::to_value(crd()).expect("serializes")["spec"]["versions"][0]
            ["schema"]["openAPIV3Schema"]["properties"]["spec"]["x-kubernetes-validations"]
            .clone();
        let text = rules.to_string();
        for field in ["self.projectId == oldSelf.projectId", "self.bucket == oldSelf.bucket", "self.keyPrefix == oldSelf.keyPrefix"] {
            assert!(text.contains(field), "missing the immutability rule {field}: {text}");
        }
        // A prefix without a trailing slash also matches its siblings:
        // "tenant-a" matches "tenant-agency/".
        assert!(text.contains("endsWith('/')"), "{text}");
        // An export is a lean workspace of its own, never a second
        // writer inside the repository's prefix.
        assert!(text.contains("self.export.prefix != self.keyPrefix"), "{text}");
    }

    /// Every phase this CRD can publish must project onto a share phase
    /// whose DECISION is the same, because the door runs ONE
    /// implementation of that decision. A phase added without a
    /// projection would be a repository the door cannot judge.
    #[test]
    fn every_phase_projects_onto_the_doors_decision() {
        use crate::lite_operator::crd::Phase as S;
        for (repo, want) in [
            (RepoPhase::Pending, S::Pending),
            (RepoPhase::Starting, S::Starting),
            (RepoPhase::Ready, S::Ready),
            (RepoPhase::IdleSuspended, S::IdleSuspended),
            (RepoPhase::Suspended, S::Suspended),
            (RepoPhase::Failed, S::Failed),
            (RepoPhase::Terminating, S::Terminating),
        ] {
            assert_eq!(repo.as_share_phase(), want, "{repo:?}");
        }
    }

    /// The branch policy is one rule in two crates. The conversion is
    /// field by field so a field either side grows and nobody maps
    /// fails to compile — this pins that every field actually carries.
    #[test]
    fn the_branch_policy_renders_every_field() {
        let p = BranchPolicy {
            protected: vec!["main".into()],
            pushers: BTreeMap::from([("main".into(), vec!["bot".into()])]),
            merge_into: BTreeMap::from([("main".into(), vec!["agent".into()])]),
            agent_pattern: Some("agent/*".into()),
            allow_non_fast_forward: vec!["agent/*".into()],
        };
        let r = p.render();
        assert_eq!(r.protected, p.protected);
        assert_eq!(r.pushers, p.pushers);
        assert_eq!(r.merge_into, p.merge_into);
        assert_eq!(r.agent_pattern, p.agent_pattern);
        assert_eq!(r.allow_non_fast_forward, p.allow_non_fast_forward);
    }
}
