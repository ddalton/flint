// controller.rs - Controller service implementation
use std::sync::Arc;
// Removed unused imports: HashMap, Mutex
use crate::driver::SpdkCsiDriver;
use crate::csi_snapshotter::*;
use spdk_csi_driver::csi::{
    controller_server::Controller,
    *,
};
use tonic::{Request, Response, Status};
use kube::{Api, api::{PatchParams, Patch, PostParams, ListParams}};
use reqwest::Client as HttpClient;
use serde_json::json;
use spdk_csi_driver::models::*;
use crate::node::call_spdk_rpc;

pub struct ControllerService {
    driver: Arc<SpdkCsiDriver>,
}

impl ControllerService {
    pub fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }

    /// Get count of available healthy disks with initialized LVS
    async fn get_available_disk_count(&self) -> Result<usize, Box<dyn std::error::Error>> {
        let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let disk_list = disks.list(&ListParams::default()).await?;
        
        let count = disk_list.items.iter()
            .filter(|disk| {
                if let Some(status) = &disk.status {
                    status.healthy && status.blobstore_initialized
                } else {
                    false
                }
            })
            .count();
            
        Ok(count)
    }

    // ============================================================================
    // UNIFIED VOLUME PROVISIONING (Single and Multi-Replica)
    // ============================================================================

    /// Provision a single replica volume on a single disk
    async fn provision_single_replica_volume(
        &self,
        volume_id: &str,
        capacity: i64,
    ) -> Result<(StorageBackend, String, String), Status> {
        // Find an available single disk
        let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let available_disks = self.get_available_disks(&disks, capacity).await?;
        
        if available_disks.is_empty() {
            return Err(Status::resource_exhausted("No available disks for single replica volume"));
        }
        
        // Select the best disk (first available)
        let selected_disk = &available_disks[0];
        
        // Create logical volume on the selected disk's LVS
        let lvol_uuid = self.create_volume_lvol(selected_disk, capacity, volume_id).await
            .map_err(|e| Status::internal(format!("Failed to create lvol: {}", e)))?;
        
        // Get the LVS name for this disk
        let lvs_name = selected_disk.spec.lvs_name();
        
        // Create storage backend reference
        let storage_backend = StorageBackend::SingleDisk {
            disk_ref: selected_disk.metadata.name.clone().unwrap_or_default(),
            node_id: selected_disk.spec.node_id.clone(),
        };
        
        println!("✅ [SINGLE_VOLUME] Created single replica volume {} on disk {} (node {})", 
                 volume_id, selected_disk.metadata.name.as_ref().unwrap_or(&"unknown".to_string()), selected_disk.spec.node_id);
        
        Ok((storage_backend, lvol_uuid, lvs_name))
    }

    /// Provision a multi-replica volume on a RAID disk
    async fn provision_multi_replica_volume(
        &self,
        volume_id: &str,
        capacity: i64,
        num_replicas: i32,
    ) -> Result<(StorageBackend, String, String), Status> {
        // Find or create a suitable RAID disk
        let raid_disk = self.find_or_create_raid_disk(num_replicas, capacity).await?;
        
        // Create logical volume on the RAID disk's LVS
        let lvol_uuid = self.create_volume_lvol_on_raid(&raid_disk, capacity, volume_id).await?;
        
        // Get the LVS name for this RAID disk
        let lvs_name = raid_disk.spec.lvs_name();
        
        // Create storage backend reference
        let storage_backend = StorageBackend::RaidDisk {
            raid_disk_ref: raid_disk.metadata.name.clone().unwrap_or_default(),
            node_id: raid_disk.spec.created_on_node.clone(),
        };
        
        println!("✅ [MULTI_VOLUME] Created multi-replica volume {} on RAID disk {} (node {})", 
                 volume_id, raid_disk.metadata.name.as_ref().unwrap_or(&"unknown".to_string()), raid_disk.spec.created_on_node);
        
        Ok((storage_backend, lvol_uuid, lvs_name))
    }

    /// Create logical volume on a RAID disk (same interface as single disk)
    async fn create_volume_lvol_on_raid(
        &self,
        raid_disk: &SpdkRaidDisk,
        capacity: i64,
        volume_id: &str,
    ) -> Result<String, Status> {
        let target_node = &raid_disk.spec.created_on_node;
        let node_ip = self.driver.get_node_ip(target_node).await?;
        let spdk_rpc_url = format!("http://{}:9009", node_ip);
        let lvs_name = raid_disk.spec.lvs_name();

        // Create logical volume on the RAID disk's LVS (same RPC as single disk)
        let response = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvol_name": volume_id,
                "size": capacity,
                "lvs_name": lvs_name
            }
        })).await
        .map_err(|e| Status::internal(format!("Failed to create lvol on RAID disk: {}", e)))?;

        let lvol_uuid = response["uuid"].as_str()
            .ok_or_else(|| Status::internal("SPDK response missing lvol UUID"))?
            .to_string();

        println!("✅ [RAID_LVOL] Created lvol {} (UUID: {}) on RAID disk LVS {}", volume_id, lvol_uuid, lvs_name);
        Ok(lvol_uuid)
    }

    // ============================================================================
    // RAID DISK MANAGEMENT (Only for Multi-Replica)
    // ============================================================================

    /// Find or create a suitable RAID disk for multi-replica volume
    async fn find_or_create_raid_disk(
        &self,
        num_replicas: i32,
        required_capacity: i64,
    ) -> Result<SpdkRaidDisk, Status> {
        // First, try to find an existing RAID disk that can accommodate the volume
        if let Ok(existing_raid) = self.find_suitable_raid_disk(num_replicas, required_capacity).await {
            return Ok(existing_raid);
        }

        // If no suitable RAID disk exists, create a new one
        self.create_new_raid_disk(num_replicas, required_capacity).await
    }

    /// Find an existing RAID disk that can accommodate a volume of given size
    async fn find_suitable_raid_disk(
        &self,
        num_replicas: i32,
        required_capacity: i64,
    ) -> Result<SpdkRaidDisk, Status> {
        let raid_disks: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let raid_disk_list = raid_disks.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list SpdkRaidDisks: {}", e)))?;

        for raid_disk in raid_disk_list.items {
            // Check if this RAID disk matches our requirements
            if raid_disk.spec.num_member_disks == num_replicas &&
               raid_disk.spec.raid_level == "1" && // Support RAID1 for now
               raid_disk.status.as_ref().map_or(false, |status| {
                   raid_disk.spec.can_accommodate_volume(required_capacity, status)
               }) {
                return Ok(raid_disk);
            }
        }

        Err(Status::not_found("No suitable RAID disk found"))
    }

    /// Create a new RAID disk from available member disks
    async fn create_new_raid_disk(
        &self,
        num_replicas: i32,
        required_capacity: i64,
    ) -> Result<SpdkRaidDisk, Status> {
        // Find available disks for creating RAID
        let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let available_disks = self.get_available_disks(&disks, required_capacity).await?;

        // Validate we have enough disks
        if available_disks.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(format!(
                "Not enough available disks: need {}, found {}",
                num_replicas, available_disks.len()
            )));
        }

        // Select disks with node separation for fault tolerance
        let selected_disks = self.select_disks_with_node_separation(available_disks, num_replicas as usize)?;
        
        // Create RAID disk ID
        let raid_disk_id = uuid::Uuid::new_v4().to_string();
        
        // Create member disk specifications - just references to SpdkDisk CRDs
        let mut member_disks = Vec::new();
        for (index, disk) in selected_disks.iter().enumerate() {
            let member_disk = RaidMemberDisk {
                member_index: index as u32,
                disk_ref: disk.metadata.name.clone().unwrap_or_default(),
                node_id: disk.spec.node_id.clone(),
                state: RaidMemberState::Online,
                capacity_bytes: disk.status.as_ref().map(|s| s.total_capacity).unwrap_or(0),
                connected: true,
                last_health_check: Some(chrono::Utc::now().to_rfc3339()),
            };
            member_disks.push(member_disk);
        }

        // Create SpdkRaidDisk CRD
        let raid_disk_spec = SpdkRaidDiskSpec {
            raid_disk_id: raid_disk_id.clone(),
            raid_level: "1".to_string(), // RAID1 for now
            num_member_disks: num_replicas,
            member_disks,
            stripe_size_kb: 64,
            superblock_enabled: true,
            created_on_node: selected_disks[0].spec.node_id.clone(), // Create on first node
            min_capacity_bytes: required_capacity,
            auto_rebuild: true,
        };

        let raid_disk = SpdkRaidDisk::new_with_metadata(
            &raid_disk_id,
            raid_disk_spec,
            &self.driver.target_namespace,
        );

        // Create the RAID disk CRD
        let raid_disks: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let created_raid_disk = raid_disks.create(&PostParams::default(), &raid_disk).await
            .map_err(|e| Status::internal(format!("Failed to create SpdkRaidDisk CRD: {}", e)))?;

        println!("✅ [RAID_CREATE] Created RAID disk: {}", raid_disk_id);
        
        // Create the actual RAID bdev on the target node
        self.create_raid_bdev_on_node(&created_raid_disk).await?;
        
        // Create LVS on the RAID disk
        self.create_lvs_on_raid_disk(&created_raid_disk).await?;

        Ok(created_raid_disk)
    }

    /// Create the actual RAID bdev on the specified node using SPDK RPC
    async fn create_raid_bdev_on_node(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let target_node = &raid_disk.spec.created_on_node;
        let node_ip = self.driver.get_node_ip(target_node).await?;
        let spdk_rpc_url = format!("http://{}:9009", node_ip);

        // Connect to ALL member disks via NVMe-oF (both local and remote)
        // RAID bdev members MUST be NVMe-oF bdevs regardless of disk location
        for member in raid_disk.spec.get_all_members() {
            // Get the referenced SpdkDisk 
            let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
            let disk = disks.get(&member.disk_ref).await
                .map_err(|e| Status::internal(format!("Failed to get member disk {}: {}", member.disk_ref, e)))?;
            
            // ALL disks (local and remote) must be connected via NVMe-oF for RAID membership
            let endpoint = &disk.spec.nvmeof_target;
            if endpoint.nqn.is_empty() || !endpoint.active {
                return Err(Status::internal(format!(
                    "Disk {} does not have an active NVMe-oF target. All RAID members must expose raw disk via NVMe-oF", 
                    member.disk_ref
                )));
            }
            
            println!("🔗 [RAID_MEMBER] Connecting to NVMe-oF target: {} (type: {:?})", 
                     endpoint.nqn, disk.spec.disk_type);
            
            self.driver.connect_nvme_device(
                &endpoint.nqn, 
                &endpoint.target_addr, 
                endpoint.target_port, 
                &endpoint.transport, 
                &format!("member_{}", member.member_index)
            ).await
            .map_err(|e| Status::internal(format!("Failed to connect to member disk {}: {}", member.disk_ref, e)))?;
            
            println!("✅ [RAID_MEMBER] Connected to {} disk via NVMe-oF: {}", 
                     if disk.spec.is_local() { "local" } else { "remote" }, endpoint.nqn);
        }

        // Collect NVMe-oF bdev names for RAID creation after connections are established
        let mut base_bdevs = Vec::new();
        for member in &raid_disk.spec.member_disks {
            // Get the referenced SpdkDisk 
            let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
            let disk = disks.get(&member.disk_ref).await
                .map_err(|e| Status::internal(format!("Failed to get SpdkDisk {}: {}", member.disk_ref, e)))?;
            
            // Find the actual NVMe device created by the nvme connect operation
            let endpoint = &disk.spec.nvmeof_target;
            let nvme_device = self.driver.find_existing_nvme_connection(&endpoint.nqn).await
                .map_err(|e| Status::internal(format!("Failed to find NVMe connection for {}: {}", endpoint.nqn, e)))?;
            
            // Use the actual NVMe device path as bdev name for RAID
            // Extract bdev name from device path (/dev/nvme1n1 -> nvme1n1)
            let bdev_name = if let Some(device_name) = nvme_device.device_path.strip_prefix("/dev/") {
                device_name.to_string()
            } else {
                nvme_device.device_path.clone()
            };
            
            base_bdevs.push(bdev_name.clone());
            
            println!("🔧 [RAID_MEMBER] Using NVMe-oF bdev '{}' from disk '{}' (NQN: {}, type: {:?})", 
                     bdev_name, member.disk_ref, endpoint.nqn, disk.spec.disk_type);
        }

        let base_bdevs_str = base_bdevs.join(" ");
        let raid_bdev_name = raid_disk.spec.raid_bdev_name();

        // Create RAID bdev using SPDK RPC
        call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid_bdev_name,
                "raid_level": raid_disk.spec.raid_level,
                "base_bdevs": base_bdevs_str,
                "strip_size_kb": raid_disk.spec.stripe_size_kb,
                "superblock": raid_disk.spec.superblock_enabled
            }
        })).await
        .map_err(|e| Status::internal(format!("Failed to create RAID bdev: {}", e)))?;

        println!("✅ [RAID_BDEV] Created RAID bdev '{}' on node {}", raid_bdev_name, target_node);
        Ok(())
    }

    /// Create LVS on the RAID disk
    async fn create_lvs_on_raid_disk(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let target_node = &raid_disk.spec.created_on_node;
        let node_ip = self.driver.get_node_ip(target_node).await?;
        let spdk_rpc_url = format!("http://{}:9009", node_ip);

        let raid_bdev_name = raid_disk.spec.raid_bdev_name();
        let lvs_name = raid_disk.spec.lvs_name();

        // Create LVS on the RAID bdev
        call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": raid_bdev_name,
                "lvs_name": lvs_name
            }
        })).await
        .map_err(|e| Status::internal(format!("Failed to create LVS on RAID disk: {}", e)))?;

        println!("✅ [LVS_RAID] Created LVS '{}' on RAID disk {}", lvs_name, raid_disk.spec.raid_disk_id);

        // Update RAID disk status with LVS information
        self.update_raid_disk_status(raid_disk, &lvs_name).await?;

        Ok(())
    }

    /// Update RAID disk status after successful LVS creation
    async fn update_raid_disk_status(&self, raid_disk: &SpdkRaidDisk, lvs_name: &str) -> Result<(), Status> {
        let raid_disks: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.state = "online".to_string();
        status.raid_bdev_name = Some(raid_disk.spec.raid_bdev_name());
        status.lvs_name = Some(lvs_name.to_string());
        status.health_status = "healthy".to_string();
        status.degraded = false;
        status.active_member_count = raid_disk.spec.num_member_disks as u32;
        status.failed_member_count = 0;
        status.last_checked = chrono::Utc::now().to_rfc3339();
        status.created_at = Some(chrono::Utc::now().to_rfc3339());

        let patch = json!({
            "status": status
        });

        raid_disks.patch_status(
            &raid_disk.metadata.name.as_ref().unwrap(),
            &PatchParams::default(),
            &Patch::Merge(patch)
        ).await
        .map_err(|e| Status::internal(format!("Failed to update RAID disk status: {}", e)))?;

        Ok(())
    }

    /// Provision volume with specified number of replicas - unified for single and multi-replica
    async fn provision_volume(
        &self,
        volume_id: &str,
        capacity: i64,
        num_replicas: i32,
    ) -> Result<SpdkVolume, Status> {
        // Validate inputs
        self.validate_volume_request(volume_id, capacity, num_replicas).await?;
        
        // Unified storage backend selection and logical volume creation
        let (storage_backend, lvol_uuid, lvs_name) = if num_replicas > 1 {
            // Multi-replica: Use or create RAID disk, then create logical volume on it
            self.provision_multi_replica_volume(volume_id, capacity, num_replicas).await?
        } else {
            // Single replica: Use single disk, create logical volume on it  
            self.provision_single_replica_volume(volume_id, capacity).await?
        };

        // Create unified SpdkVolume spec (same structure for both single and multi-replica)
        let spdk_volume = SpdkVolume::new_with_metadata(
            volume_id,
            SpdkVolumeSpec {
                volume_id: volume_id.to_string(),
                size_bytes: capacity,
                num_replicas,
                storage_backend,
                lvol_uuid: Some(lvol_uuid),
                lvs_name: Some(lvs_name),
                nvmeof_transport: Some(self.driver.nvmeof_transport.clone()),
                nvmeof_target_port: Some(self.driver.nvmeof_target_port),
                // Legacy fields for backward compatibility (empty for new volumes)
                replicas: Vec::new(),
                primary_lvol_uuid: None,
                write_ordering_enabled: false,
                raid_auto_rebuild: num_replicas > 1,
                ..Default::default()
            },
            &self.driver.target_namespace,
        );

        // Create CRD with enhanced debugging
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        // Debug: Log the SpdkVolume object we're trying to create
        println!("🔍 [CRD_DEBUG] Attempting to create SpdkVolume CRD:");
        println!("   Volume ID: {}", spdk_volume.spec.volume_id);
        println!("   Namespace: {}", self.driver.target_namespace);
        println!("   Size: {} bytes", spdk_volume.spec.size_bytes);
        println!("   Storage Backend: {:?}", spdk_volume.spec.storage_backend);
        
        // Serialize to JSON for debugging
        match serde_json::to_string_pretty(&spdk_volume) {
            Ok(json_str) => {
                println!("🔍 [CRD_DEBUG] SpdkVolume JSON payload:");
                println!("{}", json_str);
            },
            Err(e) => {
                println!("❌ [CRD_DEBUG] Failed to serialize SpdkVolume to JSON: {}", e);
                return Err(Status::internal(format!("Failed to serialize SpdkVolume: {}", e)));
            }
        }
        
        // Try to create the CRD with idempotency handling
        match crd_api.create(&PostParams::default(), &spdk_volume).await {
            Ok(created_volume) => {
                println!("✅ [CRD_DEBUG] Successfully created SpdkVolume CRD: {}", created_volume.metadata.name.as_deref().unwrap_or("unknown"));
            },
            Err(kube::Error::Api(api_error)) if api_error.code == 409 => {
                // Handle "AlreadyExists" - this is expected for idempotent operations
                println!("🔍 [CRD_DEBUG] SpdkVolume CRD already exists, checking compatibility...");
                
                match crd_api.get(volume_id).await {
                    Ok(existing_volume) => {
                        println!("✅ [CRD_DEBUG] Found existing SpdkVolume CRD");
                        
                        // Validate that existing volume is compatible
                        if existing_volume.spec.size_bytes == spdk_volume.spec.size_bytes &&
                           existing_volume.spec.num_replicas == spdk_volume.spec.num_replicas {
                            println!("✅ [CRD_DEBUG] Existing SpdkVolume is compatible (size: {}, replicas: {})", 
                                existing_volume.spec.size_bytes, existing_volume.spec.num_replicas);
                            
                            // Update our return value to the existing volume
                            let compatible_volume = existing_volume;
                            
                            // Note: Disk status updates are handled during volume provisioning
                            return Ok(compatible_volume);
                        } else {
                            println!("❌ [CRD_DEBUG] Existing SpdkVolume is incompatible:");
                            println!("   Existing: size={}, replicas={}", existing_volume.spec.size_bytes, existing_volume.spec.num_replicas);
                            println!("   Requested: size={}, replicas={}", spdk_volume.spec.size_bytes, spdk_volume.spec.num_replicas);
                            return Err(Status::already_exists(format!(
                                "Volume {} already exists with incompatible specifications", volume_id
                            )));
                        }
                    },
                    Err(get_error) => {
                        println!("❌ [CRD_DEBUG] Failed to get existing SpdkVolume for compatibility check: {}", get_error);
                        return Err(Status::internal(format!("Failed to validate existing SpdkVolume: {}", get_error)));
                    }
                }
            },
            Err(kube::Error::Api(api_error)) => {
                println!("❌ [CRD_DEBUG] Kubernetes API error creating SpdkVolume:");
                println!("   Status: {}", api_error.status);
                println!("   Code: {}", api_error.code);
                println!("   Message: {}", api_error.message);
                println!("   Reason: {}", api_error.reason);
                return Err(Status::internal(format!("Failed to create SpdkVolume CRD: Kubernetes API error: {}", api_error.message)));
            },
            Err(kube::Error::SerdeError(serde_error)) => {
                println!("❌ [CRD_DEBUG] Serialization/Deserialization error:");
                println!("   Error: {}", serde_error);
                return Err(Status::internal(format!("Failed to create SpdkVolume CRD: Serialization error: {}", serde_error)));
            },
            Err(other_error) => {
                println!("❌ [CRD_DEBUG] Other error creating SpdkVolume:");
                println!("   Error type: {:?}", std::any::type_name_of_val(&other_error));
                println!("   Error: {}", other_error);
                return Err(Status::internal(format!("Failed to create SpdkVolume CRD: {}", other_error)));
            }
        }

        // Update disk statuses
        // Note: Disk status updates are handled during storage backend provisioning

        Ok(spdk_volume)
    }

    /// Comprehensive validation for volume creation requests
    async fn validate_volume_request(
        &self,
        volume_id: &str,
        capacity: i64,
        num_replicas: i32,
    ) -> Result<(), Status> {
        // Validate volume ID
        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID cannot be empty"));
        }

        if volume_id.len() > 63 {
            return Err(Status::invalid_argument("Volume ID cannot exceed 63 characters"));
        }

        // Validate volume ID format (DNS-1123 subdomain)
        let volume_id_regex = regex::Regex::new(r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$").unwrap();
        if !volume_id_regex.is_match(volume_id) {
            return Err(Status::invalid_argument(
                "Volume ID must be a valid DNS-1123 subdomain (lowercase alphanumeric and hyphens)"
            ));
        }

        // Validate capacity
        const MIN_CAPACITY: i64 = 1024 * 1024 * 1024; // 1GB
        const MAX_CAPACITY: i64 = 64 * 1024 * 1024 * 1024 * 1024; // 64TB

        if capacity < MIN_CAPACITY {
            return Err(Status::invalid_argument(
                format!("Volume capacity must be at least {} bytes (1GB)", MIN_CAPACITY)
            ));
        }

        if capacity > MAX_CAPACITY {
            return Err(Status::invalid_argument(
                format!("Volume capacity cannot exceed {} bytes (64TB)", MAX_CAPACITY)
            ));
        }

        // Validate replica count
        if num_replicas < 1 {
            return Err(Status::invalid_argument("Number of replicas must be at least 1"));
        }

        if num_replicas > 5 {
            return Err(Status::invalid_argument(
                "Number of replicas cannot exceed 5 (performance and complexity limitations)"
            ));
        }

        // For RAID1, only support 2 replicas currently
        if num_replicas > 2 {
            return Err(Status::invalid_argument(
                "Multi-replica volumes currently support only 2 replicas (RAID1)"
            ));
        }

        // Check if volume already exists
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        if volumes_api.get(volume_id).await.is_ok() {
            return Err(Status::already_exists(format!("Volume {} already exists", volume_id)));
        }

        Ok(())
    }

    // REMOVED: validate_disk_availability - unused in simplified architecture

    /// Enhanced disk selection with better node separation logic
    fn select_disks_with_node_separation(
        &self, 
        available_disks: Vec<SpdkDisk>, 
        num_replicas: usize
    ) -> Result<Vec<SpdkDisk>, Status> {
        let mut selected_disks = Vec::new();
        let mut used_nodes = std::collections::HashSet::new();
        
        // Sort disks by free space (descending) for better selection
        let mut sorted_disks = available_disks;
        sorted_disks.sort_by(|a, b| {
            let a_free = a.status.as_ref().map(|s| s.free_space).unwrap_or(0);
            let b_free = b.status.as_ref().map(|s| s.free_space).unwrap_or(0);
            b_free.cmp(&a_free)
        });

        for disk in sorted_disks {
            if !used_nodes.contains(&disk.spec.node_id) && selected_disks.len() < num_replicas {
                used_nodes.insert(disk.spec.node_id.clone());
                selected_disks.push(disk);
            }
        }

        if selected_disks.len() < num_replicas {
            return Err(Status::resource_exhausted(
                format!(
                    "Cannot achieve node separation: selected {} disks from {} unique nodes, need {}",
                    selected_disks.len(),
                    used_nodes.len(),
                    num_replicas
                )
            ));
        }

        // Log selection for debugging
        let selected_nodes: Vec<_> = selected_disks.iter()
            .map(|d| &d.spec.node_id)
            .collect();
        println!("Selected disks on nodes: {:?}", selected_nodes);

        Ok(selected_disks)
    }

    async fn get_available_disks(&self, disks: &Api<SpdkDisk>, capacity: i64) -> Result<Vec<SpdkDisk>, Status> {
        Ok(disks
            .list(&ListParams::default())
            .await
            .map_err(|e| Status::internal(format!("Failed to list SpdkDisks: {}", e)))?
            .items
            .into_iter()
            .filter(|d| {
                d.status.as_ref().map_or(false, |s| 
                    s.healthy && s.blobstore_initialized && s.free_space >= capacity)
            })
            .collect())
    }

    // REMOVED: create_replicas - replaced by unified RAID disk provisioning

    async fn create_volume_lvol(
        &self,
        disk: &SpdkDisk,
        size_bytes: i64,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        println!("🚀 [DEBUG] create_volume_lvol called - volume_id: {}, size: {} bytes", volume_id, size_bytes);
        let rpc_url = self.driver.get_rpc_url_for_node(&disk.spec.node_id).await?;
        println!("🚀 [DEBUG] RPC URL: {}", rpc_url);
        let http_client = HttpClient::new();
        
        // Get the actual LVS name from the disk status (don't guess it from metadata name)
        let lvs_name = disk.status.as_ref()
            .and_then(|s| s.lvs_name.as_ref())
            .ok_or("Disk does not have LVS initialized or LVS name missing")?
            .clone();
        
        let lvol_name = format!("vol_{}", volume_id);

        // Convert bytes to MiB as required by SPDK bdev_lvol_create RPC
        let size_in_mib = (size_bytes + 1048575) / 1048576; // Round up to nearest MiB

        let create_params = json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size_in_mib": size_in_mib,
                "thin_provision": false,
                "clear_method": "write_zeroes"
            }
        });

        println!("🔧 [CREATE_LVOL] Creating logical volume with parameters:");
        println!("   LVS name: '{}'", lvs_name);
        println!("   LVOL name: '{}'", lvol_name);
        println!("   Size: {} bytes", size_bytes);
        println!("   RPC URL: {}", rpc_url);
        println!("   Full JSON payload: {}", serde_json::to_string_pretty(&create_params).unwrap_or_else(|_| "Failed to serialize".to_string()));

        let lvol_response = http_client
            .post(&rpc_url)
            .json(&create_params)
            .send()
            .await?;

        let response_status = lvol_response.status();
        println!("📥 [CREATE_LVOL] Response status: {}", response_status);
        
        if !response_status.is_success() {
            let error_text = lvol_response.text().await?;
            println!("❌ [CREATE_LVOL] HTTP request failed with status {}: {}", response_status, error_text);
            
            // Check if this is a "File exists" error - if so, try to handle it idempotently
            if error_text.contains("File exists") || error_text.contains("Code=-17") {
                return self.handle_existing_volume(disk, &rpc_url, &lvol_name, size_bytes).await;
            }
            
            return Err(format!("Failed to create lvol: {}", error_text).into());
        }

        // Get the response text first to log it, then parse as JSON
        let response_text = lvol_response.text().await?;
        println!("📥 [CREATE_LVOL] Raw response: {}", response_text);
        
        let lvol_info: serde_json::Value = match serde_json::from_str(&response_text) {
            Ok(json) => {
                println!("✅ [CREATE_LVOL] Successfully parsed response JSON");
                json
            }
            Err(e) => {
                println!("❌ [CREATE_LVOL] Failed to parse response as JSON: {}", e);
                println!("❌ [CREATE_LVOL] Raw response was: {}", response_text);
                return Err(format!("Failed to parse SPDK response as JSON: {}", e).into());
            }
        };

        // Check if the response contains an error
        if let Some(error) = lvol_info.get("error") {
            println!("❌ [CREATE_LVOL] SPDK returned error: {}", serde_json::to_string_pretty(error).unwrap_or_else(|_| format!("{:?}", error)));
            
            // Handle "File exists" error with idempotency
            if let Some(error_code) = error.get("code").and_then(|c| c.as_i64()) {
                if error_code == -17 {  // SPDK "File exists" error
                    println!("⚠️ [CREATE_LVOL] Volume already exists, checking compatibility...");
                    return self.handle_existing_volume(disk, &rpc_url, &lvol_name, size_bytes).await;
                }
            }
            
            return Err(format!("SPDK RPC error: {}", serde_json::to_string(error).unwrap_or_else(|_| format!("{:?}", error))).into());
        }
        
        println!("🔍 [CREATE_LVOL] Extracting UUID from result...");
        let lvol_uuid = {
            let result = lvol_info.get("result").cloned().unwrap_or(json!(null));
            
            // Try to extract UUID from different possible response formats
            let uuid_str = if let Some(uuid_in_object) = result.get("uuid").and_then(|u| u.as_str()) {
                // Format: {"result": {"uuid": "abc-123"}}
                println!("📝 [CREATE_LVOL] Found UUID in nested object format");
                uuid_in_object
            } else if let Some(uuid_direct) = result.as_str() {
                // Format: {"result": "abc-123"}
                println!("📝 [CREATE_LVOL] Found UUID in direct string format");
                uuid_direct
            } else {
                println!("❌ [CREATE_LVOL] No UUID found in result. Result section: {}", serde_json::to_string_pretty(&result).unwrap_or_else(|_| format!("{:?}", result)));
                return Err("Failed to get lvol UUID from SPDK response".into());
            };
            
            uuid_str.to_string()
        };

        println!("✅ [CREATE_LVOL] Successfully created logical volume with UUID: {}", lvol_uuid);

        Ok(lvol_uuid)
    }

    /// Handle the case where a logical volume already exists - implement CSI idempotency
    async fn handle_existing_volume(
        &self,
        disk: &SpdkDisk,
        rpc_url: &str,
        lvol_name: &str,
        requested_size_bytes: i64,
    ) -> Result<String, Box<dyn std::error::Error>> {
        println!("🔍 [IDEMPOTENT] Checking existing volume: {}", lvol_name);
        
        let http_client = HttpClient::new();
        
        // Query the existing logical volume using the full alias path
        let full_lvol_name = format!("{}/{}", 
            disk.status.as_ref()
                .and_then(|s| s.lvs_name.as_ref())
                .ok_or("Disk does not have LVS initialized")?,
            lvol_name);
        
        let query_params = json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": full_lvol_name
            }
        });

        let query_response = http_client
            .post(rpc_url)
            .json(&query_params)
            .send()
            .await?;

        if !query_response.status().is_success() {
            let error_text = query_response.text().await?;
            println!("❌ [IDEMPOTENT] Failed to query existing volume: {}", error_text);
            return Err(format!("Failed to query existing volume: {}", error_text).into());
        }

        let query_result: serde_json::Value = query_response.json().await?;
        
        if let Some(error) = query_result.get("error") {
            println!("❌ [IDEMPOTENT] SPDK query error: {}", serde_json::to_string_pretty(error).unwrap_or_else(|_| format!("{:?}", error)));
            return Err(format!("Failed to query existing volume: {}", serde_json::to_string(error).unwrap_or_else(|_| format!("{:?}", error))).into());
        }

        if let Some(bdevs) = query_result.get("result").and_then(|r| r.as_array()) {
            if bdevs.is_empty() {
                println!("❌ [IDEMPOTENT] Volume {} not found, but creation failed with 'File exists'", lvol_name);
                return Err("Volume creation failed with 'File exists' but volume cannot be found".into());
            }

            let existing_bdev = &bdevs[0];
            let existing_size_bytes = existing_bdev["num_blocks"]
                .as_u64()
                .and_then(|blocks| existing_bdev["block_size"].as_u64().map(|bs| blocks * bs))
                .ok_or("Failed to get existing volume size")?;
                
            let existing_uuid = existing_bdev["uuid"]
                .as_str()
                .ok_or("Failed to get existing volume UUID")?;

            println!("🔍 [IDEMPOTENT] Found existing volume:");
            println!("   Name: {}", lvol_name);
            println!("   UUID: {}", existing_uuid);
            println!("   Size: {} bytes", existing_size_bytes);
            println!("   Requested size: {} bytes", requested_size_bytes);

            // Check if the size is compatible (allow some tolerance for MiB alignment)
            let size_tolerance = 1048576; // 1 MiB tolerance
            if (existing_size_bytes as i64 - requested_size_bytes).abs() <= size_tolerance {
                println!("✅ [IDEMPOTENT] Existing volume is compatible, returning existing UUID");
                return Ok(existing_uuid.to_string());
            } else {
                println!("❌ [IDEMPOTENT] Size mismatch - existing: {} bytes, requested: {} bytes", 
                        existing_size_bytes, requested_size_bytes);
                return Err(format!(
                    "Volume {} already exists with different size: existing {} bytes, requested {} bytes",
                    lvol_name, existing_size_bytes, requested_size_bytes
                ).into());
            }
        } else {
            println!("❌ [IDEMPOTENT] Unexpected response format from bdev_get_bdevs");
            return Err("Unexpected response format when querying existing volume".into());
        }
    }

    async fn update_disk_statuses(&self, disks: &Api<SpdkDisk>, selected_disks: &[SpdkDisk], capacity: i64, delta: i32) -> Result<(), Status> {
        for disk in selected_disks {
            let disk_name = disk.metadata.name.as_ref().unwrap();
            let mut disk_status = disk.status.clone().unwrap_or_default();
            
            disk_status.free_space -= capacity * delta as i64;
            disk_status.used_space += capacity * delta as i64;
            disk_status.lvol_count = if delta > 0 {
                disk_status.lvol_count + delta as u32
            } else {
                disk_status.lvol_count.saturating_sub((-delta) as u32)
            };

            disks.patch_status(disk_name, &PatchParams::default(), 
                             &Patch::Merge(json!({ "status": disk_status })))
                .await
                .map_err(|e| Status::internal(format!("Failed to update SpdkDisk: {}", e)))?;
        }
        Ok(())
    }

    async fn delete_volume_replicas(&self, volume: &SpdkVolume) -> Result<(), Status> {
        for replica in &volume.spec.replicas {
            // Delete NVMe-oF target if exists
            if let Some(nqn) = &replica.nqn {
                let rpc_url = self.driver.get_rpc_url_for_node(&replica.node).await?;
                let http_client = HttpClient::new();
                
                http_client
                    .post(&rpc_url)
                    .json(&json!({
                        "method": "nvmf_delete_subsystem",
                        "params": { "nqn": nqn }
                    }))
                    .send()
                    .await
                    .ok(); // Best effort
            }

            // Delete lvol
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                // Get the actual LVS name from the disk CRD status
                // Use UUID directly for logical volume deletion
                let lvol_bdev_name = lvol_uuid.clone();
                
                let rpc_url = self.driver.get_rpc_url_for_node(&replica.node).await?;
                let http_client = HttpClient::new();
                
                http_client
                    .post(&rpc_url)
                    .json(&json!({
                        "method": "bdev_lvol_delete",
                        "params": { "name": lvol_bdev_name }
                    }))
                    .send()
                    .await
                    .ok(); // Best effort
            }
        }
        Ok(())
    }

    fn build_volume_topology(&self, replicas: &[Replica]) -> Vec<Topology> {
        // Return empty topology to allow multi-node NVMe-oF access
        // This enables pods to mount volumes from any node via NVMe-oF networking
        println!("🌐 [MULTINODE] Enabling multi-node access via NVMe-oF for volume with {} replicas", replicas.len());
        vec![]
    }

    fn build_volume_context(&self) -> std::collections::HashMap<String, String> {
        [
            ("storageType".to_string(), "spdk-nvmeof".to_string()),
            ("transport".to_string(), self.driver.nvmeof_transport.clone()),
            ("port".to_string(), self.driver.nvmeof_target_port.to_string())
        ].into_iter().collect()
    }

    // REMOVED: get_actual_lvs_name - LVS names are now deterministic with lvs_uuid format
}

