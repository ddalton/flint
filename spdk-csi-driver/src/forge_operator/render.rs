//! What a `FlintRepo` becomes: a ConfigMap, a headless Service, and a
//! Deployment of one pod.
//!
//! Slim rather than a trim of `lite_operator::render`. That module is
//! 1,800 lines because a share has a PVC, a hibernate rung, a
//! reprovision path, an auto-expand rule, an optional file API and four
//! Service types; a repository has an `emptyDir`, one rung and one
//! port. What is copied is the SHAPE — the label sets, the ownership,
//! the checksum-annotation trick and its deliberate absence here — not
//! the code.
//!
//! Three decisions the design (§2) makes and this file implements:
//!
//! - **A headless Service, not a ClusterIP.** git carries the
//!   repository in the request path, so the door routes to the pod. At
//!   3,000 repositories a ClusterIP each would take 73 % of a
//!   GKE-default /20, which the lite fleet plan already calls out.
//! - **Requests sized for git**: 25m/32Mi per container, 50m/64Mi for
//!   the pod. An idle `git http-backend` is about 1.5 MB RSS; the hub's
//!   100m/128Mi would reserve twice what the whole pod uses.
//! - **The policy ConfigMap is deliberately NOT in the pod's checksum
//!   annotation.** A ConfigMap volume updates in place, both enforcers
//!   re-read the document, and a branch-policy edit that rolled the
//!   server would drop every in-flight clone to change who may push.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvFromSource,
    EnvVar, EnvVarSource, HTTPGetAction, ObjectFieldSelector, PodSpec, PodTemplateSpec, Probe,
    ResourceRequirements, SecretEnvSource, Service, ServicePort, ServiceSpec, TCPSocketAction,
    SecretKeySelector, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{Resource, ResourceExt};

use super::crd::FlintRepo;

/// Where the bare repositories live inside the pod. `GIT_PROJECT_ROOT`
/// for `http-backend`, and the parent of the syncer's repository.
pub const REPO_ROOT: &str = "/repo";
/// The read-only mount the branch policy arrives on.
pub const POLICY_DIR: &str = "/etc/flint-forge";
/// Where the hooks live in the git image. `core.hooksPath` points every
/// repository at it, so the repository on the shared `emptyDir` carries
/// no binaries and a hook upgrade is an image change.
pub const HOOKS_PATH: &str = "/usr/local/share/flint-forge/hooks";
pub const GIT_PORT: i32 = 8080;
pub const STATUS_PORT: i32 = 9848;
/// The file API's listener, and deliberately NOT `STATUS_PORT`.
///
/// The syncer serves an unauthenticated `/status` — the epoch holder,
/// the ref map, the phase — on the status listener. Serving the file
/// API there would mean admitting the door to that port, which is
/// exactly what `the_policy_admits_the_door_to_git_only` refuses. A
/// third port keeps that test true and makes the separation structural
/// rather than a route table nobody can regress.
pub const FILE_PORT: i32 = 9850;

#[derive(Debug, Clone)]
pub struct RenderDefaults {
    /// The image carrying `flint-forge-syncer`.
    pub syncer_image: String,
    /// The image carrying `flint-forge-gitcgi`, `git http-backend` and
    /// the hook binary. Separate from the syncer's on purpose: it
    /// contains a git (and the runner in front of it), and the syncer's
    /// contains neither.
    pub git_image: String,
    pub log_level: String,
    /// Where the door runs, for the NetworkPolicy that admits only it.
    /// `None` renders no policy — see [`network_policy`], which is
    /// where the consequence of that is written down.
    pub door: Option<DoorSelector>,
    /// Where THIS operator runs. Rendered into the same policy so its
    /// `/status` poll is not denied by the policy it just created.
    /// `None` and every guarded repository goes dark to its operator.
    pub operator: Option<PodPeer>,
    /// Seconds the pod is given to release its lease on the way out. A
    /// clean release lets a successor claim at once instead of waiting
    /// out six quiet polls.
    pub termination_grace_secs: i64,
    /// The `iss` the DOOR verifies, when one is configured. Not used to
    /// render anything — the operator is not in the request path — but
    /// it is what makes `consumers_audit` an exact check rather than a
    /// guess about what a list was meant to express. The chart renders
    /// both this operator and the door from one values file, so the
    /// fact is already there to pass.
    pub door_jwt_issuer: Option<String>,
}

impl Default for RenderDefaults {
    fn default() -> Self {
        RenderDefaults {
            syncer_image: "ghcr.io/chert-us/flint-forge-syncer:latest".into(),
            git_image: "ghcr.io/chert-us/flint-forge-git:latest".into(),
            log_level: "info".into(),
            door: None,
            operator: None,
            termination_grace_secs: 30,
            door_jwt_issuer: None,
        }
    }
}

/// The tag of an image reference, if it names one: `repo/name:tag` →
/// `tag`, `repo/name@sha256:…` → `None` (a digest is not a tag), and a
/// registry port (`host:5000/name:tag`) is not mistaken for one.
pub fn image_tag(image: &str) -> Option<&str> {
    let last = image.rsplit('/').next().unwrap_or(image);
    if last.contains('@') {
        return None;
    }
    last.split_once(':').map(|(_, t)| t).filter(|t| !t.is_empty())
}

/// The two server images must be one build: the hook in the git image
/// is the syncer binary, and a hook from one build talking to a syncer
/// from another over the pod's socket is the drift the
/// published-artifact drill found. The chart derives both from one tag;
/// this is the check on what the operator was actually handed. `Some`
/// is the complaint; a digest-pinned reference (no tag) is not judged.
pub fn server_images_disagree(d: &RenderDefaults) -> Option<String> {
    match (image_tag(&d.syncer_image), image_tag(&d.git_image)) {
        (Some(a), Some(b)) if a != b => Some(format!(
            "the syncer image is tagged {a:?} and the git image {b:?}: the hooks in the git \
             image are the syncer binary, and two builds on one socket is the drift the \
             chart's single `server.tag` exists to prevent"
        )),
        _ => None,
    }
}

/// What `spec.consumers` ACTUALLY AUTHORIZES, said out loud (design D6a
/// and §7.3).
///
/// With an issuer configured, `serviceAccounts: ["*"]` does not mean
/// "any pod in this cluster holding a token for the door's audience".
/// It means that AND "any person that issuer will vouch for" — not a
/// pod, not in this cluster. `aud` cannot narrow it, because Knox mints
/// only a topology-wide audience and this deployment leaves it unset,
/// which is precisely why D6a says `spec.consumers` is the only
/// forge-specific check left in the chain.
///
/// **The operator is TOLD whether an issuer is configured** rather than
/// inferring it from the shape of the list. The chart renders this
/// operator and the door from one values file, so the fact is already
/// there to pass; guessing from "does this repository name a person"
/// would both miss a bare `*` on a cluster that does have an issuer and
/// invent a warning on one that does not.
///
/// **It REFUSES NOTHING, and that is a decision.** Marking the
/// repository `Failed` takes it to zero replicas, so a wrong opinion
/// here would cost a working repository its service — far worse than
/// anything it warns about. The operator can see the situation; it
/// cannot know whether it is intended.
///
/// Returned most-severe first, as `(reason, message)`, security before
/// breakage: an over-grant is worse than a grant that silently does
/// nothing. The prefix comes from the MATCHER rather than being spelled
/// again here — a guard carrying its own copy of the thing it guards is
/// one edit away from warning about a syntax nothing enforces.
pub fn consumers_audit(
    repo: &FlintRepo,
    policy_rendered: bool,
    issuer: Option<&str>,
) -> Vec<(&'static str, String)> {
    use crate::lite_gateway::git::JWT_USER_PREFIX;
    let mut out = Vec::new();
    let Some(c) = repo.spec.consumers.as_ref() else { return out };
    let entries = &c.service_accounts;

    let people = entries.iter().filter(|e| e.starts_with(JWT_USER_PREFIX)).count();
    let wildcard = entries.iter().any(|e| e == "*");
    // Can a PERSON reach this repository at all? Only with an issuer,
    // and then by either spelling.
    let admits_people = issuer.is_some() && (people > 0 || wildcard);

    // MOST SEVERE. The last hop from door to syncer is a plain
    // `X-Remote-User`, trusted behind the NetworkPolicy this operator
    // renders only when it was told where the door runs. Forging that
    // header buys the coarse rights shared by every pod on a
    // ServiceAccount; once people can reach the repository the same
    // forgery is worth exactly a named person's branch rights, and
    // under a wildcard, any person's.
    if admits_people && !policy_rendered {
        out.push((
            "PrincipalsUnenforced",
            format!(
                "person principals can reach this repository (issuer {}), and NO NetworkPolicy \
                 is rendered for it — this operator was not told where the door runs. The \
                 syncer takes X-Remote-User on trust, so anything that can reach the git port \
                 can assert any of them and gets exactly their branch rights. Set \
                 `door.namespace`, and check the CNI enforces NetworkPolicy.",
                issuer.unwrap_or_default()
            ),
        ));
    }

    // D6a proper, and now an exact check rather than a guess about
    // intent.
    if wildcard && issuer.is_some() {
        out.push((
            "WildcardAdmitsPeople",
            format!(
                "`*` is listed while the door verifies tokens from {}. It no longer means \
                 \"any pod we trust\": it admits ANY user of ANY service sharing that issuer. \
                 List the principals, or accept the wildcard knowingly.",
                issuer.unwrap_or_default()
            ),
        ));
    }

    // The mirror, and it is a SILENT breakage rather than an exposure:
    // with no issuer there is no verifier for these entries, so the
    // people named are refused and nothing anywhere says why.
    if people > 0 && issuer.is_none() {
        out.push((
            "PrincipalsWithNoIssuer",
            format!(
                "{people} person principal(s) are listed, but the door has no issuer \
                 configured — nothing can verify a token for them, so every one of them is \
                 refused. Set the door's `jwt.issuer`, or remove the entries."
            ),
        ));
    }

    // A typo that grants nothing and that nothing else will ever
    // report: the matcher requires a non-empty subject, so a bare
    // prefix matches no one and the person it was meant for is simply
    // refused.
    let empty = entries.iter().filter(|e| *e == JWT_USER_PREFIX).count();
    if empty > 0 {
        out.push((
            "EntryMatchesNobody",
            format!(
                "{empty} entr(ies) are a bare `{JWT_USER_PREFIX}` with no subject. These match \
                 nobody, so whoever they were meant for is refused with no other sign."
            ),
        ));
    }
    out
}

