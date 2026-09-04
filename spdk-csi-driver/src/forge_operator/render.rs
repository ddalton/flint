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
    Volume, VolumeMount,
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

#[derive(Debug, Clone)]
pub struct RenderDefaults {
    /// The image carrying `flint-forge-syncer`.
    pub syncer_image: String,
    /// The image carrying nginx, fcgiwrap, `git http-backend` and the
    /// hook binary. Separate from the syncer's on purpose: it contains
    /// a web server and a git, and the syncer's contains neither.
    pub git_image: String,
    pub log_level: String,
    /// Where the door runs, for the NetworkPolicy that admits only it.
    /// `None` renders no policy — see [`network_policy`], which is
    /// where the consequence of that is written down.
    pub door: Option<DoorSelector>,
    /// Seconds the pod is given to release its lease on the way out. A
    /// clean release lets a successor claim at once instead of waiting
    /// out six quiet polls.
    pub termination_grace_secs: i64,
}

impl Default for RenderDefaults {
    fn default() -> Self {
        RenderDefaults {
            syncer_image: "ghcr.io/chert-us/flint-forge-syncer:latest".into(),
            git_image: "ghcr.io/chert-us/flint-forge-git:latest".into(),
            log_level: "info".into(),
            door: None,
            termination_grace_secs: 30,
        }
    }
}

/// Where the gateway's pods are, as a NetworkPolicy peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoorSelector {
    pub namespace: String,
    pub pod_labels: BTreeMap<String, String>,
}

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
pub fn service(repo: &FlintRepo) -> Service {
    let n = names(repo);
    Service {
        metadata: meta(repo, n.service),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            selector: Some(selector_labels(repo)),
            ports: Some(vec![
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
            ]),
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

    let syncer = Container {
        name: "syncer".to_string(),
        image: Some(d.syncer_image.clone()),
        args: Some(vec![]),
        env: Some(env),
        env_from,
        volume_mounts: Some(vec![repo_mount.clone(), policy_mount]),
        ports: Some(vec![ContainerPort {
            name: Some("status".to_string()),
            container_port: STATUS_PORT,
            ..Default::default()
        }]),
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
    use crate::forge_operator::crd::{BranchPolicy, ExportSpec, FlintRepoSpec};

    fn repo() -> FlintRepo {
        let mut r = FlintRepo::new(
            "proj",
            FlintRepoSpec {
                project_id: "proj".into(),
                bucket: "bkt".into(),
                key_prefix: "tenant/proj/".into(),
                endpoint: None,
                credentials_secret_ref: Some("forge-creds".into()),
                default_branch: None,
                consumers: None,
                branches: None,
                idle: None,
                export: None,
                wip_snapshots: None,
                log_level: None,
                lifecycle: None,
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
        let np = network_policy(&repo(), &door);
        let spec = np.spec.unwrap();
        assert_eq!(spec.policy_types.as_deref(), Some(&["Ingress".to_string()][..]));
        assert_eq!(
            spec.pod_selector.unwrap().match_labels.unwrap()["chert.us/repo"],
            "proj",
            "the policy must select this repository's pod and no other"
        );
        let rule = &spec.ingress.unwrap()[0];
        let ports = rule.ports.as_ref().unwrap();
        assert_eq!(ports.len(), 1, "the git port only");
        assert_eq!(ports[0].port, Some(IntOrString::Int(GIT_PORT)));
        let peer = &rule.from.as_ref().unwrap()[0];
        assert_eq!(
            peer.namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap()
                ["kubernetes.io/metadata.name"],
            "flint-system"
        );
        assert!(peer.pod_selector.is_some(), "a namespace alone admits every pod in it");
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
    door: &DoorSelector,
) -> k8s_openapi::api::networking::v1::NetworkPolicy {
    use k8s_openapi::api::networking::v1::{
        NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort,
        NetworkPolicySpec,
    };
    let n = names(repo);
    let peer = NetworkPolicyPeer {
        namespace_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "kubernetes.io/metadata.name".to_string(),
                door.namespace.clone(),
            )])),
            ..Default::default()
        }),
        pod_selector: Some(LabelSelector {
            match_labels: Some(door.pod_labels.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    NetworkPolicy {
        metadata: meta(repo, n.network_policy),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(selector_labels(repo)),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                from: Some(vec![peer]),
                ports: Some(vec![NetworkPolicyPort {
                    port: Some(IntOrString::Int(GIT_PORT)),
                    protocol: Some("TCP".to_string()),
                    end_port: None,
                }]),
            }]),
            egress: None,
        }),
    }
}
