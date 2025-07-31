// models.rs - Common data structures used across the SPDK CSI driver
use kube::CustomResource;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use chrono::{DateTime, Utc};

// ============================================================================
// SPDK VOLUME RELATED STRUCTURES
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[kube(group = "flint.csi.storage.io", version = "v1", kind = "SpdkVolume", plural = "spdkvolumes")]
#[kube(namespaced)]
#[kube(status = "SpdkVolumeStatus")]
pub struct SpdkVolumeSpec {
    pub volume_id: String,
    pub size_bytes: i64,
    pub num_replicas: i32,
    pub replicas: Vec<Replica>,
    pub primary_lvol_uuid: Option<String>,
    pub write_ordering_enabled: bool,
    pub raid_auto_rebuild: bool,
    pub nvmeof_transport: Option<String>,
    pub nvmeof_target_port: Option<u16>,
    // Scheduling and optimization fields
    pub scheduling_policy: Option<String>,
    pub preferred_nodes: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct Replica {
    pub node: String,
    pub replica_type: String,
    pub pcie_addr: Option<String>,
    pub nqn: Option<String>,
    pub ip: Option<String>,
    pub port: Option<String>,
    pub local_pod_scheduled: bool,
    pub pod_name: Option<String>,
    pub disk_ref: String,
    pub lvol_uuid: Option<String>,
    pub health_status: ReplicaHealth,
    pub last_io_timestamp: Option<String>,
    pub write_sequence: u64,
    pub raid_member_index: usize,
    pub raid_member_state: RaidMemberState,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RaidMemberState {
    #[default]
    Online,
    Degraded,
    Failed,
    Rebuilding,
    Spare,
    Removing,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReplicaHealth {
    #[default]
    Healthy,
    Degraded,
    Failed,
    Rebuilding,
    Syncing,
}

// models.rs - Add ublk device info to SpdkVolumeStatus

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct SpdkVolumeStatus {
    pub state: String,
    pub degraded: bool,
    pub last_checked: String,
    pub active_replicas: Vec<usize>,
    pub failed_replicas: Vec<usize>,
    pub write_sequence: u64,
    pub last_successful_write: Option<String>,
    pub raid_status: Option<RaidStatus>,
    pub nvmeof_targets: Vec<NvmeofTarget>,
    
    // Add ublk device information
    pub ublk_device: Option<UblkDevice>,
    
    // Existing scheduling and optimization tracking fields
    pub scheduled_node: Option<String>,
    pub has_local_replica: bool,
    pub scheduling_policy: Option<String>,
    pub replica_nodes: Vec<String>,
    pub read_optimized: bool,
    pub read_policy: Option<String>,
    pub local_replica_performance: Option<LocalReplicaMetrics>,
}

// Add new struct for ublk device information
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct UblkDevice {
    pub id: u32,
    pub device_path: String,
    pub created_at: String,
    pub node: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct NvmeofTarget {
    pub nqn: String,
    pub transport: String,
    pub target_addr: String,
    pub target_port: u16,
    pub node: String,
    pub bdev_name: String,
    pub active: bool,
}

// ============================================================================
// RAID RELATED STRUCTURES
// ============================================================================

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RaidStatus {
    pub raid_level: u32,
    pub state: String, // "online", "degraded", "failed"
    pub num_base_bdevs: u32,
    pub num_base_bdevs_discovered: u32,
    pub num_base_bdevs_operational: u32,
    pub base_bdevs_list: Vec<RaidMember>,
    pub rebuild_info: Option<RaidRebuildInfo>,
    pub superblock_version: Option<u32>,
    pub process_request_fn: Option<String>,
    // Fields for read optimization
    pub read_policy: Option<String>,
    pub primary_member_slot: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RaidMember {
    pub name: String,
    pub state: String, // "online", "failed", "rebuilding"
    pub slot: u32,
    pub uuid: Option<String>,
    pub is_configured: bool,
    // Fields for optimization tracking
    pub node: Option<String>,
    pub is_local: Option<bool>,
    pub read_priority: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RaidRebuildInfo {
    pub state: String, // "init", "running", "completed", "failed"
    pub target_slot: u32,
    pub source_slot: u32,
    pub blocks_remaining: u64,
    pub blocks_total: u64,
    pub progress_percentage: f64,
}

// ============================================================================
// SPDK DISK RELATED STRUCTURES
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[kube(group = "flint.csi.storage.io", version = "v1", kind = "SpdkDisk", plural = "spdkdisks")]
#[kube(namespaced)]
#[kube(status = "SpdkDiskStatus")]
pub struct SpdkDiskSpec {
    pub node_id: String,        // Changed from 'node' to match CRD
    pub device_path: String,    // Added required field
    pub size: String,           // Changed from 'capacity' (i64) to 'size' (String) to match CRD
    pub pcie_addr: String,
    pub blobstore_uuid: Option<String>,
    pub nvme_controller_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct SpdkDiskStatus {
    #[serde(default)]
    pub total_capacity: i64,
    #[serde(default)]
    pub free_space: i64,
    #[serde(default)]
    pub used_space: i64,
    #[serde(default)]
    pub healthy: bool,
    #[serde(default)]
    pub last_checked: String,
    #[serde(default)]
    pub lvol_count: u32,
    #[serde(default)]
    pub blobstore_initialized: bool,
    #[serde(default)]
    pub io_stats: IoStatistics,
    pub lvs_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct IoStatistics {
    pub read_iops: u64,
    pub write_iops: u64,
    pub read_latency_us: u64,
    pub write_latency_us: u64,
    pub error_count: u64,
}

// ============================================================================
// SNAPSHOT RELATED STRUCTURES
// ============================================================================

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub enum SnapshotType {
    #[default]
    Bdev,
    LvolClone,
    External,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct ReplicaSnapshot {
    pub node_name: String,
    pub spdk_snapshot_lvol: String,
    pub source_lvol_bdev: String,
    pub disk_ref: String,
    pub nvmeof_export: Option<NvmeofExportInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct NvmeofExportInfo {
    pub nqn: String,
    pub target_ip: String,
    pub target_port: u16,
    pub transport: String,
    pub exported: bool,
    pub export_time: Option<String>,
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[kube(group = "flint.csi.storage.io", version = "v1", kind = "SpdkSnapshot", plural = "spdksnapshots")]
#[kube(namespaced)]
#[kube(status = "SpdkSnapshotStatus")]
pub struct SpdkSnapshotSpec {
    pub source_volume_id: String,
    pub snapshot_id: String,
    pub replica_snapshots: Vec<ReplicaSnapshot>,
    #[serde(default)]
    pub snapshot_type: SnapshotType,
    pub clone_source_snapshot_id: Option<String>,
    pub nvmeof_access_enabled: bool,
    pub nvmeof_transport: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct SpdkSnapshotStatus {
    pub creation_time: Option<DateTime<Utc>>,
    pub ready_to_use: bool,
    pub size_bytes: i64,
    pub error: Option<String>,
    pub nvmeof_targets: Vec<NvmeofExportInfo>,
    pub accessible_nodes: Vec<String>,
}

// ============================================================================
// PERFORMANCE AND METRICS STRUCTURES
// ============================================================================

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct LocalReplicaMetrics {
    pub local_read_percentage: f64,
    pub local_read_latency_avg: u64,
    pub remote_read_latency_avg: u64,
    pub optimization_ratio: f64,
    pub last_updated: String,
}

// ============================================================================
// AUXILIARY STRUCTURES
// ============================================================================

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LvolStatus {
    pub name: String,
    pub is_healthy: bool,
    pub error_reason: Option<String>,
}

impl RaidStatus {
    /// Parse RAID status from SPDK RPC response
    pub fn from_spdk_response(raid_bdev: &serde_json::Value) -> Result<Self, Box<dyn std::error::Error>> {
        let base_bdevs_list: Vec<RaidMember> = raid_bdev["base_bdevs"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .enumerate()
            .map(|(i, member)| RaidMember {
                name: member["name"].as_str().unwrap_or("").to_string(),
                state: member["state"].as_str().unwrap_or("unknown").to_string(),
                slot: i as u32,
                uuid: member["uuid"].as_str().map(|s| s.to_string()),
                is_configured: member["is_configured"].as_bool().unwrap_or(false),
                node: None, // Will be filled in later if needed
                is_local: None, // Will be determined by caller
                read_priority: Some(i as u32), // Assign based on slot order
            })
            .collect();

        let rebuild_info = if let Some(rebuild) = raid_bdev["rebuild_info"].as_object() {
            Some(RaidRebuildInfo {
                state: rebuild["state"].as_str().unwrap_or("").to_string(),
                target_slot: rebuild["target_slot"].as_u64().unwrap_or(0) as u32,
                source_slot: rebuild["source_slot"].as_u64().unwrap_or(0) as u32,
                blocks_remaining: rebuild["blocks_remaining"].as_u64().unwrap_or(0),
                blocks_total: rebuild["blocks_total"].as_u64().unwrap_or(0),
                progress_percentage: rebuild["progress_percentage"].as_f64().unwrap_or(0.0),
            })
        } else {
            None
        };

        Ok(RaidStatus {
            raid_level: raid_bdev["raid_level"].as_u64().unwrap_or(1) as u32,
            state: raid_bdev["state"].as_str().unwrap_or("unknown").to_string(),
            num_base_bdevs: raid_bdev["num_base_bdevs"].as_u64().unwrap_or(0) as u32,
            num_base_bdevs_discovered: raid_bdev["num_base_bdevs_discovered"].as_u64().unwrap_or(0) as u32,
            num_base_bdevs_operational: raid_bdev["num_base_bdevs_operational"].as_u64().unwrap_or(0) as u32,
            base_bdevs_list,
            rebuild_info,
            superblock_version: raid_bdev["superblock_version"].as_u64().map(|v| v as u32),
            process_request_fn: raid_bdev["process_request_fn"].as_str().map(|s| s.to_string()),
            read_policy: raid_bdev["read_policy"].as_str().map(|s| s.to_string()),
            primary_member_slot: Some(0), // Assume first member is primary
        })
    }
}

/// Create a new SpdkVolume instance with proper metadata
impl SpdkVolume {
    pub fn new_with_metadata(name: &str, spec: SpdkVolumeSpec, namespace: &str) -> Self {
        use kube::api::ObjectMeta;
        
        // Debug validation of the spec before creating the volume
        println!("🔍 [VOLUME_DEBUG] Creating SpdkVolume with metadata:");
        println!("   Name: {}", name);
        println!("   Namespace: {}", namespace);
        println!("   Volume ID: {}", spec.volume_id);
        println!("   Size: {} bytes", spec.size_bytes);
        println!("   Num replicas: {}", spec.num_replicas);
        println!("   Replicas count: {}", spec.replicas.len());
        
        // Validate each replica
        for (i, replica) in spec.replicas.iter().enumerate() {
            println!("   Replica {}: node={}, type={}, health={:?}", 
                i, replica.node, replica.replica_type, replica.health_status);
            
            // Test serialization of individual replica
            match serde_json::to_string(replica) {
                Ok(replica_json) => {
                    println!("   ✅ Replica {} JSON: {}", i, replica_json);
                },
                Err(e) => {
                    println!("   ❌ Replica {} failed to serialize: {}", i, e);
                }
            }
        }
        
        // Test serialization of the entire spec
        match serde_json::to_string(&spec) {
            Ok(spec_json) => {
                println!("✅ [VOLUME_DEBUG] SpdkVolumeSpec serializes successfully");
                println!("   Spec JSON length: {} characters", spec_json.len());
            },
            Err(e) => {
                println!("❌ [VOLUME_DEBUG] SpdkVolumeSpec failed to serialize: {}", e);
            }
        }
        
        let volume = SpdkVolume {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            spec,
            status: None,
        };
        
        // Final validation: test full volume serialization
        match serde_json::to_string(&volume) {
            Ok(volume_json) => {
                println!("✅ [VOLUME_DEBUG] Complete SpdkVolume serializes successfully");
                println!("   Volume JSON length: {} characters", volume_json.len());
            },
            Err(e) => {
                println!("❌ [VOLUME_DEBUG] Complete SpdkVolume failed to serialize: {}", e);
            }
        }
        
        volume
    }
}

/// Create a new SpdkSnapshot instance with proper metadata  
impl SpdkSnapshot {
    pub fn new_with_metadata(name: &str, spec: SpdkSnapshotSpec, namespace: &str) -> Self {
        use kube::api::ObjectMeta;
        
        SpdkSnapshot {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            spec,
            status: None,
        }
    }
}

/// Create a new SpdkDisk instance with proper metadata
impl SpdkDisk {
    pub fn new_with_metadata(name: &str, spec: SpdkDiskSpec, namespace: &str) -> Self {
        use kube::api::ObjectMeta;
        
        SpdkDisk {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            spec,
            status: None,
        }
    }
}