/// Where the gateway's pods are, as a NetworkPolicy peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodPeer {
    pub namespace: String,
    pub pod_labels: BTreeMap<String, String>,
}

/// The door, as a NetworkPolicy peer. It was the only peer once, which
/// is why the type was named for it.
pub type DoorSelector = PodPeer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    pub deployment: String,
    pub service: String,
    pub config_map: String,
    pub network_policy: String,
}

pub fn names(repo: &FlintRepo) -> Names {
    let n = repo.name_any();
    Names {
        deployment: format!("forge-{n}"),
        service: format!("forge-{n}"),
        config_map: format!("forge-{n}-policy"),
        network_policy: format!("forge-{n}"),
    }
}

pub fn labels(repo: &FlintRepo) -> BTreeMap<String, String> {
    let name = repo.name_any();
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "flint-forge".into()),
        ("app.kubernetes.io/instance".into(), name.clone()),
        ("app.kubernetes.io/component".into(), "forge".into()),
        ("app.kubernetes.io/managed-by".into(), "flint-forge-operator".into()),
        ("chert.us/repo".into(), name),
        ("chert.us/role".into(), "forge".into()),
    ])
}

/// The Deployment's selector — immutable after creation, so deliberately
/// minimal and repository-scoped.
pub fn selector_labels(repo: &FlintRepo) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "flint-forge".into()),
        ("chert.us/repo".into(), repo.name_any()),
    ])
}

fn owner(repo: &FlintRepo) -> Vec<OwnerReference> {
    repo.controller_owner_ref(&()).into_iter().collect()
}

fn meta(repo: &FlintRepo, name: String) -> ObjectMeta {
    ObjectMeta {
        name: Some(name),
        namespace: repo.namespace(),
        labels: Some(labels(repo)),
        owner_references: Some(owner(repo)),
        ..Default::default()
    }
}

/// The repository's path inside the pod, and inside the URL.
///
/// `<root>/<namespace>/<name>.git` — namespaced on disk because the
/// server is multi-repo-capable from day one (design §2's hedge), so
/// moving to one server per N repositories is an operator change and
/// not a server change. It is also what makes the door's endpoint a
/// complete URL: `http-backend` resolves `$GIT_PROJECT_ROOT$PATH_INFO`,
/// so the path in the URL and the path on disk are the same string.
pub fn repo_path(repo: &FlintRepo) -> String {
    format!("{}/{}", REPO_ROOT, repo_url_path(repo))
}

pub fn repo_url_path(repo: &FlintRepo) -> String {
    format!("{}/{}.git", repo.namespace().unwrap_or_default(), repo.name_any())
}

/// What the operator publishes as `status.gitEndpoint`, and what the
/// door appends a static suffix to.
///
/// An in-cluster headless Service name: it resolves through this
/// cluster's DNS to the pod's own address, and only while the pod is
/// READY — which is exactly right, because the door waits on the CR and
/// dials only what the CR says is serving.
pub fn git_endpoint(repo: &FlintRepo) -> Option<String> {
    let ns = repo.namespace()?;
    let n = names(repo);
    Some(format!(
        "http://{}.{}.svc.cluster.local:{}/{}",
        n.service,
        ns,
        GIT_PORT,
        repo_url_path(repo)
    ))
}

/// The branch policy, in the shape both enforcers parse.
///
/// Rendered through the `flint-forge` crate's own type rather than as
/// hand-built JSON: the enforcers deserialize that type, so anything
/// this can produce is something they can read, and a field either side
/// grows that nobody maps fails to compile.
pub fn policy_document(repo: &FlintRepo) -> String {
    let policy = repo.spec.branches.as_ref().map(|b| b.render()).unwrap_or_default();
    serde_json::to_string_pretty(&policy).unwrap_or_else(|_| "{}".to_string())
}

pub fn config_map(repo: &FlintRepo) -> ConfigMap {
    let n = names(repo);
    ConfigMap {
        metadata: meta(repo, n.config_map),
        data: Some(BTreeMap::from([(
            flint_forge::policy::POLICY_FILE.to_string(),
            policy_document(repo),
        )])),
        ..Default::default()
    }
}

/// A headless Service. `cluster_ip: None` would be "assign me one";
/// the string `"None"` is what makes it headless, and getting that
/// wrong is how a fleet quietly spends 3,000 addresses.
/// Where the file API answers, as an absolute URL.
///
/// No repository in the path, unlike `git_endpoint`: one syncer process
/// serves one repository, so the API needs no repo segment — and
/// omitting it removes a whole class of path-joining question at the
/// door.
pub fn api_endpoint(repo: &FlintRepo) -> Option<String> {
    if repo.spec.file_api.as_ref().map(|f| f.enabled) != Some(true) {
        return None;
    }
    let n = names(repo);
    let ns = repo.namespace().unwrap_or_default();
    Some(format!("http://{}.{}.svc.cluster.local:{}", n.service, ns, FILE_PORT))
}

pub fn service(repo: &FlintRepo) -> Service {
    let n = names(repo);
    Service {
        metadata: meta(repo, n.service),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            selector: Some(selector_labels(repo)),
            ports: Some({
                let mut ports = vec![
                    ServicePort {
                        name: Some("git".to_string()),
                        port: GIT_PORT,
                        target_port: Some(IntOrString::Int(GIT_PORT)),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("status".to_string()),
                        port: STATUS_PORT,
                        target_port: Some(IntOrString::Int(STATUS_PORT)),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                ];
                // Only when it is served. A port on a Service that
                // nothing listens on is an endpoint that answers
                // "connection refused" to anything that finds it.
                if repo.spec.file_api.as_ref().map(|f| f.enabled) == Some(true) {
                    ports.push(ServicePort {
                        name: Some("files".to_string()),
                        port: FILE_PORT,
                        target_port: Some(IntOrString::Int(FILE_PORT)),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    });
                }
                ports
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn quantity(s: &str) -> Quantity {
    Quantity(s.to_string())
}

fn git_sized() -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), quantity("25m")),
            ("memory".to_string(), quantity("32Mi")),
        ])),
        ..Default::default()
    }
}