#[tonic::async_trait]
impl Controller for ControllerService {
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_name = req.name.clone();
        let capacity = req.capacity_range.as_ref().map(|cr| cr.required_bytes).unwrap_or(0);
        
        println!("🚀 [CSI_CONTROLLER] CreateVolume request received:");
        println!("   Volume name: {}", volume_name);
        println!("   Capacity: {} bytes ({} GB)", capacity, capacity / (1024 * 1024 * 1024));
        println!("   Parameters: {:?}", req.parameters);
        
        if volume_name.is_empty() || capacity == 0 {
            let error_msg = "Missing name or capacity";
            println!("❌ [CSI_CONTROLLER] CreateVolume failed: {}", error_msg);
            return Err(Status::invalid_argument(error_msg));
        }

        let num_replicas = req.parameters
            .get("numReplicas")
            .and_then(|n| n.parse::<i32>().ok())
            .unwrap_or(1);

        println!("   Number of replicas requested: {}", num_replicas);

        match self.provision_volume(&volume_name, capacity, num_replicas).await {
            Ok(spdk_volume) => {
                println!("✅ [CSI_CONTROLLER] Volume provisioned successfully: {}", volume_name);
                let accessible_topology = self.build_volume_topology(&spdk_volume.spec.replicas);

                let volume = Volume {
                    volume_id: spdk_volume.spec.volume_id.clone(),
                    capacity_bytes: spdk_volume.spec.size_bytes,
                    volume_context: self.build_volume_context(),
                    content_source: req.volume_content_source,
                    accessible_topology,
                    ..Default::default()
                };

                Ok(Response::new(CreateVolumeResponse {
                    volume: Some(volume),
                }))
            },
            Err(status) => {
                println!("❌ [CSI_CONTROLLER] Volume provisioning failed for '{}': {}", volume_name, status.message());
                println!("   Error code: {:?}", status.code());
                
                // For resource exhaustion errors, provide more detailed context
                if status.code() == tonic::Code::ResourceExhausted {
                    if status.message().contains("Insufficient healthy disks") {
                        let enhanced_message = format!(
                            "Cannot create {}-replica volume: {}. Available SPDK disks with LVS: {}. For RAID volumes, ensure you have at least {} healthy disks with initialized LVS (Logical Volume Store) across different nodes.",
                            num_replicas,
                            status.message(),
                            self.get_available_disk_count().await.unwrap_or(0),
                            num_replicas
                        );
                        println!("   Enhanced error message: {}", enhanced_message);
                        return Err(Status::resource_exhausted(enhanced_message));
                    }
                }
                
                Err(status)
            }
        }
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let volume_id = request.into_inner().volume_id;
        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Missing volume ID"));
        }

        let crd_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let spdk_volume = match crd_api.get(&volume_id).await {
            Ok(vol) => vol,
            Err(_) => return Ok(Response::new(DeleteVolumeResponse {})),
        };

        // Delete replicas
        self.delete_volume_replicas(&spdk_volume).await?;

        // Update disk statuses
        let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let mut disk_refs = Vec::new();
        for replica in &spdk_volume.spec.replicas {
            if let Ok(disk) = disks.get(&replica.disk_ref).await {
                disk_refs.push(disk);
            }
        }
        
        self.update_disk_statuses(&disks, &disk_refs, spdk_volume.spec.size_bytes, -1).await?;

        // Delete CRD
        crd_api.delete(&volume_id, &Default::default()).await.ok();

        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        _request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        Ok(Response::new(ControllerPublishVolumeResponse {
            publish_context: std::collections::HashMap::new(),
        }))
    }

    async fn controller_unpublish_volume(
        &self,
        _request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        Ok(Response::new(ControllerUnpublishVolumeResponse {}))
    }

    async fn validate_volume_capabilities(
        &self,
        request: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID is required"));
        }

        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        match volumes_api.get(&volume_id).await {
            Ok(_) => {
                let confirmed_capabilities: Vec<_> = req.volume_capabilities.into_iter()
                    .filter(|capability| {
                        let supported_access_mode = capability.access_mode.as_ref()
                            .map(|am| {
                                let mode = am.mode;
                                mode == volume_capability::access_mode::Mode::SingleNodeWriter as i32 ||
                                mode == volume_capability::access_mode::Mode::SingleNodeReaderOnly as i32 ||
                                mode == volume_capability::access_mode::Mode::SingleNodeSingleWriter as i32
                            })
                            .unwrap_or(false);

                        let supported_access_type = matches!(
                            capability.access_type,
                            Some(volume_capability::AccessType::Block(_)) |
                            Some(volume_capability::AccessType::Mount(_))
                        );

                        supported_access_mode && supported_access_type
                    })
                    .collect();

                let is_confirmed = !confirmed_capabilities.is_empty();

                Ok(Response::new(ValidateVolumeCapabilitiesResponse {
                    confirmed: if is_confirmed {
                        Some(validate_volume_capabilities_response::Confirmed { 
                            volume_capabilities: confirmed_capabilities,
                            volume_context: req.volume_context,
                            parameters: req.parameters,
                            mutable_parameters: std::collections::HashMap::new(),
                        })
                    } else {
                        None
                    },
                    message: if is_confirmed {
                        "Volume capabilities validated successfully".to_string()
                    } else {
                        "Unsupported volume capabilities".to_string()
                    },
                }))
            }
            Err(_) => Err(Status::not_found(format!("Volume {} not found", volume_id))),
        }
    }

    async fn list_volumes(
        &self,
        _request: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let volume_list = volumes_api.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list volumes: {}", e)))?;

        let entries = volume_list.items.iter().map(|volume| {
            list_volumes_response::Entry {
                volume: Some(Volume {
                    volume_id: volume.spec.volume_id.clone(),
                    capacity_bytes: volume.spec.size_bytes,
                    volume_context: self.build_volume_context(),
                    content_source: None,
                    accessible_topology: self.build_volume_topology(&volume.spec.replicas),
                }),
                status: volume.status.as_ref().map(|s| list_volumes_response::VolumeStatus {
                    published_node_ids: vec![],
                    volume_condition: if s.degraded {
                        Some(VolumeCondition {
                            abnormal: true,
                            message: "Volume is in degraded state".to_string(),
                        })
                    } else {
                        None
                    },
                }),
            }
        }).collect();

        Ok(Response::new(ListVolumesResponse {
            entries,
            next_token: String::new(),
        }))
    }

    async fn get_capacity(
        &self,
        _request: Request<GetCapacityRequest>,
    ) -> Result<Response<GetCapacityResponse>, Status> {
        let disks_api: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let disk_list = disks_api.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list disks: {}", e)))?;

        let total_capacity = disk_list.items.iter()
            .filter(|disk| disk.status.as_ref().map_or(false, |s| s.healthy && s.blobstore_initialized))
            .map(|disk| disk.status.as_ref().unwrap().free_space)
            .sum::<i64>();

        Ok(Response::new(GetCapacityResponse {
            available_capacity: total_capacity,
            maximum_volume_size: Some(total_capacity),
            minimum_volume_size: Some(1024 * 1024 * 1024), // 1GB minimum
        }))
    }

    async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        create_snapshot_impl(&self.driver, request).await
    }

    async fn delete_snapshot(
        &self,
        request: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        delete_snapshot_impl(&self.driver, request).await
    }

    async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        list_snapshots_impl(&self.driver, request).await
    }

    async fn controller_expand_volume(
        &self,
        request: Request<ControllerExpandVolumeRequest>,
    ) -> Result<Response<ControllerExpandVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let new_capacity = req.capacity_range
            .as_ref()
            .map(|cr| cr.required_bytes)
            .unwrap_or(0);

        if volume_id.is_empty() || new_capacity <= 0 {
            return Err(Status::invalid_argument("Volume ID and new capacity are required"));
        }

        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;

        if new_capacity <= volume.spec.size_bytes {
            return Err(Status::invalid_argument("New capacity must be larger than current capacity"));
        }

        // Expand lvols on each replica
        let mut failed_replicas = Vec::new();
        for replica in &volume.spec.replicas {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let rpc_url = self.driver.get_rpc_url_for_node(&replica.node).await?;
                let http_client = HttpClient::new();
                
                // Get the actual LVS name from the disk CRD status
                // Use UUID directly for logical volume expansion
                let lvol_name = lvol_uuid.clone();

                // Convert bytes to MiB as required by SPDK bdev_lvol_resize RPC
                let size_in_mib = (new_capacity + 1048575) / 1048576; // Round up to nearest MiB

                let response = http_client
                    .post(&rpc_url)
                    .json(&json!({
                        "method": "bdev_lvol_resize",
                        "params": {
                            "name": lvol_name,
                            "size_in_mib": size_in_mib
                        }
                    }))
                    .send()
                    .await
                    .map_err(|e| Status::internal(format!("SPDK RPC call failed: {}", e)))?;

                if !response.status().is_success() {
                    let error_text = response.text().await.unwrap_or_default();
                    failed_replicas.push(format!("Replica on node {}: {}", replica.node, error_text));
                }
            }
        }

        if !failed_replicas.is_empty() {
            return Err(Status::internal(format!("Failed to expand replicas: {:?}", failed_replicas)));
        }

        // Update volume spec
        let patch = json!({ "spec": { "size_bytes": new_capacity } });
        volumes_api.patch(&volume_id, &PatchParams::default(), &Patch::Merge(patch)).await
            .map_err(|e| Status::internal(format!("Failed to update volume spec: {}", e)))?;

        Ok(Response::new(ControllerExpandVolumeResponse {
            capacity_bytes: new_capacity,
            node_expansion_required: true,
        }))
    }

    async fn controller_get_volume(
        &self,
        request: Request<ControllerGetVolumeRequest>,
    ) -> Result<Response<ControllerGetVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID is required"));
        }

        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let volume = volumes_api.get(&volume_id).await
            .map_err(|_| Status::not_found(format!("Volume {} not found", volume_id)))?;

        let csi_volume = Volume {
            volume_id: volume.spec.volume_id.clone(),
            capacity_bytes: volume.spec.size_bytes,
            volume_context: self.build_volume_context(),
            content_source: None,
            accessible_topology: self.build_volume_topology(&volume.spec.replicas),
        };

        let status = volume.status.as_ref().map(|vol_status| {
            controller_get_volume_response::VolumeStatus {
                published_node_ids: vec![],
                volume_condition: if vol_status.degraded {
                    Some(VolumeCondition {
                        abnormal: true,
                        message: format!("Volume state: {}", vol_status.state),
                    })
                } else {
                    None
                },
            }
        });

        Ok(Response::new(ControllerGetVolumeResponse {
            volume: Some(csi_volume),
            status,
        }))
    }

    async fn controller_modify_volume(
        &self,
        _request: Request<ControllerModifyVolumeRequest>,
    ) -> Result<Response<ControllerModifyVolumeResponse>, Status> {
        Err(Status::unimplemented("Volume modification is not supported"))
    }

    async fn controller_get_capabilities(
        &self,
        _request: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        Ok(Response::new(ControllerGetCapabilitiesResponse {
            capabilities: vec![
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::CreateDeleteVolume as i32,
                        },
                    )),
                },
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::CreateDeleteSnapshot as i32,
                        },
                    )),
                },
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::ListSnapshots as i32,
                        },
                    )),
                },
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::CloneVolume as i32,
                        },
                    )),
                },
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::ExpandVolume as i32,
                        },
                    )),
                },
            ],
        }))
    }
}
