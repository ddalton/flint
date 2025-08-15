// models.rs - Common data structures used across the SPDK CSI driver
use kube::CustomResource;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use chrono;
use chrono::{DateTime, Utc};
use uuid;
// Shared NVMe-oF endpoint type used by NvmeofDisk and RAID member specs
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct NvmeofEndpoint {
    pub nqn: String,
    pub target_addr: String,
    pub target_port: u16,
    pub transport: String,
    pub created_at: Option<String>,
    pub active: bool,
}

impl Default for NvmeofEndpoint {
    fn default() -> Self {
        NvmeofEndpoint {
            nqn: String::new(),
            target_addr: String::new(),
            target_port: 4420,
            transport: "tcp".to_string(),
            created_at: None,
            active: false,
        }
    }
}

// ============================================================================
// SPDK RAID DISK RELATED STRUCTURES  
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[kube(group = "flint.csi.storage.io", version = "v1", kind = "SpdkRaidDisk", plural = "spdkraiddisks")]
#[kube(namespaced)]
#[kube(status = "SpdkRaidDiskStatus")]
pub struct SpdkRaidDiskSpec {
    pub raid_disk_id: String,                    // Unique identifier for this RAID disk
    pub raid_level: String,                      // "1" for RAID1, "0" for RAID0, etc.
    pub num_member_disks: i32,                   // Number of member disks configured (>=1)
    pub member_disks: Vec<RaidMemberDisk>,       // List of member disks
    pub stripe_size_kb: u32,                     // RAID stripe size in KB
    pub superblock_enabled: bool,                // Whether RAID superblock is enabled
    pub created_on_node: String,                 // Node where RAID was initially created
    pub min_capacity_bytes: i64,                 // Minimum usable capacity
    pub auto_rebuild: bool,                      // Enable automatic rebuild on failure
}

// ============================================================================
// NVMe-oF DISK INVENTORY (LOCAL AND REMOTE)
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[kube(group = "flint.csi.storage.io", version = "v1", kind = "NvmeofDisk", plural = "nvmeofdisks")]
#[kube(namespaced)]
#[kube(status = "NvmeofDiskStatus")]
pub struct NvmeofDiskSpec {
    // Local or remote endpoint
    pub is_remote: bool,
    pub node_id: Option<String>,

    // Stable identifiers for endpoint repair on local disks
    pub hardware_id: Option<String>,
    pub serial_number: Option<String>,
    pub wwn: Option<String>,
    pub model: Option<String>,
    pub vendor: Option<String>,

    // Capacity/size information
    pub size_bytes: i64,

    // Endpoint used for NVMe-oF access
    pub nvmeof_endpoint: NvmeofEndpoint,

