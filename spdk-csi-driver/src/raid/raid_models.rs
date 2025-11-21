// raid/raid_models.rs - Data structures for RAID functionality

use serde::{Deserialize, Serialize};
use crate::minimal_models::DiskInfo;

/// Node and disk selection for a replica
#[derive(Debug, Clone)]
pub struct NodeDiskSelection {
    pub node_name: String,
    pub disk: DiskInfo,
}

/// NVMe-oF connection information for remote replicas
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeofConnectionInfo {
    pub nqn: String,
    pub target_ip: String,
    pub target_port: u16,
    pub transport: String, // "TCP" or "RDMA"
}

/// RAID rebuild status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaidRebuildStatus {
    pub raid_name: String,
    pub rebuilding: bool,
    pub progress_percent: f32,
    pub estimated_time_remaining_sec: Option<u64>,
}

/// RAID health status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaidHealthStatus {
    pub raid_name: String,
    pub status: String, // "online", "degraded", "failed"
    pub total_replicas: u32,
    pub online_replicas: u32,
    pub failed_replicas: Vec<String>,
}

