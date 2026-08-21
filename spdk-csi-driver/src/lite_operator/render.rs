//! CR → the four objects. Pure functions, no cluster, no I/O.
//!
//! # The parity contract
//!
//! The lite chart and this renderer are two sources of truth for the
//! same hub, and they stay one shape by test: `render_matches_the_helm_chart`
//! compares this output against a checked-in fixture that
//! `scripts/check-render-parity.sh` regenerates from `helm template`.
//! The fixture carries the chart's own hash, and the test recomputes
//! it — so a chart edit without a regenerated fixture FAILS instead of
//! quietly comparing against yesterday's chart. (A test that shells
//! out to helm was rejected: the Linux suite runs inside a lima VM
//! with no helm, and a test that skips when its tool is missing is a
//! test that passes by not looking.)
//!
//! # Where the two deliberately differ
//!
//! - **Selector labels.** The chart's `app: flint-lite` is fixed per
//!   release; a fleet of them would have every Service selecting every
//!   hub's pods. Operator children are labelled per share
//!   (`flint.io/share`). This is the whole reason the operator can run
//!   a fleet in one namespace, so the test normalizes labels away
//!   explicitly rather than pretending they match.
//! - **`checksum/creds`.** Only the operator has it: `helm template`
//!   cannot see a Secret's contents, so the chart cannot roll a hub on
//!   credential rotation. (It is one of the things an operator buys.)
//!
//! Everything else — the rendered `mds.yaml` the server actually
//! parses, the container, its probes, volumes, the PVC and the Service
//! — is compared, and the mds.yaml is compared as PARSED YAML, since
//! that is the equality the server cares about (comments and key order
//! are not contract).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EnvFromSource, EnvVar,
    EnvVarSource, ObjectFieldSelector,
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, Probe, ResourceRequirements, SecretEnvSource, Service, ServicePort,
    SecretVolumeSource, ServiceSpec as K8sServiceSpec, TCPSocketAction, Volume, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use sha2::{Digest, Sha256};

use super::crd::{FlintShare, Lifecycle, ServiceType};

/// Mount path of the PVC inside the hub container. `/data/exports` is
/// the export root and `/data/state` holds state.db — both on the one
/// RWO claim, which is why two hub pods must never run at once.
const DATA_MOUNT: &str = "/data";
const CONFIG_MOUNT: &str = "/etc/flint";
const NFS_PORT: i32 = 2049;

/// Fleet-wide defaults the operator supplies for anything the CR left
/// unset. These live in the OPERATOR, not in the CRD schema: a default
/// materialized into stored CRs can never be re-decided (that is the
/// `--reuse-values` failure class), while a default resolved here
/// applies to the whole fleet the moment the operator rolls.
#[derive(Debug, Clone)]
pub struct RenderDefaults {
    /// Hub image used when `spec.image` is absent.
    pub image: String,
    pub image_pull_policy: String,
    /// `failureThreshold` for the tiered startupProbe, in 10s periods.
    pub startup_failure_threshold: i32,
    pub termination_grace_period_seconds: i64,
    pub log_level: String,
    pub service_port: i32,
    /// Fleet-wide hub resource REQUESTS, used when `spec.resources`
    /// says nothing. Without these a hub renders `resources: None` and
    /// runs BestEffort: the scheduler sees a zero-cost pod, packs by
    /// pod count alone, and every hub is first in line under node
    /// memory pressure. At 300 live hubs that stops being a default
    /// and becomes a capacity plan nobody wrote down.
    ///
    /// Requests only, no limits, and deliberately so. A memory LIMIT on
    /// the hub is what turned a large download into an OOMKill that
    /// took the NFS export down with it; streaming bounded that, but a
    /// hub is still a filesystem server whose working set is the
    /// caller's, not ours. Requests make it schedulable; a limit makes
    /// it killable.
    pub hub_cpu_request: String,
    pub hub_memory_request: String,
}

impl Default for RenderDefaults {
    fn default() -> Self {
        Self {
            // Matches flint-lite-chart's appVersion. The operator image
            // and the hub image version together are the fleet's
            // upgrade unit.
            image: "dilipdalton/flint-pnfs:1.34.0".to_string(),
            image_pull_policy: "IfNotPresent".to_string(),
            // 60 x 10s = 10 minutes of pre-listener work: an epoch claim
            // that waits out a dead holder's lease plus a DR import that
            // walks the whole bucket. Reading this window as failure is
            // how a liveness probe kills a takeover at 55 seconds.
            startup_failure_threshold: 60,
            termination_grace_period_seconds: 120,
            log_level: "info".to_string(),
            service_port: NFS_PORT,
            hub_cpu_request: "100m".to_string(),
            hub_memory_request: "128Mi".to_string(),
        }
    }
}

/// Child object names, derived from the CR (never from a release).
/// Adoption is the one exception: an adopted share keeps the chart's
/// fixed names, which is why the claim is a field and not a formula.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    pub deployment: String,
    pub service: String,
    pub config_map: String,
    pub claim: String,
    /// True when `claim` came from `spec.existingClaim` — the operator
    /// did not create it and must not assume it may.
    pub claim_is_adopted: bool,
}

/// Everything one reconcile needs to apply, plus the two derived
/// strings status and the roll-trigger depend on.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub names: Names,
    pub config_map: ConfigMap,
    /// `None` when the share adopted an existing claim: the operator
    /// binds to it, never re-declares its size or class (which are
    /// immutable in ways SSA would just error on).
    pub pvc: Option<PersistentVolumeClaim>,
    pub service: Service,
    pub deployment: Deployment,
    pub mds_yaml: String,
    /// sha256 of `mds_yaml` — the pod-template annotation that turns a
    /// config edit into a rollout. Without it nothing restarts the hub
    /// and the new setting never applies: the server parses `--config`
    /// exactly once, at boot.
    pub config_checksum: String,
}