pub fn deployment(repo: &FlintRepo, d: &RenderDefaults, replicas: i32) -> Deployment {
    let n = names(repo);
    let s = &repo.spec;
    let volumes = vec![
        Volume {
            // The cache. A repository restored from the snapshot on
            // every start, which is what makes `Suspended` and
            // `Hibernated` the same state and why forge has one rung.
            name: "repo".to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        Volume {
            name: "policy".to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: n.config_map.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    let repo_mount = VolumeMount {
        name: "repo".to_string(),
        mount_path: REPO_ROOT.to_string(),
        ..Default::default()
    };
    let policy_mount = VolumeMount {
        name: "policy".to_string(),
        mount_path: POLICY_DIR.to_string(),
        read_only: Some(true),
        ..Default::default()
    };

    let env_from = s.credentials_secret_ref.as_deref().filter(|r| !r.is_empty()).map(|r| {
        vec![EnvFromSource {
            secret_ref: Some(SecretEnvSource { name: r.to_string(), ..Default::default() }),
            ..Default::default()
        }]
    });

    let mut env = vec![
        EnvVar {
            name: "RUST_LOG".into(),
            value: Some(s.log_level_or(&d.log_level)),
            ..Default::default()
        },
        EnvVar { name: "FLINT_FORGE_BUCKET".into(), value: Some(s.bucket.clone()), ..Default::default() },
        EnvVar {
            name: "FLINT_FORGE_PREFIX".into(),
            value: Some(s.key_prefix.trim_end_matches('/').to_string()),
            ..Default::default()
        },
        EnvVar { name: "FLINT_FORGE_REPO".into(), value: Some(repo_path(repo)), ..Default::default() },
        EnvVar {
            name: "FLINT_FORGE_PROJECT_ID".into(),
            value: Some(s.project_id.clone()),
            ..Default::default()
        },
        // The branch policy arrives on a read-only ConfigMap mount, not
        // in the repository's own state directory: the operator cannot
        // write into an `emptyDir`, and a mount updates in place.
        EnvVar {
            name: "FLINT_FORGE_POLICY_DIR".into(),
            value: Some(POLICY_DIR.to_string()),
            ..Default::default()
        },
        // The hooks ship in the git image; the repository is a shared
        // volume that carries no binaries. The path is resolved inside
        // whichever container runs git, which is the one that has them.
        EnvVar {
            name: "FLINT_FORGE_HOOKS_PATH".into(),
            value: Some(HOOKS_PATH.to_string()),
            ..Default::default()
        },
        // Bound on all interfaces so the operator can poll it. The
        // door has no route that can produce this URL — its verb table
        // is closed — so `/status` stays a pod-network surface exactly
        // as the hub's is.
        EnvVar {
            name: "FLINT_FORGE_STATUS_ADDR".into(),
            value: Some(format!("0.0.0.0:{STATUS_PORT}")),
            ..Default::default()
        },
        EnvVar {
            name: "POD_NAME".into(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    field_path: "metadata.name".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    if let Some(b) = s.default_branch.as_deref().filter(|b| !b.is_empty()) {
        env.push(EnvVar {
            name: "FLINT_FORGE_DEFAULT_BRANCH".into(),
            value: Some(b.to_string()),
            ..Default::default()
        });
    }
    if let Some(e) = s.endpoint.as_deref().filter(|e| !e.is_empty()) {
        env.push(EnvVar {
            name: "FLINT_FORGE_ENDPOINT".into(),
            value: Some(e.to_string()),
            ..Default::default()
        });
    }
    // The pack-residue rules. Rendered ONLY when on, so a repository
    // that has not asked for them carries no new environment at all and
    // the syncer takes its own defaults — which are off.
    // EMITTED IN BOTH DIRECTIONS. These default ON in the syncer, so a
    // block that only rendered the `true` case could turn them on and
    // never off: `nameAcceptedSet: false` would emit nothing and the
    // syncer would take its own default, which is now the opposite of
    // what the repository asked for. An ABSENT block still emits
    // nothing, which is what lets the default be a default.
    if let Some(pr) = s.packs.as_ref() {
        env.push(EnvVar {
            name: "FLINT_FORGE_NAME_ACCEPTED_SET".into(),
            value: Some(if pr.name_accepted_set { "1" } else { "0" }.into()),
            ..Default::default()
        });
        env.push(EnvVar {
            name: "FLINT_FORGE_RECLAIM_AT_REST".into(),
            value: Some(if pr.reclaim_at_rest { "1" } else { "0" }.into()),
            ..Default::default()
        });
    }
    // The file API (`docs/plans/forge-file-api-design.md`). Its own
    // port, never the status one — `/status` is unauthenticated there
    // and the door must not be able to reach it.
    if let Some(fa) = s.file_api.as_ref().filter(|f| f.enabled) {
        env.push(EnvVar {
            name: "FLINT_FORGE_FILE_ADDR".into(),
            value: Some(format!("0.0.0.0:{FILE_PORT}")),
            ..Default::default()
        });
        if let Some(b) = fa.branch.as_ref().filter(|b| !b.is_empty()) {
            env.push(EnvVar {
                name: "FLINT_FORGE_FILE_BRANCH".into(),
                value: Some(b.clone()),
                ..Default::default()
            });
        }
        if let Some(mb) = fa.max_mb {
            env.push(EnvVar {
                name: "FLINT_FORGE_FILE_MAX_MB".into(),
                value: Some(mb.to_string()),
                ..Default::default()
            });
        }
        if let Some(sec) = fa.token_secret.as_ref().filter(|t| !t.is_empty()) {
            env.push(EnvVar {
                name: "FLINT_FORGE_FILE_TOKEN".into(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: sec.clone(),
                        key: "token".into(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
    }

    // The legible export (§9). Both halves or neither: the syncer
    // refuses half a configuration rather than defaulting a prefix,
    // because the only plausible default — the repository's own — is
    // the one value that must never be used.
    if let Some(ex) = s.export.as_ref().filter(|e| !e.refs.is_empty() && !e.prefix.is_empty()) {
        env.push(EnvVar {
            name: "FLINT_FORGE_EXPORT_REF".into(),
            value: Some(ex.refs[0].clone()),
            ..Default::default()
        });
        env.push(EnvVar {
            name: "FLINT_FORGE_EXPORT_PREFIX".into(),
            value: Some(ex.prefix.trim_end_matches('/').to_string()),
            ..Default::default()
        });
        if let Some(secs) = ex.every_secs {
            env.push(EnvVar {
                name: "FLINT_FORGE_EXPORT_EVERY_SECS".into(),
                value: Some(secs.to_string()),
                ..Default::default()
            });
        }
    }

    // The fleet levers (§8), both opt-in and both off by default.
    if let Some(f) = s.fleet.as_ref() {
        if let Some(b) = f.bundles.as_ref().filter(|b| b.enabled) {
            env.push(EnvVar {
                name: "FLINT_FORGE_BUNDLES".into(),
                value: Some("true".into()),
                ..Default::default()
            });
            if let Some(v) = b.every_secs {
                env.push(EnvVar {
                    name: "FLINT_FORGE_BUNDLE_EVERY_SECS".into(),
                    value: Some(v.to_string()),
                    ..Default::default()
                });
            }
            if let Some(v) = b.url_ttl_secs {
                env.push(EnvVar {
                    name: "FLINT_FORGE_BUNDLE_URL_TTL_SECS".into(),
                    value: Some(v.to_string()),
                    ..Default::default()
                });
            }
        }
        if let Some(p) = f.prune_agent_branches.as_ref().filter(|p| !p.pattern.is_empty()) {
            env.push(EnvVar {
                name: "FLINT_FORGE_PRUNE_PATTERN".into(),
                value: Some(p.pattern.clone()),
                ..Default::default()
            });
            env.push(EnvVar {
                name: "FLINT_FORGE_PRUNE_AFTER_SECS".into(),
                value: Some(p.after_secs.to_string()),
                ..Default::default()
            });
            if let Some(v) = p.every_secs {
                env.push(EnvVar {
                    name: "FLINT_FORGE_PRUNE_EVERY_SECS".into(),
                    value: Some(v.to_string()),
                    ..Default::default()
                });
            }
        }
    }

    if let Some(l) = s.lfs.as_ref().filter(|l| l.enabled) {
        env.push(EnvVar {
            name: "FLINT_FORGE_LFS".into(),
            value: Some("true".into()),
            ..Default::default()
        });
        if let Some(v) = l.ttl_secs {
            env.push(EnvVar {
                name: "FLINT_FORGE_LFS_TTL_SECS".into(),
                value: Some(v.to_string()),
                ..Default::default()
            });
        }
    }

    // LAST, and overriding: `syncerEnv` is the operator's only tuning
    // surface for the knobs the syncer binary documents but the CRD
    // does not model one-by-one. Applied after everything derived, so a
    // repository can be given a different compaction rule from its
    // neighbour — which is what an A/B whose arms must differ in
    // exactly one variable needs.
    if let Some(extra) = s.syncer_env.as_ref() {
        for (k, v) in extra {
            env.retain(|e| &e.name != k);
            env.push(EnvVar { name: k.clone(), value: Some(v.clone()), ..Default::default() });
        }
    }

    let syncer = Container {
        name: "syncer".to_string(),
        image: Some(d.syncer_image.clone()),
        args: Some(vec![]),
        env: Some(env),
        env_from,
        volume_mounts: Some(vec![repo_mount.clone(), policy_mount]),
        ports: Some({
            let mut p = vec![ContainerPort {
                name: Some("status".to_string()),
                container_port: STATUS_PORT,
                ..Default::default()
            }];
            if s.file_api.as_ref().map(|f| f.enabled) == Some(true) {
                p.push(ContainerPort {
                    name: Some("files".to_string()),
                    container_port: FILE_PORT,
                    ..Default::default()
                });
            }
            p
        }),
        resources: Some(git_sized()),
        // Readiness is "serving", which `/healthz` answers and
        // `/status` describes. A headless Service publishes DNS only
        // for READY pods, so a restoring server is simply not
        // resolvable — and the door, which waits on the CR rather than
        // the pod, holds the request instead of dialling into it.
        readiness_probe: Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/healthz".to_string()),
                port: IntOrString::Int(STATUS_PORT),
                ..Default::default()
            }),
            period_seconds: Some(5),
            ..Default::default()
        }),
        // A restore of a large repository legitimately runs long before
        // it is ready, so liveness must not begin until the startup
        // probe succeeds — and liveness is TCP, because a syncer that
        // is restoring is alive and must not be killed for saying so.
        startup_probe: Some(Probe {
            tcp_socket: Some(TCPSocketAction {
                port: IntOrString::Int(STATUS_PORT),
                ..Default::default()
            }),
            period_seconds: Some(5),
            failure_threshold: Some(60),
            ..Default::default()
        }),
        liveness_probe: Some(Probe {
            tcp_socket: Some(TCPSocketAction {
                port: IntOrString::Int(STATUS_PORT),
                ..Default::default()
            }),
            period_seconds: Some(20),
            failure_threshold: Some(3),
            ..Default::default()
        }),
        // No `preStop`. It runs BEFORE SIGTERM, so a sleep there would
        // only delay the signal the syncer is waiting for. The clean
        // lease release is on SIGTERM itself, inside the syncer, and
        // `terminationGracePeriodSeconds` is the budget for it — a
        // successor then claims at once instead of waiting out six
        // quiet polls.
        ..Default::default()
    };

    let git_http = Container {
        name: "git-http".to_string(),
        image: Some(d.git_image.clone()),
        env: Some(vec![
            EnvVar {
                name: "GIT_PROJECT_ROOT".into(),
                value: Some(REPO_ROOT.to_string()),
                ..Default::default()
            },
            // The syncer owns the pack directory and the refs; git's
            // own auto-gc would be a second, unowned writer of both.
            EnvVar { name: "GIT_HTTP_EXPORT_ALL".into(), value: Some("1".into()), ..Default::default() },
        ]),
        volume_mounts: Some(vec![repo_mount]),
        ports: Some(vec![ContainerPort {
            name: Some("git".to_string()),
            container_port: GIT_PORT,
            ..Default::default()
        }]),
        resources: Some(git_sized()),
        readiness_probe: Some(Probe {
            tcp_socket: Some(TCPSocketAction {
                port: IntOrString::Int(GIT_PORT),
                ..Default::default()
            }),
            period_seconds: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };

    Deployment {
        metadata: meta(repo, n.deployment),
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            selector: LabelSelector {
                match_labels: Some(selector_labels(repo)),
                ..Default::default()
            },
            // Recreate, never RollingUpdate. Two servers for one
            // repository is a state the lease resolves — the straggler
            // fences and exits — but resolving it costs every push in
            // flight, and a rolling update would create it on every
            // image change for no benefit: there is one replica and its
            // local disk is a cache.
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                ..Default::default()
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels(repo)),
                    // Deliberately NO `checksum/policy` annotation. The
                    // policy arrives on a ConfigMap mount that updates
                    // in place and both enforcers re-read it, so
                    // rolling the pod to change who may push would
                    // drop every in-flight clone for nothing.
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![syncer, git_http],
                    volumes: Some(volumes),
                    termination_grace_period_seconds: Some(d.termination_grace_secs),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A `FlintRepo`'s namespace-scoped address, for logs and conflict
/// messages.
pub fn slug(repo: &FlintRepo) -> String {
    format!("{}/{}", repo.namespace().unwrap_or_default(), repo.name_any())
}

/// The bucket subtree a repository owns. Two CRs naming the same one
/// are two servers over one prefix, which the snapshot CAS cannot
/// arbitrate because they were never supposed to meet.
pub fn subtree(repo: &FlintRepo) -> (String, String) {
    (repo.spec.bucket.clone(), repo.spec.key_prefix.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {

    use super::*;

    /// THE FLAG MUST BE TWO-WAY. Both rules default ON in the syncer,
    /// so a render that only emitted the `true` case could turn them on
    /// and never off: `nameAcceptedSet: false` would emit nothing, the
    /// syncer would take its own default, and the repository would get
    /// the OPPOSITE of what it asked for. An absent block must still
    /// emit nothing, because that is what lets a default be a default.
    #[test]
    fn the_pack_rules_can_be_turned_off_and_not_only_on() {
        let d = RenderDefaults::default();
        let mut r = repo();
        let env_of = |dep: &Deployment, name: &str| -> Option<String> {
            dep.spec.as_ref()?.template.spec.as_ref()?.containers.iter()
                .find(|c| c.name == "syncer")?
                .env.as_ref()?.iter().find(|e| e.name == name)?.value.clone()
        };

        // ABSENT: no environment at all, so the syncer's default stands.
        r.spec.packs = None;
        let plain = deployment(&r, &d, 1);
        assert!(
            env_of(&plain, "FLINT_FORGE_NAME_ACCEPTED_SET").is_none(),
            "an absent block must render nothing — a value would override the default"
        );
        assert!(env_of(&plain, "FLINT_FORGE_RECLAIM_AT_REST").is_none());

        // OFF: rendered EXPLICITLY, or it cannot be turned off at all.
        r.spec.packs = Some(crate::forge_operator::crd::PackRules {
            name_accepted_set: false,
            reclaim_at_rest: false,
        });
        let off = deployment(&r, &d, 1);
        assert_eq!(
            env_of(&off, "FLINT_FORGE_NAME_ACCEPTED_SET").as_deref(),
            Some("0"),
            "false must render 0; emitting nothing would leave the default ON"
        );
        assert_eq!(env_of(&off, "FLINT_FORGE_RECLAIM_AT_REST").as_deref(), Some("0"));

        // ON: the control for the pair above.
        r.spec.packs = Some(crate::forge_operator::crd::PackRules {
            name_accepted_set: true,
            reclaim_at_rest: true,
        });
        let on = deployment(&r, &d, 1);
        assert_eq!(env_of(&on, "FLINT_FORGE_NAME_ACCEPTED_SET").as_deref(), Some("1"));
        assert_eq!(env_of(&on, "FLINT_FORGE_RECLAIM_AT_REST").as_deref(), Some("1"));
    }

    /// A tag is the text after the last colon of the last path
    /// component; a digest is not a tag; a registry port is not a tag.
    #[test]
    fn image_tags_are_read_from_references() {
        assert_eq!(image_tag("dilipdalton/flint-forge-git:1.46.0-forge.6"), Some("1.46.0-forge.6"));
        assert_eq!(image_tag("localhost:5000/flint-forge-git:drill-1"), Some("drill-1"));
        assert_eq!(image_tag("localhost:5000/flint-forge-git"), None);
        assert_eq!(image_tag("dilipdalton/flint-forge-git@sha256:abcd"), None);
        assert_eq!(image_tag("flint-forge-git"), None);
    }

    /// Two tags that differ are the complaint; the same tag, or a
    /// digest on either side, is not.
    #[test]
    fn server_images_on_two_tags_are_reported() {
        let mut d = RenderDefaults::default();
        assert!(server_images_disagree(&d).is_none(), "the defaults share :latest");
        d.git_image = "ghcr.io/chert-us/flint-forge-git:1.46.0-forge.5".into();
        d.syncer_image = "ghcr.io/chert-us/flint-forge-syncer:1.46.0-forge.6".into();
        let why = server_images_disagree(&d).expect("two tags");
        assert!(why.contains("forge.5") && why.contains("forge.6"), "{why}");
        d.git_image = "ghcr.io/chert-us/flint-forge-git@sha256:0000".into();
        assert!(server_images_disagree(&d).is_none(), "a digest is not judged");
    }
    use crate::forge_operator::crd::{BranchPolicy, ExportSpec, FlintRepoSpec};

    /// `syncerEnv` is the operator's only tuning surface for the knobs
    /// the syncer documents and the CRD does not model one-by-one, and
    /// it must be applied LAST so it can override a derived value —
    /// otherwise an A/B whose arms differ in exactly one knob cannot be
    /// expressed at all.
    ///
    /// The control is the override arm. A test that only checked a new
    /// name appears would pass just as well if the map were applied
    /// FIRST and then silently overwritten by the derived value, which
    /// is the failure that matters.
    #[test]
    fn syncer_env_is_applied_last_and_overrides() {
        let d = RenderDefaults::default();
        let mut r = repo();
        let plain = deployment(&r, &d, 1);
        let env_of = |dep: &Deployment, name: &str| -> Option<String> {
            dep.spec.as_ref()?.template.spec.as_ref()?.containers.iter()
                .find(|c| c.name == "syncer")?
                .env.as_ref()?.iter().find(|e| e.name == name)?.value.clone()
        };
        assert!(env_of(&plain, "FLINT_FORGE_FOLD_MIN_MIB").is_none(), "not set by default");
        let derived = env_of(&plain, "FLINT_FORGE_BUCKET").expect("bucket is derived");

        r.spec.syncer_env = Some(std::collections::BTreeMap::from([
            ("FLINT_FORGE_FOLD_MIN_MIB".to_string(), "0".to_string()),
            ("FLINT_FORGE_BUCKET".to_string(), "overridden-bucket".to_string()),
        ]));
        let tuned = deployment(&r, &d, 1);
        assert_eq!(env_of(&tuned, "FLINT_FORGE_FOLD_MIN_MIB").as_deref(), Some("0"),
                   "a knob the CRD does not model is settable");
        assert_eq!(env_of(&tuned, "FLINT_FORGE_BUCKET").as_deref(), Some("overridden-bucket"),
                   "and it wins over a DERIVED value — applied last, not first");
        assert_ne!(derived, "overridden-bucket", "the control: the derived value really differed");

        let names: Vec<&String> = tuned.spec.as_ref().unwrap().template.spec.as_ref().unwrap()
            .containers.iter().find(|c| c.name == "syncer").unwrap()
            .env.as_ref().unwrap().iter().map(|e| &e.name).collect();
        let n = names.iter().filter(|x| **x == "FLINT_FORGE_BUCKET").count();
        assert_eq!(n, 1, "overriding REPLACES, it does not duplicate: {names:?}");
    }

    pub(super) fn repo() -> FlintRepo {
        let mut r = FlintRepo::new(
            "proj",
            FlintRepoSpec {
                syncer_env: None,
                project_id: "proj".into(),
                bucket: "bkt".into(),
                key_prefix: "tenant/proj/".into(),
                endpoint: None,
                packs: None,
                credentials_secret_ref: Some("forge-creds".into()),
                default_branch: None,
                consumers: None,
                branches: None,
                idle: None,
                export: None,
                fleet: None,
                lfs: None,
                log_level: None,
                lifecycle: None,
                file_api: None,
            },
        );
        r.metadata.namespace = Some("tenant".into());
        r.metadata.uid = Some("uid-1".into());
        r
    }

    /// A ClusterIP per repository would take 73 % of a GKE-default /20
    /// at the fleet size the design costs. `clusterIP: None` is what
    /// makes it headless, and `None` in the Rust sense would mean
    /// "assign me one" — the two spellings are one character apart and
    /// opposite in effect.
    #[test]
    fn the_service_is_headless() {
        let svc = service(&repo());
        assert_eq!(svc.spec.as_ref().unwrap().cluster_ip.as_deref(), Some("None"));
        let ports = svc.spec.unwrap().ports.unwrap();
        assert_eq!(ports.len(), 2, "git and status");
        assert!(ports.iter().any(|p| p.port == GIT_PORT));
    }

    /// 25m/32Mi per container, 50m/64Mi for the pod — the design's
    /// number. The hub's 100m/128Mi would reserve twice what the whole
    /// pod uses, and at 300 live pods that is the difference between
    /// three nodes and six.
    #[test]
    fn the_pod_is_sized_for_git_and_not_for_a_hub() {
        let dep = deployment(&repo(), &RenderDefaults::default(), 1);
        let pod = dep.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.containers.len(), 2, "the syncer and the git server");
        for c in &pod.containers {
            let req = c.resources.as_ref().unwrap().requests.as_ref().unwrap();
            assert_eq!(req["cpu"].0, "25m", "{}", c.name);
            assert_eq!(req["memory"].0, "32Mi", "{}", c.name);
        }
    }

    /// The cache is an `emptyDir` and there is no PVC anywhere — which
    /// is the whole reason forge has one idle rung instead of lite's
    /// three.
    #[test]
    fn the_cache_is_an_empty_dir_and_there_is_no_claim() {
        let dep = deployment(&repo(), &RenderDefaults::default(), 1);
        let pod = dep.spec.unwrap().template.spec.unwrap();
        let vols = pod.volumes.unwrap();
        let cache = vols.iter().find(|v| v.name == "repo").expect("the cache volume");
        assert!(cache.empty_dir.is_some());
        assert!(
            vols.iter().all(|v| v.persistent_volume_claim.is_none()),
            "a repository has no PVC; its bucket is the only durable copy"
        );
    }

    /// Rolling the pod to change who may push would drop every clone in
    /// flight, and would be pointless: the ConfigMap mount updates in
    /// place and both enforcers re-read the document. So the policy
    /// must NOT appear in the pod template's annotations, which is the
    /// opposite of what lite does with its config.
    #[test]
    fn a_policy_change_does_not_roll_the_server() {
        let mut a = repo();
        a.spec.branches = Some(BranchPolicy {
            protected: vec!["main".into()],
            ..Default::default()
        });
        let mut b = repo();
        b.spec.branches = Some(BranchPolicy {
            protected: vec!["main".into(), "release/*".into()],
            ..Default::default()
        });
        let d = RenderDefaults::default();
        let (da, db) = (deployment(&a, &d, 1), deployment(&b, &d, 1));
        assert_eq!(
            da.spec.unwrap().template, db.spec.unwrap().template,
            "the pod template must not depend on the branch policy"
        );
        assert_ne!(
            config_map(&a).data, config_map(&b).data,
            "the control: the ConfigMap does change"
        );
    }

    /// The path in the URL and the path on disk are the same string,
    /// because `http-backend` resolves `$GIT_PROJECT_ROOT$PATH_INFO`.
    /// A test pins it: they are constructed in two places and a drift
    /// would 404 every request with no other symptom.
    #[test]
    fn the_url_path_and_the_disk_path_agree() {
        let r = repo();
        let endpoint = git_endpoint(&r).unwrap();
        assert!(endpoint.ends_with("/tenant/proj.git"), "{endpoint}");
        assert_eq!(repo_path(&r), "/repo/tenant/proj.git");
        assert!(endpoint.contains("forge-proj.tenant.svc.cluster.local:8080"), "{endpoint}");

        let dep = deployment(&r, &RenderDefaults::default(), 1);
        let pod = dep.spec.unwrap().template.spec.unwrap();
        let syncer = pod.containers.iter().find(|c| c.name == "syncer").unwrap();
        let env: BTreeMap<_, _> = syncer
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect();
        assert_eq!(env["FLINT_FORGE_REPO"], repo_path(&r));
        let git = pod.containers.iter().find(|c| c.name == "git-http").unwrap();
        let genv: BTreeMap<_, _> = git
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect();
        assert_eq!(genv["GIT_PROJECT_ROOT"], REPO_ROOT);
    }

    /// The syncer's prefix has no trailing slash, because
    /// `ForgeConfig::new` trims one and the CRD requires one — the two
    /// conventions meet here and a mismatch would put every key under
    /// `<prefix>//git/`.
    #[test]
    fn the_prefix_reaches_the_syncer_without_its_trailing_slash() {
        let dep = deployment(&repo(), &RenderDefaults::default(), 1);
        let pod = dep.spec.unwrap().template.spec.unwrap();
        let syncer = pod.containers.iter().find(|c| c.name == "syncer").unwrap();
        let prefix = syncer
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "FLINT_FORGE_PREFIX")
            .and_then(|e| e.value.clone())
            .unwrap();
        assert_eq!(prefix, "tenant/proj");
    }

    /// Recreate, not RollingUpdate: two servers for one repository is a
    /// state the lease resolves, but resolving it costs every push in
    /// flight, and there is nothing to gain — one replica, and its disk
    /// is a cache.
    #[test]
    fn the_strategy_is_recreate() {
        let dep = deployment(&repo(), &RenderDefaults::default(), 1);
        assert_eq!(
            dep.spec.unwrap().strategy.unwrap().type_.as_deref(),
            Some("Recreate")
        );
    }

    /// The rendered document is the one the enforcers parse. Round-trip
    /// it through their type rather than asserting on the text.
    #[test]
    fn the_rendered_policy_is_what_the_enforcers_read() {
        let mut r = repo();
        r.spec.branches = Some(BranchPolicy {
            protected: vec!["main".into()],
            merge_into: BTreeMap::from([("main".into(), vec!["agent-runner".into()])]),
            agent_pattern: Some("agent/*".into()),
            ..Default::default()
        });
        let doc = policy_document(&r);
        let parsed: flint_forge::policy::Policy =
            serde_json::from_str(&doc).expect("the enforcers must be able to read this");
        assert!(parsed.is_protected("refs/heads/main"));
        assert_eq!(
            parsed.judge("agent-runner", "refs/for/main", "abc"),
            flint_forge::policy::Verdict::Allow
        );
        assert!(matches!(
            parsed.judge("someone", "refs/heads/other", "abc"),
            flint_forge::policy::Verdict::Refuse(_)
        ));
    }

    /// The policy that makes `X-Remote-User` mean anything: only the
    /// door reaches the git port. The status port is deliberately not
    /// in the rule — the operator polls it, and admitting the door to
    /// it would be admitting the door to a document the door has no
    /// route that can produce.
    #[test]
    fn the_network_policy_admits_only_the_door_to_the_git_port() {
        let door = DoorSelector {
            namespace: "flint-system".into(),
            pod_labels: BTreeMap::from([(
                "app.kubernetes.io/name".into(),
                "flint-hub-gateway".into(),
            )]),
        };
        let np = network_policy(&repo(), &door, None);
        let spec = np.spec.unwrap();
        assert_eq!(spec.policy_types.as_deref(), Some(&["Ingress".to_string()][..]));
        assert_eq!(
            spec.pod_selector.unwrap().match_labels.unwrap()["chert.us/repo"],
            "proj",
            "the policy must select this repository's pod and no other"
        );
        let rule = &spec.ingress.unwrap()[0];
        let ports = rule.ports.as_ref().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, Some(IntOrString::Int(GIT_PORT)));
        let peer = &rule.from.as_ref().unwrap()[0];
        assert_eq!(
            peer.namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap()
                ["kubernetes.io/metadata.name"],
            "flint-system"
        );
        assert!(peer.pod_selector.is_some(), "a namespace alone admits every pod in it");
    }

    /// THE DEFECT THIS PINS, found on a real cluster and not by any
    /// test: a NetworkPolicy that selects a pod is default-deny for
    /// every port it does not name. Naming only the git port denied the
    /// operator's own `/status` poll, so the repository sat in
    /// `Starting` forever — with the pod 2/2 Ready, the syncer
    /// reporting `serving`, and nothing logging an error anywhere.
    ///
    /// The test that was here asserted `ports.len() == 1, "the git port
    /// only"`. It passed. It was ENCODING the bug, which is why this
    /// one asserts on the operator's port being reachable rather than
    /// on how many rules there are.
    #[test]
    fn the_policy_does_not_blind_the_operator_that_wrote_it() {
        let door = DoorSelector {
            namespace: "flint-system".into(),
            pod_labels: BTreeMap::from([(
                "app.kubernetes.io/name".into(),
                "flint-forge-door".into(),
            )]),
        };
        let operator = PodPeer {
            namespace: "flint-system".into(),
            pod_labels: BTreeMap::from([(
                "app.kubernetes.io/name".into(),
                "flint-forge".into(),
            )]),
        };
        let np = network_policy(&repo(), &door, Some(&operator));
        let ingress = np.spec.unwrap().ingress.unwrap();

        let admits = |port: i32, label: &str| {
            ingress.iter().any(|r| {
                let port_ok = r
                    .ports
                    .as_ref()
                    .is_some_and(|ps| ps.iter().any(|p| p.port == Some(IntOrString::Int(port))));
                let peer_ok = r.from.as_ref().is_some_and(|fs| {
                    fs.iter().any(|f| {
                        f.pod_selector
                            .as_ref()
                            .and_then(|s| s.match_labels.as_ref())
                            .is_some_and(|m| m.values().any(|v| v == label))
                    })
                });
                port_ok && peer_ok
            })
        };

        assert!(
            admits(STATUS_PORT, "flint-forge"),
            "the operator must reach /status through the policy it wrote, or it goes blind \
             and every repository stays in Starting"
        );
        assert!(admits(GIT_PORT, "flint-forge-door"), "the door must still reach git");
        assert!(
            !admits(GIT_PORT, "flint-forge"),
            "admitting the operator to the STATUS port must not also admit it to git"
        );
        assert!(
            !admits(STATUS_PORT, "flint-forge-door"),
            "the door has no business on the status port — /status is exactly what the \
             gateway design refuses to proxy"
        );
    }

    /// The hooks ship in the git image, and the repository — a shared
    /// volume — carries no binaries. The syncer has to be told, because
    /// it is the process that creates the repository.
    #[test]
    fn the_syncer_is_told_where_the_hooks_live() {
        let dep = deployment(&repo(), &RenderDefaults::default(), 1);
        let pod = dep.spec.unwrap().template.spec.unwrap();
        let syncer = pod.containers.iter().find(|c| c.name == "syncer").unwrap();
        let hooks = syncer
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "FLINT_FORGE_HOOKS_PATH")
            .and_then(|e| e.value.clone())
            .expect("the hooks path");
        assert_eq!(hooks, HOOKS_PATH);
    }

    /// The export reaches the syncer as a pair or not at all, and the
    /// prefix loses its trailing slash on the way — the CRD requires
    /// one and `ForgeConfig` trims one, and this is where the two
    /// conventions meet.
    #[test]
    fn the_export_reaches_the_syncer_as_a_pair() {
        let plain = deployment(&repo(), &RenderDefaults::default(), 1);
        let env_of = |d: &Deployment| -> BTreeMap<String, String> {
            d.spec
                .clone()
                .unwrap()
                .template
                .spec
                .unwrap()
                .containers
                .iter()
                .find(|c| c.name == "syncer")
                .unwrap()
                .env
                .as_ref()
                .unwrap()
                .iter()
                .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
                .collect()
        };
        assert!(!env_of(&plain).contains_key("FLINT_FORGE_EXPORT_REF"), "off by default");

        let mut r = repo();
        r.spec.export = Some(ExportSpec {
            refs: vec!["main".into()],
            prefix: "tenant/proj-export/".into(),
            every_secs: Some(120),
        });
        let env = env_of(&deployment(&r, &RenderDefaults::default(), 1));
        assert_eq!(env["FLINT_FORGE_EXPORT_REF"], "main");
        assert_eq!(env["FLINT_FORGE_EXPORT_PREFIX"], "tenant/proj-export");
        assert_eq!(env["FLINT_FORGE_EXPORT_EVERY_SECS"], "120");
    }

    /// Both fleet levers are off unless asked for, and both reach the
    /// syncer whole. A bundle spec with `enabled: false` must render
    /// NOTHING — arming it costs a full copy of the repository per cut.
    #[test]
    fn the_fleet_levers_are_off_unless_asked_for() {
        use crate::forge_operator::crd::{BundleSpec, FleetSpec, PruneSpec};
        let env_of = |r: &FlintRepo| -> BTreeMap<String, String> {
            deployment(r, &RenderDefaults::default(), 1)
                .spec
                .unwrap()
                .template
                .spec
                .unwrap()
                .containers
                .iter()
                .find(|c| c.name == "syncer")
                .unwrap()
                .env
                .as_ref()
                .unwrap()
                .iter()
                .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
                .collect()
        };
        assert!(!env_of(&repo()).contains_key("FLINT_FORGE_BUNDLES"), "off by default");

        let mut off = repo();
        off.spec.fleet = Some(FleetSpec {
            bundles: Some(BundleSpec { enabled: false, every_secs: Some(60), url_ttl_secs: None }),
            prune_agent_branches: None,
        });
        assert!(
            !env_of(&off).contains_key("FLINT_FORGE_BUNDLES"),
            "enabled:false must render nothing, cadence or no cadence"
        );

        let mut on = repo();
        on.spec.fleet = Some(FleetSpec {
            bundles: Some(BundleSpec {
                enabled: true,
                every_secs: Some(1800),
                url_ttl_secs: Some(7200),
            }),
            prune_agent_branches: Some(PruneSpec {
                pattern: "agent/*".into(),
                after_secs: 604_800,
                every_secs: None,
            }),
        });
        let env = env_of(&on);
        assert_eq!(env["FLINT_FORGE_BUNDLES"], "true");
        assert_eq!(env["FLINT_FORGE_BUNDLE_EVERY_SECS"], "1800");
        assert_eq!(env["FLINT_FORGE_BUNDLE_URL_TTL_SECS"], "7200");
        assert_eq!(env["FLINT_FORGE_PRUNE_PATTERN"], "agent/*");
        assert_eq!(
            env["FLINT_FORGE_PRUNE_AFTER_SECS"], "604800",
            "the pattern and the TTL travel together; the syncer refuses half of them"
        );
    }

    /// An export claims a second subtree, and the arbitration has to
    /// see it — the CRD's own CEL rule can only compare a CR against
    /// itself.
    #[test]
    fn an_export_prefix_is_part_of_what_a_repository_claims() {
        let mut r = repo();
        r.spec.export = Some(ExportSpec {
            refs: vec!["main".into()],
            prefix: "tenant/proj-export/".into(),
            every_secs: None,
        });
        let claims = crate::forge_operator::reconcile::claims(&r);
        assert_eq!(claims.len(), 2);
        assert!(claims.iter().any(|c| c.prefix == "tenant/proj-export" && c.kind == "export"));
    }

    // ── D6a: what `spec.consumers` actually authorizes ──────────────
    //
    // Every assertion is paired with a control in the SAME test,
    // because "no finding" is also what a guard that inspects nothing
    // produces — and this guard's entire job is to notice something the
    // CR does not say.

    const ISS: &str = "https://knox.example/token";

    fn consumers(list: &[&str]) -> FlintRepo {
        let mut r = repo();
        r.spec.consumers = Some(crate::s3csi::policy::Consumers {
            service_accounts: list.iter().map(|s| s.to_string()).collect(),
        });
        r
    }
    fn reasons(r: &FlintRepo, policy: bool, iss: Option<&str>) -> Vec<&'static str> {
        consumers_audit(r, policy, iss).into_iter().map(|(x, _)| x).collect()
    }

    /// A DOOR WITH NO ISSUER IS SILENT. The ordinary forge deployment
    /// has the JWT path off, and a guard that put a standing condition
    /// on every repository there would be noise nobody reads by the
    /// second week.
    #[test]
    fn nothing_is_reported_when_the_door_verifies_no_issuer() {
        for list in [&["*"][..], &["agent-runner"][..], &[][..]] {
            assert!(reasons(&consumers(list), true, None).is_empty(), "{list:?}");
            assert!(reasons(&consumers(list), false, None).is_empty(), "{list:?} no policy");
        }
        assert!(consumers_audit(&repo(), false, None).is_empty(), "no consumers list at all");

        // THE CONTROL: the SAME wildcard, with an issuer configured, IS
        // reported. Without it every assertion above is equally true of
        // a function that returns an empty vector.
        assert!(
            reasons(&consumers(&["*"]), true, Some(ISS)).contains(&"WildcardAdmitsPeople"),
            "the guard reports nothing at all"
        );
    }

    /// D6a proper, and the reason the operator is TOLD rather than
    /// guessing: a bare `*` — naming no person, so indistinguishable by
    /// shape from an ordinary list — is exactly the case that must be
    /// reported once an issuer exists.
    #[test]
    fn a_wildcard_is_reported_only_when_an_issuer_exists() {
        let wild = consumers(&["*"]);
        assert_eq!(reasons(&wild, true, Some(ISS)), vec!["WildcardAdmitsPeople"]);
        // Control, moving ONE dimension: the same list, no issuer.
        assert!(reasons(&wild, true, None).is_empty());
        // Control, moving the other: an issuer, no wildcard.
        let named = consumers(&["agent-runner", "jwt:user:alice@example.com"]);
        assert!(!reasons(&named, true, Some(ISS)).contains(&"WildcardAdmitsPeople"));

        // Actionable: the message must name the issuer whose users are
        // now admitted, since that is the fact the CR does not carry.
        let (_, msg) = consumers_audit(&wild, true, Some(ISS)).remove(0);
        assert!(msg.contains(ISS), "{msg}");
        assert!(msg.contains('*'), "{msg}");
    }

    /// THE MIRROR, and it is a silent breakage rather than an exposure:
    /// people listed with no issuer to verify them are refused by
    /// something that logs nothing.
    #[test]
    fn people_listed_with_no_issuer_are_reported_as_refused() {
        let named = consumers(&["agent-runner", "jwt:user:alice@example.com"]);
        assert_eq!(reasons(&named, true, None), vec!["PrincipalsWithNoIssuer"]);
        // Control: the same list once an issuer exists.
        assert!(!reasons(&named, true, Some(ISS)).contains(&"PrincipalsWithNoIssuer"));
        // Control: a list with no people is not reported either way.
        assert!(reasons(&consumers(&["agent-runner"]), true, None).is_empty());
    }

    /// §7.3. The last hop is a plain `X-Remote-User` behind a policy
    /// this operator renders only when told where the door runs. People
    /// reaching the repository does not change the forgery — it changes
    /// what the forgery is WORTH, from rights shared by every pod on a
    /// ServiceAccount to exactly one person's, and under a wildcard, any
    /// person's.
    #[test]
    fn an_unenforced_last_hop_is_reported_once_people_can_reach_it() {
        // By either spelling.
        for list in [&["jwt:user:alice@example.com"][..], &["*"][..]] {
            assert_eq!(
                reasons(&consumers(list), false, Some(ISS))[0],
                "PrincipalsUnenforced",
                "{list:?} with no policy"
            );
            // Control: the SAME repository with a policy rendered.
            assert!(
                !reasons(&consumers(list), true, Some(ISS)).contains(&"PrincipalsUnenforced"),
                "{list:?} with a policy"
            );
        }
        // Control: no issuer means no person can reach it, so an
        // unrendered policy is the ordinary posture and not this
        // finding.
        assert!(reasons(&consumers(&["*"]), false, None).is_empty());

        // SECURITY BEFORE BREAKAGE. An over-grant outranks a list that
        // silently grants nothing, and an unenforced hop outranks both.
        let everything = consumers(&["*", "jwt:user:alice@example.com", "jwt:user:"]);
        assert_eq!(
            reasons(&everything, false, Some(ISS)),
            vec!["PrincipalsUnenforced", "WildcardAdmitsPeople", "EntryMatchesNobody"]
        );
    }

    /// The guard and the matcher must agree on what a person entry IS.
    /// A guard carrying its own copy of the prefix is one edit away
    /// from warning about a syntax nothing enforces — so it imports the
    /// matcher's, and this pins the two together.
    #[test]
    fn the_guard_keys_on_the_matchers_own_prefix() {
        let entry = format!("{}alice@example.com", crate::lite_gateway::git::JWT_USER_PREFIX);
        let r = consumers(&["agent-runner", &entry]);
        // The guard sees a person here…
        assert_eq!(reasons(&r, true, None), vec!["PrincipalsWithNoIssuer"]);
        // …and so does the matcher, for the same entry.
        assert!(crate::lite_gateway::git::consumer_allows(
            r.spec.consumers.as_ref(),
            "tenant",
            &crate::s3csi::broker::Identity::person("alice@example.com")
        ));
    }

    /// A bare prefix matches nobody — the matcher requires a non-empty
    /// subject — so it is a typo that grants nothing and that nothing
    /// else in the system will ever mention.
    #[test]
    fn a_bare_prefix_is_reported_as_matching_nobody() {
        let typo = consumers(&["agent-runner", "jwt:user:", "jwt:user:alice@example.com"]);
        assert!(reasons(&typo, true, Some(ISS)).contains(&"EntryMatchesNobody"));
        // Control: a prefix WITH a subject is not a typo.
        let fine = consumers(&["agent-runner", "jwt:user:alice@example.com"]);
        assert!(!reasons(&fine, true, Some(ISS)).contains(&"EntryMatchesNobody"));

        // And the claim it rests on, asserted against the MATCHER
        // rather than restated, so the two cannot drift into
        // disagreeing about the same string.
        assert!(!crate::lite_gateway::git::consumer_allows(
            typo.spec.consumers.as_ref(),
            "tenant",
            &crate::s3csi::broker::Identity::person("")
        ));
    }


}

/// Admit only the door to the git port.
///
/// **This is the trust boundary that makes `X-Remote-User` mean
/// anything.** The header is set by the door from a verified
/// `TokenReview`, and the door builds its upstream headers from an
/// allowlist so no caller can smuggle one past it — but anything that
/// can reach port 8080 directly can set the header itself, and
/// `pre-receive` and the syncer would believe it.
///
/// So: a headless Service with no external address, and this. `None`
/// renders nothing, and that is a real posture rather than an
/// oversight — a NetworkPolicy naming the wrong door breaks every
/// clone, and a cluster whose CNI does not enforce NetworkPolicy gets
/// no protection from one either. Where it is not rendered, **reaching
/// the port is the authorization**, and the design says so (§6).
///
/// The status port is deliberately NOT admitted here: the operator
/// polls it, and it is on the same allowlist by way of the operator's
/// own namespace being the one the chart names.
pub fn network_policy(
    repo: &FlintRepo,
    door: &PodPeer,
    operator: Option<&PodPeer>,
) -> k8s_openapi::api::networking::v1::NetworkPolicy {
    use k8s_openapi::api::networking::v1::{
        NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort,
        NetworkPolicySpec,
    };
    let n = names(repo);
    let as_peer = |p: &PodPeer| NetworkPolicyPeer {
        namespace_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "kubernetes.io/metadata.name".to_string(),
                p.namespace.clone(),
            )])),
            ..Default::default()
        }),
        pod_selector: Some(LabelSelector {
            match_labels: Some(p.pod_labels.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let rule = |peer: NetworkPolicyPeer, port: i32| NetworkPolicyIngressRule {
        from: Some(vec![peer]),
        ports: Some(vec![NetworkPolicyPort {
            port: Some(IntOrString::Int(port)),
            protocol: Some("TCP".to_string()),
            end_port: None,
        }]),
    };

    let mut ingress = vec![rule(as_peer(door), GIT_PORT)];

    // The file API's own port, and only when it is served. The door
    // reaches this and the status port stays closed to it — which is
    // the whole reason the file API does not share the status
    // listener. `the_policy_admits_the_door_to_git_only` still holds
    // for STATUS_PORT and must keep holding.
    if repo.spec.file_api.as_ref().map(|f| f.enabled) == Some(true) {
        ingress.push(rule(as_peer(door), FILE_PORT));
    }

    // THE STATUS PORT IS NOT OPTIONAL, and leaving it out is how this
    // policy shipped broken. A NetworkPolicy that selects a pod is
    // default-deny for every port it does not name, so admitting only
    // the git port silently blocked the operator's own `/status` poll
    // — and that document is the sole input to the phase and to the
    // idle ladder. The repository stayed `Starting` forever, nothing
    // logged an error, and the pod was 2/2 Ready the whole time.
    //
    // So: whenever a policy is rendered at all, the operator must be
    // admitted to the status port, or the operator loses sight of
    // every repository the moment it starts guarding them.
    if let Some(op) = operator {
        ingress.push(rule(as_peer(op), STATUS_PORT));
    }

    NetworkPolicy {
        metadata: meta(repo, n.network_policy),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(selector_labels(repo)),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress: Some(ingress),
            egress: None,
        }),
    }
}

#[cfg(test)]
mod file_api_render_tests {
    use super::*;

    fn repo_with_file_api(enabled: bool) -> FlintRepo {
        let mut r = super::tests::repo();
        r.spec.file_api = Some(crate::forge_operator::crd::FileApiSpec {
            enabled,
            branch: Some("agents".into()),
            max_mb: Some(4),
            token_secret: Some("forge-file-token".into()),
        });
        r
    }

    /// Off by default, and off means nothing is rendered: no port, no
    /// endpoint, no environment. A repository nobody browses opens no
    /// second door.
    #[test]
    fn the_file_api_is_off_unless_asked_for() {
        let r = super::tests::repo();
        assert!(api_endpoint(&r).is_none());
        let svc = service(&r);
        let names: Vec<String> =
            svc.spec.unwrap().ports.unwrap().iter().filter_map(|p| p.name.clone()).collect();
        assert!(!names.contains(&"files".to_string()), "{names:?}");

        let off = repo_with_file_api(false);
        assert!(api_endpoint(&off).is_none(), "enabled:false is not enabled");
    }

    /// The endpoint says WHERE. It carries no repository segment —
    /// one syncer serves one repository — and it names the file port,
    /// never the status one.
    #[test]
    fn the_endpoint_names_the_file_port_and_no_repository_path() {
        let r = repo_with_file_api(true);
        let ep = api_endpoint(&r).expect("published");
        assert!(ep.ends_with(&format!(":{FILE_PORT}")), "{ep}");
        assert!(!ep.contains(".git"), "no repository in the path: {ep}");
        assert!(!ep.contains(&STATUS_PORT.to_string()), "{ep}");

        let names: Vec<String> = service(&r)
            .spec
            .unwrap()
            .ports
            .unwrap()
            .iter()
            .filter_map(|p| p.name.clone())
            .collect();
        assert!(names.contains(&"files".to_string()), "{names:?}");
    }

    /// The environment the syncer reads, and the Secret reference that
    /// keeps the bearer out of the pod spec.
    #[test]
    fn the_syncer_is_told_where_to_serve_and_on_which_branch() {
        let r = repo_with_file_api(true);
        let d = deployment(&r, &RenderDefaults::default(), 1);
        let syncer = d
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .containers
            .into_iter()
            .find(|c| c.name == "syncer")
            .expect("the syncer container");
        let env = syncer.env.unwrap_or_default();
        let get = |k: &str| env.iter().find(|e| e.name == k).cloned();

        assert_eq!(
            get("FLINT_FORGE_FILE_ADDR").and_then(|e| e.value),
            Some(format!("0.0.0.0:{FILE_PORT}"))
        );
        assert_eq!(get("FLINT_FORGE_FILE_BRANCH").and_then(|e| e.value), Some("agents".into()));
        assert_eq!(get("FLINT_FORGE_FILE_MAX_MB").and_then(|e| e.value), Some("4".into()));

        let tok = get("FLINT_FORGE_FILE_TOKEN").expect("the bearer");
        assert!(tok.value.is_none(), "the token must never be a literal in the pod spec");
        let sel = tok.value_from.unwrap().secret_key_ref.unwrap();
        assert_eq!(sel.name, "forge-file-token");
        assert_eq!(sel.key, "token");

        let ports: Vec<String> =
            syncer.ports.unwrap_or_default().iter().filter_map(|p| p.name.clone()).collect();
        assert!(ports.contains(&"files".to_string()), "{ports:?}");
    }

    /// The door reaches the file port and STILL never the status port.
    /// That second half is the reason the file API has its own port at
    /// all, so it is asserted here as well as in the policy's own test.
    #[test]
    fn the_policy_opens_the_file_port_and_keeps_status_shut() {
        let r = repo_with_file_api(true);
        let door = PodPeer {
            namespace: "flint-system".into(),
            pod_labels: BTreeMap::from([("app".to_string(), "flint-forge-door".to_string())]),
        };
        let op = PodPeer {
            namespace: "flint-system".into(),
            pod_labels: BTreeMap::from([("app".to_string(), "flint-forge".to_string())]),
        };
        let np = network_policy(&r, &door, Some(&op));
        let rules = np.spec.unwrap().ingress.unwrap_or_default();
        let admits = |port: i32, app: &str| {
            rules.iter().any(|r| {
                r.ports.as_ref().is_some_and(|ps| {
                    ps.iter().any(|p| p.port == Some(IntOrString::Int(port)))
                }) && r.from.as_ref().is_some_and(|f| {
                    f.iter().any(|peer| {
                        peer.pod_selector
                            .as_ref()
                            .and_then(|s| s.match_labels.as_ref())
                            .and_then(|l| l.get("app"))
                            .map(|v| v == app)
                            .unwrap_or(false)
                    })
                })
            })
        };
        assert!(admits(FILE_PORT, "flint-forge-door"), "the door must reach the file API");
        assert!(admits(GIT_PORT, "flint-forge-door"), "and still git");
        assert!(
            !admits(STATUS_PORT, "flint-forge-door"),
            "the door has no business on the status port — that is why the file API has its own"
        );
        assert!(admits(STATUS_PORT, "flint-forge"), "the operator still polls status");

        // And with the API off, the port is not opened at all.
        let off = super::tests::repo();
        let np = network_policy(&off, &door, Some(&op));
        let rules = np.spec.unwrap().ingress.unwrap_or_default();
        assert!(
            !rules.iter().any(|r| r.ports.as_ref().is_some_and(|ps| ps
                .iter()
                .any(|p| p.port == Some(IntOrString::Int(FILE_PORT))))),
            "a port nobody serves must not be opened"
        );
    }
}
