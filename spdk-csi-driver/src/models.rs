// models.rs - Common data structures used across the SPDK CSI driver
use kube::CustomResource;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use chrono;
use chrono::{DateTime, Utc};
use uuid;

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
    
    // NVMe-oF networking fields
    pub pcie_addr: Option<String>,
    pub nqn: Option<String>,
    pub ip: Option<String>,
    pub port: Option<String>,
    
    // Scheduling and pod management
    pub local_pod_scheduled: bool,
    pub pod_name: Option<String>,
    
    // SPDK and storage fields
    pub disk_ref: String,
    pub lvol_uuid: Option<String>,
    pub health_status: ReplicaHealth,
    
    // Monitoring and consistency fields
    pub last_io_timestamp: Option<String>,
    pub write_sequence: u64,
    
    // RAID management
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

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(default)]
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

impl Default for SpdkVolumeStatus {
    fn default() -> Self {
        SpdkVolumeStatus {
            state: "creating".to_string(), // Use valid CRD state instead of empty string
            degraded: false,
            last_checked: chrono::Utc::now().to_rfc3339(),
            active_replicas: Vec::new(),
            failed_replicas: Vec::new(),
            write_sequence: 0,
            last_successful_write: None,
            raid_status: None,
            nvmeof_targets: Vec::new(),
            ublk_device: None,
            scheduled_node: None,
            has_local_replica: false,
            scheduling_policy: None,
            replica_nodes: Vec::new(),
            read_optimized: false,
            read_policy: None,
            local_replica_performance: None,
        }
    }
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
    // Location-dependent fields (can change when disk moves) - similar to Portworx node attachment
    pub node_id: String,        // Current node where disk is located
    pub device_path: String,    // Current device path (e.g., /dev/nvme1n1)
    pub pcie_addr: String,      // Current PCIe address
    
    // Immutable disk identification (Portworx-style hardware identification)
    pub disk_id: String,        // Hardware disk ID (/dev/disk/by-id/ path)
    pub serial_number: String,  // NVMe serial number (primary identifier)
    pub wwn: Option<String>,    // World Wide Name if available
    pub model: String,          // Disk model
    pub vendor: String,         // Disk vendor
    
    // Flint disk metadata (with cluster safety for NVMe-oF scenarios)  
    pub cluster_id: Option<String>,      // Kubernetes cluster this disk belongs to (CRITICAL for security)
    pub disk_uuid: Option<String>,       // Flint internal disk UUID (PRIMARY identifier, stored in blobstore)
    pub pool_uuid: Option<String>,       // Storage pool UUID
    pub first_attached_node: Option<String>, // Node that first initialized this disk
    pub initialized_at: Option<String>,  // When disk joined the cluster
    
    // Storage configuration
    pub size: String,           // Disk size
    pub blobstore_uuid: Option<String>,
    pub nvme_controller_id: Option<String>,
    
    // Disk health and status (for failure detection and recovery)
    pub status: Option<String>,           // online, offline, failed, missing, degraded
    pub last_seen: Option<String>,        // Last successful discovery timestamp
    pub health_status: Option<String>,    // healthy, warning, critical
    pub failure_reason: Option<String>,   // Reason for failure/offline status
}

impl SpdkDiskSpec {
    /// Get the LVS name for this disk (legacy method for compatibility)
    /// Uses cluster metadata if available, falls back to hardware ID
    pub fn lvs_name(&self) -> String {
        if let Some(disk_uuid) = &self.disk_uuid {
            // Use stored disk UUID from cluster metadata (preferred)
            format!("flint_{}", disk_uuid)
        } else {
            // Fallback: generate from hardware serial number (like Portworx does)
            let safe_serial = self.serial_number
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>();
            format!("flint_{}", safe_serial)
        }
    }
    
    /// Generate LVS name with embedded disk UUID and cluster ID (OPTIMIZED APPROACH)
    /// Encodes full disk UUID + shortened cluster ID for perfect disk identification
    /// Format: lvs_{32_char_disk_uuid}_{8_char_cluster_id} (45 chars total, well under 63 limit)
    pub fn lvs_name_with_cluster(&self, cluster_id: &str) -> String {
        match (&self.disk_uuid, &self.cluster_id) {
            (Some(disk_uuid), Some(_)) => {
                // Use FULL disk UUID (32 hex chars, no hyphens) for perfect identification
                let disk_full = disk_uuid.replace("-", "");
                
                // Use first 8 hex chars of cluster ID (sufficient for cluster distinction)
                let cluster_short = cluster_id.replace("-", "")
                    .chars()
                    .take(8)
                    .collect::<String>();
                    
                let lvs_name = format!("lvs_{}_{}", disk_full, cluster_short);
                
                // Validate length constraint (should be ~45 chars, well under 63)
                if lvs_name.len() > 63 {
                    panic!("LVS name exceeds 63 character limit: {} (length: {})", lvs_name, lvs_name.len());
                }
                
                println!("🔧 [LVS_NAME] Generated: {} (len: {}, disk: full, cluster: 8-char)", 
                         lvs_name, lvs_name.len());
                
                lvs_name
            }
            _ => {
                // Fallback for uninitialized disks or missing cluster info
                self.lvs_name() // Uses existing serial-based naming
            }
        }
    }
    
