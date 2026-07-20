//! ReadWriteMany (RWX) Support via NFS Server
//!
//! This module provides NFS-based ReadWriteMany volume support for the Flint CSI driver.
//! It is completely isolated from existing RWO (ReadWriteOnce) functionality to ensure
//! zero regression.
//!
//! # Architecture
//!
//! When a PVC requests ReadWriteMany access:
//! 1. Controller detects RWX in CreateVolume
//! 2. Controller creates NFS server pod during ControllerPublishVolume
//! 3. NFS pod is constrained to run on nodes with volume replicas (node affinity)
//! 4. Client nodes mount NFS export in NodePublishVolume
//!
//! # Feature Flag
//!
//! All functionality is gated behind the `NFS_ENABLED` environment variable.
//! When disabled (default), all functions return early without affecting RWO volumes.
//!
//! # Safety
//!
//! - Zero modification to existing RWO code paths
//! - All NFS logic is additive only
//! - Feature disabled by default
//! - Comprehensive logging for visibility

use std::collections::{HashMap, BTreeMap};
use std::env;
use kube::{Api, Client, api::{PostParams, DeleteParams}};
use k8s_openapi::api::core::v1::{
    Pod, PodSpec, Container, EnvVar, VolumeMount, Volume,
    SecurityContext, Capabilities,
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource,
    PersistentVolume, PersistentVolumeSpec, ObjectReference,
    CSIPersistentVolumeSource, ContainerPort,
    Affinity, NodeAffinity, NodeSelectorTerm,
    NodeSelectorRequirement, PreferredSchedulingTerm, ResourceRequirements,
    Service, ServiceSpec, ServicePort,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use tokio::time::{sleep, Duration};
use tonic::Status;

/// Stable NFSv4 file-handle instance id for a volume — identical for every
/// incarnation of the volume's NFS server pod, on any node. The server
/// embeds the id in every file handle and rejects handles minted under a
/// different one; its default (startup nanos) deliberately invalidates all
/// handles on restart, which turns every server bounce into permanent
/// EBADHANDLE for live client mounts (RWX cutover round, 2026-06-12).
/// Handles are self-describing (the path is embedded), so under a stable id
/// a replacement server resolves any handle its predecessors minted.
pub fn stable_nfs_instance_id(volume_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "flint-nfs-instance".hash(&mut h);
    volume_id.hash(&mut h);
    h.finish()
}

#[derive(Debug, Clone, PartialEq)]
pub enum NfsBackend {
    Pvc,
    EmptyDir,
}

/// NFS configuration loaded from environment variables (set by Helm chart)
#[derive(Clone, Debug)]
pub struct NfsConfig {
    /// Whether NFS support is enabled
    pub enabled: bool,
    /// NFS server image (full path: repository/name:tag)
    pub image: String,
    /// Image pull policy
    pub pull_policy: String,
    /// NFS server port
    pub port: u16,
    /// Namespace for NFS pods
    pub namespace: String,
    /// Resource requests and limits
    pub resources: NfsResources,
    /// Per-op DEBUG logging in the NFS server (hex dumps included).
    /// A data-path tax measured at multiples of the request latency
    /// under bulk load — debugging only, never the default.
    pub verbose: bool,
}

#[derive(Clone, Debug)]
pub struct NfsResources {
    pub memory_request: String,
    pub cpu_request: String,
    pub memory_limit: String,
    pub cpu_limit: String,
}

impl NfsConfig {
    /// Load NFS configuration from environment variables
    /// Returns None if NFS_ENABLED is false or not set
    pub fn from_env() -> Option<Self> {
        let enabled = env::var("NFS_ENABLED")
            .unwrap_or_else(|_| "false".to_string())
            .to_lowercase() == "true";
        
        if !enabled {
            eprintln!("ℹ️  [NFS] NFS support disabled (NFS_ENABLED=false)");
            eprintln!("ℹ️  [NFS] All RWX requests will be rejected");
            eprintln!("ℹ️  [NFS] Existing RWO functionality unaffected");
            return None;
        }
        
        // Use CSI driver image (which includes flint-nfs-server binary)
        // This simplifies deployment - only one image to build/maintain
        let repository = env::var("NFS_IMAGE_REPOSITORY")
            .unwrap_or_else(|_| env::var("IMAGE_REPOSITORY")
                .unwrap_or_else(|_| "docker-sandbox.infra.cloudera.com/ddalton".to_string()));
        let name = env::var("NFS_IMAGE_NAME")
            .unwrap_or_else(|_| env::var("CSI_DRIVER_IMAGE_NAME")
                .unwrap_or_else(|_| "flint-driver".to_string()));
        let tag = env::var("NFS_IMAGE_TAG")
            .unwrap_or_else(|_| env::var("CSI_DRIVER_IMAGE_TAG")
                .unwrap_or_else(|_| "latest".to_string()));
        let image = format!("{}/{}:{}", repository, name, tag);
        
        let config = Self {
            enabled: true,
            image,
            pull_policy: env::var("NFS_IMAGE_PULL_POLICY")
                .unwrap_or_else(|_| "IfNotPresent".to_string()),
            port: env::var("NFS_SERVER_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(2049),
            namespace: env::var("NFS_NAMESPACE")
                .unwrap_or_else(|_| "flint-system".to_string()),
            resources: NfsResources {
                // The NFS pod IS the data path for its volume: a bulk
                // loader drives WRITE+ALLOCATE+COMMIT through one pod.
                // The old 500m/256Mi defaults throttled pgbench -i to
                // ~15k tuples/s (drill evidence, 2026-07-18).
                memory_request: env::var("NFS_MEMORY_REQUEST")
                    .unwrap_or_else(|_| "256Mi".to_string()),
                cpu_request: env::var("NFS_CPU_REQUEST")
                    .unwrap_or_else(|_| "500m".to_string()),
                memory_limit: env::var("NFS_MEMORY_LIMIT")
                    .unwrap_or_else(|_| "1Gi".to_string()),
                cpu_limit: env::var("NFS_CPU_LIMIT")
                    .unwrap_or_else(|_| "2".to_string()),
            },
            verbose: env::var("NFS_VERBOSE")
                .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
                .unwrap_or(false),
        };
        
        eprintln!("✅ [NFS] NFS support ENABLED");
        eprintln!("   Image: {}", config.image);
        eprintln!("   Port: {}", config.port);
        eprintln!("   Namespace: {}", config.namespace);
        
        Some(config)
    }
}

/// Check if NFS is enabled
pub fn is_nfs_enabled() -> bool {
    NfsConfig::from_env().is_some()
}

/// Parse replica nodes from volume_context comma-separated string
pub fn parse_replica_nodes(volume_context: &HashMap<String, String>) -> Result<Vec<String>, Status> {
    let nodes_str = volume_context
        .get("nfs.flint.io/replica-nodes")
        .ok_or_else(|| Status::internal("Missing replica nodes in volume context"))?;
    
    let nodes: Vec<String> = nodes_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    
    if nodes.is_empty() {
        return Err(Status::internal("No replica nodes found in volume context"));
    }
    
    Ok(nodes)
}

/// Create NFS server pod with RWO PVC/PV (HA-capable)
/// 
/// # Parameters
/// - `volume_context`: Full volume metadata from user's PV
/// - `capacity_bytes`: Volume size in bytes
/// - `read_only`: If true, exports volume as read-only (for ROX volumes)
/// 
/// # Architecture
/// - Creates RWO PVC+PV in flint-system namespace
/// - PV uses synthetic volumeHandle to avoid conflicts with user PV
/// - NFS pod mounts this RWO PVC
/// - Leverages multi-replica/RAID for HA
/// - Preferred node affinity for performance
/// 
/// # Zero-Regression Design
/// - Only called when nfs.flint.io/enabled=true in volume_context
/// - Returns early if NFS_ENABLED=false
/// - No modification to existing RWO pod creation
/// - Pod lifecycle managed entirely within this module
pub async fn create_nfs_server_pod(
    kube_client: Client,
    volume_id: &str,
    replica_nodes: &[String],
    volume_context: &HashMap<String, String>,
    capacity_bytes: i64,
    read_only: bool,
    backend: NfsBackend,
) -> Result<(), Status> {
    // SAFETY: Early return if NFS disabled (zero-regression guarantee)
    let config = match NfsConfig::from_env() {
        Some(c) => c,
        None => {
            eprintln!("⚠️  [NFS] Cannot create NFS pod: NFS_ENABLED=false");
            return Err(Status::failed_precondition(
                "NFS support is disabled. Set nfs.enabled=true in Helm values."
            ));
        }
    };
    
    let pod_name = format!("flint-nfs-{}", volume_id);
    let pvc_name = format!("flint-nfs-pvc-{}", volume_id);
    let pv_name = format!("flint-nfs-pv-{}", volume_id);
    
    // Synthetic volumeHandle to avoid conflict with user PV
    let nfs_volume_handle = crate::identity::backing_handle(volume_id);
    
    let mode = if read_only { "ROX (ReadOnlyMany)" } else { "RWX (ReadWriteMany)" };
    let backend_desc = if backend == NfsBackend::EmptyDir { "emptyDir" } else { "RWO PVC" };
    eprintln!("🚀 [NFS] Creating NFS server infrastructure: {}", pod_name);
    eprintln!("   Volume ID: {}", volume_id);
    eprintln!("   Namespace: {}", config.namespace);
    eprintln!("   Access Mode: {}", mode);
    eprintln!("   Backend: {}", backend_desc);
    eprintln!("   Replica nodes: {:?}", replica_nodes);

    if backend == NfsBackend::Pvc {
    // Step 1: Create PV (RWO mode) with synthetic volumeHandle
    eprintln!("📦 [NFS] Step 1: Creating PV for NFS pod (RWO mode)");
    let pv_api: Api<PersistentVolume> = Api::all(kube_client.clone());
    
    let pv = PersistentVolume {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(pv_name.clone()),
            labels: Some([
                ("app".to_string(), "flint-nfs-server".to_string()),
                ("flint.io/volume-id".to_string(), volume_id.to_string()),
            ].into_iter().collect()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeSpec {
            capacity: Some([
                ("storage".to_string(), Quantity(format!("{}", capacity_bytes))),
            ].into_iter().collect()),
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            persistent_volume_reclaim_policy: Some("Retain".to_string()),
            storage_class_name: Some("flint".to_string()),
            claim_ref: Some(ObjectReference {
                namespace: Some(config.namespace.clone()),
                name: Some(pvc_name.clone()),
                ..Default::default()
            }),
            csi: Some(CSIPersistentVolumeSource {
                driver: "flint.csi.storage.io".to_string(),
                volume_handle: nfs_volume_handle.clone(),  // Synthetic handle!
                volume_attributes: Some({
                    let mut attrs: BTreeMap<String, String> = volume_context.iter()
                        .filter(|(k, _)| {
                            // Filter out NFS-specific attributes to prevent recursion
                            // The NFS PV should be treated as a regular RWO volume
                            !k.starts_with("nfs.flint.io/")
                        })
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    // Add original volume ID so CSI driver knows which real volume to mount
                    attrs.insert("originalVolumeId".to_string(), volume_id.to_string());
                    attrs
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    
    match pv_api.create(&PostParams::default(), &pv).await {
        Ok(_) => {
            eprintln!("✅ [NFS] PV created: {}", pv_name);
        }
        Err(e) if e.to_string().contains("AlreadyExists") => {
            eprintln!("ℹ️  [NFS] PV already exists: {}", pv_name);
        }
        Err(e) => {
            eprintln!("❌ [NFS] Failed to create PV: {}", e);
            return Err(Status::internal(format!("Failed to create NFS PV: {}", e)));
        }
    }
    
    // Step 2: Create PVC in flint-system
    eprintln!("📦 [NFS] Step 2: Creating PVC for NFS pod");
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(kube_client.clone(), &config.namespace);
    
    let pvc = PersistentVolumeClaim {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(pvc_name.clone()),
            namespace: Some(config.namespace.clone()),
            labels: Some([
                ("app".to_string(), "flint-nfs-server".to_string()),
                ("flint.io/volume-id".to_string(), volume_id.to_string()),
            ].into_iter().collect()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                requests: Some([
                    ("storage".to_string(), Quantity(format!("{}", capacity_bytes))),
                ].into_iter().collect()),
                ..Default::default()
            }),
            storage_class_name: Some("flint".to_string()),
            volume_name: Some(pv_name.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    
    match pvc_api.create(&PostParams::default(), &pvc).await {
        Ok(_) => {
            eprintln!("✅ [NFS] PVC created: {}", pvc_name);
        }
        Err(e) if e.to_string().contains("AlreadyExists") => {
            eprintln!("ℹ️  [NFS] PVC already exists: {}", pvc_name);
        }
        Err(e) => {
            eprintln!("❌ [NFS] Failed to create PVC: {}", e);
            return Err(Status::internal(format!("Failed to create NFS PVC: {}", e)));
        }
    }
    } // end if backend == NfsBackend::Pvc

    // Create NFS pod
    eprintln!("📦 [NFS] Creating NFS pod (backend: {})", backend_desc);
    
    // Build preferred node affinity to optimize for replica nodes
    // Uses "preferred" (not "required") for HA flexibility:
    // - Scheduler tries replica nodes first (local access via ublk)
    // - Can schedule elsewhere if needed (via NVMe-oF to replica)
    // - Works with multi-replica RAID for HA
    let node_affinity = NodeAffinity {
        preferred_during_scheduling_ignored_during_execution: Some(
            replica_nodes.iter().enumerate().map(|(i, node)| {
                PreferredSchedulingTerm {
                    weight: (replica_nodes.len() as i32) - (i as i32), // Prefer first replica
                    preference: NodeSelectorTerm {
                        match_expressions: Some(vec![NodeSelectorRequirement {
                            key: "kubernetes.io/hostname".to_string(),
                            operator: "In".to_string(),
                            values: Some(vec![node.clone()]),
                        }]),
                        ..Default::default()
                    },
                }
            }).collect()
        ),
        ..Default::default()
    };
    
    // Build resource requirements
    let mut requests = BTreeMap::new();
    requests.insert("memory".to_string(), Quantity(config.resources.memory_request.clone()));
    requests.insert("cpu".to_string(), Quantity(config.resources.cpu_request.clone()));
    
    let mut limits = BTreeMap::new();
    limits.insert("memory".to_string(), Quantity(config.resources.memory_limit.clone()));
    limits.insert("cpu".to_string(), Quantity(config.resources.cpu_limit.clone()));
    
    let resources = ResourceRequirements {
        requests: Some(requests),
        limits: Some(limits),
        ..Default::default()
    };
    
    // Build NFS server pod
    let pod = Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(pod_name.clone()),
            namespace: Some(config.namespace.clone()),
            labels: Some([
                ("app".to_string(), "flint-nfs-server".to_string()),
                ("flint.io/volume-id".to_string(), volume_id.to_string()),
                ("flint.io/component".to_string(), "nfs-server".to_string()),
            ].into_iter().collect()),
            annotations: Some([
                ("flint.io/volume-id".to_string(), volume_id.to_string()),
                ("flint.io/replica-nodes".to_string(), replica_nodes.join(","))
            ].into_iter().collect()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            node_name: if backend == NfsBackend::EmptyDir {
                replica_nodes.first().cloned()
            } else {
                None
            },
            affinity: Some(Affinity {
                node_affinity: Some(node_affinity),
                ..Default::default()
            }),
            
            containers: vec![Container {
                name: "nfs-server".to_string(),
                image: Some(config.image.clone()),
                image_pull_policy: Some(config.pull_policy.clone()),
                // Override entrypoint to use flint-nfs-server instead of csi-driver
                command: Some(vec!["/usr/local/bin/flint-nfs-server".to_string()]),
                args: Some({
                    let mut args = vec![
                        "--export-path".to_string(),
                        "/mnt/volume".to_string(),
                        "--volume-id".to_string(),
                        volume_id.to_string(),
                        "--port".to_string(),
                        config.port.to_string(),
                    ];
                    // Per-op DEBUG logging only when asked for
                    // (NFS_VERBOSE) — it multiplies data-path latency.
                    if config.verbose {
                        args.push("--verbose".to_string());
                    }
                    // Add --read-only flag for ROX volumes
                    if read_only {
                        args.push("--read-only".to_string());
                    }
                    args
                }),
                ports: Some(vec![ContainerPort {
                    name: Some("nfs".to_string()),
                    container_port: config.port as i32,
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                volume_mounts: Some(vec![VolumeMount {
                    name: "volume-data".to_string(),
                    mount_path: "/mnt/volume".to_string(),
                    ..Default::default()
                }]),
                // Stable per-volume file-handle instance id: without it the
                // server mints a boot-time id and rejects every handle held
                // by clients across a pod bounce (permanent EBADHANDLE on
                // live mounts — the cutover bounce becomes an outage).
                env: Some(vec![
                    EnvVar {
                        name: "PNFS_INSTANCE_ID".to_string(),
                        value: Some(stable_nfs_instance_id(volume_id).to_string()),
                        ..Default::default()
                    },
                    // F26 §12: v4 kernel inode filehandles. The server
                    // probes mint/resolve at startup and falls back to
                    // path handles (with a loud warning) if the cap
                    // grant below didn't take effect.
                    EnvVar {
                        name: "FLINT_FH_KERNEL".to_string(),
                        value: Some("1".to_string()),
                        ..Default::default()
                    },
                ]),
                resources: Some(resources),
                // Non-root NFS server (F26 §12 C3, decided 2026-07-19).
                // The caps activate via FILE capabilities on the
                // flint-nfs-server binary (setcap in the Dockerfile);
                // the adds here keep them in the bounding set.
                // DAC_READ_SEARCH: open_by_handle_at (v4 handles);
                // NET_BIND_SERVICE: port 2049; CHOWN/FOWNER/
                // DAC_OVERRIDE: F12 ownership semantics on other-uid
                // files. allowPrivilegeEscalation is DELIBERATELY
                // unset: no_new_privs would block file-cap activation
                // on execve.
                security_context: Some(SecurityContext {
                    run_as_user: Some(65532),
                    run_as_group: Some(65532),
                    capabilities: Some(Capabilities {
                        drop: Some(vec!["ALL".to_string()]),
                        add: Some(vec![
                            "DAC_READ_SEARCH".to_string(),
                            "NET_BIND_SERVICE".to_string(),
                            "CHOWN".to_string(),
                            "FOWNER".to_string(),
                            "DAC_OVERRIDE".to_string(),
                        ]),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            
            volumes: Some(vec![Volume {
                name: "volume-data".to_string(),
                persistent_volume_claim: if backend == NfsBackend::Pvc {
                    Some(PersistentVolumeClaimVolumeSource {
                        claim_name: pvc_name.clone(),
                        read_only: Some(false),
                    })
                } else {
                    None
                },
                host_path: if backend == NfsBackend::EmptyDir {
                    Some(k8s_openapi::api::core::v1::HostPathVolumeSource {
                        path: format!("/var/flint-data/{}", volume_id),
                        type_: Some("DirectoryOrCreate".to_string()),
                    })
                } else {
                    None
                },
                ..Default::default()
            }]),
            
            restart_policy: Some("Always".to_string()),
            
            ..Default::default()
        }),
        ..Default::default()
    };
    
    // Create pod via Kubernetes API
    let pods_api: Api<Pod> = Api::namespaced(kube_client.clone(), &config.namespace);
    
    match pods_api.create(&PostParams::default(), &pod).await {
        Ok(_) => {
            eprintln!("✅ [NFS] NFS server pod created successfully: {}", pod_name);
            eprintln!("   Kubernetes will schedule it to one of: {:?}", replica_nodes);
        }
        Err(e) => {
            eprintln!("❌ [NFS] Failed to create NFS pod: {}", e);
            return Err(Status::internal(format!("Failed to create NFS server pod: {}", e)));
        }
    }
    
    // Create Service for stable NFS endpoint (Longhorn share-manager pattern)
    // This provides a stable DNS name that survives pod restarts
    let service_name = format!("flint-nfs-{}", volume_id);
    let service = Service {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(service_name.clone()),
            namespace: Some(config.namespace.clone()),
            labels: Some([
                ("app".to_string(), "flint-nfs-server".to_string()),
                ("flint.io/volume-id".to_string(), volume_id.to_string()),
            ].into_iter().collect()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some([
                ("app".to_string(), "flint-nfs-server".to_string()),
                ("flint.io/volume-id".to_string(), volume_id.to_string()),
            ].into_iter().collect()),
            ports: Some(vec![ServicePort {
                name: Some("nfs".to_string()),
                port: config.port as i32,
                target_port: Some(IntOrString::Int(config.port as i32)),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            // ClusterIP service (default) - gets stable virtual IP
            // This IP survives pod restarts and rescheduling
            // Note: type field defaults to ClusterIP if not specified
            ..Default::default()
        }),
        ..Default::default()
    };
    
    let services_api: Api<Service> = Api::namespaced(kube_client, &config.namespace);
    
    match services_api.create(&PostParams::default(), &service).await {
        Ok(_) => {
            eprintln!("✅ [NFS] Service created: {}.{}.svc.cluster.local", service_name, config.namespace);
            eprintln!("   Provides stable DNS endpoint for NFS clients");
            Ok(())
        }
        // The per-volume Service outlives the server pod by design (the
        // stable client endpoint) — every server RECREATION therefore hits
        // AlreadyExists here. Treat it as success: the existing Service's
        // selector picks up the new pod. First seen live when the
        // liveness reconciler recreated a killed server (2026-07-04);
        // the publish-path recreation had the same latent failure.
        Err(kube::Error::Api(ref ae)) if ae.code == 409 => {
            eprintln!("ℹ️  [NFS] Service already exists: {}.{}.svc.cluster.local (kept — stable endpoint)", service_name, config.namespace);
            Ok(())
        }
        Err(e) => {
            eprintln!("❌ [NFS] Failed to create Service: {}", e);
            Err(Status::internal(format!("Failed to create NFS service: {}", e)))
        }
    }
}

/// Check if NFS server pod exists for a volume
/// Liveness of the volume's NFS server pod for the publish-time ensure
/// flow. `Terminating` (deletionTimestamp set — phase still reads
/// Running while the pod drains) MUST be distinguished from `Present`:
/// ControllerPublish once raced a graceful server delete, "reused" the
/// Terminating pod, and returned a Service IP whose backend vanished
/// seconds later — the client then hangs mounting a backendless Service
/// and nothing recreates the server (identity Phase-3 drill A′,
/// 2026-07-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsPodLiveness {
    Absent,
    Terminating,
    Present,
}

pub async fn nfs_pod_liveness(
    kube_client: Client,
    volume_id: &str,
) -> Result<NfsPodLiveness, Status> {
    // SAFETY: Early return if NFS disabled
    let config = match NfsConfig::from_env() {
        Some(c) => c,
        None => return Ok(NfsPodLiveness::Absent),  // NFS disabled, pod doesn't exist
    };

    let pod_name = format!("flint-nfs-{}", volume_id);
    let pods_api: Api<Pod> = Api::namespaced(kube_client, &config.namespace);

    match pods_api.get(&pod_name).await {
        Ok(pod) => {
            if pod.metadata.deletion_timestamp.is_some() {
                Ok(NfsPodLiveness::Terminating)
            } else {
                Ok(NfsPodLiveness::Present)
            }
        }
        Err(e) if e.to_string().contains("NotFound") => Ok(NfsPodLiveness::Absent),
        Err(e) => {
            eprintln!("⚠️  [NFS] Error checking pod existence: {}", e);
            Err(Status::internal(format!("Failed to check NFS pod: {}", e)))
        }
    }
}

/// Bounded wait for a Terminating NFS server pod to fully exit so the
/// deterministic pod name frees up for recreation. Returns true when the
/// pod is gone; false when it is still draining at the deadline (the
/// caller's create will then 409 and the CO retries the publish — a
/// bounded, honest failure instead of binding clients to a dying pod).
pub async fn wait_for_nfs_pod_gone(
    kube_client: Client,
    volume_id: &str,
    deadline_secs: u64,
) -> bool {
    let config = match NfsConfig::from_env() {
        Some(c) => c,
        None => return true,
    };
    let pod_name = format!("flint-nfs-{}", volume_id);
    let pods_api: Api<Pod> = Api::namespaced(kube_client, &config.namespace);
    let attempts = deadline_secs.div_ceil(2).max(1);
    for _ in 0..attempts {
        match pods_api.get(&pod_name).await {
            Err(e) if e.to_string().contains("NotFound") => return true,
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    false
}

/// Wait for NFS server pod to become ready and return (node_name, service_endpoint)
/// 
/// Returns the Service ClusterIP (stable endpoint) instead of pod IP
/// 
/// # Timeout
/// Waits up to 60 seconds for pod to be ready
pub async fn wait_for_nfs_pod_ready(
    kube_client: Client,
    volume_id: &str,
) -> Result<(String, String), Status> {
    // SAFETY: Early return if NFS disabled
    let config = match NfsConfig::from_env() {
        Some(c) => c,
        None => {
            return Err(Status::failed_precondition("NFS support is disabled"));
        }
    };
    
    let pod_name = format!("flint-nfs-{}", volume_id);
    let service_name = format!("flint-nfs-{}", volume_id);
    let pods_api: Api<Pod> = Api::namespaced(kube_client.clone(), &config.namespace);
    let services_api: Api<Service> = Api::namespaced(kube_client, &config.namespace);
    
    eprintln!("⏳ [NFS] Waiting for NFS pod and service to be ready: {}", pod_name);
    
    // Wait up to 60 seconds
    for attempt in 1..=60 {
        match pods_api.get(&pod_name).await {
            Ok(pod) => {
                // A draining pod still reports phase Running — never hand
                // its endpoint to a new client (drill A′ race).
                if pod.metadata.deletion_timestamp.is_some() {
                    eprintln!("⏳ [NFS] Pod {} is Terminating — not treating as ready (attempt {})", pod_name, attempt);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                if let Some(status) = &pod.status {
                    // Check if pod is running and has IP
                    if let (Some(phase), Some(pod_ip)) = (&status.phase, &status.pod_ip) {
                        if phase == "Running" {
                            // Get the node it was scheduled to
                            if let Some(node_name) = pod.spec
                                .as_ref()
                                .and_then(|s| s.node_name.as_ref()) 
                            {
                                // Get Service ClusterIP (stable virtual IP)
                                match services_api.get(&service_name).await {
                                    Ok(svc) => {
                                        if let Some(spec) = &svc.spec {
                                            if let Some(cluster_ip) = &spec.cluster_ip {
                                                eprintln!("✅ [NFS] Pod ready!");
                                                eprintln!("   Node: {}", node_name);
                                                eprintln!("   Pod IP: {}", pod_ip);
                                                eprintln!("   Service ClusterIP: {}", cluster_ip);
                                                eprintln!("   Service DNS: {}.{}.svc.cluster.local", service_name, config.namespace);
                                                eprintln!("   Attempts: {}/60", attempt);
                                                return Ok((node_name.clone(), cluster_ip.clone()));
                                            }
                                        }
                                        eprintln!("   Attempt {}/60: Service exists but no ClusterIP yet", attempt);
                                    }
                                    Err(_) => {
                                        eprintln!("   Attempt {}/60: Service not ready yet", attempt);
                                    }
                                }
                            }
                        } else {
                            eprintln!("   Attempt {}/60: Pod phase: {}", attempt, phase);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("   Attempt {}/60: {}", attempt, e);
            }
        }
        
        sleep(Duration::from_secs(1)).await;
    }
    
    Err(Status::deadline_exceeded(
        format!("NFS pod {} did not become ready within 60 seconds", pod_name)
    ))
}

/// Delete NFS server infrastructure (Pod, Service, PVC, PV) for a volume
/// 
/// # Safety
/// - Only deletes resources with label flint.io/volume-id=<volume_id>
/// - Safe to call even if resources don't exist
pub async fn delete_nfs_server_pod(
    kube_client: Client,
    volume_id: &str,
) -> Result<(), Status> {
    // SAFETY: Early return if NFS disabled
    let config = match NfsConfig::from_env() {
        Some(c) => c,
        None => return Ok(()),  // NFS disabled, nothing to delete
    };
    
    let pod_name = format!("flint-nfs-{}", volume_id);
    let service_name = format!("flint-nfs-{}", volume_id);
    let pvc_name = format!("flint-nfs-pvc-{}", volume_id);
    let pv_name = format!("flint-nfs-pv-{}", volume_id);
    
    eprintln!("🗑️  [NFS] Deleting NFS infrastructure for volume: {}", volume_id);
    
    // Delete Service first
    let services_api: Api<Service> = Api::namespaced(kube_client.clone(), &config.namespace);
    match services_api.delete(&service_name, &DeleteParams::default()).await {
        Ok(_) => {
            eprintln!("✅ [NFS] Service deleted: {}", service_name);
        }
        Err(e) if e.to_string().contains("NotFound") => {
            eprintln!("ℹ️  [NFS] Service already deleted: {}", service_name);
        }
        Err(e) => {
            eprintln!("⚠️  [NFS] Failed to delete Service: {}", e);
        }
    }
    
    // Delete Pod
    let pods_api: Api<Pod> = Api::namespaced(kube_client.clone(), &config.namespace);
    let mut pod_was_present = false;
    match pods_api.delete(&pod_name, &DeleteParams::default()).await {
        Ok(_) => {
            pod_was_present = true;
            eprintln!("✅ [NFS] Pod delete issued: {}", pod_name);
        }
        Err(e) if e.to_string().contains("NotFound") => {
            eprintln!("ℹ️  [NFS] Pod already deleted: {}", pod_name);
        }
        Err(e) => {
            eprintln!("⚠️  [NFS] Failed to delete pod: {}", e);
        }
    }

    // ORDERING: the NFS server pod is this volume's consumer — its dying
    // flush (dirty ext4 journal on the backing raid) goes through the
    // NVMe-oF target our caller tears down as soon as we return. Tearing
    // the target down first strands the kernel initiator in a reconnect
    // loop against a vanished subsystem with the journal pinned in
    // D-state; the pod then cannot be killed until ctrl_loss_tmo expires
    // (~10 minutes — observed live on the v1.5.0 gate teardown). Kubelet
    // removes the Pod object only after container shutdown and volume
    // unmount, i.e. after dirty data is flushed — so wait (bounded) for
    // the object to go away before letting target teardown proceed.
    if pod_was_present {
        let mut pod_gone = false;
        for _ in 0..45 {
            match pods_api.get(&pod_name).await {
                Err(e) if e.to_string().contains("NotFound") => {
                    pod_gone = true;
                    break;
                }
                Ok(_) => {}
                // Transient API error: keep waiting, the deadline bounds us.
                Err(e) => eprintln!("⚠️  [NFS] Pod termination poll error: {}", e),
            }
            sleep(Duration::from_secs(2)).await;
        }
        if pod_gone {
            eprintln!("✅ [NFS] Pod terminated (volume flushed and unmounted): {}", pod_name);
        } else {
            eprintln!(
                "⚠️  [NFS] Pod {} still terminating after 90s — proceeding with volume \
                 teardown; its initiator may reconnect-loop until ctrl_loss_tmo",
                pod_name
            );
        }
    }

    // Delete PVC
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(kube_client.clone(), &config.namespace);
    match pvc_api.delete(&pvc_name, &DeleteParams::default()).await {
        Ok(_) => {
            eprintln!("✅ [NFS] PVC deleted: {}", pvc_name);
        }
        Err(e) if e.to_string().contains("NotFound") => {
            eprintln!("ℹ️  [NFS] PVC already deleted: {}", pvc_name);
        }
        Err(e) => {
            eprintln!("⚠️  [NFS] Failed to delete PVC: {}", e);
        }
    }
    
    // Delete PV
    let pv_api: Api<PersistentVolume> = Api::all(kube_client);
    match pv_api.delete(&pv_name, &DeleteParams::default()).await {
        Ok(_) => {
            eprintln!("✅ [NFS] PV deleted: {}", pv_name);
        }
        Err(e) if e.to_string().contains("NotFound") => {
            eprintln!("ℹ️  [NFS] PV already deleted: {}", pv_name);
        }
        Err(e) => {
            eprintln!("⚠️  [NFS] Failed to delete PV: {}", e);
        }
    }
    
    // Don't fail volume deletion if NFS resource cleanup fails
    // User can manually clean up if needed
    Ok(())
}

// ---------------------------------------------------------------------------
// NFS server-pod liveness reconciler
//
// Closes the availability gap the identity contract recorded as an open
// item: a bare server-pod death was only healed by the NEXT client
// ControllerPublish or a cutover trigger. An RWX volume with stable,
// long-lived clients and no new publishes therefore hung indefinitely —
// contained (bounded stats, assume-mounted teardown, no corpse-binding)
// but never recovered. The reconciler recreates an Absent server for any
// pvc-backed NFS volume that still has client attachments, through the
// exact ensure machinery ControllerPublish uses.
//
// Deliberately NOT reconciled: emptydir-backed volumes. Their share dies
// with the pod (ephemeral by contract); auto-recreating would silently
// swap a hung mount for an EMPTY export. They keep the legacy
// next-publish semantics.
// ---------------------------------------------------------------------------

/// What the reconciler should do for one volume this tick. Pure — the
/// truth table is pinned in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsReconcileAction {
    Skip(&'static str),
    Recreate,
}

pub fn nfs_reconcile_decision(
    backend_is_emptydir: bool,
    pv_terminating: bool,
    attachment_count: usize,
    liveness: NfsPodLiveness,
) -> NfsReconcileAction {
    if backend_is_emptydir {
        return NfsReconcileAction::Skip("emptydir backend — ephemeral by contract, recreation belongs to the next publish");
    }
    if pv_terminating {
        return NfsReconcileAction::Skip("PV deleting — DeleteVolume owns teardown");
    }
    if attachment_count == 0 {
        return NfsReconcileAction::Skip("no client attachments — the next publish creates the server");
    }
    match liveness {
        NfsPodLiveness::Present => NfsReconcileAction::Skip("server pod present"),
        NfsPodLiveness::Terminating => {
            NfsReconcileAction::Skip("server pod terminating — recreate once it has exited")
        }
        NfsPodLiveness::Absent => NfsReconcileAction::Recreate,
    }
}

/// One reconcile pass over all flint NFS-backed user PVs. Returns the
/// number of servers recreated (for logging/tests of the loop).
pub async fn nfs_reconciler_pass(kube_client: &Client, source_node: &str) -> usize {
    use k8s_openapi::api::storage::v1::VolumeAttachment;
    use kube::api::ListParams;

    let pv_api: Api<PersistentVolume> = Api::all(kube_client.clone());
    let va_api: Api<VolumeAttachment> = Api::all(kube_client.clone());

    let (pvs, vas) = match (
        pv_api.list(&ListParams::default()).await,
        va_api.list(&ListParams::default()).await,
    ) {
        (Ok(p), Ok(v)) => (p, v),
        (p, v) => {
            eprintln!(
                "⚠️  [NFS-RECONCILER] Listing failed (pv ok: {}, va ok: {}) — skipping this pass",
                p.is_ok(),
                v.is_ok()
            );
            return 0;
        }
    };

    // Client attachments per PV name. Attachment INTENT counts (attached
    // or still attaching): either way a client is depending on the share.
    let mut attachments: HashMap<String, usize> = HashMap::new();
    for va in &vas.items {
        if let Some(pv_name) = va.spec.source.persistent_volume_name.as_ref() {
            *attachments.entry(pv_name.clone()).or_insert(0) += 1;
        }
    }

    let mut recreated = 0;
    for pv in &pvs.items {
        let Some(name) = pv.metadata.name.as_ref() else { continue };
        let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) else { continue };
        if csi.driver != "flint.csi.storage.io" {
            continue;
        }
        let attrs = csi.volume_attributes.as_ref();
        let nfs_enabled = attrs
            .and_then(|a| a.get("nfs.flint.io/enabled"))
            .map(|v| v == "true")
            .unwrap_or(false);
        if !nfs_enabled {
            // Backing PVs are automatically excluded here too: their
            // attributes are minted with every nfs.flint.io/* key filtered
            // out (create_nfs_server_pod), so only USER shared PVs match.
            continue;
        }
        let backend_is_emptydir = attrs
            .and_then(|a| a.get("nfs.flint.io/backend"))
            .map(|v| v == "emptydir")
            .unwrap_or(false);
        let pv_terminating = pv.metadata.deletion_timestamp.is_some();
        let n_attached = attachments.get(name).copied().unwrap_or(0);

        let liveness = match nfs_pod_liveness(kube_client.clone(), name).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("⚠️  [NFS-RECONCILER] {}: liveness check failed ({}); skipping", name, e);
                continue;
            }
        };

        match nfs_reconcile_decision(backend_is_emptydir, pv_terminating, n_attached, liveness) {
            NfsReconcileAction::Skip(_) => {}
            NfsReconcileAction::Recreate => {
                println!(
                    "🩺 [NFS-RECONCILER] Server pod for {} is ABSENT with {} client attachment(s) — recreating",
                    name, n_attached
                );
                let ctx: HashMap<String, String> = attrs
                    .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                let replica_nodes = match parse_replica_nodes(&ctx) {
                    Ok(nodes) => nodes,
                    Err(e) => {
                        eprintln!(
                            "⚠️  [NFS-RECONCILER] {}: cannot reconstruct replica nodes ({}); skipping",
                            name, e
                        );
                        continue;
                    }
                };
                // Same capacity derivation as the publish path (context
                // "size", Gi suffix or raw bytes; publish's 1 GiB default).
                let capacity_bytes = ctx
                    .get("size")
                    .and_then(|s| {
                        if s.ends_with("Gi") {
                            s.trim_end_matches("Gi").parse::<i64>().ok().map(|v| v * 1024 * 1024 * 1024)
                        } else {
                            s.parse::<i64>().ok()
                        }
                    })
                    .unwrap_or(1073741824);
                // ROX ⇔ the PV can only ever be shared read-only (RWM wins
                // when both are present — same dominance as role_from_modes).
                let modes = pv
                    .spec
                    .as_ref()
                    .and_then(|s| s.access_modes.as_ref())
                    .cloned()
                    .unwrap_or_default();
                let is_rox = !modes.iter().any(|m| m == "ReadWriteMany")
                    && modes.iter().any(|m| m == "ReadOnlyMany");

                match create_nfs_server_pod(
                    kube_client.clone(),
                    name,
                    &replica_nodes,
                    &ctx,
                    capacity_bytes,
                    is_rox,
                    NfsBackend::Pvc,
                )
                .await
                {
                    Ok(()) => {
                        recreated += 1;
                        crate::replica_sync::emit_pv_event(
                            kube_client,
                            source_node,
                            name,
                            "Normal",
                            "NfsServerPodRecreated",
                            &format!(
                                "NFS server pod was absent while {} client attachment(s) depended on it — \
                                 recreated by the liveness reconciler (the pod died outside any \
                                 publish/cutover event). Client mounts resume via the stable Service \
                                 endpoint once the server is Ready.",
                                n_attached
                            ),
                        )
                        .await;
                    }
                    Err(e) => {
                        // AlreadyExists = lost a benign race against a
                        // concurrent ControllerPublish ensure — fine.
                        eprintln!("⚠️  [NFS-RECONCILER] {}: recreate failed ({}); retrying next tick", name, e);
                    }
                }
            }
        }
    }

    // F22 inverse sweep: NFS infrastructure whose VOLUME no longer exists.
    // The normal DeleteVolume path removes service+pod+companions, but any
    // unhappy path that skips it (controller crash mid-delete, operator
    // finalizer surgery) leaks them. A leaked SERVICE is the dangerous
    // half: its ClusterIP stays allocated with no endpoints, and any
    // straggler client mount then BLACKHOLES (Cilium drops endpoint-less
    // service traffic — no RST). The orphan client's 60s RPC timeout
    // cycles stall the node's shared SUNRPC workqueues and freeze every
    // LIVE NFS mount on that node in sympathy — the phase-3 bulk-load
    // wedge. Sweeping the service turns the blackhole into fast RST
    // failures at worst, and normally removes it before any client
    // notices.
    if let Some(config) = NfsConfig::from_env() {
        let live_pvs: std::collections::HashSet<&str> = pvs
            .items
            .iter()
            .filter_map(|p| p.metadata.name.as_deref())
            .collect();
        let services_api: Api<Service> = Api::namespaced(kube_client.clone(), &config.namespace);
        let lp = ListParams::default().labels("flint.io/volume-id");
        if let Ok(svcs) = services_api.list(&lp).await {
            for svc in &svcs.items {
                let Some(vol) = svc
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("flint.io/volume-id"))
                else {
                    continue;
                };
                if live_pvs.contains(vol.as_str()) {
                    continue;
                }
                eprintln!(
                    "🧹 [NFS-RECONCILER] NFS infra for {} has no PV — sweeping leaked service/pod/companions",
                    vol
                );
                if let Err(e) = delete_nfs_server_pod(kube_client.clone(), vol).await {
                    eprintln!("⚠️  [NFS-RECONCILER] sweep of {} failed: {}", vol, e);
                }
            }
        }
    }

    recreated
}

/// Controller-role loop. Enabled by default — it only acts on the
/// unambiguous state [pvc-backed NFS user PV, not deleting, ≥1 client
/// attachment, server pod Absent] and only through the same ensure path
/// ControllerPublish runs. Opt out with FLINT_NFS_RECONCILER=disabled.
pub fn nfs_reconciler_enabled() -> bool {
    env::var("FLINT_NFS_RECONCILER")
        .map(|v| v != "disabled")
        .unwrap_or(true)
}

pub async fn run_nfs_server_reconciler(kube_client: Client, source_node: String) {
    // NFS entirely off ⇒ nothing to reconcile, don't even loop.
    if !is_nfs_enabled() {
        println!("ℹ️ [NFS-RECONCILER] NFS_ENABLED=false — reconciler idle");
        return;
    }
    println!("🩺 [NFS-RECONCILER] NFS server-pod liveness reconciler running (30s tick; FLINT_NFS_RECONCILER=disabled to opt out)");
    loop {
        let n = nfs_reconciler_pass(&kube_client, &source_node).await;
        if n > 0 {
            println!("🩺 [NFS-RECONCILER] Recreated {} NFS server pod(s) this pass", n);
        }
        sleep(Duration::from_secs(30)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reconciler acts on exactly one state: pvc-backed, PV alive,
    /// clients attached, server pod Absent. Everything else is a Skip —
    /// emptydir NEVER recreates (an auto-recreated emptydir share would
    /// silently replace a hung mount with an empty export).
    #[test]
    fn nfs_reconcile_truth_table() {
        use NfsPodLiveness::*;
        use NfsReconcileAction::*;
        // The one Recreate cell.
        assert_eq!(nfs_reconcile_decision(false, false, 2, Absent), Recreate);
        assert_eq!(nfs_reconcile_decision(false, false, 1, Absent), Recreate);
        // Liveness gates.
        assert!(matches!(nfs_reconcile_decision(false, false, 2, Present), Skip(_)));
        assert!(matches!(nfs_reconcile_decision(false, false, 2, Terminating), Skip(_)));
        // No clients — next publish owns creation.
        assert!(matches!(nfs_reconcile_decision(false, false, 0, Absent), Skip(_)));
        // PV deleting — DeleteVolume owns teardown.
        assert!(matches!(nfs_reconcile_decision(false, true, 2, Absent), Skip(_)));
        // emptydir: never, regardless of everything else.
        assert!(matches!(nfs_reconcile_decision(true, false, 5, Absent), Skip(_)));
    }

    #[test]
    fn nfs_reconciler_gate_default_and_optout() {
        // Default (unset) = enabled; only the literal "disabled" opts out.
        // NOTE: env-var manipulation is process-global — this test only
        // asserts the pure default since the suite runs multi-threaded.
        assert!(nfs_reconciler_enabled());
    }

    #[test]
    fn nfs_instance_id_stable_and_per_volume() {
        // Same volume → same id across calls (and across server incarnations);
        // different volumes → different ids (handles must not cross-resolve).
        let a1 = stable_nfs_instance_id("pvc-aaa");
        let a2 = stable_nfs_instance_id("pvc-aaa");
        let b = stable_nfs_instance_id("pvc-bbb");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_ne!(a1, 0);
    }
}
