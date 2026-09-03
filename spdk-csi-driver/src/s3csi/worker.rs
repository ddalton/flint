//! Worker pods: one per published volume, in a system namespace, created
//! by the node plugin on its own node (design §3.1, §3.6).
//!
//! The pod is built by a pure function so its shape is unit-tested:
//! non-root, every capability dropped, seccomp `RuntimeDefault`,
//! read-only rootfs, no ServiceAccount token, no hostPath (passthrough)
//! or exactly one (the lean tree), pinned to this node with `nodeName`
//! (skips the scheduler), tolerating every taint (the tenant already
//! scheduled here), owned by the Node so a vanished node GCs it.
//!
//! Nothing secret is in the spec: credentials travel over the launch
//! socket and into the memory-backed `comm` emptyDir, host-side.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, ExecAction, HostPathVolumeSource, Lifecycle,
    LifecycleHandler, Pod, PodSecurityContext,
    PodSpec, ResourceRequirements, SeccompProfile, SecurityContext, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, DeleteParams, PostParams};
use kube::Client;
use sha2::{Digest, Sha256};

use super::state::TenantRef;

pub const LABEL_VOLUME_HASH: &str = "chert.us/volume-hash";
pub const LABEL_NODE: &str = "chert.us/node";
pub const LABEL_MODE: &str = "chert.us/mode";
pub const LABEL_TENANT_NS: &str = "chert.us/tenant-namespace";
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const ANN_VOLUME_ID: &str = "chert.us/volume-id";
pub const ANN_TENANT_POD: &str = "chert.us/tenant-pod";
pub const ANN_CR: &str = "chert.us/cr";
pub const MANAGED_BY: &str = "flint-s3-csi-node";
pub const CONTAINER_NAME: &str = "worker";
pub const COMM_VOLUME: &str = "comm";
pub const WORKER_BIN: &str = "/usr/local/bin/flint-s3-worker";

/// 16 hex of sha256(volume id): fits a label value, unguessable enough
/// to key a name on, stable across plugin restarts.
pub fn volume_hash(volume_id: &str) -> String {
    let h = Sha256::digest(volume_id.as_bytes());
    h.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

pub fn worker_name(volume_id: &str) -> String {
    format!("s3w-{}", volume_hash(volume_id))
}

/// `/var/lib/kubelet/pods/<uid>/volumes/kubernetes.io~empty-dir/comm`
pub fn comm_dir(kubelet_root: &Path, worker_pod_uid: &str) -> PathBuf {
    kubelet_root
        .join("pods")
        .join(worker_pod_uid)
        .join("volumes")
        .join("kubernetes.io~empty-dir")
        .join(COMM_VOLUME)
}

pub struct WorkerInputs<'a> {
    pub namespace: String,
    pub node_name: String,
    pub node_uid: Option<String>,
    pub image: String,
    pub mode: &'a str,
    pub volume_id: &'a str,
    pub tenant: &'a TenantRef,
    pub cr: &'a str,
    pub run_as_uid: u32,
    pub run_as_gid: u32,
    pub resources: Option<ResourceRequirements>,
    /// Non-secret env in the pod spec (FLINT_S3W_*, and the lean
    /// FLINT_SYNC_* list). Secrets never go here.
    pub env: BTreeMap<String, String>,
    /// preStop budget in seconds: how long a worker holds itself open
    /// waiting for NodeUnpublish to release its volume. None ⇒ 60.
    pub prestop_secs: Option<i64>,
    /// Lean: the plugin-owned tree, hostPath'd at `/workspace`.
    pub lean_tree_hostpath: Option<String>,
    /// Lean: the derived drain budget.
    pub grace_secs: Option<i64>,
    pub priority_class: Option<String>,
    pub comm_size: String,
    pub scratch_size: String,
}