    /// Parse disk UUID and cluster ID from encoded LVS name (OPTIMIZED)
    /// Returns (full_disk_uuid, cluster_id_prefix) for perfect disk matching
    pub fn parse_lvs_name(lvs_name: &str) -> Option<(String, String)> {
        if !lvs_name.starts_with("lvs_") {
            return None;
        }
        
        let parts: Vec<&str> = lvs_name[4..].split('_').collect();
        if parts.len() != 2 || parts[0].len() != 32 || parts[1].len() != 8 {
            return None;
        }
        
        // Return full disk UUID (32 chars) and cluster prefix (8 chars)
        let full_disk_uuid = format!("{}-{}-{}-{}-{}", 
                                     &parts[0][0..8],   // 8 chars
                                     &parts[0][8..12],  // 4 chars  
                                     &parts[0][12..16], // 4 chars
                                     &parts[0][16..20], // 4 chars
                                     &parts[0][20..32]  // 12 chars
        );
        
        Some((full_disk_uuid, parts[1].to_string()))
    }
    
    /// Check if this disk's UUID matches the full UUID from LVS name (OPTIMIZED)
    pub fn matches_disk_uuid_full(&self, uuid_from_lvs: &str) -> bool {
        if let Some(disk_uuid) = &self.disk_uuid {
            *disk_uuid == uuid_from_lvs
        } else {
            false
        }
    }
    
    /// Check if cluster ID matches the prefix from LVS name
    pub fn matches_cluster_prefix(&self, cluster_prefix: &str) -> bool {
        if let Some(cluster_id) = &self.cluster_id {
            let cluster_short = cluster_id.replace("-", "")
                .chars()
                .take(8)
                .collect::<String>();
            cluster_short == cluster_prefix
        } else {
            false
        }
    }
    
    /// Generate a hardware-based disk identifier (similar to Portworx disk ID)
    pub fn generate_hardware_disk_id(&self) -> String {
        format!("{}-{}", self.vendor, self.serial_number)
    }
    
    /// Check if this disk is initialized for Flint
    pub fn is_flint_initialized(&self) -> bool {
        self.disk_uuid.is_some()
    }
    
    /// Update location-dependent fields when disk moves to a different node
    /// Similar to Portworx node attachment updates
    pub fn update_location(&mut self, node_id: String, device_path: String, pcie_addr: String, nvme_controller_id: Option<String>) {
        println!("🔄 [DISK_MOVE] Disk {} moving from node {} to node {}", 
                 self.serial_number, self.node_id, node_id);
        
        self.node_id = node_id;
        self.device_path = device_path;
        self.pcie_addr = pcie_addr;
        self.nvme_controller_id = nvme_controller_id;
    }
    
    /// Initialize disk for Flint usage with cluster protection
    pub fn initialize_for_flint(&mut self, cluster_id: String, pool_uuid: String, node_id: String) {
        self.cluster_id = Some(cluster_id);
        self.pool_uuid = Some(pool_uuid);
        self.disk_uuid = Some(uuid::Uuid::new_v4().to_string());
        self.first_attached_node = Some(node_id);
        self.initialized_at = Some(chrono::Utc::now().to_rfc3339());
    }
    
    /// Validate that this disk belongs to the specified cluster (CRITICAL for NVMe-oF safety)
    pub fn validate_cluster_membership(&self, expected_cluster_id: &str) -> Result<(), String> {
        match &self.cluster_id {
            Some(disk_cluster_id) => {
                if disk_cluster_id == expected_cluster_id {
                    Ok(())
                } else {
                    Err(format!(
                        "SECURITY: Disk belongs to cluster '{}' but current cluster is '{}'. Access blocked to prevent data corruption.",
                        disk_cluster_id, expected_cluster_id
                    ))
                }
            }
            None => {
                // Uninitialized disk - safe to use
                Ok(())
            }
        }
    }
    
    /// Mark disk as online and healthy
    pub fn mark_online(&mut self) {
        self.status = Some("online".to_string());
        self.health_status = Some("healthy".to_string());
        self.last_seen = Some(chrono::Utc::now().to_rfc3339());
        self.failure_reason = None;
    }
    
    /// Mark disk as offline due to detection failure
    pub fn mark_offline(&mut self, reason: &str) {
        self.status = Some("offline".to_string());
        self.health_status = Some("critical".to_string());
        self.failure_reason = Some(reason.to_string());
        // Keep last_seen unchanged to track when it was last available
    }
    
