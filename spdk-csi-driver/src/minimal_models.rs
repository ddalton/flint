// minimal_models.rs - Minimal data structures for pure SPDK approach
// These replace the Kubernetes CRD structures

use serde::{Deserialize, Serialize};

/// Essential volume creation information (no CRDs needed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub volume_id: String,
    pub size_bytes: u64,
    pub replica_count: u32,
    pub storage_class: String,
}

/// Volume replica information from SPDK
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaInfo {
    pub node_name: String,
    pub disk_pci_address: String,
    pub lvol_uuid: String,
    pub lvol_name: String,
    pub lvs_name: String,
    pub nqn: Option<String>,
    pub target_ip: Option<String>,
    pub target_port: Option<u16>,
    pub health: String, // "online", "degraded", "failed"
}

/// Disk information from SPDK (no CRD needed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub node_name: String,
    pub pci_address: String,
    pub device_name: String, // e.g. "nvme3n1"
    pub bdev_name: String,   // e.g. "uring_nvme3n1"
    pub size_bytes: u64,
    pub free_space: u64,
    pub model: String,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub healthy: bool,
    pub blobstore_initialized: bool,
    pub lvs_name: Option<String>,
    pub lvol_count: u32,
}

/// Volume information aggregated from SPDK
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub volume_id: String,
    pub size_bytes: u64,
    pub replicas: Vec<ReplicaInfo>,
    pub health: String, // "healthy", "degraded", "failed"
    pub created_at: String,
}

/// Volume creation result with replica information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeCreationResult {
    pub volume_id: String,
    pub size_bytes: u64,
    pub replicas: Vec<ReplicaInfo>,
}

/// Node information for cluster discovery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_name: String,
    pub pod_ip: String,
    pub rpc_url: String,
    pub status: String, // "ready", "not_ready", "unknown"
}

/// Cluster state summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterState {
    pub nodes: Vec<NodeInfo>,
    pub disks: Vec<DiskInfo>,
    pub volumes: Vec<VolumeInfo>,
    pub total_capacity: u64,
    pub free_capacity: u64,
    pub healthy_disks: u32,
    pub healthy_volumes: u32,
}

/// Storage class parameters (minimal)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageClassParams {
    pub num_replicas: u32,
    pub transport: Option<String>,
    pub target_port: Option<u16>,
}

impl StorageClassParams {
    pub fn from_parameters(params: &std::collections::HashMap<String, String>) -> Self {
        Self {
            num_replicas: params
                .get("numReplicas")
                .and_then(|n| n.parse().ok())
                .unwrap_or(1),
            transport: params.get("transport").cloned(),
            target_port: params
                .get("targetPort")
                .and_then(|p| p.parse().ok()),
        }
    }
}

/// Error types for minimal state operations
#[derive(Debug)]
pub enum MinimalStateError {
    DiskNotFound { node: String, pci: String },
    VolumeNotFound { volume_id: String },
    InsufficientCapacity { required: u64, available: u64 },
    NodeSeparationFailed { required: u32, available: u32 },
    InsufficientNodes { required: u32, available: u32, message: String },
    RaidCreationFailed { message: String, available_replicas: u32, required_replicas: u32 },
    InvalidParameter { message: String },
    SpdkRpcError { message: String },
    KubernetesError { message: String },
    SerializationError { message: String },
    DeserializationError { message: String },
    HttpError { message: String },
    InternalError { message: String },
}

impl std::fmt::Display for MinimalStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MinimalStateError::DiskNotFound { node, pci } => 
                write!(f, "Disk not found: {}:{}", node, pci),
            MinimalStateError::VolumeNotFound { volume_id } => 
                write!(f, "Volume not found: {}", volume_id),
            MinimalStateError::InsufficientCapacity { required, available } => 
                write!(f, "Insufficient capacity: need {}, have {}", required, available),
            MinimalStateError::NodeSeparationFailed { required, available } => 
                write!(f, "Node separation failed: need {} nodes, have {}", required, available),
            MinimalStateError::InsufficientNodes { required, available, message } => 
                write!(f, "Insufficient nodes: need {}, have {} - {}", required, available, message),
            MinimalStateError::RaidCreationFailed { message, available_replicas, required_replicas } => 
                write!(f, "RAID creation failed: {}/{} replicas - {}", available_replicas, required_replicas, message),
            MinimalStateError::InvalidParameter { message } => 
                write!(f, "Invalid parameter: {}", message),
            MinimalStateError::SpdkRpcError { message } => 
                write!(f, "SPDK RPC error: {}", message),
                MinimalStateError::KubernetesError { message } => 
                    write!(f, "Kubernetes error: {}", message),
                MinimalStateError::SerializationError { message } => 
                    write!(f, "Serialization error: {}", message),
                MinimalStateError::DeserializationError { message } => 
                    write!(f, "Deserialization error: {}", message),
                MinimalStateError::HttpError { message } => 
                    write!(f, "HTTP error: {}", message),
                MinimalStateError::InternalError { message } => 
                    write!(f, "Internal error: {}", message),
        }
    }
}

impl std::error::Error for MinimalStateError {}