pub fn build_pod(i: &WorkerInputs) -> Pod {
    let name = worker_name(i.volume_id);
    let lean = i.mode == "lean";

    let mut labels = BTreeMap::from([
        (LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string()),
        (LABEL_VOLUME_HASH.to_string(), volume_hash(i.volume_id)),
        (LABEL_NODE.to_string(), i.node_name.clone()),
        (LABEL_MODE.to_string(), i.mode.to_string()),
        (LABEL_TENANT_NS.to_string(), i.tenant.namespace.clone()),
    ]);
    labels.insert("app.kubernetes.io/name".into(), "flint-s3-worker".into());
    let annotations = BTreeMap::from([
        (ANN_VOLUME_ID.to_string(), i.volume_id.to_string()),
        (ANN_TENANT_POD.to_string(), format!("{}/{}", i.tenant.namespace, i.tenant.pod)),
        (ANN_CR.to_string(), i.cr.to_string()),
        ("cluster-autoscaler.kubernetes.io/safe-to-evict".to_string(), "true".to_string()),
    ]);
    let owner_references = i.node_uid.as_ref().map(|uid| {
        vec![OwnerReference {
            api_version: "v1".into(),
            kind: "Node".into(),
            name: i.node_name.clone(),
            uid: uid.clone(),
            controller: Some(true),
            block_owner_deletion: None,
        }]
    });

    let mut volumes = vec![
        Volume {
            name: COMM_VOLUME.into(),
            empty_dir: Some(EmptyDirVolumeSource { medium: Some("Memory".into()), size_limit: Some(Quantity(i.comm_size.clone())) }),
            ..Default::default()
        },
        Volume {
            name: "scratch".into(),
            empty_dir: Some(EmptyDirVolumeSource { medium: None, size_limit: Some(Quantity(i.scratch_size.clone())) }),
            ..Default::default()
        },
    ];
    let mut mounts = vec![
        VolumeMount { name: COMM_VOLUME.into(), mount_path: super::creds::COMM_MOUNT.into(), ..Default::default() },
        VolumeMount { name: "scratch".into(), mount_path: "/tmp".into(), ..Default::default() },
    ];
    if let Some(tree) = &i.lean_tree_hostpath {
        volumes.push(Volume {
            name: "workspace".into(),
            host_path: Some(HostPathVolumeSource { path: tree.clone(), type_: Some("Directory".into()) }),
            ..Default::default()
        });
        mounts.push(VolumeMount { name: "workspace".into(), mount_path: "/workspace".into(), ..Default::default() });
    }

    let mut env: Vec<EnvVar> = i
        .env
        .iter()
        .map(|(k, v)| EnvVar { name: k.clone(), value: Some(v.clone()), value_from: None })
        .collect();
    env.push(EnvVar { name: "FLINT_S3W_COMM".into(), value: Some(super::creds::COMM_MOUNT.into()), value_from: None });

    // Nothing in Kubernetes orders a worker's death against its tenant's:
    // drain evicts both at once, and kubelet's graceful shutdown goes by
    // priority. A worker that dies first leaves ENOTCONN behind, and for
    // lean a final publish that never runs. kubelet runs this hook BEFORE
    // the SIGTERM, on every termination path, and it returns as soon as
    // NodeUnpublish has released the volume — one stat on the ordinary
    // path, because the plugin writes the marker before it deletes us.
    // A PodDisruptionBudget was the first answer and covered the eviction
    // path only, while stalling scale-down and blocking drains; the
    // upstream drivers with this architecture answered it with ordering
    // instead (awslabs/mountpoint-s3-csi-driver#607 ships graceful
    // eviction — a mount pod outlives the workloads using it;
    // juicedata/juicefs-csi-driver#856 is the same failure on a drained
    // spot node).
    // It cannot wedge a shutdown: its budget is its own and it exits 0
    // when the budget runs out. No shell is involved — the wait is a
    // subcommand of this same binary, which both worker images carry at
    // a fixed path, so it does not depend on the base image having sh.
    let prestop = Duration::from_secs(i.prestop_secs.unwrap_or(60) as u64);
    let lifecycle = Lifecycle {
        pre_stop: Some(LifecycleHandler {
            exec: Some(ExecAction { command: Some(vec![WORKER_BIN.into(), "await-release".into()]) }),
            ..Default::default()
        }),
        ..Default::default()
    };
    env.push(EnvVar {
        name: "FLINT_S3W_PRESTOP_SECS".into(),
        value: Some(prestop.as_secs().to_string()),
        value_from: None,
    });

    let container = Container {
        name: CONTAINER_NAME.into(),
        image: Some(i.image.clone()),
        image_pull_policy: Some("IfNotPresent".into()),
        command: Some(vec![WORKER_BIN.into()]),
        lifecycle: Some(lifecycle),
        env: Some(env),
        volume_mounts: Some(mounts),
        resources: i.resources.clone(),
        termination_message_policy: Some("FallbackToLogsOnError".into()),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            capabilities: Some(Capabilities { drop: Some(vec!["ALL".into()]), add: None }),
            read_only_root_filesystem: Some(true),
            privileged: Some(false),
            run_as_non_root: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };

    Pod {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: Some(i.namespace.clone()),
            labels: Some(labels),
            annotations: Some(annotations),
            owner_references,
            ..Default::default()
        },
        spec: Some(PodSpec {
            node_name: Some(i.node_name.clone()),
            restart_policy: Some(if lean { "OnFailure".into() } else { "Never".into() }),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            // kubelet counts the preStop hook AGAINST this budget, so the
            // hook's seconds are ADDED rather than carved out — otherwise a
            // worker that waited 60 s for its tenant would have nothing left
            // for the lean syncer's final publish, which is the very thing
            // the wait exists to make possible.
            termination_grace_period_seconds: Some(i.grace_secs.unwrap_or(30) + prestop.as_secs() as i64),
            priority_class_name: i.priority_class.clone(),
            tolerations: Some(vec![Toleration { operator: Some("Exists".into()), ..Default::default() }]),
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(true),
                run_as_user: Some(i.run_as_uid as i64),
                run_as_group: Some(i.run_as_gid as i64),
                fs_group: Some(i.run_as_gid as i64),
                seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".into(), localhost_profile: None }),
                ..Default::default()
            }),
            volumes: Some(volumes),
            containers: vec![container],
            ..Default::default()
        }),
        status: None,
    }
}