pub fn names(share: &FlintShare) -> Names {
    let base = share.metadata.name.clone().unwrap_or_default();
    let claim = share
        .spec
        .existing_claim
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| format!("{base}-data"));
    Names {
        claim_is_adopted: share
            .spec
            .existing_claim
            .as_deref()
            .is_some_and(|c| !c.is_empty()),
        config_map: format!("{base}-config"),
        deployment: base.clone(),
        service: base,
        claim,
    }
}

/// Labels every child carries. `flint.io/share` is the one that makes
/// a fleet possible — it scopes selectors, and it is how PVC events
/// (which cannot travel by ownerReference, see [`super::reconcile`])
/// find their way back to a CR.
pub fn labels(share: &FlintShare) -> BTreeMap<String, String> {
    let name = share.metadata.name.clone().unwrap_or_default();
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "flint-lite".into()),
        ("app.kubernetes.io/instance".into(), name.clone()),
        ("app.kubernetes.io/component".into(), "lite".into()),
        (
            "app.kubernetes.io/managed-by".into(),
            "flint-lite-operator".into(),
        ),
        ("flint.io/share".into(), name),
        ("flint.io/role".into(), "lite".into()),
    ])
}

/// The subset that is the Deployment's selector — immutable after
/// creation, so it is deliberately minimal and share-scoped.
pub fn selector_labels(share: &FlintShare) -> BTreeMap<String, String> {
    let name = share.metadata.name.clone().unwrap_or_default();
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "flint-lite".into()),
        ("flint.io/share".into(), name),
    ])
}

/// The server config, as the hub will parse it.
///
/// Written as text rather than serialized from [`crate::pnfs::config::PnfsConfig`]
/// on purpose: the config type carries a dozen fields this posture
/// never sets, and a `serde_yaml` dump of it would put `null`s in a
/// file humans read with `kubectl get cm -o yaml`. The knobs come from
/// the CR's typed mirror, so the part that CAN drift is schema-checked
/// (`crd::tests::crd_settings_mirror_matches_tier_knobs`), and the
/// parity fixture compares this against the chart's version parsed.
pub fn mds_yaml(share: &FlintShare, d: &RenderDefaults) -> String {
    let s = &share.spec;
    let log = s.log_level.clone().unwrap_or_else(|| d.log_level.clone());
    let wake_warm = super::idle::wake_warm_fill(share);
    let mut y = String::new();

    let _ = writeln!(y, "apiVersion: flint.io/v1alpha1");
    let _ = writeln!(y, "kind: PnfsConfig");
    let _ = writeln!(y, "mode: standalone");
    let _ = writeln!(y, "mds:");
    let _ = writeln!(y, "  bind:");
    let _ = writeln!(y, "    address: \"0.0.0.0\"");
    let _ = writeln!(y, "    port: {NFS_PORT}");
    // Inert in standalone (no layout is ever granted) but part of the
    // schema the server validates.
    let _ = writeln!(y, "  layout:");
    let _ = writeln!(y, "    type: file");
    let _ = writeln!(y, "    stripeSize: 8388608");
    let _ = writeln!(y, "    policy: stripe");
    let _ = writeln!(y, "  dataServers: []");
    let _ = writeln!(y, "  state:");
    let _ = writeln!(y, "    backend: sqlite");
    let _ = writeln!(y, "    config:");
    let _ = writeln!(y, "      path: {DATA_MOUNT}/state/state.db");

    if s.tiered() {
        // A field of `mds:`, not a top-level key: the parser ignores
        // unknown top-level keys, so misplacing this renders a silently
        // tierless hub (the kind tier e2e's leg 1 caught exactly that).
        let _ = writeln!(y, "  tier:");
        let _ = writeln!(y, "    enabled: true");
        let _ = writeln!(y, "    bucket: {}", yaml_str(s.bucket.as_deref().unwrap_or("")));
        if !s.prefix().is_empty() {
            let _ = writeln!(y, "    keyPrefix: {}", yaml_str(s.prefix()));
        }
        if let Some(ep) = s.endpoint.as_deref().filter(|e| !e.is_empty()) {
            let _ = writeln!(y, "    endpoint: {}", yaml_str(ep));
        }
        if let Some(v) = s.import_on_start {
            let _ = writeln!(y, "    importOnStart: {v}");
        }
        // Only the knobs the user actually set — an unset knob must
        // reach the server as ABSENT so the server's own default
        // applies. This is the entire point of the all-Option mirror.
        if let Some(settings) = &s.settings {
            let map = serde_yaml::to_value(settings)
                .ok()
                .and_then(|v| v.as_mapping().cloned())
                .unwrap_or_default();
            for (k, v) in map {
                let key = k.as_str().unwrap_or_default();
                if key == WARM_FILL_KNOB && wake_warm.is_some() {
                    continue; // the intent below wins
                }
                let val = serde_yaml::to_string(&v).unwrap_or_default();
                let _ = writeln!(y, "    {key}: {}", val.trim_end());
            }
        }
        // `flint.io/wake-intent` — what the front door knows and the
        // operator cannot: whether a person is about to open this
        // project (`warm`, pull the working set back during import) or
        // something merely touched it (`cold`, hydrate on demand). It
        // overrides the standing knob for exactly one boot, and it is
        // meaningful only at boot, which is why `checksum` ignores this
        // line: a hub already running past its import gains nothing
        // from a rollout, and rolling one minutes after it woke would
        // hang the very agent the wake was for.
        if let Some(warm) = wake_warm {
            let _ = writeln!(y, "    {WARM_FILL_KNOB}: {warm}");
        }
    }

    let _ = writeln!(y, "exports:");
    let _ = writeln!(y, "  - path: {DATA_MOUNT}/exports");
    let _ = writeln!(y, "    fsid: 1");
    let _ = writeln!(y, "    options: [rw, sync, no_subtree_check]");
    let _ = writeln!(y, "    access:");
    let _ = writeln!(y, "      - network: 0.0.0.0/0");
    let _ = writeln!(y, "        permissions: rw");
    let _ = writeln!(y, "logging:");
    let _ = writeln!(y, "  level: {}", yaml_str(&log));
    let _ = writeln!(y, "  format: text");

    // The hub's HTTP surface. Rendered to match the chart's helper
    // template byte for byte (minus its comments) — the render-parity
    // golden test compares the two, because two hand-written emitters
    // of one schema drift, and the drift is silent: this parser ignores
    // keys it does not recognise.
    if let Some(m) = s.monitoring.as_ref().filter(|m| m.enabled.unwrap_or(false)) {
        let _ = writeln!(y, "monitoring:");
        let _ = writeln!(y, "  health:");
        let _ = writeln!(y, "    enabled: true");
        let _ = writeln!(y, "    port: {}", m.port.unwrap_or(HEALTH_PORT));
        let _ = writeln!(y, "    path: {}", yaml_str(HEALTH_PATH));
        if let Some(api) = m.file_api.as_ref().filter(|a| a.enabled.unwrap_or(false)) {
            let _ = writeln!(y, "  fileApi:");
            let _ = writeln!(y, "    enabled: true");
            // The path the Secret is projected at, below. The server
            // knows only about files; the CRD knows only about Secrets;
            // this is the one place the two meet.
            if api.token_secret_ref.as_deref().is_some_and(|r| !r.is_empty()) {
                let _ = writeln!(
                    y,
                    "    tokenFile: {}",
                    yaml_str(&format!("{FILE_API_TOKEN_MOUNT}/token"))
                );
            }
            let _ = writeln!(
                y,
                "    maxUploadBytes: {}",
                api.max_upload_bytes.unwrap_or(FILE_API_MAX_BYTES)
            );
            let _ = writeln!(
                y,
                "    maxDownloadBytes: {}",
                api.max_download_bytes.unwrap_or(FILE_API_MAX_BYTES)
            );
            let _ = writeln!(
                y,
                "    hydrateWaitSecs: {}",
                api.hydrate_wait_secs.unwrap_or(FILE_API_HYDRATE_WAIT_SECS)
            );
        }
    }
    y
}