    /// Mark disk as failed (hardware failure detected)
    pub fn mark_failed(&mut self, reason: &str) {
        self.status = Some("failed".to_string());
        self.health_status = Some("critical".to_string());
        self.failure_reason = Some(reason.to_string());
        // Keep last_seen unchanged to track when it was last available
    }
    
    /// Mark disk as missing (not found during discovery)
    pub fn mark_missing(&mut self) {
        self.status = Some("missing".to_string());
        self.health_status = Some("critical".to_string());
        self.failure_reason = Some("Disk not found during discovery - may be physically removed".to_string());
        // Keep last_seen unchanged to track when it was last available
    }
    
    /// Check if disk is currently healthy and available
    pub fn is_healthy(&self) -> bool {
        matches!(self.status.as_deref(), Some("online")) && 
        matches!(self.health_status.as_deref(), Some("healthy"))
    }
    
    /// Check if disk is in a failed state
    pub fn is_failed(&self) -> bool {
        matches!(self.status.as_deref(), Some("failed" | "missing" | "offline"))
    }
    
    /// Get human-readable status description
    pub fn status_description(&self) -> String {
        match (self.status.as_deref(), self.failure_reason.as_deref()) {
            (Some("online"), _) => "Online and healthy".to_string(),
            (Some("offline"), Some(reason)) => format!("Offline: {}", reason),
            (Some("failed"), Some(reason)) => format!("Failed: {}", reason),
            (Some("missing"), _) => "Missing - disk not found during discovery".to_string(),
            (Some(status), _) => format!("Status: {}", status),
            (None, _) => "Status unknown".to_string(),
        }
    }
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
// FLINT CLUSTER METADATA (PORTWORX-STYLE)
// ============================================================================

/// Flint disk metadata stored in SPDK blobstore
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct FlintDiskMetadata {
    pub version: u32,                    // Metadata format version
    pub cluster_id: String,              // Kubernetes cluster this disk belongs to (CRITICAL for NVMe-oF safety)
    pub cluster_name: Option<String>,    // Human-readable cluster name
    pub disk_uuid: String,               // Unique disk identifier within Flint
    pub pool_uuid: String,               // Storage pool this disk belongs to
    pub pool_name: String,               // Human-readable pool name
    
    // Disk hardware identification
    pub hardware_id: String,             // Hardware-based disk identifier
    pub serial_number: String,           // NVMe serial number
    pub model: String,                   // Disk model
    pub vendor: String,                  // Disk vendor
    pub wwn: Option<String>,             // World Wide Name if available
    
    // Cluster membership information
    pub initialized_at: String,          // ISO 8601 timestamp when disk joined cluster
    pub initialized_by_node: String,     // Node that first added this disk to cluster
    pub last_attached_node: String,      // Last node this disk was attached to
    pub attachment_history: Vec<DiskAttachmentRecord>, // History of node attachments
    
    // Storage configuration
    pub total_size: u64,                 // Total disk size in bytes
    pub usable_size: u64,                // Usable size after metadata overhead
    pub sector_size: u32,                // Disk sector size
    pub optimal_io_size: u32,            // Optimal I/O size for this disk
}

/// Record of disk attachment to a node (similar to Portworx attachment history)
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct DiskAttachmentRecord {
    pub node_id: String,                 // Node ID
    pub attached_at: String,             // ISO 8601 timestamp
    pub detached_at: Option<String>,     // ISO 8601 timestamp when detached
    pub pcie_addr: String,               // PCIe address on this node
    pub device_path: String,             // Device path on this node
    pub attachment_reason: String,       // Why disk was attached (discovery, migration, etc.)
}

/// Flint storage pool configuration (similar to Portworx pools)
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct FlintStoragePool {
    pub uuid: String,                    // Pool UUID
    pub name: String,                    // Pool name
    pub pool_type: StoragePoolType,      // Pool type
    pub disks: Vec<String>,              // List of disk UUIDs in this pool
    pub total_size: u64,                 // Total pool size
    pub used_size: u64,                  // Used space in pool
    pub replication_factor: u32,         // Default replication factor
    pub created_at: String,              // ISO 8601 timestamp
    pub created_by_node: String,         // Node that created the pool
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StoragePoolType {
    Auto,                                // Auto-managed pool
    Manual,                              // Manually configured pool
    Journal,                             // Journal/metadata pool
    Cache,                               // Cache pool
}

impl Default for StoragePoolType {
    fn default() -> Self {
        StoragePoolType::Auto
    }
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
        use std::collections::BTreeMap;
        
        // Create labels for efficient node filtering
        let mut labels = BTreeMap::new();
        labels.insert("node".to_string(), spec.node_id.clone());
        labels.insert("app".to_string(), "flint-csi".to_string());
        labels.insert("component".to_string(), "spdk-disk".to_string());
        
        SpdkDisk {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            spec,
            status: None,
        }
    }
}