/// Create the pod, or adopt an existing one that carries the SAME
/// volume id (a retried publish, or a plugin restart).
///
/// An existing pod that is terminating, or whose PID 1 has already
/// exited, is a DEAD worker from an earlier attempt (the cleanup of a
/// failed publish deletes it with a short grace, and kubelet's retry
/// can arrive before the API server has dropped the object). Adopting
/// it would only report its exit as this publish's failure, so it is
/// deleted (grace 0) and waited out — bounded — before the create.
pub async fn ensure(client: &Client, pod: &Pod) -> Result<Pod, String> {
    let ns = pod.metadata.namespace.clone().unwrap_or_default();
    let name = pod.metadata.name.clone().unwrap_or_default();
    let api: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let want = pod.metadata.annotations.as_ref().and_then(|a| a.get(ANN_VOLUME_ID));
    for _attempt in 0..2 {
        match api.create(&PostParams::default(), pod).await {
            Ok(p) => return Ok(p),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                let existing = api.get(&name).await.map_err(|e| format!("get worker {ns}/{name}: {e}"))?;
                let have = existing.metadata.annotations.as_ref().and_then(|a| a.get(ANN_VOLUME_ID));
                if want != have {
                    return Err(format!(
                        "worker {ns}/{name} exists for volume {:?}, not {:?} — refusing to adopt",
                        have, want
                    ));
                }
                if !is_dead(&existing) {
                    return Ok(existing);
                }
                delete(client, &ns, &name, Some(0)).await?;
                let start = Instant::now();
                while !is_gone(client, &ns, &name).await? {
                    if start.elapsed() > DEAD_WORKER_WAIT {
                        return Err(format!("worker {ns}/{name} from an earlier attempt is still terminating; retrying"));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            Err(e) => return Err(format!("create worker {ns}/{name}: {e}")),
        }
    }
    Err(format!("worker {ns}/{name} could not be created after replacing a dead one; retrying"))
}

/// How long a retried publish waits for a dead predecessor worker to
/// leave the API before giving kubelet an `Unavailable` to retry.
const DEAD_WORKER_WAIT: Duration = Duration::from_secs(20);

/// Terminating (deletion requested) or already exited.
pub fn is_dead(pod: &Pod) -> bool {
    if pod.metadata.deletion_timestamp.is_some() {
        return true;
    }
    matches!(pod.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Succeeded") | Some("Failed"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Running, with the pod's uid (the comm dir's path component).
    Running { uid: String },
    /// Kubelet admission or the container refused: final for this worker.
    Failed { reason: String, message: String },
    /// Still Pending at the deadline: retryable.
    Timeout { phase: String },
}

pub async fn wait_running(client: &Client, ns: &str, name: &str, deadline: Duration) -> Result<WaitOutcome, String> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let start = Instant::now();
    loop {
        let pod = api.get(name).await.map_err(|e| format!("get worker {ns}/{name}: {e}"))?;
        let st = pod.status.as_ref();
        let phase = st.and_then(|s| s.phase.clone()).unwrap_or_default();
        let uid = pod.metadata.uid.clone().unwrap_or_default();
        let last_phase: String;
        match phase.as_str() {
            "Running" => return Ok(WaitOutcome::Running { uid }),
            "Failed" | "Succeeded" => {
                let reason = st.and_then(|s| s.reason.clone()).unwrap_or_else(|| phase.clone());
                let mut message = st.and_then(|s| s.message.clone()).unwrap_or_default();
                if let Some(cs) = st.and_then(|s| s.container_statuses.as_ref()).and_then(|c| c.first()) {
                    if let Some(t) = cs.state.as_ref().and_then(|s| s.terminated.as_ref()) {
                        message = format!(
                            "{message} container exited {} ({}): {}",
                            t.exit_code,
                            t.reason.clone().unwrap_or_default(),
                            t.message.clone().unwrap_or_default().trim()
                        );
                    }
                }
                return Ok(WaitOutcome::Failed { reason, message: message.trim().to_string() });
            }
            _ => {
                // A container that cannot start (ImagePullBackOff,
                // CreateContainerConfigError) leaves the pod Pending
                // forever; surface the waiting reason in the timeout.
                if let Some(cs) = st.and_then(|s| s.container_statuses.as_ref()).and_then(|c| c.first()) {
                    if let Some(w) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
                        last_phase = format!("{phase} ({})", w.reason.clone().unwrap_or_default());
                    } else {
                        last_phase = phase.clone();
                    }
                } else {
                    last_phase = phase.clone();
                }
            }
        }
        if start.elapsed() > deadline {
            return Ok(WaitOutcome::Timeout { phase: last_phase });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Delete with the given grace; 404 is success.
pub async fn delete(client: &Client, ns: &str, name: &str, grace_secs: Option<u32>) -> Result<(), String> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let dp = DeleteParams { grace_period_seconds: grace_secs, ..Default::default() };
    match api.delete(name, &dp).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(format!("delete worker {ns}/{name}: {e}")),
    }
}

pub async fn is_gone(client: &Client, ns: &str, name: &str) -> Result<bool, String> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    match api.get_opt(name).await {
        Ok(None) => Ok(true),
        Ok(Some(_)) => Ok(false),
        Err(e) => Err(format!("get worker {ns}/{name}: {e}")),
    }
}

/// The worker's PID-1 phase, from its container status: `true` while
/// the container is running.
pub async fn is_running(client: &Client, ns: &str, name: &str) -> Result<bool, String> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    match api.get_opt(name).await {
        Ok(None) => Ok(false),
        Ok(Some(p)) => Ok(p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running")),
        Err(e) => Err(format!("get worker {ns}/{name}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(mode: &'a str, tenant: &'a TenantRef, tree: Option<String>) -> WorkerInputs<'a> {
        WorkerInputs {
            namespace: "flint-workers".into(),
            node_name: "node-1".into(),
            node_uid: Some("nuid".into()),
            image: "img:1".into(),
            mode,
            volume_id: "csi-abc",
            tenant,
            cr: "datasets",
            prestop_secs: Some(60),
            run_as_uid: 1001,
            run_as_gid: 1001,
            resources: None,
            env: BTreeMap::from([("FLINT_SYNC_ROOT".into(), "/workspace".into())]),
            lean_tree_hostpath: tree,
            grace_secs: Some(90),
            priority_class: None,
            comm_size: "16Mi".into(),
            scratch_size: "1Gi".into(),
        }
    }

    fn tenant() -> TenantRef {
        TenantRef { namespace: "team-a".into(), pod: "agent".into(), pod_uid: "puid".into(), service_account: "trainer".into() }
    }

    #[test]
    fn name_and_hash_are_stable_and_label_sized() {
        let vid = "csi-".to_string() + &"a".repeat(64);
        assert_eq!(worker_name(&vid), worker_name(&vid));
        assert!(volume_hash(&vid).len() <= 63);
        assert!(worker_name(&vid).starts_with("s3w-"));
        assert_ne!(worker_name(&vid), worker_name("csi-other"));
    }

    #[test]
    fn passthrough_worker_is_unprivileged_and_hostpath_free() {
        let t = tenant();
        let pod = build_pod(&inputs("passthrough", &t, None));
        let spec = pod.spec.as_ref().unwrap();
        assert_eq!(spec.node_name.as_deref(), Some("node-1"));
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        assert_eq!(spec.automount_service_account_token, Some(false));
        let psc = spec.security_context.as_ref().unwrap();
        assert_eq!(psc.run_as_non_root, Some(true));
        assert_eq!(psc.run_as_user, Some(1001));
        assert_eq!(psc.seccomp_profile.as_ref().unwrap().type_, "RuntimeDefault");
        assert!(spec.volumes.as_ref().unwrap().iter().all(|v| v.host_path.is_none()), "no hostPath for passthrough");
        let c = &spec.containers[0];
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.capabilities.as_ref().unwrap().drop.as_ref().unwrap(), &vec!["ALL".to_string()]);
        assert_eq!(sc.read_only_root_filesystem, Some(true));
        assert_eq!(c.command.as_ref().unwrap()[0], WORKER_BIN);
        let comm = spec.volumes.as_ref().unwrap().iter().find(|v| v.name == COMM_VOLUME).unwrap();
        assert_eq!(comm.empty_dir.as_ref().unwrap().medium.as_deref(), Some("Memory"));
        let owner = &pod.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!((owner.kind.as_str(), owner.controller), ("Node", Some(true)));
        assert_eq!(pod.metadata.annotations.as_ref().unwrap()[ANN_VOLUME_ID], "csi-abc");
        assert_eq!(pod.metadata.labels.as_ref().unwrap()[LABEL_TENANT_NS], "team-a");
        // Nothing secret-shaped in env.
        for e in c.env.as_ref().unwrap() {
            assert!(!e.name.contains("SECRET") && !e.name.contains("TOKEN"), "{}", e.name);
        }
    }

    #[test]
    fn lean_worker_gets_exactly_one_hostpath_and_restarts_on_failure() {
        let t = tenant();
        let pod = build_pod(&inputs("lean", &t, Some("/var/lib/kubelet/plugins/s3.csi.chert.us/volumes/csi-abc/tree".into())));
        let spec = pod.spec.as_ref().unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("OnFailure"));
        // The grace budget is the drain's PLUS the preStop hook's, never
        // the drain's alone: kubelet counts the hook against this number,
        // so carving the wait out of it would spend exactly the seconds
        // the lean syncer needs for its final publish.
        assert_eq!(spec.termination_grace_period_seconds, Some(90 + 60));
        let hp: Vec<_> = spec.volumes.as_ref().unwrap().iter().filter(|v| v.host_path.is_some()).collect();
        assert_eq!(hp.len(), 1);
        assert_eq!(hp[0].host_path.as_ref().unwrap().type_.as_deref(), Some("Directory"));
        let m = spec.containers[0].volume_mounts.as_ref().unwrap().iter().find(|m| m.name == "workspace").unwrap();
        assert_eq!(m.mount_path, "/workspace");
    }

    /// Nothing in Kubernetes orders a worker's death against its
    /// tenant's, and there is deliberately no PodDisruptionBudget: the
    /// hook is the ordering. If it ever stops being emitted, a drain
    /// silently goes back to evicting workers out from under live pods.
    #[test]
    fn every_worker_carries_the_prestop_hook_and_needs_no_shell_for_it() {
        let t = tenant();
        for mode in ["lean", "passthrough"] {
            let pod = build_pod(&inputs(mode, &t, None));
            let c = &pod.spec.as_ref().unwrap().containers[0];
            let cmd = c
                .lifecycle
                .as_ref()
                .and_then(|l| l.pre_stop.as_ref())
                .and_then(|h| h.exec.as_ref())
                .and_then(|e| e.command.clone())
                .unwrap_or_else(|| panic!("{mode} worker has no preStop hook"));
            assert_eq!(cmd, vec![WORKER_BIN.to_string(), "await-release".to_string()], "{mode}");
            // A hook that needs /bin/sh is a hook that breaks the day a
            // worker image goes distroless. It must be argv on our own
            // binary, never a shell string.
            assert!(!cmd[0].contains("sh"), "{mode} preStop goes through a shell");
            let env = c.env.as_ref().unwrap();
            assert!(
                env.iter().any(|e| e.name == "FLINT_S3W_PRESTOP_SECS"),
                "{mode} worker has a hook but no budget for it"
            );
        }
    }

    /// A retried publish must not adopt the previous attempt's worker
    /// once it is terminating or has exited: the cleanup deleted it, and
    /// its exit is not this attempt's verdict.
    #[test]
    fn a_terminating_or_exited_worker_is_dead() {
        use k8s_openapi::api::core::v1::PodStatus;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        let mut p = Pod::default();
        assert!(!is_dead(&p), "a fresh pod with no status is alive (Pending)");
        p.status = Some(PodStatus { phase: Some("Running".into()), ..Default::default() });
        assert!(!is_dead(&p));
        p.status = Some(PodStatus { phase: Some("Succeeded".into()), ..Default::default() });
        assert!(is_dead(&p), "exited 0 is still dead: an fd passed once cannot be re-acquired");
        p.status = Some(PodStatus { phase: Some("Failed".into()), ..Default::default() });
        assert!(is_dead(&p));
        p.status = Some(PodStatus { phase: Some("Running".into()), ..Default::default() });
        p.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        assert!(is_dead(&p), "deletion requested wins over Running");
    }

    #[test]
    fn comm_dir_is_the_kubelet_emptydir_path() {
        assert_eq!(
            comm_dir(Path::new("/var/lib/kubelet"), "u-1"),
            PathBuf::from("/var/lib/kubelet/pods/u-1/volumes/kubernetes.io~empty-dir/comm")
        );
    }
}