/// Defaults mirrored from the chart's values.yaml. They live here as
/// named constants so the parity test's failure names the drift.
pub const HEALTH_PORT: i32 = 8080;
const HEALTH_PATH: &str = "/health";
/// Where a file-API token Secret is projected in the hub container.
pub const FILE_API_TOKEN_MOUNT: &str = "/etc/flint/api-token";
const FILE_API_MAX_BYTES: i64 = 5 * 1024 * 1024 * 1024;
const FILE_API_HYDRATE_WAIT_SECS: i64 = 30;

/// Quote a scalar the way YAML wants it. Bucket names and prefixes are
/// user input; an unquoted `endpoint: http://x` is a comment waiting to
/// happen.
fn yaml_str(s: &str) -> String {
    serde_yaml::to_string(&serde_yaml::Value::String(s.to_string()))
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

pub fn checksum(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

/// The one tier knob that is BOOT-ONLY, so a change to it can never
/// justify rolling a running hub.
pub const WARM_FILL_KNOB: &str = "hydrateWarmAfterImport";

/// The checksum that decides whether the hub rolls.
///
/// Boot-only settings are stripped before hashing. The warm fill runs
/// during import and never again, so a hub that is already serving
/// gains exactly nothing from a restart that changes it — while the
/// restart itself costs mounted clients a ~90s grace window. The
/// pathological case is the one this exists for: the front door writes
/// `wake-intent: warm`, the share wakes and imports, the intent is
/// consumed and cleared, and the resulting config change rolls the hub
/// minutes after it came up — hanging the agent the wake was for.
///
/// The ConfigMap still carries the real value; only the decision to
/// restart ignores it.
pub fn rollout_checksum(mds_yaml: &str) -> String {
    let stripped: String = mds_yaml
        .lines()
        .filter(|l| l.trim_start().split(':').next() != Some(WARM_FILL_KNOB))
        .map(|l| format!("{l}\n"))
        .collect();
    checksum(&stripped)
}

fn meta(share: &FlintShare, name: String) -> ObjectMeta {
    ObjectMeta {
        name: Some(name),
        namespace: share.metadata.namespace.clone(),
        labels: Some(labels(share)),
        ..Default::default()
    }
}

pub fn config_map(share: &FlintShare, d: &RenderDefaults) -> ConfigMap {
    let n = names(share);
    ConfigMap {
        metadata: meta(share, n.config_map),
        data: Some(BTreeMap::from([(
            "mds.yaml".to_string(),
            mds_yaml(share, d),
        )])),
        ..Default::default()
    }
}

/// The hub's claim — or `None` when the share adopted one.
pub fn pvc(share: &FlintShare) -> Option<PersistentVolumeClaim> {
    let n = names(share);
    if n.claim_is_adopted {
        return None;
    }
    Some(PersistentVolumeClaim {
        metadata: meta(share, n.claim),
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            storage_class_name: share
                .spec
                .persistence
                .storage_class_name
                .clone()
                .filter(|c| !c.is_empty()),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    // The EFFECTIVE size — auto-expand records its
                    // target in an annotation rather than in spec, so
                    // rendering the raw spec value here would undo
                    // every expansion on the next apply.
                    Quantity(crate::lite_operator::persistence::effective_size(share)),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

pub fn service(share: &FlintShare, d: &RenderDefaults) -> Service {
    let n = names(share);
    let svc = share.spec.service.as_ref();
    let ty = svc
        .and_then(|s| s.r#type.clone())
        .unwrap_or(ServiceType::ClusterIP);
    let port = svc.and_then(|s| s.port).unwrap_or(d.service_port);
    let node_port = svc
        .and_then(|s| s.node_port)
        .filter(|_| ty == ServiceType::NodePort);

    let mut m = meta(share, n.service);
    if let Some(a) = svc.and_then(|s| s.annotations.clone()).filter(|a| !a.is_empty()) {
        m.annotations = Some(a);
    }

    Service {
        metadata: m,
        spec: Some(K8sServiceSpec {
            type_: Some(
                match ty {
                    ServiceType::ClusterIP => "ClusterIP",
                    ServiceType::NodePort => "NodePort",
                    ServiceType::LoadBalancer => "LoadBalancer",
                }
                .to_string(),
            ),
            selector: Some(selector_labels(share)),
            ports: Some(vec![ServicePort {
                name: Some("nfs".to_string()),
                port,
                target_port: Some(IntOrString::Int(NFS_PORT)),
                protocol: Some("TCP".to_string()),
                node_port,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The hub Deployment.
///
/// `selector_override` exists for adoption: a Deployment's selector is
/// immutable, so a chart-installed hub (`app: flint-lite`) keeps the
/// selector it was born with — the alternative is an SSA that fails
/// forever on an object we specifically promised to adopt in place.
///
/// `creds_checksum` comes from the live Secret, which is why it is an
/// argument and not computed here: a rotation must roll the pod, and
/// nothing in the CR changes when a Secret does.
pub fn deployment(
    share: &FlintShare,
    d: &RenderDefaults,
    config_checksum: &str,
    creds_checksum: Option<&str>,
    selector_override: Option<LabelSelector>,
) -> Deployment {
    let n = names(share);
    let s = &share.spec;
    let image = s
        .image
        .clone()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| d.image.clone());
    let log = s.log_level.clone().unwrap_or_else(|| d.log_level.clone());

    let mut annotations = BTreeMap::from([("checksum/config".to_string(), config_checksum.to_string())]);
    if let Some(c) = creds_checksum {
        annotations.insert("checksum/creds".to_string(), c.to_string());
    }

    let mut env = vec![
        EnvVar {
            name: "RUST_LOG".to_string(),
            value: Some(log),
            ..Default::default()
        },
        // Published on /status as `podName`. With `serverId` it is how
        // a caller tells a plain restart (podName changed, the state
        // survived) from a wake onto a fresh PVC (serverId changed,
        // every client stateid is stale). Mirrored in the chart; the
        // parity test compares the two.
        EnvVar {
            name: "POD_NAME".to_string(),
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
    if let Some(region) = s.region.as_deref().filter(|r| !r.is_empty() && s.tiered()) {
        env.push(EnvVar {
            name: "AWS_REGION".to_string(),
            value: Some(region.to_string()),
            ..Default::default()
        });
    }

    let env_from = s
        .credentials_secret_ref
        .as_deref()
        .filter(|r| !r.is_empty() && s.tiered())
        .map(|r| {
            vec![EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: r.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }]
        });

    let tcp = || Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(NFS_PORT),
            ..Default::default()
        }),
        ..Default::default()
    };

    // With the tier on, startup legitimately runs long BEFORE the
    // socket opens (epoch claim, DR import) — liveness must not begin
    // until it succeeds.
    let startup_probe = s.tiered().then(|| Probe {
        period_seconds: Some(10),
        failure_threshold: Some(
            s.startup_failure_threshold
                .unwrap_or(d.startup_failure_threshold),
        ),
        ..tcp()
    });

    // The monitoring listener. Declared as a containerPort for
    // legibility only — it is deliberately NOT added to the Service,
    // which carries NFS and may be a LoadBalancer. The lifecycle
    // controller reaches /status by POD IP.
    let monitoring = s.monitoring.as_ref().filter(|m| m.enabled.unwrap_or(false));
    let mut ports = vec![ContainerPort {
        container_port: NFS_PORT,
        name: Some("nfs".to_string()),
        ..Default::default()
    }];
    if let Some(m) = monitoring {
        ports.push(ContainerPort {
            container_port: m.port.unwrap_or(HEALTH_PORT),
            name: Some("http".to_string()),
            ..Default::default()
        });
    }

    let mut volume_mounts = vec![
        VolumeMount {
            name: "config".to_string(),
            mount_path: CONFIG_MOUNT.to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "data".to_string(),
            mount_path: DATA_MOUNT.to_string(),
            ..Default::default()
        },
    ];
    let mut volumes = vec![
        Volume {
            name: "config".to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: n.config_map.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "data".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: n.claim.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    // The file API's bearer token, projected read-only. Mounting the
    // Secret rather than passing it as env means rotating the token is
    // a Secret edit; the kubelet refreshes the projection and the next
    // hub start picks it up.
    if let Some(secret) = monitoring
        .and_then(|m| m.file_api.as_ref())
        .filter(|a| a.enabled.unwrap_or(false))
        .and_then(|a| a.token_secret_ref.as_deref())
        .filter(|r| !r.is_empty())
    {
        volume_mounts.push(VolumeMount {
            name: "api-token".to_string(),
            mount_path: FILE_API_TOKEN_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(Volume {
            name: "api-token".to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    // `spec.resources` wins outright when set — an operator who wrote
    // a number meant it. Absent, fall back to the fleet defaults rather
    // than to nothing: see `RenderDefaults::hub_cpu_request`.
    let resources = match s.resources.as_ref() {
        Some(r) => Some(ResourceRequirements {
            requests: r.requests.clone().map(quantities),
            limits: r.limits.clone().map(quantities),
            ..Default::default()
        }),
        None => Some(ResourceRequirements {
            requests: Some(std::collections::BTreeMap::from([
                ("cpu".to_string(), Quantity(d.hub_cpu_request.clone())),
                ("memory".to_string(), Quantity(d.hub_memory_request.clone())),
            ])),
            limits: None,
            ..Default::default()
        }),
    };

    // Suspended keeps every object and the PVC — only the pod goes.
    // Waking is a replica count, not a restore.
    //
    // The idle ladder gets a vote here, and MUST: this renderer is
    // level-triggered and server-side-applies, so a suspend recorded
    // anywhere the render does not read would be undone by the very
    // next reconcile, seconds later, forever. The ladder's position
    // lives in an annotation on the CR — metadata, not spec, because
    // the user owns spec and the operator does not write it.
    let replicas = match s.lifecycle.clone().unwrap_or(Lifecycle::Active) {
        Lifecycle::Suspended => 0,
        Lifecycle::Active if crate::lite_operator::idle::state_of(share).is_down() => 0,
        Lifecycle::Active => 1,
    };

    // The pod template MUST satisfy the selector, and an adopted
    // Deployment's selector is the chart's (`app: flint-lite`), not
    // ours — apply a template that does not match it and the API server
    // rejects the whole object with "selector does not match template
    // labels", forever, on the one operation we promised would be
    // boring. Our labels stay too: they are what the Service selects
    // and what PVC events map by.
    let selector = selector_override.unwrap_or(LabelSelector {
        match_labels: Some(selector_labels(share)),
        ..Default::default()
    });
    let mut pod_labels = labels(share);
    if let Some(required) = selector.match_labels.as_ref() {
        for (k, v) in required {
            pod_labels.insert(k.clone(), v.clone());
        }
    }

    Deployment {
        metadata: meta(share, n.deployment),
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            // Recreate, never RollingUpdate: the sqlite state.db is
            // single-writer and the RWO attach is the fence. Two hub
            // pods must never overlap.
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                ..Default::default()
            }),
            selector,
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    annotations: Some(annotations),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    // The shutdown budget: drain, final flush, manifest
                    // barrier, epoch release. The 30s Kubernetes
                    // default cannot publish a large dirty set.
                    termination_grace_period_seconds: Some(
                        s.termination_grace_period_seconds
                            .unwrap_or(d.termination_grace_period_seconds),
                    ),
                    node_selector: s.node_selector.clone().filter(|m| !m.is_empty()),
                    containers: vec![Container {
                        name: "hub".to_string(),
                        image: Some(image),
                        image_pull_policy: Some(d.image_pull_policy.clone()),
                        command: Some(vec!["/usr/local/bin/flint-pnfs-mds".to_string()]),
                        args: Some(vec![
                            "--config".to_string(),
                            format!("{CONFIG_MOUNT}/mds.yaml"),
                        ]),
                        env: Some(env),
                        env_from,
                        ports: Some(ports),
                        readiness_probe: Some(Probe {
                            initial_delay_seconds: Some(3),
                            period_seconds: Some(5),
                            ..tcp()
                        }),
                        liveness_probe: Some(Probe {
                            initial_delay_seconds: Some(10),
                            period_seconds: Some(15),
                            ..tcp()
                        }),
                        startup_probe,
                        volume_mounts: Some(volume_mounts),
                        resources,
                        ..Default::default()
                    }],
                    volumes: Some(volumes),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn quantities(m: BTreeMap<String, String>) -> BTreeMap<String, Quantity> {
    m.into_iter().map(|(k, v)| (k, Quantity(v))).collect()
}

/// Render everything one reconcile applies.
pub fn render(
    share: &FlintShare,
    d: &RenderDefaults,
    creds_checksum: Option<&str>,
    selector_override: Option<LabelSelector>,
) -> Rendered {
    let yaml = mds_yaml(share, d);
    let sum = rollout_checksum(&yaml);
    Rendered {
        names: names(share),
        config_map: config_map(share, d),
        pvc: pvc(share),
        service: service(share, d),
        deployment: deployment(share, d, &sum, creds_checksum, selector_override),
        mds_yaml: yaml,
        config_checksum: sum,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lite_operator::crd::{
        FlintShareSpec, PersistenceSpec, ServiceSpec, TierSettings,
    };
    use serde_json::Value;

    fn share(name: &str, spec: FlintShareSpec) -> FlintShare {
        let mut s = FlintShare::new(name, spec);
        s.metadata.namespace = Some("flint".to_string());
        s
    }

    fn base_spec() -> FlintShareSpec {
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
        }
    }

    fn tiered_spec() -> FlintShareSpec {
        FlintShareSpec {
            bucket: Some("my-team-flint".into()),
            key_prefix: Some("tenant-a/".into()),
            endpoint: Some("http://minio.minio:9000".into()),
            region: Some("us-east-1".into()),
            credentials_secret_ref: Some("flint-s3".into()),
            import_on_start: Some(true),
            ..base_spec()
        }
    }

    /// `flint.io/wake-intent` is what the front door knows and the
    /// operator cannot: whether a person is about to open this project
    /// or something merely touched it. It has to reach the pod, and it
    /// must not roll one that is already up.
    #[test]
    fn a_wake_intent_reaches_the_config_but_never_rolls_a_running_hub() {
        let with_intent = |v: Option<&str>| {
            let mut sh = share("t", tiered_spec());
            if let Some(v) = v {
                sh.metadata
                    .annotations
                    .get_or_insert_with(Default::default)
                    .insert(crate::lite_operator::idle::ANN_WAKE_INTENT.into(), v.into());
            }
            sh
        };
        let d = RenderDefaults::default();

        // No intent: the knob is absent, so the server default stands.
        let plain = mds_yaml(&with_intent(None), &d);
        assert!(!plain.contains(WARM_FILL_KNOB), "no intent must render nothing:\n{plain}");

        // warm/cold reach the config as the boot-only knob.
        assert!(mds_yaml(&with_intent(Some("warm")), &d)
            .contains(&format!("{WARM_FILL_KNOB}: true")));
        assert!(mds_yaml(&with_intent(Some("cold")), &d)
            .contains(&format!("{WARM_FILL_KNOB}: false")));

        // A typo is NOT read as "cold" — guessing "do less" on a
        // misspelling shows up as a slow project and nothing else.
        assert!(!mds_yaml(&with_intent(Some("wrm")), &d).contains(WARM_FILL_KNOB));

        // THE POINT: the rollout decision ignores all of it. Every
        // variant hashes the same, so consuming the intent after the
        // wake cannot restart the hub it just woke.
        let sums: Vec<_> = [None, Some("warm"), Some("cold"), Some("wrm")]
            .iter()
            .map(|v| rollout_checksum(&mds_yaml(&with_intent(*v), &d)))
            .collect();
        assert!(
            sums.windows(2).all(|w| w[0] == w[1]),
            "a boot-only knob changed the rollout checksum — clearing the intent would roll \
             the hub minutes after it woke, hanging the agent the wake was for"
        );

        // And the guard is not vacuous: a real setting still rolls.
        let mut louder = share("t", tiered_spec());
        louder.spec.log_level = Some("debug".into());
        assert_ne!(
            rollout_checksum(&mds_yaml(&louder, &d)),
            rollout_checksum(&plain),
            "stripping the boot-only line must not have blunted the checksum entirely"
        );
    }

    /// The knob half is the whole reason `settings` is typed: what the
    /// user set appears, what they did not is ABSENT — not defaulted,
    /// not null — so the server applies its own value.
    #[test]
    fn only_the_knobs_that_were_set_reach_the_config() {
        let spec = FlintShareSpec {
            settings: Some(TierSettings {
                watermark_pct: Some(90),
                hydrate_warm_after_import: Some(true),
                ..Default::default()
            }),
            ..tiered_spec()
        };
        let y = mds_yaml(&share("t", spec), &RenderDefaults::default());
        assert!(y.contains("watermarkPct: 90"), "{y}");
        assert!(y.contains("hydrateWarmAfterImport: true"), "{y}");
        assert!(!y.contains("flushFloorSecs"), "an unset knob must not be rendered:\n{y}");
        assert!(!y.contains("null"), "no nulls in a config the server parses:\n{y}");

        // And it parses back into the server's own type with the
        // untouched knobs at their server defaults.
        let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
        let t: crate::pnfs::config::TierConfig =
            serde_yaml::from_value(v["mds"]["tier"].clone()).unwrap();
        assert_eq!(t.knobs.watermark_pct, 90);
        assert!(t.knobs.hydrate_warm_after_import);
        assert_eq!(t.knobs.flush_floor_secs, 60, "unset knob took the server default");
        assert_eq!(t.bucket, "my-team-flint");
        assert_eq!(t.key_prefix, "tenant-a/");
    }

    /// A tier-off share is a share: no `tier:` block at all, and no
    /// startupProbe (there is no pre-listener work to wait for).
    #[test]
    fn tier_off_renders_a_plain_hub() {
        let sh = share("plain", base_spec());
        let d = RenderDefaults::default();
        let y = mds_yaml(&sh, &d);
        assert!(!y.contains("tier:"), "{y}");
        let cfg: crate::pnfs::config::PnfsConfig = serde_yaml::from_str(&y).unwrap();
        assert!(cfg.mds.unwrap().tier.is_none());

        let dep = deployment(&sh, &d, "sum", None, None);
        let c = &dep.spec.unwrap().template.spec.unwrap().containers[0];
        assert!(c.startup_probe.is_none());
        assert!(c.env_from.is_none(), "no bucket ⇒ no credentials to inject");
    }

    /// The whole rendered config is what the server parses — so it has
    /// to parse, in both postures, with the values we think we wrote.
    #[test]
    fn rendered_config_parses_as_the_servers_own_type() {
        let d = RenderDefaults::default();
        let cfg: crate::pnfs::config::PnfsConfig =
            serde_yaml::from_str(&mds_yaml(&share("t", tiered_spec()), &d)).unwrap();
        assert!(matches!(cfg.mode, crate::pnfs::config::PnfsMode::Standalone));
        let mds = cfg.mds.expect("mds section");
        assert_eq!(mds.bind.port, 2049);
        assert_eq!(mds.state.config.get("path").map(String::as_str), Some("/data/state/state.db"));
        let t = mds.tier.expect("tier section");
        assert!(t.enabled);
        assert_eq!(t.endpoint.as_deref(), Some("http://minio.minio:9000"));
        assert!(t.import_on_start);
        assert_eq!(cfg.exports[0].path, "/data/exports");
    }

    /// Names come from the CR, never from a release — that is what
    /// lets a fleet share one namespace. A share named `flint-lite`
    /// lands on exactly the chart's names, which is what makes the
    /// parity fixture a like-for-like comparison and adoption
    /// mechanically boring.
    #[test]
    fn child_names_derive_from_the_cr() {
        let n = names(&share("tenant-a", base_spec()));
        assert_eq!(n.deployment, "tenant-a");
        assert_eq!(n.service, "tenant-a");
        assert_eq!(n.config_map, "tenant-a-config");
        assert_eq!(n.claim, "tenant-a-data");
        assert!(!n.claim_is_adopted);

        let n = names(&share("flint-lite", base_spec()));
        assert_eq!(
            (n.deployment.as_str(), n.config_map.as_str(), n.claim.as_str()),
            ("flint-lite", "flint-lite-config", "flint-lite-data"),
            "the chart's fixed names are the CR-derived names of a share called flint-lite"
        );
    }

    /// Adoption: bind to the existing claim, and do NOT render a PVC —
    /// re-declaring someone else's claim is how you get an SSA that
    /// errors forever on an immutable field.
    #[test]
    fn an_adopted_claim_is_bound_but_never_rendered() {
        let spec = FlintShareSpec {
            existing_claim: Some("flint-lite-data".into()),
            ..base_spec()
        };
        let sh = share("tenant-a", spec);
        assert!(pvc(&sh).is_none());
        let n = names(&sh);
        assert!(n.claim_is_adopted);
        assert_eq!(n.claim, "flint-lite-data");
        let dep = deployment(&sh, &RenderDefaults::default(), "sum", None, None);
        let vols = dep.spec.unwrap().template.spec.unwrap().volumes.unwrap();
        let data = vols.iter().find(|v| v.name == "data").unwrap();
        assert_eq!(
            data.persistent_volume_claim.as_ref().unwrap().claim_name,
            "flint-lite-data"
        );
    }

    /// A chart-born Deployment has `app: flint-lite` as its selector,
    /// and a selector is immutable: adoption must keep it or every
    /// apply fails. (The pod template still gains our labels, so the
    /// operator's Service and watches find the pod.)
    #[test]
    fn adoption_keeps_a_foreign_selector() {
        let existing = LabelSelector {
            match_labels: Some(BTreeMap::from([("app".to_string(), "flint-lite".to_string())])),
            ..Default::default()
        };
        let dep = deployment(
            &share("tenant-a", base_spec()),
            &RenderDefaults::default(),
            "sum",
            None,
            Some(existing.clone()),
        );
        let spec = dep.spec.unwrap();
        assert_eq!(spec.selector, existing);
        let pod_labels = spec.template.metadata.unwrap().labels.unwrap();
        assert_eq!(pod_labels.get("flint.io/share").map(String::as_str), Some("tenant-a"));
        // ... and the template must SATISFY that selector, or the API
        // server refuses the object outright ("selector does not match
        // template labels") — on adoption, of all operations.
        assert_eq!(pod_labels.get("app").map(String::as_str), Some("flint-lite"));
    }

    /// Holds for every share, not just adopted ones: a Deployment whose
    /// template does not match its own selector is rejected at apply.
    #[test]
    fn the_pod_template_always_satisfies_the_selector() {
        let d = RenderDefaults::default();
        for selector in [
            None,
            Some(LabelSelector {
                match_labels: Some(BTreeMap::from([("app".into(), "flint-lite".into())])),
                ..Default::default()
            }),
        ] {
            let dep = deployment(&share("tenant-a", tiered_spec()), &d, "sum", None, selector);
            let spec = dep.spec.unwrap();
            let pod = spec.template.metadata.unwrap().labels.unwrap();
            for (k, v) in spec.selector.match_labels.unwrap() {
                assert_eq!(pod.get(&k), Some(&v), "template misses selector label {k}");
            }
        }
    }

    /// The roll trigger. Nothing else restarts the hub: the server
    /// parses its config once at boot and has no reload path, so a
    /// settings edit that does not change this annotation is a settings
    /// edit that never happens.
    #[test]
    fn a_settings_edit_changes_the_config_checksum() {
        let d = RenderDefaults::default();
        let before = render(&share("t", tiered_spec()), &d, None, None);
        let after = render(
            &share(
                "t",
                FlintShareSpec {
                    settings: Some(TierSettings {
                        watermark_pct: Some(90),
                        ..Default::default()
                    }),
                    ..tiered_spec()
                },
            ),
            &d,
            None,
            None,
        );
        assert_ne!(before.config_checksum, after.config_checksum);

        let ann = |r: &Rendered| {
            r.deployment
                .spec
                .as_ref()
                .unwrap()
                .template
                .metadata
                .as_ref()
                .unwrap()
                .annotations
                .clone()
                .unwrap()
        };
        assert_eq!(ann(&after)["checksum/config"], after.config_checksum);
        assert!(!ann(&after).contains_key("checksum/creds"));

        // A rotated Secret changes nothing in the CR — which is why the
        // operator hashes the Secret itself.
        let rotated = render(&share("t", tiered_spec()), &d, Some("deadbeef"), None);
        assert_eq!(ann(&rotated)["checksum/creds"], "deadbeef");
        assert_eq!(ann(&rotated)["checksum/config"], before.config_checksum);
    }

    /// Suspended is a replica count and nothing else: the PVC, the
    /// Service and the config all stay, so waking is instant and the
    /// data never moved.
    #[test]
    fn suspended_scales_to_zero_and_keeps_everything_else() {
        let spec = FlintShareSpec {
            lifecycle: Some(Lifecycle::Suspended),
            ..tiered_spec()
        };
        let r = render(&share("t", spec), &RenderDefaults::default(), None, None);
        assert_eq!(r.deployment.spec.as_ref().unwrap().replicas, Some(0));
        assert!(r.pvc.is_some());
        assert!(r.service.spec.is_some());
    }

    #[test]
    fn service_shape_follows_the_spec() {
        let d = RenderDefaults::default();
        let spec = FlintShareSpec {
            service: Some(ServiceSpec {
                r#type: Some(ServiceType::NodePort),
                port: Some(2050),
                node_port: Some(30049),
                advertise_address: None,
                annotations: Some(BTreeMap::from([("a".into(), "b".into())])),
            }),
            ..base_spec()
        };
        let svc = service(&share("t", spec), &d);
        let s = svc.spec.unwrap();
        assert_eq!(s.type_.as_deref(), Some("NodePort"));
        let p = &s.ports.unwrap()[0];
        assert_eq!((p.port, p.node_port), (2050, Some(30049)));
        assert_eq!(p.target_port, Some(IntOrString::Int(2049)));
        assert_eq!(svc.metadata.annotations.unwrap()["a"], "b");

        // A nodePort on a ClusterIP service is rejected by the API
        // server; drop it rather than render an object that cannot apply.
        let spec = FlintShareSpec {
            service: Some(ServiceSpec {
                r#type: Some(ServiceType::ClusterIP),
                port: None,
                node_port: Some(30049),
                advertise_address: None,
                annotations: None,
            }),
            ..base_spec()
        };
        let svc = service(&share("t", spec), &d);
        let p = &svc.spec.unwrap().ports.unwrap()[0];
        assert_eq!((p.port, p.node_port), (2049, None));
    }

    /// The Service must select THIS share's pods and no others. With a
    /// fleet in one namespace, the chart's fixed `app: flint-lite`
    /// would have every Service fronting every hub — silent
    /// cross-tenant traffic, no error anywhere.
    #[test]
    fn selectors_are_share_scoped() {
        let d = RenderDefaults::default();
        let a = service(&share("tenant-a", base_spec()), &d);
        let b = service(&share("tenant-b", base_spec()), &d);
        let sel = |s: &Service| s.spec.as_ref().unwrap().selector.clone().unwrap();
        assert_ne!(sel(&a), sel(&b));
        assert_eq!(sel(&a)["flint.io/share"], "tenant-a");
    }

    // ---------------------------------------------------------------
    // Render parity with the chart
    // ---------------------------------------------------------------

    /// Keys whose difference is intended (see the module doc). Every
    /// entry here is a promise that the difference is understood, not a
    /// place to hide a mismatch.
    fn normalize(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("labels");
                m.remove("selector");
                m.remove("matchLabels");
                if let Some(Value::Object(a)) = m.get_mut("annotations") {
                    // helm cannot see a Secret's contents; only the
                    // operator can hash credentials.
                    a.remove("checksum/creds");
                    // Both sides MUST carry a config checksum — that is
                    // the roll trigger, and its absence on either side
                    // is a real bug. The VALUES differ by construction
                    // (each hashes its own rendered text, and the
                    // chart's carries comments), so presence is what is
                    // compared: the key survives normalization, the
                    // digest does not.
                    if let Some(c) = a.get_mut("checksum/config") {
                        *c = Value::String("<sha256>".into());
                    }
                    if a.is_empty() {
                        m.remove("annotations");
                    }
                }
                // Helm renders into a release namespace; the operator
                // into the CR's. Compared cases use the same one, but
                // the field is noise either way.
                m.remove("namespace");
                m.remove("creationTimestamp");
                for (_, child) in m.iter_mut() {
                    normalize(child);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(normalize),
            _ => {}
        }
    }

    fn strip_nulls(v: Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.into_iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k, strip_nulls(v)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
            other => other,
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/lite-chart-render.json"))
            .expect("parity fixture is JSON")
    }

    /// The fixture is only evidence while it matches the chart it was
    /// generated from. This recomputes the chart's hash and fails the
    /// build when someone edits the chart without re-running
    /// `scripts/check-render-parity.sh` — the alternative (a test that
    /// silently compares against last month's chart) is the "passes by
    /// not looking" failure this project keeps meeting.
    #[test]
    fn parity_fixture_matches_the_current_chart() {
        let recorded = fixture()["chartSha256"].as_str().unwrap().to_string();
        assert_eq!(
            recorded,
            crate::lite_operator::render::chart_sha256(),
            "flint-lite-chart changed since tests/fixtures/lite-chart-render.json was generated — \
             re-run scripts/check-render-parity.sh"
        );
    }

    /// The golden test itself: for every case in the fixture, what the
    /// operator renders equals what `helm template` rendered, modulo
    /// the documented normalization.
    #[test]
    fn render_matches_the_helm_chart() {
        let fx = fixture();
        let cases = fx["cases"].as_object().expect("fixture cases");
        assert!(!cases.is_empty(), "the fixture has no cases");

        for (name, case) in cases {
            let spec: FlintShareSpec = serde_json::from_value(case["spec"].clone())
                .unwrap_or_else(|e| panic!("case {name}: spec: {e}"));
            let sh = share("flint-lite", spec);
            let r = render(&sh, &RenderDefaults::default(), None, None);

            // The config the server parses, compared as parsed YAML:
            // comments and key order are not contract, values are.
            let ours: serde_yaml::Value = serde_yaml::from_str(&r.mds_yaml).unwrap();
            let theirs: serde_yaml::Value =
                serde_yaml::from_str(case["mdsYaml"].as_str().unwrap()).unwrap();
            assert_eq!(ours, theirs, "case {name}: rendered mds.yaml differs from the chart's");

            for (kind, mine) in [
                ("deployment", serde_json::to_value(&r.deployment).unwrap()),
                ("service", serde_json::to_value(&r.service).unwrap()),
                (
                    "pvc",
                    r.pvc
                        .as_ref()
                        .map(|p| serde_json::to_value(p).unwrap())
                        .unwrap_or(Value::Null),
                ),
            ] {
                let mut mine = strip_nulls(mine);
                let mut theirs = strip_nulls(case[kind].clone());
                normalize(&mut mine);
                normalize(&mut theirs);
                assert_eq!(
                    mine, theirs,
                    "case {name}: rendered {kind} differs from the chart's"
                );
            }
        }
    }
}

/// sha256 over every chart input that can change a render. Used by the
/// parity fixture's provenance check and by
/// `scripts/check-render-parity.sh` (which prints it) so the two
/// cannot disagree about what "the current chart" means.
pub fn chart_sha256() -> String {
    let mut h = Sha256::new();
    for part in [
        include_str!("../../../flint-lite-chart/Chart.yaml"),
        include_str!("../../../flint-lite-chart/values.yaml"),
        include_str!("../../../flint-lite-chart/templates/hub.yaml"),
        include_str!("../../../flint-lite-chart/templates/_helpers.tpl"),
    ] {
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}