    // Remote endpoint credentials reference (for out-of-cluster disks)
    pub credential_secret_name: Option<String>,
    pub credential_secret_namespace: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct NvmeofDiskStatus {
    pub healthy: bool,
    pub endpoint_validated: bool,
    pub available_bytes: i64,
    pub last_checked: String,
    pub message: Option<String>,
    pub consecutive_failures: u32,         // Track failure streaks to avoid false positives
    pub last_successful_check: Option<String>, // When it last worked
    pub failure_reason: Option<String>,    // Why it failed (network, spdk, timeout, etc.)
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct RaidMemberDisk {
    pub member_index: u32,                       // Position in RAID array (0, 1, 2...)
    // Node where this member disk is accessed from
    pub node_id: String,
    // Stable hardware identity for local disks (used to repair endpoints)
    pub hardware_id: Option<String>,
    pub serial_number: Option<String>,
    pub wwn: Option<String>,
    pub model: Option<String>,
    pub vendor: Option<String>,
    // NVMe-oF endpoint to reach this member's raw device (local or remote)
    pub nvmeof_endpoint: NvmeofEndpoint,
    
    // Member disk status
    pub state: RaidMemberState,                  // online, degraded, failed, rebuilding
    pub capacity_bytes: i64,                     // Member disk capacity
    pub connected: bool,                         // Whether member is currently connected
    pub last_health_check: Option<String>,       // Last health check timestamp
}

// MemberDiskType enum removed - member disks are now just references to SpdkDisk CRDs
// The SpdkDisk CRD itself contains the disk type (local or remote)

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct SpdkRaidDiskStatus {
    pub state: String,                           // creating, online, degraded, failed, rebuilding
    pub raid_bdev_name: Option<String>,          // SPDK RAID bdev name
    pub lvs_name: Option<String>,                // LVS created on this RAID disk
    pub lvs_uuid: Option<String>,                // LVS UUID
    pub total_capacity_bytes: i64,               // Total RAID capacity
    pub usable_capacity_bytes: i64,              // Available capacity for volumes
    pub used_capacity_bytes: i64,                // Capacity used by volumes
    pub health_status: String,                   // healthy, degraded, failed
    pub degraded: bool,                          // Whether RAID is in degraded state
    pub rebuild_progress: Option<f64>,           // Rebuild progress percentage (0.0-100.0)
    pub active_member_count: u32,                // Number of active members
    pub failed_member_count: u32,                // Number of failed members
    pub last_checked: String,                    // Last health check timestamp
    pub created_at: Option<String>,              // When RAID disk was created
    pub raid_status: Option<RaidStatus>,         // Detailed RAID status from SPDK
}

impl SpdkRaidDisk {
    /// Create a new SpdkRaidDisk with metadata
    pub fn new_with_metadata(name: &str, spec: SpdkRaidDiskSpec, namespace: &str) -> Self {
        use kube::api::ObjectMeta;
        
        SpdkRaidDisk {
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

impl RaidBdevConfig {
    /// Determine optimal node placement for RAID based on member locality
    /// Returns the node ID where the RAID should be created, given the current node context
    /// 
    /// Logic:
    /// - If any member is local (member_type="local"), prioritize the current node
    /// - If no local members exist, the RAID can be placed on any node with remote access
    /// - This implements the locality optimization requirement
    pub fn determine_optimal_placement_node(&self, current_node_id: &str) -> Option<String> {
        // Check if this RAID has any local members
        if self.has_local_members() {
            // If there are local members, the RAID should be created on the local node
            // for locality optimization (as per the requirement)
            Some(current_node_id.to_string())
        } else {
            // No local members - can be placed on any node that can access all remote members
            // For now, return None to indicate no specific placement requirement
            None
        }
    }
    
    /// Advanced placement algorithm with locality + load balancing
    /// 
    /// Logic:
    /// 1. First priority: nodes with local members (locality optimization)
    /// 2. Second priority: among nodes with local members, choose node with the FEWEST existing RAIDs (load balancing)
    /// 
    /// Parameters:
    /// - node_local_members: Map of node_id -> list of local member device paths
    /// - existing_raid_counts: Map of node_id -> count of existing SpdkRaidDisks
    pub fn determine_optimal_placement_with_load_balancing(
        &self,
        node_local_members: &std::collections::HashMap<String, Vec<String>>,
        existing_raid_counts: &std::collections::HashMap<String, u32>
    ) -> Option<String> {
        // Step 1: Find all nodes that have local members for this RAID
        let mut candidate_nodes: Vec<String> = Vec::new();
        
        for member in &self.members {
            if member.member_type == "local" {
                if let Some(device_path) = &member.local_device {
                    // Find which node has this local device
                    for (node_id, local_devices) in node_local_members {
                        if local_devices.contains(device_path) {
                            if !candidate_nodes.contains(node_id) {
                                candidate_nodes.push(node_id.clone());
                            }
                        }
                    }
                }
            }
        }
        
        // Step 2: If no nodes have local members, return None (flexible placement)
        if candidate_nodes.is_empty() {
            return None;
        }
        
        // Step 3: Among candidate nodes, choose the one with the lowest RAID count (load balancing)
        let optimal_node = candidate_nodes.into_iter()
            .min_by_key(|node_id| existing_raid_counts.get(node_id).unwrap_or(&0));
            
        optimal_node
    }
    
    /// Get node IDs that provide remote members for this RAID
    /// This helps understand the distribution of RAID members across nodes
    pub fn get_remote_member_nodes(&self) -> Vec<String> {
        self.members
            .iter()
            .filter(|m| m.member_type == "nvmeof")
            .filter_map(|m| m.nvmeof_config.as_ref())
            .map(|config| config.target_node_id.clone())
            .collect()
    }
    
    /// Validate that this RAID configuration is suitable for the given node
    /// Returns true if the node can host this RAID (has local members or can access remotes)
    pub fn is_suitable_for_node(&self, node_id: &str) -> bool {
        // If RAID has local members, it should only be on the local node
        if self.has_local_members() {
            // This would require knowledge of which node the local members belong to
            // For now, assume local members belong to the current node being evaluated
            true
        } else {
            // If no local members, any node can potentially host it if it can reach remotes
            true
        }
    }
    
    /// Check if this RAID configuration has any local members
    pub fn has_local_members(&self) -> bool {
        self.members.iter().any(|m| m.member_type == "local")
    }
    
    /// Get all local member device paths/addresses
    pub fn get_local_member_devices(&self) -> Vec<String> {
        self.members
            .iter()
            .filter(|m| m.member_type == "local")
            .filter_map(|m| m.local_device.clone())
            .collect()
    }
    
    /// Demonstrate the locality-based placement logic
    /// This function shows how the RAID placement decision works
    #[allow(dead_code)]
    pub fn demonstrate_locality_placement(&self, node_id: &str) -> String {
        let local_devices = self.get_local_member_devices();
        let remote_nodes = self.get_remote_member_nodes();
        
        if self.has_local_members() {
            format!(
                "🏠 RAID '{}' should be placed on LOCAL node '{}' for locality optimization.\n\
                 📀 Local devices: {:?}\n\
                 🌐 Remote members from nodes: {:?}\n\
                 ✅ LOCALITY RULE: At least one member is local → create RAID locally",
                self.name, node_id, local_devices, remote_nodes
            )
        } else {
            format!(
                "🌐 RAID '{}' has NO local members - can be placed on any node.\n\
                 🔗 All members are remote from nodes: {:?}\n\
                 ℹ️  LOCALITY RULE: No local members → placement flexible",
                self.name, remote_nodes
            )
        }
    }
    
    /// Demonstrate the advanced placement algorithm with load balancing
    /// Shows how locality + load balancing decisions are made
    #[allow(dead_code)]
    pub fn demonstrate_advanced_placement(
        &self,
        node_local_members: &std::collections::HashMap<String, Vec<String>>,
        existing_raid_counts: &std::collections::HashMap<String, u32>
    ) -> String {
        let placement_result = self.determine_optimal_placement_with_load_balancing(
            node_local_members, 
            existing_raid_counts
        );
        
        // Find candidate nodes with local members
        let mut candidate_nodes = Vec::new();
        for member in &self.members {
            if member.member_type == "local" {
                if let Some(device_path) = &member.local_device {
                    for (node_id, local_devices) in node_local_members {
                        if local_devices.contains(device_path) && !candidate_nodes.contains(node_id) {
                            candidate_nodes.push(node_id.clone());
                        }
                    }
                }
            }
        }
        
        if candidate_nodes.is_empty() {
            format!(
                "🌐 RAID '{}' - NO LOCAL MEMBERS\n\
                 ℹ️  Can be placed on any node (no locality preference)\n\
                 🔗 All members are remote from various nodes",
                self.name
            )
        } else {
            let mut analysis = format!(
                "🎯 RAID '{}' - ADVANCED PLACEMENT ANALYSIS\n\n\
                 📍 STEP 1 - LOCALITY ANALYSIS:\n",
                self.name
            );
            
            for node in &candidate_nodes {
                let raid_count = existing_raid_counts.get(node).unwrap_or(&0);
                analysis.push_str(&format!(
                    "   • Node '{}': {} existing RAIDs\n",
                    node, raid_count
                ));
            }
            
            match placement_result {
                Some(chosen_node) => {
                    let chosen_raid_count = existing_raid_counts.get(&chosen_node).unwrap_or(&0);
                    analysis.push_str(&format!(
                        "\n 🏆 STEP 2 - LOAD BALANCING DECISION:\n\
                         ✅ Chosen Node: '{}' (has {} existing RAIDs)\n\
                         📊 REASON: Among nodes with local members, '{}' serves the FEWEST RAIDs\n\
                         🎯 ALGORITHM: Locality First → Choose Node with Fewest Existing RAIDs",
                        chosen_node, chosen_raid_count, chosen_node
                    ));
                }
                None => {
                    analysis.push_str("\n ❌ No optimal placement found");
                }
            }
            
            analysis
        }
    }
}

impl SpdkConfigSpec {
    /// Analyze existing RAID load across nodes for load balancing decisions
    /// Returns a map of node_id -> count of existing SpdkRaidDisks
    pub fn get_raid_load_per_node(&self) -> std::collections::HashMap<String, u32> {
        let mut raid_counts = std::collections::HashMap::new();
        
        // Count RAIDs in this node's config
        raid_counts.insert(self.node_id.clone(), self.raid_bdevs.len() as u32);
        
        raid_counts
    }
    
    /// Get all local devices available on this node
    /// Returns device paths/PCI addresses that are local to this node
    pub fn get_local_devices(&self) -> Vec<String> {
        let mut local_devices = Vec::new();
        
        for raid in &self.raid_bdevs {
            for member in &raid.members {
                if member.member_type == "local" {
                    if let Some(device) = &member.local_device {
                        if !local_devices.contains(device) {
                            local_devices.push(device.clone());
                        }
                    }
                }
            }
        }
        
        local_devices
    }
    
    /// Create a global view of node capabilities and RAID load
    /// This would typically be called with configs from all nodes
    pub fn create_global_placement_view(
        all_node_configs: &[SpdkConfigSpec]
    ) -> (std::collections::HashMap<String, Vec<String>>, std::collections::HashMap<String, u32>) {
        let mut node_local_devices = std::collections::HashMap::new();
        let mut node_raid_counts = std::collections::HashMap::new();
        
        for config in all_node_configs {
            // Map node -> local devices
            node_local_devices.insert(config.node_id.clone(), config.get_local_devices());
            
            // Map node -> RAID count
            node_raid_counts.insert(config.node_id.clone(), config.raid_bdevs.len() as u32);
        }
        
        (node_local_devices, node_raid_counts)
    }
}

impl SpdkRaidDiskSpec {
    /// Generate LVS name for this RAID disk
    /// Format: lvs_raid_{raid_disk_id}
    pub fn lvs_name(&self) -> String {
        format!("lvs_raid_{}", self.raid_disk_id)
    }
    
    /// Generate RAID bdev name for SPDK
    /// Format: raid_{raid_disk_id}
    pub fn raid_bdev_name(&self) -> String {
        format!("raid_{}", self.raid_disk_id)
    }
    
    /// Check if this RAID disk can accommodate a volume of given size
    pub fn can_accommodate_volume(&self, required_bytes: i64, current_status: &SpdkRaidDiskStatus) -> bool {
        current_status.state == "online" &&
        !current_status.degraded &&
        (current_status.usable_capacity_bytes - current_status.used_capacity_bytes) >= required_bytes
    }
    
    /// Get all member disks - they are all just references to SpdkDisk CRDs now
    /// The actual disk type (local/remote) is determined by looking up the SpdkDisk CRD
    pub fn get_all_members(&self) -> &Vec<RaidMemberDisk> {
        &self.member_disks
    }
    
    // Removed: member disk refs helper (no longer applicable since members embed NvmeofEndpoint)
}

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
    
    // Unified storage backend reference (either single disk or RAID disk)
    pub storage_backend: StorageBackend,
    
    // Logical volume information (same for both single and multi-replica)
    pub lvol_uuid: Option<String>,              // UUID of the logical volume 
    pub lvs_name: Option<String>,               // Name of the LVS containing this volume
    
    // NVMe-oF configuration (same for both single and multi-replica)
    pub nvmeof_transport: Option<String>,
    pub nvmeof_target_port: Option<u16>,
    
    // Legacy replica-based architecture (deprecated, for backward compatibility during migration)
    pub replicas: Vec<Replica>,
    pub primary_lvol_uuid: Option<String>,
    pub write_ordering_enabled: bool,
    pub raid_auto_rebuild: bool,
    
    // Scheduling and optimization fields
    pub scheduling_policy: Option<String>,
    pub preferred_nodes: Option<Vec<String>>,
}

/// Unified storage backend - can be either a single disk or a RAID disk
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageBackend {
    /// Multi-replica volume on a RAID disk  
    RaidDisk {
        raid_disk_ref: String,                  // Reference to SpdkRaidDisk CRD
        node_id: String,                        // Node where RAID disk is created
    },
}

impl Default for StorageBackend {
    fn default() -> Self {
        // Default to RAID-backed (caller must set fields)
        StorageBackend::RaidDisk { raid_disk_ref: String::new(), node_id: String::new() }
    }
}

impl StorageBackend {
    /// Get the node ID where this storage backend is located
    pub fn node_id(&self) -> &str {
        match self {
            StorageBackend::RaidDisk { node_id, .. } => node_id,
        }
    }
    
    /// Get the storage backend reference (either disk_ref or raid_disk_ref)
    pub fn backend_ref(&self) -> &str {
        match self {
            StorageBackend::RaidDisk { raid_disk_ref, .. } => raid_disk_ref,
        }
    }
    
    /// Check if this is a RAID-based storage backend
    pub fn is_raid(&self) -> bool {
        matches!(self, StorageBackend::RaidDisk { .. })
    }
    
    /// Check if this is a single disk storage backend
    pub fn is_single_disk(&self) -> bool {
        false
    }
}

impl SpdkVolumeSpec {
    /// Get the LVS name for this volume based on its storage backend
    pub fn get_lvs_name(&self) -> String {
        // If explicitly set, use that
        if let Some(lvs_name) = &self.lvs_name {
            return lvs_name.clone();
        }
        
        // Otherwise, generate based on storage backend
        match &self.storage_backend {
            StorageBackend::RaidDisk { raid_disk_ref, .. } => {
                // For RAID disk, use the RAID disk's LVS name format
                format!("lvs_raid_{}", raid_disk_ref)
            }
        }
    }
    
    /// Check if this volume is backed by a RAID disk (multi-replica)
    pub fn is_multi_replica(&self) -> bool {
        self.num_replicas > 1 || self.storage_backend.is_raid()
    }
    
    /// Check if this volume is backed by a single disk  
    pub fn is_single_replica(&self) -> bool {
        self.num_replicas <= 1 && self.storage_backend.is_single_disk()
    }
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
    
    // Add ublk device information (deprecated - use nvme_device instead)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ublk_device: Option<UblkDevice>,
    
    // New NVMe client device information (replaces ublk_device)
    pub nvme_device: Option<NvmeClientDevice>,
    
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
            nvme_device: None,
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

// Add new struct for ublk device information (legacy - being replaced)
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct UblkDevice {
    pub id: u32,
    pub device_path: String,
    pub volume_id: String,
    pub bdev_name: String,
    pub queue_depth: u32,
    pub block_size: u32,
    pub created_at: String,
}

// Helper struct for ublk device information
#[derive(Debug, Clone)]
pub struct UblkDeviceInfo {
    pub bdev_name: String,
    pub queue_depth: u32,
    pub block_size: u32,
}

// New struct for NVMe client device information (replaces ublk)
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct NvmeClientDevice {
    pub device_path: String,        // e.g., "/dev/nvme1n1"
    pub nqn: String,               // NVMe Qualified Name for connection
    pub transport: String,         // "tcp" or "rdma"
    pub target_addr: String,       // Target IP address
    pub target_port: u16,          // Target port
    pub connected_at: String,      // ISO 8601 timestamp
    pub node: String,              // Node where device was connected
    pub controller_id: Option<String>, // NVMe controller ID (e.g., "nvme1")
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

// Removed legacy SpdkDisk CRD and related types; replaced by NvmeofDisk

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

// Removed legacy SpdkDisk constructor

// ============================================================================
// SPDK CONFIGURATION CRD (FOR PERSISTENCE AND MAINTENANCE MODE)
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[kube(group = "flint.csi.storage.io", version = "v1", kind = "SpdkConfig", plural = "spdkconfigs")]
#[kube(namespaced)]
#[kube(status = "SpdkConfigStatus")]
pub struct SpdkConfigSpec {
    /// Node ID that this configuration belongs to
    pub node_id: String,
    
    /// Whether this node is in maintenance mode
    pub maintenance_mode: bool,
    
    /// Timestamp when config was last saved from SPDK
    pub last_config_save: Option<String>,
    
    /// RAID bdev configurations managed by this node
    /// Each RAID has exactly one LVS with multiple logical volumes
    pub raid_bdevs: Vec<RaidBdevConfig>,
    
    /// NVMe-oF subsystems (volume exports) managed by this node
    /// These reference logical volumes within RAID bdev LVS structures
    pub nvmeof_subsystems: Vec<NvmeofSubsystemConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct RaidBdevConfig {
    /// RAID bdev name (e.g., "raid_disk_1")
    pub name: String,
    /// RAID level ("1", "0", "5", etc.)
    pub raid_level: String,
    /// Whether RAID superblock is enabled for persistence
    pub superblock_enabled: bool,
    /// Stripe size in KB
    pub stripe_size_kb: u32,
    
    /// RAID member configurations (both local and remote NVMe-oF)
    /// These become the base_bdevs for the RAID
    pub members: Vec<RaidMemberBdevConfig>,
    
    /// Single Logical Volume Store on this RAID bdev (1:1 mapping)
    /// This is where all volumes for this RAID are carved out
    pub lvstore: LvstoreConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct RaidMemberBdevConfig {
    /// Local bdev name for this RAID member
    pub bdev_name: String,
    /// Member type: "local" or "nvmeof"
    pub member_type: String,
    
    /// For local members: PCI address or device path
    pub local_device: Option<String>,
    
    /// For NVMe-oF members: connection configuration
    pub nvmeof_config: Option<NvmeofMemberConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct NvmeofMemberConfig {
    /// Target node providing the raw disk
    pub target_node_id: String,
    /// NVMe-oF connection details
    pub nqn: String,
    pub transport: String,
    pub target_addr: String,
    pub target_port: u16,
    /// Creation timestamp
    pub created_at: Option<String>,
    /// Connection state: "connected", "disconnected", "failed"
    pub state: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct LvstoreConfig {
    /// LVS name (e.g., "lvs_raid_disk_1")
    pub name: String,
    /// LVS UUID (persistent identifier)
    pub uuid: String,
    /// Cluster size for thin provisioning
    pub cluster_size: u64,
    /// Total data clusters available
    pub total_data_clusters: u64,
    /// Free data clusters available
    pub free_clusters: u64,
    /// Block size (typically 4096)
    pub block_size: u64,
    /// Logical volumes created in this LVS
    pub logical_volumes: Vec<LogicalVolumeConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct LogicalVolumeConfig {
    /// Logical volume name (e.g., "vol-abc123")
    pub name: String,
    /// LVOL UUID (unique identifier for this logical volume)
    pub uuid: String,
    /// Size in bytes
    pub size_bytes: i64,
    /// Size in clusters (for SPDK internal management)
    pub size_clusters: u64,
    /// Whether this is a thin-provisioned volume
    pub thin_provision: bool,
    /// Associated SpdkVolume CRD name (if managed by CSI)
    pub volume_crd_ref: Option<String>,
    /// Creation timestamp
    pub created_at: Option<String>,
    /// Custom metadata for this volume
    pub metadata: std::collections::HashMap<String, String>,
    
    // Volume state and health information
    /// Current state: "online", "degraded", "failed"
    pub state: String,
    /// Health status: "healthy", "warning", "critical"
    pub health_status: String,
    /// Last health check timestamp
    pub last_health_check: Option<String>,
    
    // Performance and usage statistics
    /// Read operations count
    pub read_ops: u64,
    /// Write operations count  
    pub write_ops: u64,
    /// Total bytes read
    pub read_bytes: u64,
    /// Total bytes written
    pub write_bytes: u64,
    /// Current allocated size (for thin provisioning)
    pub allocated_bytes: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct NvmeofSubsystemConfig {
    /// NQN for the subsystem (e.g., "nqn.2023.io.flint:volume-vol-123")
    pub nqn: String,
    /// RAID bdev name containing the logical volume
    pub raid_bdev_name: String,
    /// Logical volume UUID exposed by this subsystem
    pub lvol_uuid: String,
    /// Logical volume name (for human reference)
    pub lvol_name: String,
    /// Namespace ID within the subsystem (typically 1)
    pub namespace_id: u32,
    /// Whether to allow any host to connect
    pub allow_any_host: bool,
    /// Specific hosts allowed (if allow_any_host is false)
    pub allowed_hosts: Vec<String>,
    /// Transport type ("tcp", "rdma")
    pub transport: String,
    /// Listen address for this subsystem
    pub listen_address: String,
    /// Listen port for this subsystem
    pub listen_port: u16,
    /// Associated SpdkVolume CRD (if managed by CSI)
    pub volume_crd_ref: Option<String>,
    /// Creation timestamp
    pub created_at: Option<String>,
    /// Current state: "active", "inactive", "error"
    pub state: String,
    
    // Connection statistics
    /// Number of currently connected hosts
    pub connected_hosts: u32,
    /// Total connection count since creation
    pub total_connections: u64,
    /// Last connection timestamp
    pub last_connection: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct SpdkConfigStatus {
    /// Whether the configuration has been successfully applied to SPDK
    pub config_applied: bool,
    /// Last time configuration was synchronized
    pub last_sync: Option<String>,
    /// SPDK version running on this node
    pub spdk_version: Option<String>,
    /// Any errors encountered during config application
    pub errors: Vec<String>,
    /// Maintenance mode status
    pub maintenance_status: Option<MaintenanceStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct MaintenanceStatus {
    /// Whether maintenance mode is active
    pub active: bool,
    /// When maintenance mode was entered
    pub started_at: Option<String>,
    /// Migration progress
    pub migration_progress: Vec<MigrationProgress>,
    /// Whether node is ready for shutdown
    pub ready_for_shutdown: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct MigrationProgress {
    /// Type of migration (raid, single_replica)
    pub migration_type: String,
    /// Source identifier (RAID name, volume ID)
    pub source_id: String,
    /// Target node
    pub target_node: String,
    /// Progress percentage (0-100)
    pub progress_percent: f64,
    /// Current status (planning, executing, completed, failed)
    pub status: String,
    /// Error message if failed
    pub error_message: Option<String>,
}
