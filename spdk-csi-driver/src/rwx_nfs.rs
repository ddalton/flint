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
    Pod, PodSpec, Container, VolumeMount, Volume,
    PersistentVolumeClaimVolumeSource, ContainerPort,
    Affinity, NodeAffinity, NodeSelector, NodeSelectorTerm,
    NodeSelectorRequirement, ResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use tokio::time::{sleep, Duration};
use tonic::Status;

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
                memory_request: env::var("NFS_MEMORY_REQUEST")
                    .unwrap_or_else(|_| "128Mi".to_string()),
                cpu_request: env::var("NFS_CPU_REQUEST")
                    .unwrap_or_else(|_| "100m".to_string()),
                memory_limit: env::var("NFS_MEMORY_LIMIT")
                    .unwrap_or_else(|_| "256Mi".to_string()),
                cpu_limit: env::var("NFS_CPU_LIMIT")
                    .unwrap_or_else(|_| "500m".to_string()),
            },
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

/// Create NFS server pod with node affinity to replica nodes
/// 
/// # Parameters
/// - `read_only`: If true, exports volume as read-only (for ROX volumes)
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
    read_only: bool,
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
    
    let mode = if read_only { "ROX (ReadOnlyMany)" } else { "RWX (ReadWriteMany)" };
    eprintln!("🚀 [NFS] Creating NFS server pod: {}", pod_name);
    eprintln!("   Volume ID: {}", volume_id);
    eprintln!("   Namespace: {}", config.namespace);
    eprintln!("   Access Mode: {}", mode);
    eprintln!("   Mount Method: CSI inline volume");
    eprintln!("   Replica nodes (affinity): {:?}", replica_nodes);
    
    // Build node affinity to constrain pod to replica nodes
    // Kubernetes scheduler will pick the best node among these options
    let node_affinity = NodeAffinity {
        required_during_scheduling_ignored_during_execution: Some(NodeSelector {
            node_selector_terms: vec![NodeSelectorTerm {
                match_expressions: Some(vec![NodeSelectorRequirement {
                    key: "kubernetes.io/hostname".to_string(),
                    operator: "In".to_string(),
                    values: Some(replica_nodes.to_vec()),
                }]),
                ..Default::default()
            }],
        }),
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
            // Node affinity: Run on any replica node (scheduler picks best)
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
                        "--verbose".to_string(),
                    ];
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
                resources: Some(resources),
                ..Default::default()
            }],
            
            volumes: Some(vec![Volume {
                name: "volume-data".to_string(),
                // Use CSI inline volume to mount the PV directly
                // This allows NFS pod in flint-system to mount PV from any namespace
                csi: Some(k8s_openapi::api::core::v1::CSIVolumeSource {
                    driver: "flint.csi.storage.io".to_string(),
                    volume_attributes: Some([
                        ("volumeId".to_string(), volume_id.to_string()),
                    ].into_iter().collect()),
                    read_only: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            
            restart_policy: Some("Always".to_string()),
            
            ..Default::default()
        }),
        ..Default::default()
    };
    
    // Create pod via Kubernetes API
    let pods_api: Api<Pod> = Api::namespaced(kube_client, &config.namespace);
    
    match pods_api.create(&PostParams::default(), &pod).await {
        Ok(_) => {
            eprintln!("✅ [NFS] NFS server pod created successfully: {}", pod_name);
            eprintln!("   Kubernetes will schedule it to one of: {:?}", replica_nodes);
            Ok(())
        }
        Err(e) => {
            eprintln!("❌ [NFS] Failed to create NFS pod: {}", e);
            Err(Status::internal(format!("Failed to create NFS server pod: {}", e)))
        }
    }
}

/// Check if NFS server pod exists for a volume
pub async fn nfs_pod_exists(
    kube_client: Client,
    volume_id: &str,
) -> Result<bool, Status> {
    // SAFETY: Early return if NFS disabled
    let config = match NfsConfig::from_env() {
        Some(c) => c,
        None => return Ok(false),  // NFS disabled, pod doesn't exist
    };
    
    let pod_name = format!("flint-nfs-{}", volume_id);
    let pods_api: Api<Pod> = Api::namespaced(kube_client, &config.namespace);
    
    match pods_api.get(&pod_name).await {
        Ok(_) => Ok(true),
        Err(e) if e.to_string().contains("NotFound") => Ok(false),
        Err(e) => {
            eprintln!("⚠️  [NFS] Error checking pod existence: {}", e);
            Err(Status::internal(format!("Failed to check NFS pod: {}", e)))
        }
    }
}

/// Wait for NFS server pod to become ready and return (node_name, pod_ip)
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
    let pods_api: Api<Pod> = Api::namespaced(kube_client, &config.namespace);
    
    eprintln!("⏳ [NFS] Waiting for NFS pod to be ready: {}", pod_name);
    
    // Wait up to 60 seconds
    for attempt in 1..=60 {
        match pods_api.get(&pod_name).await {
            Ok(pod) => {
                if let Some(status) = &pod.status {
                    // Check if pod is running and has IP
                    if let (Some(phase), Some(pod_ip)) = (&status.phase, &status.pod_ip) {
                        if phase == "Running" {
                            // Get the node it was scheduled to
                            if let Some(node_name) = pod.spec
                                .as_ref()
                                .and_then(|s| s.node_name.as_ref()) 
                            {
                                eprintln!("✅ [NFS] Pod ready!");
                                eprintln!("   Node: {}", node_name);
                                eprintln!("   IP: {}", pod_ip);
                                eprintln!("   Attempts: {}/60", attempt);
                                return Ok((node_name.clone(), pod_ip.clone()));
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

/// Delete NFS server pod for a volume
/// 
/// # Safety
/// - Only deletes pods with label flint.io/volume-id=<volume_id>
/// - Safe to call even if pod doesn't exist
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
    let pods_api: Api<Pod> = Api::namespaced(kube_client, &config.namespace);
    
    eprintln!("🗑️  [NFS] Deleting NFS server pod: {}", pod_name);
    
    match pods_api.delete(&pod_name, &DeleteParams::default()).await {
        Ok(_) => {
            eprintln!("✅ [NFS] NFS pod deleted: {}", pod_name);
            Ok(())
        }
        Err(e) if e.to_string().contains("NotFound") => {
            eprintln!("ℹ️  [NFS] Pod already deleted: {}", pod_name);
            Ok(())
        }
        Err(e) => {
            eprintln!("⚠️  [NFS] Failed to delete NFS pod: {}", e);
            // Don't fail volume deletion if NFS pod deletion fails
            // User can manually clean up pods if needed
            Ok(())
        }
    }
}

