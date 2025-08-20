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

/// Available NVMe disk information for automatic RAID creation
#[derive(Debug, Clone)]
struct AvailableNvmeDisk {
    pub node_id: String,
    pub device_path: String,
    pub serial_number: String,
    pub wwn: Option<String>,
    pub model: String,
    pub vendor: String,
    pub capacity: i64,
    pub pci_address: String,
}

/// SPDK state information for idempotent operations
#[derive(Debug, Default)]
struct SpdkRaidState {
    pub bdevs: Vec<String>,        // All available bdevs
    pub raid_bdevs: Vec<String>,   // RAID bdevs specifically
    pub lvs_stores: Vec<LvsState>, // LVS information
}

#[derive(Debug)]
struct LvsState {
    pub name: String,
    pub base_bdev: String,
    pub total_capacity: u64,
    pub free_capacity: u64,
    pub cluster_size: u64,
}

pub struct ControllerService {
    driver: Arc<SpdkCsiDriver>,
}

impl ControllerService {
    pub fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }

    /// Get count of available healthy RAID disks (online and healthy)
    async fn get_available_disk_count(&self) -> Result<usize, Box<dyn std::error::Error>> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let raid_list = raids.list(&ListParams::default()).await?;
        let count = raid_list.items.iter()
            .filter(|raid| raid.status.as_ref().map_or(false, |s| s.state == "online" && !s.degraded))
            .count();
        Ok(count)
    }

    // ============================================================================
    // UNIFIED VOLUME PROVISIONING (Single and Multi-Replica)
    // ============================================================================

    // Single replica path removed: unified RAID-based provisioning is used for all volumes

    /// Provision a multi-replica volume on a RAID disk


    /// Create logical volume on a RAID disk with thin provisioning
    async fn create_volume_lvol_on_raid(
        &self,
        raid_disk: &SpdkRaidDisk,
        capacity: i64,
        volume_id: &str,
    ) -> Result<String, Status> {
        let target_node = &raid_disk.spec.created_on_node;
        let spdk_rpc_url = self.driver.get_rpc_url_for_node(target_node).await?;
        let lvs_name = raid_disk.spec.lvs_name();

        println!("🔧 [THIN_PROVISION] Creating thin-provisioned logical volume {} ({}GB) on LVS {}", 
                 volume_id, capacity / (1024 * 1024 * 1024), lvs_name);

        // Create thin-provisioned logical volume on the RAID disk's LVS
        let response = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvol_name": volume_id,
                "size": capacity,
                "lvs_name": lvs_name,
                "thin_provision": true,  // Enable thin provisioning for efficient storage usage
                "clear_method": "none"   // Don't clear blocks on allocation for performance
            }
        })).await
        .map_err(|e| Status::internal(format!("Failed to create thin-provisioned lvol on RAID disk: {}", e)))?;

        let lvol_uuid = response["uuid"].as_str()
            .ok_or_else(|| Status::internal("SPDK response missing lvol UUID"))?
            .to_string();

        println!("✅ [THIN_PROVISION] Created thin-provisioned lvol {} (UUID: {}) on RAID disk LVS {}", 
                 volume_id, lvol_uuid, lvs_name);
        
        // Log thin provisioning benefits for operators
        println!("💡 [THIN_PROVISION] Benefits: Storage allocated on-demand, improved utilization, faster provisioning");
        
        // Update RAID disk used capacity after volume creation
        self.update_raid_disk_used_capacity(raid_disk, capacity).await?;
        
        Ok(lvol_uuid)
    }

    // ============================================================================
    // RAID DISK MANAGEMENT (Only for Multi-Replica)
    // ============================================================================

    /// Find or create a suitable RAID disk for volume provisioning
    async fn find_or_create_raid_disk(
        &self,
        num_replicas: i32,
        required_capacity: i64,
        raid_level: &str,
    ) -> Result<SpdkRaidDisk, Status> {
        println!("🔍 [RAID_PROVISION] Looking for RAID disk: {} replicas, {} bytes, RAID{}", 
                 num_replicas, required_capacity, raid_level);

        // Step 1: Try to find an existing RAID disk that can accommodate the volume
        if let Ok(existing_raid) = self.find_suitable_raid_disk(num_replicas, required_capacity, raid_level).await {
            println!("✅ [RAID_PROVISION] Found existing suitable RAID disk: {}", 
                     existing_raid.metadata.name.as_ref().unwrap_or(&"unknown".to_string()));
            return Ok(existing_raid);
        }

        println!("🔄 [RAID_PROVISION] No existing RAID disk found, attempting auto-creation...");

        // Step 2: Auto-create a new RAID disk based on available resources
        match self.auto_create_raid_disk(num_replicas, required_capacity, raid_level).await {
            Ok(new_raid) => {
                println!("✅ [RAID_PROVISION] Successfully auto-created RAID disk: {}", 
                         new_raid.metadata.name.as_ref().unwrap_or(&"unknown".to_string()));
                Ok(new_raid)
            }
            Err(e) => {
                println!("❌ [RAID_PROVISION] Auto-creation failed: {}", e);
                Err(e)
            }
        }
    }

    /// Automatically create a new RAID disk from available resources
    async fn auto_create_raid_disk(
        &self,
        num_replicas: i32,
        required_capacity: i64,
        raid_level: &str,
    ) -> Result<SpdkRaidDisk, Status> {
        println!("🚀 [AUTO_RAID] Starting automatic RAID disk creation");

        // Step 1: Try to use local NVMe disks first (best performance)
        if let Ok(raid_disk) = self.create_raid_from_local_nvme(num_replicas, required_capacity, raid_level).await {
            return Ok(raid_disk);
        }

        // Step 2: Fallback to external NVMe-oF endpoints
        if let Ok(raid_disk) = self.create_raid_from_nvmeof_endpoints(num_replicas, required_capacity, raid_level).await {
            return Ok(raid_disk);
        }

        // Step 3: No resources available - generate appropriate error event
        let error_msg = format!(
            "Cannot create RAID{} disk for {} replicas ({}GB): No local NVMe disks or external NVMe-oF endpoints available",
            raid_level, num_replicas, required_capacity / (1024 * 1024 * 1024)
        );
        
        println!("❌ [AUTO_RAID] {}", error_msg);
        
        // Create detailed Kubernetes event for better user visibility
        let enhanced_error_msg = format!(
            "Storage provisioning failed: {}. This may indicate that driver unbinding is not supported on this instance type. For userspace SPDK, the system requires: 1) Kernel driver unbinding capability, 2) Userspace drivers (vfio-pci or uio_pci_generic), 3) Write access to /sys/bus/pci/drivers_probe. Consider using instances that support IOMMU and userspace driver management.",
            error_msg
        );
        
        println!("📊 [USER_GUIDANCE] {}", enhanced_error_msg);
        
        Err(Status::resource_exhausted(enhanced_error_msg))
    }

    /// Find an existing RAID disk that can accommodate a volume of given size
    async fn find_suitable_raid_disk(
        &self,
        num_replicas: i32,
        required_capacity: i64,
        raid_level: &str,
    ) -> Result<SpdkRaidDisk, Status> {
        let raid_disks: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let raid_disk_list = raid_disks.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list SpdkRaidDisks: {}", e)))?;

        for raid_disk in raid_disk_list.items {
            // Check if this RAID disk matches our requirements
            // Policy: members must reside on distinct nodes
            let mut unique_nodes = std::collections::HashSet::new();
            for m in &raid_disk.spec.member_disks {
                unique_nodes.insert(m.node_id.clone());
            }

            let has_node_separation = if num_replicas > 1 {
                unique_nodes.len() >= num_replicas as usize
            } else { true };

            if raid_disk.spec.num_member_disks >= num_replicas &&
               raid_disk.spec.raid_level == raid_level &&
               has_node_separation &&
               raid_disk.status.as_ref().map_or(false, |status| {
                   raid_disk.spec.can_accommodate_volume(required_capacity, status)
               }) {
                return Ok(raid_disk);
            }
        }

        Err(Status::not_found("No suitable RAID disk found"))
    }

    /// Create RAID disk from available local NVMe disks (preferred for performance)
    async fn create_raid_from_local_nvme(
        &self,
        num_replicas: i32,
        required_capacity: i64,
        raid_level: &str,
    ) -> Result<SpdkRaidDisk, Status> {
        println!("🏠 [LOCAL_RAID] Attempting to create RAID from local NVMe disks");

        // Step 1: Find available local NVMe disks across cluster nodes
        let available_storage = self.find_available_local_nvme_disks(required_capacity).await?;
        
        // Check if we found existing LVS to reuse (reactive approach)
        if let Some(existing_lvs) = available_storage.iter().find(|d| d.device_path.starts_with("existing-lvs:")) {
            println!("💡 [REACTIVE_STORAGE] Reusing existing LVS instead of creating new RAID");
            let lvs_name = existing_lvs.device_path.strip_prefix("existing-lvs:").unwrap();
            
                    // Create a simplified "RAID disk" CRD pointing to existing LVS
        match self.create_existing_lvs_raid_crd(existing_lvs, lvs_name).await {
            Ok(raid_disk) => {
                println!("✅ [REACTIVE_STORAGE] Successfully configured existing LVS for reuse");
                return Ok(raid_disk);
            }
            Err(e) => {
                println!("❌ [LVS_REUSE] Failed to create RAID CRD for existing LVS: {}", e);
                println!("🔍 [LVS_REUSE] Error details: {:?}", e);
                println!("⚠️ [LVS_REUSE] Falling back to new RAID creation due to CRD failure");
                // Don't return error here - let it fall through to try creating new RAID
            }
        }
        }
        
        // Fallback: Create new RAID disk if no existing LVS available
        if available_storage.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(format!(
                "Need {} disks but only {} storage options available",
                num_replicas, available_storage.len()
            )));
        }

        println!("🔧 [NEW_RAID] No existing LVS found, creating new RAID disk");
        
        // Step 2: Select optimal node for RAID creation (prefer local storage)
        let optimal_node = self.select_optimal_raid_node(&available_storage, num_replicas).await?;
        
        // Step 3: Select disks for RAID with reactive NVMe-oF creation
        let selected_disks = self.select_raid_member_disks_with_reactive_nvmeof(&available_storage, num_replicas, &optimal_node).await?;

        // Step 4: Create the SpdkRaidDisk CRD
        let raid_disk = self.create_raid_disk_crd(&selected_disks, raid_level, &optimal_node).await?;

        // Step 5: Initialize the actual RAID bdev and LVS on the target node
        println!("🔧 [RAID_INIT] Proceeding to initialize RAID bdev and LVS...");
        match self.initialize_raid_bdev_and_lvs(&raid_disk).await {
            Ok(_) => {
                println!("✅ [RAID_INIT] RAID bdev and LVS initialization completed successfully");
            }
            Err(e) => {
                println!("❌ [RAID_INIT] RAID bdev and LVS initialization failed: {}", e);
                return Err(e);
            }
        }

        println!("✅ [LOCAL_RAID] Successfully created RAID disk from local NVMe disks");
        Ok(raid_disk)
    }

    /// Create RAID disk from external NVMe-oF endpoints (fallback option)
    async fn create_raid_from_nvmeof_endpoints(
        &self,
        num_replicas: i32,
        required_capacity: i64,
        raid_level: &str,
    ) -> Result<SpdkRaidDisk, Status> {
        println!("🌐 [REMOTE_RAID] Attempting to create RAID from NVMe-oF endpoints");

        // Step 1: Find available external NVMe-oF endpoints
        let available_endpoints = self.find_available_nvmeof_endpoints(required_capacity).await?;
        
        if available_endpoints.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(format!(
                "Need {} endpoints but only {} external NVMe-oF endpoints available",
                num_replicas, available_endpoints.len()
            )));
        }

        // Step 2: Select a node for RAID creation (prefer nodes with NVMe-oF connectivity)
        let target_node = self.select_nvmeof_raid_node().await?;
        
        // Step 3: Select endpoints for RAID members
        let selected_endpoints: Vec<NvmeofDisk> = available_endpoints.into_iter().take(num_replicas as usize).collect();

        // Step 4: Create the SpdkRaidDisk CRD with NVMe-oF members
        let raid_disk = self.create_nvmeof_raid_disk_crd(&selected_endpoints, raid_level, &target_node).await?;

        // Step 5: Initialize the RAID bdev and LVS
        println!("🔧 [NVMEOF_RAID_INIT] Proceeding to initialize RAID bdev and LVS...");
        match self.initialize_raid_bdev_and_lvs(&raid_disk).await {
            Ok(_) => {
                println!("✅ [NVMEOF_RAID_INIT] RAID bdev and LVS initialization completed successfully");
            }
            Err(e) => {
                println!("❌ [NVMEOF_RAID_INIT] RAID bdev and LVS initialization failed: {}", e);
                return Err(e);
            }
        }

        println!("✅ [REMOTE_RAID] Successfully created RAID disk from NVMe-oF endpoints");
        Ok(raid_disk)
    }

    /// Initialize RAID bdev and LVS on the target node with comprehensive status updates
    async fn initialize_raid_bdev_and_lvs(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let target_node = &raid_disk.spec.created_on_node;
        
        println!("🔧 [RAID_INIT] Initializing RAID bdev and LVS on node: {}", target_node);
        println!("🔍 [RAID_INIT] Getting RPC URL for node: {}", target_node);
        
        let spdk_rpc_url = match self.driver.get_rpc_url_for_node(target_node).await {
            Ok(url) => {
                println!("✅ [RAID_INIT] Got RPC URL: {}", url);
                url
            }
            Err(e) => {
                println!("❌ [RAID_INIT] Failed to get RPC URL for node {}: {}", target_node, e);
                return Err(e);
            }
        };

        // Idempotent RAID initialization - query SPDK as source of truth
        println!("🔍 [RAID_INIT] Starting idempotent RAID initialization (SPDK is source of truth)");
        self.update_raid_disk_status_initializing(raid_disk).await?;

        // Step 1: Query SPDK current state and ensure all member bdevs are available
        println!("🔗 [RAID_INIT] Step 1: Querying SPDK state and ensuring member bdevs...");
        let current_spdk_state = self.query_spdk_state_for_raid(&spdk_rpc_url, raid_disk).await?;
        
        // Ensure all member bdevs exist (idempotent)
        for (index, member) in raid_disk.spec.member_disks.iter().enumerate() {
            let hardware_id = member.hardware_id.as_ref().unwrap();
            
            if let Some(existing_bdev_name) = self.find_existing_bdev_name(hardware_id, &current_spdk_state.bdevs) {
                println!("✅ [RAID_INIT] Member {} bdev already exists: {}", index, existing_bdev_name);
            } else {
                println!("🔧 [RAID_INIT] Creating missing member {} bdev for device: {}", index, hardware_id);
                self.ensure_member_bdev_available(&spdk_rpc_url, member, index).await?;
            }
        }

        // Step 2: Create RAID bdev
        println!("⚙️ [RAID_INIT] Step 2: Creating RAID bdev...");
        let raid_bdev_name = raid_disk.spec.raid_bdev_name();
        println!("🔍 [RAID_INIT] Target RAID bdev name: {}", raid_bdev_name);
        
        // Extract actual SPDK bdev names from current SPDK state (supports both userspace NVMe and AIO)
        let member_names: Vec<String> = raid_disk.spec.member_disks.iter()
            .filter_map(|m| {
                if let Some(hardware_id) = &m.hardware_id {
                    if let Some(existing_bdev_name) = self.find_existing_bdev_name(hardware_id, &current_spdk_state.bdevs) {
                        println!("🔍 [RAID_INIT] Found existing bdev '{}' for hardware_id '{}'", existing_bdev_name, hardware_id);
                        Some(existing_bdev_name)
                    } else {
                        println!("⚠️ [RAID_INIT] No bdev found for hardware_id '{}' - this should not happen after bdev creation", hardware_id);
                        None
                    }
                } else {
                    println!("⚠️ [RAID_INIT] Member disk missing hardware_id, skipping");
                    None
                }
            })
            .collect();

        println!("🔧 [RAID_INIT] Using member bdev names: {:?}", member_names);
        println!("🔧 [RAID_INIT] RAID level: {}", raid_disk.spec.raid_level);
        println!("🔧 [RAID_INIT] Strip size: {} KB", raid_disk.spec.stripe_size_kb);

        // Check if this is a single-replica scenario - skip RAID entirely and go directly to LVS
        if member_names.len() == 1 && (raid_disk.spec.raid_level == "1" || raid_disk.spec.raid_level == "raid1") {
            println!("🔄 [RAID_INIT] Single-replica detected - skipping RAID creation and going directly to LVS");
            println!("💡 [RAID_INIT] Single-replica volumes don't need RAID and can't be migrated");
            
            // For single-replica scenario, create LVS directly on the bdev
            // This is more efficient than attempting RAID1 just to have it fail
            return self.create_single_member_storage(&raid_disk, &member_names[0], &spdk_rpc_url).await;
        }

        // Multi-replica scenario: proceed with RAID creation
        let raid_level = raid_disk.spec.raid_level.strip_prefix("raid").unwrap_or(&raid_disk.spec.raid_level).to_string();
        println!("🔧 [RAID_INIT] Multi-replica RAID level: {} with {} members", raid_level, member_names.len());

        let raid_params = json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid_bdev_name,
                "raid_level": raid_level,
                "base_bdevs": member_names,
                "strip_size_kb": raid_disk.spec.stripe_size_kb
            }
        });
        
        println!("🔍 [RAID_INIT] SPDK RPC request: {}", raid_params);

        // Add timeout to prevent hanging on RAID creation (30 seconds)
        let raid_create_result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            call_spdk_rpc(&spdk_rpc_url, &raid_params)
        ).await;

        match raid_create_result {
            Ok(Ok(response)) => {
                println!("✅ [RAID_INIT] RAID bdev created: {}", raid_bdev_name);
                println!("🔍 [RAID_INIT] SPDK response: {}", response);
                self.update_raid_disk_status_bdev_created(raid_disk).await?;
            }
            Ok(Err(e)) => {
                let error_msg = format!("Failed to create RAID bdev: {}", e);
                println!("❌ [RAID_INIT] {}", error_msg);
                self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                return Err(Status::internal(error_msg));
            }
            Err(_) => {
                let error_msg = format!("RAID bdev creation timed out after 30 seconds: {}", raid_bdev_name);
                println!("⏰ [RAID_INIT] {}", error_msg);
                self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                return Err(Status::internal(error_msg));
            }
        }

        // Step 3: Check for existing LVS, create if not present
        println!("💾 [RAID_INIT] Step 3: Checking for existing LVS or creating new one...");
        let lvs_name = raid_disk.spec.lvs_name();
        println!("🔍 [RAID_INIT] Target LVS name: {}", lvs_name);
        println!("🔍 [RAID_INIT] Target RAID bdev for LVS: {}", raid_bdev_name);
        
        // Check if LVS already exists
        let existing_lvs_result = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_lvol_get_lvstores"
        })).await;
        
        let lvs_exists = if let Ok(lvs_data) = &existing_lvs_result {
            if let Some(lvs_array) = lvs_data["result"].as_array() {
                lvs_array.iter().any(|lvs| {
                    lvs["name"].as_str() == Some(&lvs_name) ||
                    lvs["base_bdev"].as_str() == Some(&raid_bdev_name)
                })
            } else {
                false
            }
        } else {
            false
        };
        
        let lvs_create_result = if lvs_exists {
            println!("✅ [RAID_INIT] LVS already exists: {}", lvs_name);
            // Update SPDKRaidDisk with existing LVS capacity info
            if let Ok(lvs_data) = existing_lvs_result {
                if let Some(lvs_array) = lvs_data["result"].as_array() {
                    if let Some(existing_lvs) = lvs_array.iter().find(|lvs| {
                        lvs["name"].as_str() == Some(&lvs_name) ||
                        lvs["base_bdev"].as_str() == Some(&raid_bdev_name)
                    }) {
                        self.update_raid_disk_with_lvs_info(raid_disk, existing_lvs).await?;
                    }
                }
            }
            Ok(json!({"result": "LVS already exists"}))
        } else {
            println!("🔧 [RAID_INIT] Creating new LVS with thin provisioning...");
            let lvs_params = json!({
                "method": "bdev_lvol_create_lvstore",
                "params": {
                    "bdev_name": raid_bdev_name,
                    "lvs_name": lvs_name,
                    "cluster_sz": 1048576  // 1MB clusters for thin provisioning
                }
            });
            
            println!("🔍 [RAID_INIT] LVS creation RPC request: {}", lvs_params);
            call_spdk_rpc(&spdk_rpc_url, &lvs_params).await
        };

        match lvs_create_result {
            Ok(response) => {
                println!("✅ [RAID_INIT] LVS created with thin provisioning: {}", lvs_name);
                println!("🔍 [RAID_INIT] LVS creation response: {}", response);
                
                // Update RAID disk status to indicate it's ready
                println!("🔧 [RAID_INIT] Updating RAID disk status to ready...");
                self.update_raid_disk_status_to_ready(raid_disk).await?;
                
                println!("🎉 [RAID_INIT] Multi-replica RAID initialization completed successfully!");
                Ok(())
            }
            Err(e) => {
                let error_msg = format!("Failed to create LVS: {}", e);
                println!("❌ [RAID_INIT] {}", error_msg);
                self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                Err(Status::internal(error_msg))
            }
        }
    }

    /// Create storage on single member bdev (direct LVS for single-replica scenarios)
    async fn create_single_member_storage(&self, raid_disk: &SpdkRaidDisk, bdev_name: &str, spdk_rpc_url: &str) -> Result<(), Status> {
        println!("🔧 [SINGLE_STORAGE] Creating LVS directly on single bdev: {}", bdev_name);
        
        // Update status to indicate we're using single-member approach
        self.update_raid_disk_status_bdev_created(raid_disk).await?;
        
        // Create LVS directly on the single bdev (same as RAID LVS creation)
        let lvs_name = raid_disk.spec.lvs_name();
        println!("🔍 [SINGLE_STORAGE] Creating LVS: {} on bdev: {}", lvs_name, bdev_name);
        
        let lvs_params = json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": bdev_name,
                "lvs_name": lvs_name,
                "cluster_sz": 1048576  // 1MB clusters for thin provisioning
            }
        });
        
        println!("🔍 [SINGLE_STORAGE] LVS creation request: {}", lvs_params);
        
        // Add timeout to prevent hanging on response (30 seconds for LVS creation)
        let lvs_create_result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            call_spdk_rpc(spdk_rpc_url, &lvs_params)
        ).await;
        
        match lvs_create_result {
            Ok(Ok(response)) => {
                println!("✅ [SINGLE_STORAGE] LVS created successfully: {}", lvs_name);
                println!("🔍 [SINGLE_STORAGE] SPDK response: {}", response);
                
                // Update RAID disk status to indicate it's ready (single-member)
                self.update_raid_disk_status_to_ready(raid_disk).await?;
                
                println!("✅ [SINGLE_STORAGE] Single-replica storage initialization completed successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                let error_msg = format!("Failed to create LVS on single bdev {}: {}", bdev_name, e);
                println!("❌ [SINGLE_STORAGE] {}", error_msg);
                self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                Err(Status::internal(error_msg))
            }
            Err(_) => {
                let error_msg = format!("LVS creation timed out after 30 seconds on bdev {}", bdev_name);
                println!("⏰ [SINGLE_STORAGE] {}", error_msg);
                self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                Err(Status::internal(error_msg))
            }
        }
    }

    /// Update RAID disk status to 'initializing'
    async fn update_raid_disk_status_initializing(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let raid_name = raid_disk.metadata.name.as_ref().unwrap();
        
        println!("🔄 [RAID_STATUS] Updating RAID disk {} to 'initializing' status...", raid_name);
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.state = "initializing".to_string();
        status.health_status = "initializing".to_string();
        status.last_checked = chrono::Utc::now().to_rfc3339();
        
        let patch = json!({ "status": status });
        
        // Retry mechanism for eventual consistency
        for attempt in 1..=5 {
            match raids.patch_status(raid_name, &PatchParams::default(), &Patch::Merge(patch.clone())).await {
                Ok(_) => {
                    println!("✅ [RAID_STATUS] Successfully updated RAID disk {} to 'initializing' status (attempt {})", raid_name, attempt);
                    return Ok(());
                }
                Err(kube::Error::Api(api_error)) if api_error.code == 404 => {
                    println!("⏳ [RAID_STATUS] CRD not yet available for status update (attempt {}), retrying in 1s...", attempt);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
                Err(e) => {
                    println!("❌ [RAID_STATUS] Failed to update RAID status (attempt {}): {}", attempt, e);
                    return Err(Status::internal(format!("Failed to update RAID status to initializing: {}", e)));
                }
            }
        }
        
        Err(Status::internal("Failed to update RAID status after 5 attempts - CRD may not be properly created"))
    }

    /// Update RAID disk status when bdev is created
    async fn update_raid_disk_status_bdev_created(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.state = "bdev_created".to_string();
        status.health_status = "initializing".to_string();
        status.raid_bdev_name = Some(raid_disk.spec.raid_bdev_name());
        status.last_checked = chrono::Utc::now().to_rfc3339();
        
        let patch = json!({ "status": status });
        raids.patch_status(
            &raid_disk.metadata.name.as_ref().unwrap(),
            &PatchParams::default(),
            &Patch::Merge(patch)
        ).await
        .map_err(|e| Status::internal(format!("Failed to update RAID status to bdev_created: {}", e)))?;

        println!("⚙️ [RAID_STATUS] Updated RAID disk {} to 'bdev_created' status", 
                 raid_disk.metadata.name.as_ref().unwrap());
        Ok(())
    }



    // ============================================================================
    // HELPER FUNCTIONS FOR AUTOMATIC RAID CREATION
    // ============================================================================

    /// Find available local NVMe disks across all cluster nodes
    async fn find_available_local_nvme_disks(&self, min_capacity: i64) -> Result<Vec<AvailableNvmeDisk>, Status> {
        println!("🔍 [DISK_DISCOVERY] Searching for available storage capacity (min {}GB)", 
                 min_capacity / (1024 * 1024 * 1024));
        println!("💡 [DISK_DISCOVERY] Checking for existing LVS with free space before creating new disks");

        let mut available_disks = Vec::new();
        
        // First: Check for existing LVS with sufficient free space (reactive approach)
        let nodes = self.get_cluster_nodes().await?;
        for node in &nodes {
            if let Ok(existing_capacity) = self.check_existing_lvs_capacity(node, min_capacity).await {
                if !existing_capacity.is_empty() {
                    println!("✅ [DISK_DISCOVERY] Found existing LVS with sufficient capacity on node {}", node);
                    available_disks.extend(existing_capacity);
                    // Return early - prefer reusing existing LVS over creating new disks
                    return Ok(available_disks);
                }
            }
        }
        
        // Second: Only if no existing LVS has space, look for new unclaimed disks
        println!("🔄 [DISK_DISCOVERY] No existing LVS found with sufficient space, searching for new disks");
        for node in nodes {
            match self.query_node_available_disks(&node, min_capacity).await {
                Ok(mut node_disks) => available_disks.append(&mut node_disks),
                Err(e) => println!("⚠️ [DISK_DISCOVERY] Failed to query node {}: {}", node, e),
            }
        }

        println!("✅ [DISK_DISCOVERY] Found {} available storage options", available_disks.len());
        Ok(available_disks)
    }

    /// Select optimal node for RAID creation (prioritize storage nodes over system-only nodes)
    async fn select_optimal_raid_node(&self, available_disks: &[AvailableNvmeDisk], _num_replicas: i32) -> Result<String, Status> {
        use std::collections::HashMap;

        let mut node_storage_counts: HashMap<String, usize> = HashMap::new();
        let mut node_system_counts: HashMap<String, usize> = HashMap::new();

        // Classify disks by node and type (storage vs system)
        for disk in available_disks {
            let is_system_disk = self.is_system_disk_by_path(&disk.device_path, &disk.node_id).await;
            
            if is_system_disk {
                *node_system_counts.entry(disk.node_id.clone()).or_insert(0) += 1;
                println!("🔒 [NODE_CLASSIFY] Node {} has system disk: {}", disk.node_id, disk.device_path);
            } else {
                *node_storage_counts.entry(disk.node_id.clone()).or_insert(0) += 1;
                println!("💾 [NODE_CLASSIFY] Node {} has storage disk: {}", disk.node_id, disk.device_path);
            }
        }

        // ONLY use nodes with storage disks - NEVER use system disks
        if !node_storage_counts.is_empty() {
            let optimal_node = node_storage_counts.iter()
                .max_by_key(|(_, count)| *count)
                .map(|(node, _)| node.clone())
                .unwrap(); // Safe unwrap since we checked !is_empty()

            println!("🎯 [NODE_SELECT] Selected storage node: {} ({} storage disks)", 
                     optimal_node, 
                     node_storage_counts.get(&optimal_node).unwrap_or(&0));
            return Ok(optimal_node);
        }

        // REJECT: Never use system disks for storage
        if !node_system_counts.is_empty() {
            println!("🚫 [NODE_SELECT] REJECTED: Only system disks available, refusing to use them for safety");
            let total_system_disks: usize = node_system_counts.values().sum();
            return Err(Status::resource_exhausted(format!(
                "No storage disks available. Found {} system disks across {} nodes, but system disks are not allowed for storage", 
                total_system_disks, node_system_counts.len()
            )));
        }

        Err(Status::resource_exhausted("No nodes with available disks found"))
    }

    /// Select specific disks for RAID members
    async fn select_raid_member_disks(
        &self, 
        available_disks: &[AvailableNvmeDisk], 
        num_replicas: i32, 
        optimal_node: &str
    ) -> Result<Vec<AvailableNvmeDisk>, Status> {
        let mut selected = Vec::new();
        
        // First, select disks from optimal node
        for disk in available_disks {
            if disk.node_id == optimal_node && selected.len() < num_replicas as usize {
                selected.push(disk.clone());
            }
        }
        
        // If we need more disks, select from other nodes
        for disk in available_disks {
            if disk.node_id != optimal_node && selected.len() < num_replicas as usize {
                selected.push(disk.clone());
            }
        }

        if selected.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(format!(
                "Only {} disks available, need {}", selected.len(), num_replicas
            )));
        }

        println!("💾 [DISK_SELECT] Selected {} disks for RAID members", selected.len());
        Ok(selected)
    }

    /// Create SpdkRaidDisk CRD from selected local disks
    async fn create_raid_disk_crd(
        &self,
        selected_disks: &[AvailableNvmeDisk],
        raid_level: &str,
        target_node: &str,
    ) -> Result<SpdkRaidDisk, Status> {
        use uuid::Uuid;

        println!("🏗️ [CRD_CREATE] Creating SpdkRaidDisk CRD with {} disks on node {}", selected_disks.len(), target_node);
        for (i, disk) in selected_disks.iter().enumerate() {
            println!("🏗️ [CRD_CREATE] Disk {}: {} ({}GB) on node {}", i, disk.device_path, disk.capacity / (1024*1024*1024), disk.node_id);
        }

        let raid_id = format!("auto-raid-{}", Uuid::new_v4().to_string().split('-').next().unwrap());
        
        let member_disks: Vec<RaidMemberDisk> = selected_disks.iter().enumerate().map(|(i, disk)| {
            // Extract SPDK bdev name from device path for disk_ref
            let bdev_name = if let Some(stripped) = disk.device_path.strip_prefix("/dev/") {
                stripped.to_string()
            } else {
                disk.device_path.clone()
            };
            
            RaidMemberDisk {
                member_index: i as u32,
                node_id: disk.node_id.clone(),
                disk_ref: bdev_name.clone(), // Required disk_ref field (SPDK bdev name)
                hardware_id: Some(disk.device_path.clone()),
                serial_number: Some(disk.serial_number.clone()),
                wwn: disk.wwn.clone(),
                model: Some(disk.model.clone()),
                vendor: Some(disk.vendor.clone()),
                nvmeof_endpoint: NvmeofEndpoint::default(), // Local disk, no NVMe-oF endpoint needed
                state: RaidMemberState::Online,  // Using correct enum value
                capacity_bytes: disk.capacity,
                connected: true,
                last_health_check: Some(chrono::Utc::now().to_rfc3339()),
                binding_approach: Some("aio-fallback".to_string()), // Default to AIO fallback for local disks
            }
        }).collect();

        let spec = SpdkRaidDiskSpec {
            raid_disk_id: raid_id.clone(),
            raid_level: format!("raid{}", raid_level),  // Changed from "1" to "raid1" per schema
            num_member_disks: member_disks.len() as i32,
            member_disks,
            stripe_size_kb: 1024, // 1MB default stripe size
            superblock_enabled: true,
            created_on_node: target_node.to_string(),
            min_capacity_bytes: selected_disks.iter().map(|d| d.capacity).min().unwrap_or(0),
            auto_rebuild: true,
        };

        let mut raid_disk = SpdkRaidDisk::new_with_metadata(&raid_id, spec, &self.driver.target_namespace);
        
        // Set initial status for new RAID disk
        raid_disk.status = Some(SpdkRaidDiskStatus {
            state: "online".to_string(),  // Changed from "creating" to "online" per schema
            health_status: "initializing".to_string(),
            degraded: false,
            total_capacity_bytes: selected_disks.iter().map(|d| d.capacity).min().unwrap_or(0),
            usable_capacity_bytes: 0, // Will be set after LVS creation
            used_capacity_bytes: 0,
            active_member_count: selected_disks.len() as u32,
            failed_member_count: 0,
            last_checked: chrono::Utc::now().to_rfc3339(),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        });

        // Create the CRD in Kubernetes
        println!("🏗️ [CRD_CREATE] Creating Kubernetes CRD: {}", raid_id);
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        println!("🔍 [CRD_DEBUG] About to submit CRD to Kubernetes API...");
        let created_raid = match raids.create(&PostParams::default(), &raid_disk).await {
            Ok(crd) => {
                println!("✅ [CRD_CREATE] Successfully created SpdkRaidDisk CRD: {}", raid_id);
                println!("🔍 [CRD_DEBUG] Returned CRD name: {:?}", crd.metadata.name);
                println!("🔍 [CRD_DEBUG] Returned CRD status: {:?}", crd.status);
                crd
            },
            Err(kube::Error::Api(api_error)) => {
                println!("❌ [CRD_CREATE] Kubernetes API error:");
                println!("   Status: {}", api_error.status);
                println!("   Code: {}", api_error.code);
                println!("   Message: {}", api_error.message);
                println!("   Reason: {}", api_error.reason);
                return Err(Status::internal(format!("Failed to create SpdkRaidDisk CRD: API error: {}", api_error.message)));
            },
            Err(kube::Error::SerdeError(serde_error)) => {
                println!("❌ [CRD_CREATE] Serialization/Deserialization error:");
                println!("   Error: {}", serde_error);
                println!("   This usually means the Kubernetes response doesn't match our Rust model");
                return Err(Status::internal(format!("Failed to create SpdkRaidDisk CRD: Deserialization error: {}", serde_error)));
            },
            Err(other_error) => {
                println!("❌ [CRD_CREATE] Other error:");
                println!("   Error type: {:?}", std::any::type_name_of_val(&other_error));
                println!("   Error: {}", other_error);
                return Err(Status::internal(format!("Failed to create SpdkRaidDisk CRD: {}", other_error)));
            }
        };
        
        println!("🔧 [RAID_INIT] Proceeding to initialize RAID bdev and LVS...");
        
        // Update NVMe disk usage status if needed
        self.mark_local_disks_as_used(selected_disks).await?;
        
        Ok(created_raid)
    }

    /// Find available external NVMe-oF endpoints
    async fn find_available_nvmeof_endpoints(&self, min_capacity: i64) -> Result<Vec<NvmeofDisk>, Status> {
        let nvmeof_api: Api<NvmeofDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let nvmeof_list = nvmeof_api.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list NvmeofDisk: {}", e)))?;

        let available: Vec<NvmeofDisk> = nvmeof_list.items.into_iter()
            .filter(|disk| {
                disk.spec.is_remote && 
                disk.spec.size_bytes >= min_capacity &&
                disk.status.as_ref().map_or(false, |s| s.healthy) &&
                !self.is_nvmeof_disk_in_use(disk)
            })
            .collect();

        println!("🌐 [NVMEOF_DISCOVERY] Found {} available external NVMe-oF endpoints", available.len());
        Ok(available)
    }

    /// Select node for NVMe-oF RAID creation
    async fn select_nvmeof_raid_node(&self) -> Result<String, Status> {
        // For NVMe-oF RAID, select node based on network connectivity and load
        // For now, select first available node
        let nodes = self.get_cluster_nodes().await?;
        nodes.first()
            .cloned()
            .ok_or_else(|| Status::internal("No cluster nodes available"))
    }

    /// Update RAID disk status to ready after successful initialization
    async fn update_raid_disk_status_to_ready(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        // Query SPDK for actual capacity and status
        let spdk_rpc_url = self.driver.get_rpc_url_for_node(&raid_disk.spec.created_on_node).await?;
        let raid_bdev_name = raid_disk.spec.raid_bdev_name();
        let lvs_name = raid_disk.spec.lvs_name();
        
        // Get RAID bdev info for accurate capacity
        let mut total_capacity = raid_disk.spec.min_capacity_bytes;
        let mut usable_capacity = 0i64;
        
        if let Ok(bdev_info) = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_get_bdevs",
            "params": { "name": raid_bdev_name }
        })).await {
            if let Some(bdev) = bdev_info.as_array().and_then(|arr| arr.first()) {
                if let (Some(num_blocks), Some(block_size)) = (
                    bdev["num_blocks"].as_u64(),
                    bdev["block_size"].as_u64()
                ) {
                    total_capacity = (num_blocks * block_size) as i64;
                }
            }
        }
        
        // Get LVS info for usable capacity
        if let Ok(lvs_info) = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_lvol_get_lvstores",
            "params": { "lvs_name": lvs_name }
        })).await {
            if let Some(lvs) = lvs_info.as_array().and_then(|arr| arr.first()) {
                if let (Some(total_clusters), Some(free_clusters), Some(cluster_size)) = (
                    lvs["total_data_clusters"].as_u64(),
                    lvs["free_clusters"].as_u64(),
                    lvs["cluster_size"].as_u64()
                ) {
                    usable_capacity = (total_clusters * cluster_size) as i64;
                    let used_capacity = ((total_clusters - free_clusters) * cluster_size) as i64;
                    
                    println!("📊 [RAID_STATUS] Capacity - Total: {}GB, Usable: {}GB, Used: {}GB", 
                             total_capacity / (1024*1024*1024), 
                             usable_capacity / (1024*1024*1024),
                             used_capacity / (1024*1024*1024));
                }
            }
        }
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.state = "online".to_string();
        status.health_status = "healthy".to_string();
        status.degraded = false;
        status.raid_bdev_name = Some(raid_bdev_name.clone());
        status.lvs_name = Some(lvs_name.clone());
        status.lvs_uuid = None; // Could be populated from LVS info
        status.total_capacity_bytes = total_capacity;
        status.usable_capacity_bytes = usable_capacity;
        status.used_capacity_bytes = 0; // No volumes created yet
        status.active_member_count = raid_disk.spec.num_member_disks as u32;
        status.failed_member_count = 0;
        status.last_checked = chrono::Utc::now().to_rfc3339();
        
        let patch = json!({ "status": status });
        raids.patch_status(
            &raid_disk.metadata.name.as_ref().unwrap(),
            &PatchParams::default(),
            &Patch::Merge(patch)
        ).await
        .map_err(|e| Status::internal(format!("Failed to update RAID status to ready: {}", e)))?;

        println!("✅ [RAID_STATUS] Updated RAID disk {} to 'online' status with capacity info", 
                 raid_disk.metadata.name.as_ref().unwrap());
        Ok(())
    }

    /// Update RAID disk status with existing LVS information
    async fn update_raid_disk_with_lvs_info(&self, raid_disk: &SpdkRaidDisk, lvs_info: &serde_json::Value) -> Result<(), Status> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.state = "ready".to_string();
        status.health_status = "healthy".to_string();
        status.last_checked = chrono::Utc::now().to_rfc3339();
        
        // Extract capacity information from existing LVS
        if let (Some(total_clusters), Some(free_clusters), Some(cluster_size)) = (
            lvs_info["total_data_clusters"].as_u64(),
            lvs_info["free_clusters"].as_u64(),
            lvs_info["cluster_size"].as_u64()
        ) {
            let total_capacity = total_clusters * cluster_size;
            let free_capacity = free_clusters * cluster_size;
            let used_capacity = total_capacity - free_capacity;
            
            status.total_capacity_bytes = total_capacity as i64;
            status.usable_capacity_bytes = free_capacity as i64;
            status.used_capacity_bytes = used_capacity as i64;
            
            println!("📊 [RAID_LVS_UPDATE] Existing LVS capacity - Total: {}GB, Used: {}GB, Free: {}GB", 
                     total_capacity / (1024*1024*1024),
                     used_capacity / (1024*1024*1024), 
                     free_capacity / (1024*1024*1024));
        }
        
        // Set member count based on existing LVS
        status.active_member_count = raid_disk.spec.member_disks.len() as u32;
        
        let patch = json!({ "status": status });
        let raid_name = raid_disk.metadata.name.as_ref().unwrap();
        
        match raids.patch_status(raid_name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            Ok(_) => {
                println!("✅ [RAID_LVS_UPDATE] Updated RAID disk {} with existing LVS info", raid_name);
                Ok(())
            }
            Err(e) => {
                println!("❌ [RAID_LVS_UPDATE] Failed to update RAID disk status: {}", e);
                Err(Status::internal(format!("Failed to update RAID disk status: {}", e)))
            }
        }
    }

    /// Update RAID disk status during failures
    async fn update_raid_disk_status_failed(&self, raid_disk: &SpdkRaidDisk, error_msg: &str) -> Result<(), Status> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.state = "failed".to_string();
        status.health_status = "failed".to_string();
        status.degraded = true;
        status.last_checked = chrono::Utc::now().to_rfc3339();
        
        let patch = json!({ 
            "status": status,
            "metadata": {
                "annotations": {
                    "flint.csi.storage.io/last-error": error_msg,
                    "flint.csi.storage.io/failed-at": chrono::Utc::now().to_rfc3339()
                }
            }
        });
        
        raids.patch_status(
            &raid_disk.metadata.name.as_ref().unwrap(),
            &PatchParams::default(),
            &Patch::Merge(patch)
        ).await
        .map_err(|e| Status::internal(format!("Failed to update RAID status to failed: {}", e)))?;

        println!("❌ [RAID_STATUS] Updated RAID disk {} to 'failed' status: {}", 
                 raid_disk.metadata.name.as_ref().unwrap(), error_msg);
        Ok(())
    }

    /// Update RAID disk used capacity after volume creation
    async fn update_raid_disk_used_capacity(&self, raid_disk: &SpdkRaidDisk, volume_capacity: i64) -> Result<(), Status> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        // Query current LVS status to get accurate usage
        let spdk_rpc_url = self.driver.get_rpc_url_for_node(&raid_disk.spec.created_on_node).await?;
        let lvs_name = raid_disk.spec.lvs_name();
        
        let mut used_capacity = volume_capacity; // Thin provisioned - start with requested capacity
        
        if let Ok(lvs_info) = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_lvol_get_lvstores",
            "params": { "lvs_name": lvs_name }
        })).await {
            if let Some(lvs) = lvs_info.as_array().and_then(|arr| arr.first()) {
                if let (Some(total_clusters), Some(free_clusters), Some(cluster_size)) = (
                    lvs["total_data_clusters"].as_u64(),
                    lvs["free_clusters"].as_u64(),
                    lvs["cluster_size"].as_u64()
                ) {
                    used_capacity = ((total_clusters - free_clusters) * cluster_size) as i64;
                }
            }
        }
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.used_capacity_bytes = used_capacity;
        status.last_checked = chrono::Utc::now().to_rfc3339();
        
        let patch = json!({ "status": status });
        raids.patch_status(
            &raid_disk.metadata.name.as_ref().unwrap(),
            &PatchParams::default(),
            &Patch::Merge(patch)
        ).await
        .map_err(|e| Status::internal(format!("Failed to update RAID disk used capacity: {}", e)))?;

        println!("📊 [RAID_STATUS] Updated RAID disk {} used capacity: {}GB", 
                 raid_disk.metadata.name.as_ref().unwrap(),
                 used_capacity / (1024*1024*1024));
        Ok(())
    }

    // Volume CRD creation is handled by the existing provision_volume logic below

    /// Mark local disks as used by updating any related CRDs
    async fn mark_local_disks_as_used(&self, _selected_disks: &[AvailableNvmeDisk]) -> Result<(), Status> {
        // In a full implementation, this would update NvmeofDisk or SpdkDisk CRDs
        // to mark them as in_use = true
        println!("📝 [DISK_STATUS] Marking disks as used in RAID (placeholder implementation)");
        Ok(())
    }

    // ============================================================================
    // IMPLEMENTED HELPER FUNCTIONS WITH ACTUAL NODE AGENT COMMUNICATION
    // ============================================================================

    /// Get list of cluster nodes from Kubernetes
    async fn get_cluster_nodes(&self) -> Result<Vec<String>, Status> {
        use k8s_openapi::api::core::v1::Node;
        let nodes_api: Api<Node> = Api::all(self.driver.kube_client.clone());
        
        match nodes_api.list(&ListParams::default()).await {
            Ok(nodes) => {
                let node_names: Vec<String> = nodes.items.iter()
                    .filter_map(|node| node.metadata.name.clone())
                    .filter(|name| !name.contains("master") && !name.contains("control-plane")) // Skip control plane nodes
                    .collect();
                
                println!("🔍 [CLUSTER] Found {} worker nodes: {:?}", node_names.len(), node_names);
                Ok(node_names)
            }
            Err(e) => {
                println!("❌ [CLUSTER] Failed to list cluster nodes: {}", e);
                Err(Status::internal(format!("Failed to get cluster nodes: {}", e)))
            }
        }
    }

    /// Query specific node agent for available local disks
    async fn query_node_available_disks(&self, node: &str, min_capacity: i64) -> Result<Vec<AvailableNvmeDisk>, Status> {
        println!("🌐 [NODE_QUERY] Querying node {} for available disks (min {}GB)", 
                 node, min_capacity / (1024 * 1024 * 1024));

        // Query SPDK for bdev information via existing RPC URL mechanism
        let spdk_rpc_url = self.driver.get_rpc_url_for_node(node).await
            .map_err(|e| Status::internal(format!("Failed to get RPC URL for node {}: {}", node, e)))?;

        let bdev_data = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_get_bdevs",
            "params": {}
        })).await
            .map_err(|e| Status::internal(format!("Failed to query SPDK bdevs on node {}: {}", node, e)))?;

        // Parse and filter available disks
        let mut available_disks = Vec::new();
        
        println!("🔍 [BDEV_DEBUG] Raw bdev_data response: {:?}", bdev_data);
        
        if let Some(bdevs) = bdev_data["result"].as_array() {
            println!("🔍 [BDEV_DEBUG] Found {} bdevs to evaluate", bdevs.len());
            for bdev in bdevs {
                if let (Some(name), Some(num_blocks), Some(block_size)) = (
                    bdev["name"].as_str(),
                    bdev["num_blocks"].as_u64(),
                    bdev["block_size"].as_u64()
                ) {
                    let capacity = num_blocks * block_size;
                    
                    // Filter for available storage devices based on properties, not naming assumptions
                    let is_claimed = bdev["claimed"].as_bool().unwrap_or(true);
                    let supports_read = bdev["supported_io_types"]["read"].as_bool().unwrap_or(false);
                    let supports_write = bdev["supported_io_types"]["write"].as_bool().unwrap_or(false);
                    let is_in_use = self.is_bdev_in_use(node, name).await;
                    
                    // Check if this is a system disk (should be excluded from storage)
                    let device_path = format!("/dev/{}", name);
                    let is_system_disk = self.is_system_disk_by_path(&device_path, node).await;
                    
                    // Available storage criteria:
                    // - For unclaimed bdevs: standard availability check (new disk provisioning)
                    // - For claimed bdevs: will be checked for LVS capacity in existing LVS flow
                    // NOTE: Claimed bdevs with LVS are checked via check_existing_lvs_capacity()
                    let is_available_storage = !is_claimed && 
                                             supports_read && 
                                             supports_write && 
                                             capacity >= min_capacity as u64 && 
                                             !is_in_use &&
                                             !is_system_disk; // EXCLUDE system disks from storage
                    
                    println!("🔍 [BDEV_EVAL] Device: {} | capacity: {}GB | claimed: {} | read: {} | write: {} | in_use: {} | system_disk: {} | available: {}", 
                             name, capacity / (1024*1024*1024), is_claimed, supports_read, supports_write, is_in_use, is_system_disk, is_available_storage);
                    
                    if is_available_storage {
                        let disk = AvailableNvmeDisk {
                            node_id: node.to_string(),
                            device_path: format!("/dev/{}", name),
                            serial_number: bdev["product_name"].as_str().unwrap_or("unknown").to_string(),
                            wwn: None,
                            model: bdev["product_name"].as_str().unwrap_or("unknown").to_string(),
                            vendor: "Intel".to_string(), // Default for NVMe
                            capacity: capacity as i64,
                            pci_address: format!("{}:00.0", available_disks.len()), // Placeholder PCI address
                        };
                        available_disks.push(disk);
                    }
                }
            }
        }

        println!("✅ [NODE_QUERY] Found {} available disks on node {}", available_disks.len(), node);
        Ok(available_disks)
    }

    /// Check if a bdev is currently in use by any RAID or volume
    async fn is_bdev_in_use(&self, _node: &str, bdev_name: &str) -> bool {
        // Query existing RAID disks to see if this bdev is used as a member
        let raids_api: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        if let Ok(raids) = raids_api.list(&ListParams::default()).await {
            for raid in raids.items {
                for member in &raid.spec.member_disks {
                    if let Some(hardware_id) = &member.hardware_id {
                        if hardware_id.contains(bdev_name) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Check if a disk is a system disk by querying the node agent's system disk detection
    async fn is_system_disk_by_path(&self, device_path: &str, node_id: &str) -> bool {
        // Extract device name from path (e.g., "/dev/nvme0n1" -> "nvme0n1")
        let device_name = if device_path.starts_with("/dev/") {
            device_path.strip_prefix("/dev/").unwrap_or(device_path)
        } else {
            device_path
        };

        // Use device name as-is since hardware_id should contain the actual device path
        let canonical_device = device_name.to_string();

        println!("🔍 [SYSTEM_DISK_CHECK] Checking if {} (canonical: {}) on node {} is system disk", 
                 device_path, canonical_device, node_id);

        // Query the node agent via RPC to check if this is a system disk
        match self.driver.get_rpc_url_for_node(node_id).await {
            Ok(rpc_url) => {
                // Call a custom RPC method to check system disk status
                // We'll add this RPC endpoint to the node agent
                match call_spdk_rpc(&rpc_url.replace("/rpc", "/api/system-disk-check"), &json!({
                    "device_name": canonical_device
                })).await {
                    Ok(response) => {
                        let is_system = response["is_system_disk"].as_bool().unwrap_or(false);
                        println!("🔍 [SYSTEM_DISK_CHECK] Node {} reports {} is_system_disk: {}", 
                                 node_id, canonical_device, is_system);
                        is_system
                    }
                    Err(e) => {
                        println!("⚠️ [SYSTEM_DISK_CHECK] Failed to query node {} for system disk status: {}", node_id, e);
                        // CONSERVATIVE FALLBACK: Assume system disk if we can't verify
                        // This prevents accidentally using system disks
                        canonical_device.contains("nvme0") // nvme0n1 is typically system disk
                    }
                }
            }
            Err(e) => {
                println!("⚠️ [SYSTEM_DISK_CHECK] Failed to get RPC URL for node {}: {}", node_id, e);
                // CONSERVATIVE FALLBACK: Assume system disk if we can't verify  
                canonical_device.contains("nvme0") // nvme0n1 is typically system disk
            }
        }
    }

    /// Check existing LVS for available capacity (reactive storage reuse)
    async fn check_existing_lvs_capacity(&self, node: &str, min_capacity: i64) -> Result<Vec<AvailableNvmeDisk>, Status> {
        println!("🔍 [LVS_CHECK] Checking existing LVS capacity on node: {}", node);
        
        let spdk_rpc_url = self.driver.get_rpc_url_for_node(node).await
            .map_err(|e| Status::internal(format!("Failed to get RPC URL for node {}: {}", node, e)))?;

        println!("🔍 [LVS_CHECK_DEBUG] Using RPC URL: {}", spdk_rpc_url);
        
        // Prepare the RPC request
        let rpc_request = json!({
            "method": "bdev_lvol_get_lvstores",
            "params": {}
        });
        
        println!("🔍 [LVS_CHECK_DEBUG] Sending RPC request: {}", rpc_request);
        
        // Get existing LVS information with detailed error logging
        let lvs_data = match call_spdk_rpc(&spdk_rpc_url, &rpc_request).await {
            Ok(data) => {
                println!("✅ [LVS_CHECK_DEBUG] Received response: {}", data);
                data
            }
            Err(e) => {
                println!("❌ [LVS_CHECK_DEBUG] RPC call failed with error: {}", e);
                println!("🔍 [LVS_CHECK_DEBUG] Error details: {:?}", e);
                return Err(Status::internal(format!("Failed to query LVS on node {}: {}", node, e)));
            }
        };

        let mut available_storage = Vec::new();
        
        if let Some(lvs_list) = lvs_data["result"].as_array() {
            for lvs in lvs_list {
                if let (Some(name), Some(free_clusters), Some(cluster_size), Some(base_bdev)) = (
                    lvs["name"].as_str(),
                    lvs["free_clusters"].as_u64(),
                    lvs["cluster_size"].as_u64(),
                    lvs["base_bdev"].as_str()
                ) {
                    let free_bytes = free_clusters * cluster_size;
                    
                    if free_bytes >= min_capacity as u64 {
                        println!("✅ [LVS_CHECK] Found LVS '{}' with {}GB free capacity", 
                               name, free_bytes / (1024 * 1024 * 1024));
                        
                        // Create a virtual "disk" representing this LVS capacity
                        available_storage.push(AvailableNvmeDisk {
                            node_id: node.to_string(),
                            device_path: format!("existing-lvs:{}", name),  // Special marker
                            serial_number: format!("lvs-{}", name),
                            wwn: None,
                            model: "Existing LVS".to_string(),
                            vendor: "SPDK".to_string(),
                            capacity: free_bytes as i64,
                            pci_address: base_bdev.to_string(),  // Store base bdev for reference
                        });
                    } else {
                        println!("⚠️ [LVS_CHECK] LVS '{}' only has {}GB free (need {}GB)", 
                               name, free_bytes / (1024 * 1024 * 1024), min_capacity / (1024 * 1024 * 1024));
                    }
                }
            }
        }
        
        println!("🔍 [LVS_CHECK] Found {} LVS with sufficient capacity on {}", available_storage.len(), node);
        Ok(available_storage)
    }

    /// Select RAID member disks with reactive NVMe-oF creation (only when remote disks needed)
    async fn select_raid_member_disks_with_reactive_nvmeof(
        &self, 
        available_disks: &[AvailableNvmeDisk], 
        num_replicas: i32, 
        optimal_node: &str
    ) -> Result<Vec<AvailableNvmeDisk>, Status> {
        println!("💡 [REACTIVE_NVMEOF] Selecting {} disks with local-first preference", num_replicas);
        
        let mut selected_disks = Vec::new();
        let mut local_disks = Vec::new();
        let mut remote_disks = Vec::new();
        
        // Separate local vs remote disks
        for disk in available_disks {
            if disk.node_id == optimal_node {
                local_disks.push(disk.clone());
            } else {
                remote_disks.push(disk.clone());
            }
        }
        
        println!("📊 [REACTIVE_NVMEOF] Found {} local disks, {} remote disks on optimal node {}", 
                 local_disks.len(), remote_disks.len(), optimal_node);
        
        // Step 1: Prefer local disks (no NVMe-oF needed)
        let local_count = std::cmp::min(local_disks.len(), num_replicas as usize);
        for i in 0..local_count {
            selected_disks.push(local_disks[i].clone());
            println!("✅ [LOCAL_DISK] Selected local disk: {} (no NVMe-oF needed)", local_disks[i].device_path);
        }
        
        // Step 2: Only if we need more disks, reactively create NVMe-oF exports for remote disks
        let remaining_needed = (num_replicas as usize) - selected_disks.len();
        if remaining_needed > 0 {
            println!("🌐 [REACTIVE_NVMEOF] Need {} more disks, creating NVMe-oF exports for remote disks", remaining_needed);
            
            for i in 0..std::cmp::min(remaining_needed, remote_disks.len()) {
                let remote_disk = &remote_disks[i];
                
                // Reactively create NVMe-oF export for this specific remote disk
                match self.create_nvmeof_export_for_disk(remote_disk).await {
                    Ok(nvmeof_disk) => {
                        selected_disks.push(nvmeof_disk);
                        println!("✅ [REACTIVE_NVMEOF] Created NVMe-oF export for remote disk: {}", remote_disk.device_path);
                    }
                    Err(e) => {
                        println!("⚠️ [REACTIVE_NVMEOF] Failed to create NVMe-oF export for {}: {}", remote_disk.device_path, e);
                        // Continue with next disk
                    }
                }
            }
        }
        
        if selected_disks.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(format!(
                "Could only select {} disks (needed {}). Local: {}, Remote exports created: {}",
                selected_disks.len(), num_replicas, local_count, selected_disks.len() - local_count
            )));
        }
        
        println!("✅ [REACTIVE_NVMEOF] Successfully selected {} disks ({} local, {} remote via NVMe-oF)", 
                 selected_disks.len(), local_count, selected_disks.len() - local_count);
        Ok(selected_disks)
    }
    
    /// Create NVMe-oF export for a specific remote disk (reactive approach)
    async fn create_nvmeof_export_for_disk(&self, disk: &AvailableNvmeDisk) -> Result<AvailableNvmeDisk, Status> {
        println!("🌐 [NVMEOF_CREATE] Creating on-demand NVMe-oF export for disk: {} on node {}", 
                 disk.device_path, disk.node_id);
        
        // Get the target node's SPDK RPC URL
        let target_rpc_url = self.driver.get_rpc_url_for_node(&disk.node_id).await
            .map_err(|e| Status::internal(format!("Failed to get RPC URL for node {}: {}", disk.node_id, e)))?;
        
        // Find the actual bdev name in SPDK (supports both userspace NVMe and AIO naming)
        let bdevs_result = call_spdk_rpc(&target_rpc_url, &json!({"method": "bdev_get_bdevs"})).await
            .map_err(|e| Status::internal(format!("Failed to query bdevs for NVMe-oF export: {}", e)))?;
        
        let existing_bdev_names: Vec<String> = if let Some(bdevs) = bdevs_result["result"].as_array() {
            bdevs.iter().filter_map(|bdev| bdev["name"].as_str().map(|s| s.to_string())).collect()
        } else {
            Vec::new()
        };
        
        let bdev_name = match self.find_existing_bdev_name(&disk.device_path, &existing_bdev_names) {
            Some(name) => name,
            None => {
                let error_msg = format!("No bdev found for device {} - cannot create NVMe-oF export", disk.device_path);
                println!("❌ [NVMEOF_CREATE] {}", error_msg);
                return Err(Status::internal(error_msg));
            }
        };
        
        // Create NVMe-oF subsystem for this specific disk
        let nqn = format!("nqn.2024-01.io.flint:disk-{}-{}", disk.node_id, bdev_name);
        
        // Create the NVMe-oF subsystem
        let _create_subsystem = call_spdk_rpc(&target_rpc_url, &json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true
            }
        })).await
            .map_err(|e| Status::internal(format!("Failed to create NVMe-oF subsystem: {}", e)))?;
        
        // Add the bdev as a namespace
        let _add_namespace = call_spdk_rpc(&target_rpc_url, &json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "bdev_name": bdev_name,
                    "nsid": 1
                }
            }
        })).await
            .map_err(|e| Status::internal(format!("Failed to add namespace to NVMe-oF subsystem: {}", e)))?;
        
        // Add listener (using node's IP)
        let node_ip = self.get_node_ip(&disk.node_id).await
            .map_err(|e| Status::internal(format!("Failed to get IP for node {}: {}", disk.node_id, e)))?;
        
        let _add_listener = call_spdk_rpc(&target_rpc_url, &json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": "TCP",
                    "traddr": node_ip,
                    "trsvcid": "4420"
                }
            }
        })).await
            .map_err(|e| Status::internal(format!("Failed to add listener to NVMe-oF subsystem: {}", e)))?;
        
        // Return modified disk info with NVMe-oF connection details
        let mut nvmeof_disk = disk.clone();
        nvmeof_disk.device_path = format!("nvmeof://{}:4420/{}", node_ip, nqn);
        
        println!("✅ [NVMEOF_CREATE] Successfully created NVMe-oF export: {}", nvmeof_disk.device_path);
        Ok(nvmeof_disk)
    }
    
    /// Get node IP address for NVMe-oF connection
    async fn get_node_ip(&self, node_id: &str) -> Result<String, Status> {
        use k8s_openapi::api::core::v1::Node;
        
        let nodes_api: Api<Node> = Api::all(self.driver.kube_client.clone());
        let node = nodes_api.get(node_id).await
            .map_err(|e| Status::internal(format!("Failed to get node info: {}", e)))?;
        
        // Try to get internal IP
        if let Some(status) = &node.status {
            if let Some(addresses) = &status.addresses {
                for addr in addresses {
                    if addr.type_ == "InternalIP" {
                        return Ok(addr.address.clone());
                    }
                }
            }
        }
        
        Err(Status::internal(format!("No internal IP found for node {}", node_id)))
    }

    /// Create SpdkRaidDisk CRD for existing LVS reuse
    async fn create_existing_lvs_raid_crd(&self, existing_lvs: &AvailableNvmeDisk, lvs_name: &str) -> Result<SpdkRaidDisk, Status> {
        use spdk_csi_driver::models::{SpdkRaidDisk, SpdkRaidDiskSpec, RaidMemberDisk};
        
        let raid_name = format!("reuse-{}", lvs_name.replace("_", "-")); // Replace underscores for K8s name compliance
        println!("🔄 [LVS_REUSE] Creating RAID CRD for existing LVS: {} -> {}", lvs_name, raid_name);
        
        let raid_spec = SpdkRaidDiskSpec {
            raid_disk_id: raid_name.clone(),
            raid_level: "raid1".to_string(), // Use valid CRD value for single disk
            num_member_disks: 1,
            member_disks: vec![RaidMemberDisk {
                member_index: 0,
                node_id: existing_lvs.node_id.clone(),
                disk_ref: existing_lvs.pci_address.clone(), // Base bdev name
                hardware_id: Some(existing_lvs.pci_address.clone()),
                serial_number: Some(existing_lvs.serial_number.clone()),
                wwn: existing_lvs.wwn.clone(),
                model: Some(existing_lvs.model.clone()),
                vendor: Some(existing_lvs.vendor.clone()),
                nvmeof_endpoint: spdk_csi_driver::models::NvmeofEndpoint::default(),
                state: spdk_csi_driver::models::RaidMemberState::Online,
                capacity_bytes: existing_lvs.capacity,
                connected: true,
                last_health_check: Some(chrono::Utc::now().to_rfc3339()),
                binding_approach: Some("aio-fallback".to_string()), // AIO bdev binding for existing LVS
            }],
            stripe_size_kb: 1024, // Default stripe size for LVS reuse
            superblock_enabled: true,
            created_on_node: existing_lvs.node_id.clone(),
            min_capacity_bytes: existing_lvs.capacity,
            auto_rebuild: true,
        };
        
        let mut raid_disk = SpdkRaidDisk::new(&raid_name, raid_spec);
        
        // Set status to indicate this is reusing existing LVS
        raid_disk.status = Some(spdk_csi_driver::models::SpdkRaidDiskStatus {
            state: "online".to_string(), // Skip bdev_created since LVS already exists
            raid_bdev_name: Some(existing_lvs.pci_address.clone()), // Base bdev
            lvs_name: Some(lvs_name.to_string()),
            lvs_uuid: None,
            total_capacity_bytes: existing_lvs.capacity,
            usable_capacity_bytes: existing_lvs.capacity,
            used_capacity_bytes: 0,
            health_status: "healthy".to_string(),
            degraded: false,
            rebuild_progress: None,
            active_member_count: 1,
            failed_member_count: 0,
            last_checked: chrono::Utc::now().to_rfc3339(),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            raid_status: None,
        });
        
        // Create the CRD in Kubernetes
        let raids_api: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        println!("🔍 [LVS_REUSE_DEBUG] About to create RAID CRD in namespace: {}", self.driver.target_namespace);
        println!("🔍 [LVS_REUSE_DEBUG] RAID CRD spec: {:#?}", raid_disk.spec);
        
        let created_raid = raids_api.create(&kube::api::PostParams::default(), &raid_disk).await
            .map_err(|e| {
                println!("❌ [LVS_REUSE_DEBUG] Kubernetes API error: {}", e);
                println!("🔍 [LVS_REUSE_DEBUG] Full error details: {:?}", e);
                Status::internal(format!("Failed to create RAID CRD for existing LVS: {}", e))
            })?;
        
        println!("✅ [LVS_REUSE] Successfully created RAID CRD for existing LVS: {}", raid_name);
        Ok(created_raid)
    }

    /// Check if NVMe-oF disk is already used in existing RAID
    fn is_nvmeof_disk_in_use(&self, disk: &NvmeofDisk) -> bool {
        // This is a sync method, so we'll use a simple heuristic
        // In practice, this should query the cluster state
        disk.status.as_ref().map_or(false, |status| !status.healthy)
    }

    /// Create RAID CRD with NVMe-oF endpoints  
    async fn create_nvmeof_raid_disk_crd(&self, endpoints: &[NvmeofDisk], raid_level: &str, target_node: &str) -> Result<SpdkRaidDisk, Status> {
        use uuid::Uuid;

        let raid_id = format!("auto-nvmeof-raid-{}", Uuid::new_v4().to_string().split('-').next().unwrap());
        
        let member_disks: Vec<RaidMemberDisk> = endpoints.iter().enumerate().map(|(i, endpoint)| {
            RaidMemberDisk {
                member_index: i as u32,
                node_id: target_node.to_string(),
                disk_ref: endpoint.spec.nvmeof_endpoint.nqn.clone(), // Use NQN as disk reference for NVMe-oF
                hardware_id: Some(format!("nvmeof-{}", endpoint.spec.nvmeof_endpoint.nqn)),
                serial_number: endpoint.spec.serial_number.clone(),
                wwn: None,
                model: Some("NVMe-oF".to_string()),
                vendor: Some("Remote".to_string()),
                nvmeof_endpoint: NvmeofEndpoint {
                    nqn: endpoint.spec.nvmeof_endpoint.nqn.clone(),
                    target_addr: endpoint.spec.nvmeof_endpoint.target_addr.clone(),
                    target_port: endpoint.spec.nvmeof_endpoint.target_port,
                    transport: endpoint.spec.nvmeof_endpoint.transport.clone(),
                    created_at: Some(chrono::Utc::now().to_rfc3339()),
                    active: true,
                },
                state: RaidMemberState::Online,
                capacity_bytes: endpoint.spec.size_bytes,
                connected: true,
                last_health_check: Some(chrono::Utc::now().to_rfc3339()),
                binding_approach: Some("nvmeof".to_string()), // Remote disk accessed via NVMe-oF
            }
        }).collect();

        let spec = SpdkRaidDiskSpec {
            raid_disk_id: raid_id.clone(),
            raid_level: raid_level.to_string(),
            num_member_disks: member_disks.len() as i32,
            member_disks,
            stripe_size_kb: 2048, // Larger stripe for network storage
            superblock_enabled: true,
            created_on_node: target_node.to_string(),
            min_capacity_bytes: endpoints.iter().map(|e| e.spec.size_bytes).min().unwrap_or(0),
            auto_rebuild: true,
        };

        let mut raid_disk = SpdkRaidDisk::new_with_metadata(&raid_id, spec, &self.driver.target_namespace);
        raid_disk.status = Some(SpdkRaidDiskStatus::default());

        // Create the CRD in Kubernetes
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let created_raid = raids.create(&PostParams::default(), &raid_disk).await
            .map_err(|e| Status::internal(format!("Failed to create NVMe-oF SpdkRaidDisk CRD: {}", e)))?;

        println!("✅ [NVMEOF_CRD] Created NVMe-oF RAID CRD: {}", raid_id);
        Ok(created_raid)
    }

    /// Ensure SPDK bdev is available for RAID member
    async fn ensure_member_bdev_available(&self, rpc_url: &str, member: &RaidMemberDisk, _index: usize) -> Result<(), Status> {
        if member.nvmeof_endpoint.nqn.is_empty() {
            // Local disk - ensure SPDK has attached it
            if let Some(hardware_id) = &member.hardware_id {
                println!("🔧 [BDEV_ENSURE] Ensuring local bdev available: {}", hardware_id);
                
                // Check if bdev already exists in SPDK with either naming convention
                let check_result = call_spdk_rpc(rpc_url, &json!({
                    "method": "bdev_get_bdevs",
                    "params": {}
                })).await;

                match check_result {
                    Ok(bdev_data) => {
                        // Check if our bdev exists in the list with either naming convention
                        if let Some(bdevs) = bdev_data["result"].as_array() {
                            let existing_bdev_names: Vec<String> = bdevs.iter()
                                .filter_map(|bdev| bdev["name"].as_str().map(|s| s.to_string()))
                                .collect();
                            
                            if let Some(existing_bdev_name) = self.find_existing_bdev_name(hardware_id, &existing_bdev_names) {
                                println!("✅ [BDEV_ENSURE] Local bdev already available: {}", existing_bdev_name);
                                Ok(())
                            } else {
                                println!("🔧 [BDEV_ENSURE] No bdev found for device {} - creating on-demand for PVC provisioning", hardware_id);
                                // Generate a target bdev name for creation (prefer userspace NVMe format)
                                let device_name = if let Some(name) = hardware_id.strip_prefix("/dev/") {
                                    name.to_string()
                                } else {
                                    hardware_id.clone()
                                };
                                // Create bdev on-demand during PVC provisioning
                                self.create_bdev_for_device(rpc_url, hardware_id, &device_name).await
                            }
                        } else {
                            println!("❌ [BDEV_ENSURE] Failed to parse bdev list from SPDK");
                            Err(Status::internal("Failed to parse bdev list"))
                        }
                    }
                    Err(e) => {
                        println!("❌ [BDEV_ENSURE] Failed to query SPDK bdevs: {}", e);
                        Err(Status::internal(format!("Failed to query SPDK bdevs: {}", e)))
                    }
                }
            } else {
                Err(Status::internal("Local RAID member missing hardware_id"))
            }
        } else {
            // NVMe-oF endpoint - ensure connection is established
            let endpoint = &member.nvmeof_endpoint;
            println!("🌐 [BDEV_ENSURE] Ensuring NVMe-oF connection: {}", endpoint.nqn);
            
            let connect_result = call_spdk_rpc(rpc_url, &json!({
                "method": "bdev_nvme_attach_controller",
                "params": {
                    "name": format!("nvmeof-{}", endpoint.nqn.split(':').last().unwrap_or("unknown")),
                    "trtype": endpoint.transport,
                    "traddr": endpoint.target_addr,
                    "trsvcid": endpoint.target_port.to_string(),
                    "subnqn": endpoint.nqn
                }
            })).await;

            match connect_result {
                Ok(_) => {
                    println!("✅ [BDEV_ENSURE] Connected to NVMe-oF endpoint: {}", endpoint.nqn);
                    Ok(())
                }
                Err(e) => {
                    println!("❌ [BDEV_ENSURE] Failed to connect to NVMe-oF {}: {}", endpoint.nqn, e);
                    Err(Status::internal(format!("Failed to ensure NVMe-oF bdev: {}", e)))
                }
            }
        }
    }

    /// Create bdev on-demand for PVC provisioning with fallback support
    /// 
    /// BINDING APPROACH LOGIC (mutually exclusive):
    /// 1. Try userspace NVMe (bdev_nvme_attach_controller) if driver unbinding is supported
    /// 2. Fall back to AIO (bdev_aio_create) if userspace NVMe is not available
    /// 
    /// MUTUAL EXCLUSIVITY GUARANTEE:
    /// - Only ONE of bdev_nvme_attach_controller OR bdev_aio_create is called per device
    /// - Different nodes can use different approaches based on their capabilities
    /// - Approach is clearly logged for visibility and troubleshooting
    async fn create_bdev_for_device(&self, rpc_url: &str, hardware_id: &str, bdev_name: &str) -> Result<(), Status> {
        println!("🔧 [BDEV_CREATE] Creating bdev on-demand: {} -> {}", hardware_id, bdev_name);
        
        // Extract device path from hardware_id (should be actual device path like "/dev/nvme1n1")
        let device_path = hardware_id.to_string();
        
        println!("🔍 [BDEV_CREATE] Device path: {}", device_path);
        
        // Extract PCI address - required for userspace NVMe binding
        let pci_addr = match self.extract_pci_address(&device_path).await {
            Some(addr) => addr,
            None => {
                println!("⚠️ [BDEV_CREATE] Cannot determine PCI address for device {}. Falling back to AIO.", device_path);
                return self.create_aio_bdev_fallback(rpc_url, &device_path, bdev_name).await;
            }
        };
        
        println!("🔍 [BDEV_CREATE] PCI address: {}", pci_addr);
        
        // Test driver unbinding capability first
        match self.test_driver_unbinding_capability_for_pci(&pci_addr).await {
            Ok(_) => {
                println!("✅ [BDEV_CREATE] Driver unbinding capability verified for {}", pci_addr);
                
                // Try userspace NVMe attachment
                match self.create_userspace_nvme_bdev(rpc_url, &pci_addr, bdev_name).await {
                    Ok(_) => {
                        println!("✅ [BDEV_CREATE] Successfully created userspace NVMe bdev for {}", pci_addr);
                        Ok(())
                    }
                    Err(e) => {
                        println!("⚠️ [BDEV_CREATE] Userspace NVMe attachment failed for {}: {}. Falling back to AIO.", pci_addr, e);
                        self.create_aio_bdev_fallback(rpc_url, &device_path, bdev_name).await
                    }
                }
            }
            Err(e) => {
                println!("⚠️ [BDEV_CREATE] Driver unbinding not supported for {}: {}. Using AIO fallback.", pci_addr, e);
                self.create_aio_bdev_fallback(rpc_url, &device_path, bdev_name).await
            }
        }
    }
    
    /// Create userspace NVMe bdev using bdev_nvme_attach_controller
    async fn create_userspace_nvme_bdev(&self, rpc_url: &str, pci_addr: &str, bdev_name: &str) -> Result<(), Status> {
        let controller_name = format!("nvme-{}", bdev_name);
        println!("🎯 [BINDING_APPROACH] USERSPACE_NVME: Attempting userspace NVMe attachment: {} -> {}", pci_addr, controller_name);
        println!("   🔧 Method: bdev_nvme_attach_controller (kernel driver will be unbound)");
        println!("   📋 Benefits: Direct hardware access, optimal performance, low latency");
        
        let nvme_result = call_spdk_rpc(rpc_url, &json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": controller_name,
                "trtype": "PCIe",
                "traddr": pci_addr
            }
        })).await;
        
        match nvme_result {
            Ok(_) => {
                println!("✅ [BINDING_APPROACH] USERSPACE_NVME: Successfully created bdev '{}' using userspace NVMe driver", controller_name);
                println!("   🚀 Device {} is now managed by SPDK userspace driver", pci_addr);
                // Note: binding_approach will be updated in RAID disk CRD separately
                Ok(())
            }
            Err(e) => {
                let error_msg = format!("Userspace NVMe attachment failed for {}: {}", pci_addr, e);
                println!("❌ [BINDING_APPROACH] USERSPACE_NVME: {}", error_msg);
                Err(Status::internal(error_msg))
            }
        }
    }
    
    /// Create AIO bdev as fallback when userspace NVMe is not available
    async fn create_aio_bdev_fallback(&self, rpc_url: &str, device_path: &str, bdev_name: &str) -> Result<(), Status> {
        let aio_bdev_name = format!("aio-{}", bdev_name);
        println!("🔄 [BINDING_APPROACH] AIO_FALLBACK: Creating AIO bdev: {} -> {}", device_path, aio_bdev_name);
        println!("   🔧 Method: bdev_aio_create (kernel driver remains active)");
        println!("   📋 Benefits: Compatible with all systems, no driver unbinding required");
        println!("   ⚠️  Trade-offs: Higher CPU overhead, shared kernel/userspace access");
        
        let aio_result = call_spdk_rpc(rpc_url, &json!({
            "method": "bdev_aio_create",
            "params": {
                "name": aio_bdev_name,
                "filename": device_path,
                "block_size": 512
            }
        })).await;
        
        match aio_result {
            Ok(_) => {
                println!("✅ [BINDING_APPROACH] AIO_FALLBACK: Successfully created bdev '{}' using AIO driver", aio_bdev_name);
                println!("   📁 Device {} is accessed through kernel NVMe driver", device_path);
                // Note: binding_approach will be updated in RAID disk CRD separately
                Ok(())
            }
            Err(e) => {
                let error_msg = format!("AIO bdev creation failed for {}: {}", device_path, e);
                println!("❌ [BINDING_APPROACH] AIO_FALLBACK: {}", error_msg);
                Err(Status::internal(error_msg))
            }
        }
    }
    
    /// Test if driver unbinding is possible for a PCI device (controller-side validation)
    /// Uses the same approach as the node agent for consistency
    async fn test_driver_unbinding_capability_for_pci(&self, pci_addr: &str) -> Result<(), Status> {
        println!("🔍 [CONTROLLER_UNBIND_TEST] Testing driver unbinding capability for PCI: {}", pci_addr);
        
        // Test 1: Check if basic driver management paths exist
        let required_paths = [
            "/sys/bus/pci/drivers_probe",
            "/sys/bus/pci/devices",
            "/sys/bus/pci/drivers",
        ];
        
        for path in &required_paths {
            if !std::path::Path::new(path).exists() {
                return Err(Status::internal(format!("Required driver management path missing: {}", path)));
            }
        }
        
        // Test 2: Check if we have write access to drivers_probe
        match tokio::fs::metadata("/sys/bus/pci/drivers_probe").await {
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt;
                if metadata.mode() & 0o200 == 0 {
                    return Err(Status::internal("drivers_probe is not writable - insufficient permissions"));
                }
            }
            Err(e) => {
                return Err(Status::internal(format!("Cannot access drivers_probe: {}", e)));
            }
        }
        
        // Test 3: Check for userspace driver availability
        if !self.test_userspace_driver_availability().await {
            return Err(Status::internal("No userspace drivers available (vfio-pci, uio_pci_generic)"));
        }
        
        // Test 4: Check device-specific driver_override path
        let driver_override_path = format!("/sys/bus/pci/devices/{}/driver_override", pci_addr);
        if !std::path::Path::new(&driver_override_path).exists() {
            return Err(Status::internal(format!("driver_override not available for device {}", pci_addr)));
        }
        
        match tokio::fs::read_to_string(&driver_override_path).await {
            Ok(_) => {
                println!("✅ [CONTROLLER_UNBIND_TEST] Driver unbinding capability verified for {}", pci_addr);
                Ok(())
            }
            Err(e) => {
                Err(Status::internal(format!("Cannot access driver_override on {}: {}", pci_addr, e)))
            }
        }
    }
    
    /// Test if userspace drivers (vfio-pci, uio_pci_generic) are available
    async fn test_userspace_driver_availability(&self) -> bool {
        // Check for VFIO support (preferred)
        if std::path::Path::new("/sys/bus/pci/drivers/vfio-pci").exists() {
            println!("✅ [CONTROLLER_USERSPACE_TEST] vfio-pci driver available");
            return true;
        }
        
        // Check for UIO support (fallback)
        if std::path::Path::new("/sys/bus/pci/drivers/uio_pci_generic").exists() {
            println!("✅ [CONTROLLER_USERSPACE_TEST] uio_pci_generic driver available");
            return true;
        }
        
        // Try to load vfio-pci module
        if let Ok(output) = tokio::process::Command::new("modinfo")
            .arg("vfio-pci")
            .output()
            .await
        {
            if output.status.success() {
                println!("✅ [CONTROLLER_USERSPACE_TEST] vfio-pci module available (can be loaded)");
                return true;
            }
        }
        
        // Try to load uio_pci_generic module
        if let Ok(output) = tokio::process::Command::new("modinfo")
            .arg("uio_pci_generic")
            .output()
            .await
        {
            if output.status.success() {
                println!("✅ [CONTROLLER_USERSPACE_TEST] uio_pci_generic module available (can be loaded)");
                return true;
            }
        }
        
        println!("❌ [CONTROLLER_USERSPACE_TEST] No userspace drivers available (vfio-pci, uio_pci_generic)");
        false
    }
    
    /// Create Kubernetes event on PVC for user visibility
    async fn create_pvc_event(&self, pvc_name: &str, pvc_namespace: &str, event_type: &str, reason: &str, message: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::{Event, ObjectReference};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, Time};
        use kube::api::PostParams;
        
        println!("🚨 [PVC_EVENT] Creating Kubernetes event for PVC: {}/{}", pvc_namespace, pvc_name);
        
        // Create Kubernetes client
        let client = match kube::Client::try_default().await {
            Ok(client) => client,
            Err(e) => {
                println!("⚠️ [PVC_EVENT] Failed to create Kubernetes client: {}", e);
                return Err(format!("Failed to create Kubernetes client: {}", e).into());
            }
        };
        
        let events: kube::Api<Event> = kube::Api::namespaced(client, pvc_namespace);
        
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        
        let event_time = Time(chrono::DateTime::from_timestamp(now.as_secs() as i64, 0).unwrap());
        let first_timestamp = event_time.clone();
        let last_timestamp = event_time.clone();
        let event_time_micro = MicroTime(chrono::DateTime::from_timestamp(now.as_secs() as i64, 0).unwrap());
        
        let event = Event {
            metadata: kube::api::ObjectMeta {
                name: Some(format!("pvc-userspace-binding-{}-{}", pvc_name, now.as_secs())),
                namespace: Some(pvc_namespace.to_string()),
                ..Default::default()
            },
            action: Some(event_type.to_string()),
            count: Some(1),
            event_time: Some(event_time_micro),
            first_timestamp: Some(first_timestamp),
            last_timestamp: Some(last_timestamp),
            message: Some(message.to_string()),
            reason: Some(reason.to_string()),
            reporting_component: Some("flint-csi-controller".to_string()),
            reporting_instance: Some("controller".to_string()),
            source: Some(k8s_openapi::api::core::v1::EventSource {
                component: Some("flint-csi-controller".to_string()),
                host: Some("controller".to_string()),
            }),
            type_: Some(event_type.to_string()),
            involved_object: ObjectReference {
                api_version: Some("v1".to_string()),
                kind: Some("PersistentVolumeClaim".to_string()),
                name: Some(pvc_name.to_string()),
                namespace: Some(pvc_namespace.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        
        match events.create(&PostParams::default(), &event).await {
            Ok(_) => {
                println!("✅ [PVC_EVENT] Successfully created Kubernetes event on PVC {}/{}", pvc_namespace, pvc_name);
            }
            Err(e) => {
                println!("⚠️ [PVC_EVENT] Failed to create Kubernetes event: {}", e);
                return Err(format!("Failed to create Kubernetes event: {}", e).into());
            }
        }
        
        Ok(())
    }
    
    /// Query SPDK state for idempotent RAID operations
    async fn query_spdk_state_for_raid(&self, rpc_url: &str, _raid_disk: &SpdkRaidDisk) -> Result<SpdkRaidState, Status> {
        println!("🔍 [SPDK_STATE] Querying SPDK state as source of truth");
        
        // Query all bdevs
        let bdevs_result = call_spdk_rpc(rpc_url, &json!({"method": "bdev_get_bdevs"})).await
            .map_err(|e| Status::internal(format!("Failed to query bdevs: {}", e)))?;
        
        let bdevs: Vec<String> = if let Some(bdev_array) = bdevs_result["result"].as_array() {
            bdev_array.iter()
                .filter_map(|b| b["name"].as_str().map(|s| s.to_string()))
                .collect()
        } else {
            Vec::new()
        };
        
        // Filter RAID bdevs (those created by bdev_raid_create)
        let raid_bdevs: Vec<String> = bdevs.iter()
            .filter(|name| name.starts_with("raid_") || name.contains("raid"))
            .cloned()
            .collect();
        
        // Query LVS stores
        let lvs_result = call_spdk_rpc(rpc_url, &json!({"method": "bdev_lvol_get_lvstores"})).await
            .map_err(|e| Status::internal(format!("Failed to query LVS: {}", e)))?;
        
        let lvs_stores: Vec<LvsState> = if let Some(lvs_array) = lvs_result["result"].as_array() {
            lvs_array.iter().filter_map(|lvs| {
                let name = lvs["name"].as_str()?;
                let base_bdev = lvs["base_bdev"].as_str()?;
                let total_clusters = lvs["total_data_clusters"].as_u64()?;
                let free_clusters = lvs["free_clusters"].as_u64()?;
                let cluster_size = lvs["cluster_size"].as_u64()?;
                
                Some(LvsState {
                    name: name.to_string(),
                    base_bdev: base_bdev.to_string(),
                    total_capacity: total_clusters * cluster_size,
                    free_capacity: free_clusters * cluster_size,
                    cluster_size,
                })
            }).collect()
        } else {
            Vec::new()
        };
        
        let state = SpdkRaidState {
            bdevs,
            raid_bdevs,
            lvs_stores,
        };
        
        println!("📊 [SPDK_STATE] Found {} bdevs, {} RAID bdevs, {} LVS stores", 
                 state.bdevs.len(), state.raid_bdevs.len(), state.lvs_stores.len());
        
        Ok(state)
    }
    
    /// Generate possible bdev names from hardware_id (supports both userspace NVMe and AIO)
    /// e.g., "/dev/nvme1n1" -> ["nvme-nvme1n1", "aio-nvme1n1"]
    fn generate_possible_bdev_names(&self, hardware_id: &str) -> Vec<String> {
        let device_name = if let Some(name) = hardware_id.strip_prefix("/dev/") {
            name.to_string()
        } else {
            hardware_id.to_string()
        };
        
        vec![
            format!("nvme-{}", device_name),  // Userspace NVMe naming
            format!("aio-{}", device_name),   // AIO fallback naming
        ]
    }
    
    /// Find actual bdev name from hardware_id by checking which one exists in SPDK
    /// Returns the first matching bdev name, or None if neither exists
    fn find_existing_bdev_name(&self, hardware_id: &str, existing_bdevs: &[String]) -> Option<String> {
        let possible_names = self.generate_possible_bdev_names(hardware_id);
        
        for name in possible_names {
            if existing_bdevs.contains(&name) {
                return Some(name);
            }
        }
        
        None
    }

    /// Update the binding approach for a RAID member disk after successful bdev creation
    async fn update_member_binding_approach(&self, raid_disk_name: &str, hardware_id: &str, binding_approach: &str) -> Result<(), Status> {
        println!("📝 [BINDING_UPDATE] Updating binding approach for {} in RAID {}: {}", hardware_id, raid_disk_name, binding_approach);
        
        let spdk_raid_disks: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        // Get the current RAID disk
        let mut raid_disk = match spdk_raid_disks.get(raid_disk_name).await {
            Ok(rd) => rd,
            Err(e) => {
                println!("⚠️ [BINDING_UPDATE] Failed to get RAID disk {}: {}", raid_disk_name, e);
                return Ok(()); // Don't fail the overall operation for metadata updates
            }
        };
        
        // Find and update the matching member disk
        let mut updated = false;
        for member in &mut raid_disk.spec.member_disks {
            if member.hardware_id.as_deref() == Some(hardware_id) {
                member.binding_approach = Some(binding_approach.to_string());
                updated = true;
                println!("✅ [BINDING_UPDATE] Updated binding approach for member disk {} to {}", hardware_id, binding_approach);
                break;
            }
        }
        
        if !updated {
            println!("⚠️ [BINDING_UPDATE] Member disk with hardware_id {} not found in RAID {}", hardware_id, raid_disk_name);
            return Ok(());
        }
        
        // Update the RAID disk CRD
        match spdk_raid_disks.replace(raid_disk_name, &PostParams::default(), &raid_disk).await {
            Ok(_) => {
                println!("✅ [BINDING_UPDATE] Successfully updated RAID disk CRD with binding approach");
            }
            Err(e) => {
                println!("⚠️ [BINDING_UPDATE] Failed to update RAID disk CRD: {}", e);
                // Don't fail the overall operation for metadata updates
            }
        }
        
        Ok(())
    }
    
    /// Clean up a local bdev using the correct counterpart function based on binding approach
    async fn cleanup_local_bdev(&self, rpc_url: &str, hardware_id: &str, binding_approach: &str) -> Result<(), Status> {
        println!("🧹 [BDEV_CLEANUP] Cleaning up local bdev for device: {} (approach: {})", hardware_id, binding_approach);
        
        // Determine bdev name based on binding approach
        let device_name = if let Some(name) = hardware_id.strip_prefix("/dev/") {
            name.to_string()
        } else {
            hardware_id.to_string()
        };
        
        match binding_approach {
            "userspace-nvme" => {
                // Clean up userspace NVMe bdev using bdev_nvme_detach_controller
                let controller_name = format!("nvme-{}", device_name);
                println!("🎯 [BDEV_CLEANUP] USERSPACE_NVME: Detaching userspace NVMe controller: {}", controller_name);
                
                let detach_result = call_spdk_rpc(rpc_url, &json!({
                    "method": "bdev_nvme_detach_controller",
                    "params": {
                        "name": controller_name
                    }
                })).await;
                
                match detach_result {
                    Ok(_) => {
                        println!("✅ [BDEV_CLEANUP] USERSPACE_NVME: Successfully detached controller: {}", controller_name);
                        Ok(())
                    }
                    Err(e) => {
                        // Don't fail cleanup for missing bdevs
                        if e.to_string().contains("not found") || e.to_string().contains("does not exist") {
                            println!("ℹ️ [BDEV_CLEANUP] USERSPACE_NVME: Controller {} already detached", controller_name);
                            Ok(())
                        } else {
                            println!("⚠️ [BDEV_CLEANUP] USERSPACE_NVME: Failed to detach controller {}: {}", controller_name, e);
                            Err(Status::internal(format!("Failed to detach userspace NVMe controller: {}", e)))
                        }
                    }
                }
            }
            "aio-fallback" => {
                // Clean up AIO bdev using bdev_aio_delete
                let aio_bdev_name = format!("aio-{}", device_name);
                println!("🔄 [BDEV_CLEANUP] AIO_FALLBACK: Deleting AIO bdev: {}", aio_bdev_name);
                
                let delete_result = call_spdk_rpc(rpc_url, &json!({
                    "method": "bdev_aio_delete",
                    "params": {
                        "name": aio_bdev_name
                    }
                })).await;
                
                match delete_result {
                    Ok(_) => {
                        println!("✅ [BDEV_CLEANUP] AIO_FALLBACK: Successfully deleted AIO bdev: {}", aio_bdev_name);
                        Ok(())
                    }
                    Err(e) => {
                        // Don't fail cleanup for missing bdevs
                        if e.to_string().contains("not found") || e.to_string().contains("does not exist") {
                            println!("ℹ️ [BDEV_CLEANUP] AIO_FALLBACK: AIO bdev {} already deleted", aio_bdev_name);
                            Ok(())
                        } else {
                            println!("⚠️ [BDEV_CLEANUP] AIO_FALLBACK: Failed to delete AIO bdev {}: {}", aio_bdev_name, e);
                            Err(Status::internal(format!("Failed to delete AIO bdev: {}", e)))
                        }
                    }
                }
            }
            "nvmeof" => {
                // NVMe-oF cleanup is handled separately in existing code
                println!("ℹ️ [BDEV_CLEANUP] NVMEOF: Skipping local cleanup for remote NVMe-oF disk");
                Ok(())
            }
            _ => {
                println!("⚠️ [BDEV_CLEANUP] Unknown binding approach '{}' for device {}", binding_approach, hardware_id);
                Ok(()) // Don't fail for unknown approaches
            }
        }
    }
    
    /// Clean up all member bdevs in a RAID disk using correct counterpart functions  
    async fn cleanup_raid_member_bdevs(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let raid_name = raid_disk.metadata.name.as_deref().unwrap_or("unknown");
        println!("🧹 [RAID_CLEANUP] Cleaning up member bdevs for RAID disk: {}", raid_name);
        
        let target_rpc_url = self.driver.get_rpc_url_for_node(&raid_disk.spec.created_on_node).await
            .map_err(|e| Status::internal(format!("Failed to get RPC URL for node {}: {}", raid_disk.spec.created_on_node, e)))?;
        
        for (index, member) in raid_disk.spec.member_disks.iter().enumerate() {
            if let (Some(hardware_id), Some(binding_approach)) = (&member.hardware_id, &member.binding_approach) {
                println!("🔧 [RAID_CLEANUP] Cleaning up member {} ({}): {}", index, binding_approach, hardware_id);
                
                if let Err(e) = self.cleanup_local_bdev(&target_rpc_url, hardware_id, binding_approach).await {
                    println!("⚠️ [RAID_CLEANUP] Failed to cleanup member {} bdev: {}", index, e);
                    // Continue with other members rather than failing entire cleanup
                }
            } else {
                println!("ℹ️ [RAID_CLEANUP] Skipping member {} cleanup: missing hardware_id or binding_approach", index);
            }
        }
        
        println!("✅ [RAID_CLEANUP] Completed member bdev cleanup for RAID disk: {}", raid_name);
        Ok(())
    }
    
    /// Extract PCI address from device path (helper for bare metal NVMe attachment)
    async fn extract_pci_address(&self, _device_path: &str) -> Option<String> {
        // This is a simplified implementation - in reality you'd need to query sysfs
        // For now, return None to prefer AIO attachment
        None
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
        
        // Unified RAID-based provisioning: always RAID1
        let desired_raid_level = "1";
        let (storage_backend, lvol_uuid, lvs_name, raid_disk) = {
            let raid_disk = self.find_or_create_raid_disk(num_replicas, capacity, desired_raid_level).await?;
            let lvol_uuid = self.create_volume_lvol_on_raid(&raid_disk, capacity, volume_id).await?;
            let lvs_name = raid_disk.spec.lvs_name();
            let storage_backend = StorageBackend::RaidDisk {
                raid_disk_ref: raid_disk.metadata.name.clone().unwrap_or_default(),
                node_id: raid_disk.spec.created_on_node.clone(),
            };
            (storage_backend, lvol_uuid, lvs_name, raid_disk)
        };

        // Create unified SpdkVolume spec (same structure for both single and multi-replica)
        let mut spdk_volume = SpdkVolume::new_with_metadata(
            volume_id,
            SpdkVolumeSpec {
                volume_id: volume_id.to_string(),
                size_bytes: capacity,
                num_replicas,
                storage_backend,
                lvol_uuid: Some(lvol_uuid.clone()),
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

        // Set initial status with all required fields
        spdk_volume.status = Some(SpdkVolumeStatus {
            state: "ready".to_string(),
            degraded: false,
            last_checked: chrono::Utc::now().to_rfc3339(),
            active_replicas: (0..num_replicas as usize).collect(),
            failed_replicas: vec![],
            write_sequence: 0,
            last_successful_write: Some(chrono::Utc::now().to_rfc3339()),
            raid_status: None,
            nvmeof_targets: vec![],
            ublk_device: None,
            nvme_device: Some(NvmeClientDevice {
                device_path: format!("/dev/nvme-{}", lvol_uuid),
                nqn: format!("nqn.2016-06.io.spdk:vol-{}", volume_id),
                transport: self.driver.nvmeof_transport.clone(),
                target_addr: raid_disk.spec.created_on_node.clone(),
                target_port: self.driver.nvmeof_target_port,
                connected_at: chrono::Utc::now().to_rfc3339(),
                node: raid_disk.spec.created_on_node.clone(),
                controller_id: Some(format!("flint-{}", volume_id)),
            }),
            scheduled_node: Some(raid_disk.spec.created_on_node.clone()),
            has_local_replica: true,
            scheduling_policy: Some("local-preferred".to_string()),
            replica_nodes: vec![raid_disk.spec.created_on_node.clone()],
            read_optimized: true,
            read_policy: Some("local-first".to_string()),
            local_replica_performance: None,
        });

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

        // RAID disk status is maintained by operator; nothing to update here

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

    // REMOVED legacy single-disk helpers

    // Disk status updates removed with SpdkDisk deprecation

    async fn delete_volume_replicas(&self, volume: &SpdkVolume) -> Result<(), Status> {
        println!("🗑️ [VOLUME_DELETE] Starting volume deletion for: {}", volume.spec.volume_id);
        
        // Handle unified RAID-based architecture (new approach)
        match &volume.spec.storage_backend {
            StorageBackend::RaidDisk { raid_disk_ref, node_id } => {
            println!("🗑️ [VOLUME_DELETE] RAID-based volume: cleaning up RAID disk {}", raid_disk_ref);
            
            // First delete the logical volume
            if let Some(lvs_name) = &volume.spec.lvs_name {
                let lvol_name = format!("{}/{}", lvs_name, volume.spec.volume_id);
                println!("🗑️ [VOLUME_DELETE] Deleting logical volume: {}", lvol_name);
                
                let rpc_url = self.driver.get_rpc_url_for_node(node_id).await?;
                let delete_result = call_spdk_rpc(&rpc_url, &json!({
                    "method": "bdev_lvol_delete",
                    "params": { "name": lvol_name }
                })).await;
                
                match delete_result {
                    Ok(_) => println!("✅ [VOLUME_DELETE] Successfully deleted logical volume: {}", lvol_name),
                    Err(e) => {
                        if e.to_string().contains("not found") || e.to_string().contains("does not exist") {
                            println!("ℹ️ [VOLUME_DELETE] Logical volume already deleted: {}", lvol_name);
                        } else {
                            println!("⚠️ [VOLUME_DELETE] Failed to delete logical volume {}: {}", lvol_name, e);
                        }
                    }
                }
            }
            
            // Check if RAID disk is still in use by other volumes
            let raid_still_in_use = self.is_raid_disk_in_use(raid_disk_ref).await?;
            
            if !raid_still_in_use {
                println!("🗑️ [VOLUME_DELETE] RAID disk {} no longer in use, cleaning up", raid_disk_ref);
                
                // Get the RAID disk CRD
                let spdk_raid_disks: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
                if let Ok(raid_disk) = spdk_raid_disks.get(raid_disk_ref).await {
                    // Clean up member bdevs using correct counterpart functions
                    self.cleanup_raid_member_bdevs(&raid_disk).await?;
                    
                    // Delete the RAID bdev itself
                    let raid_bdev_name = raid_disk.spec.raid_bdev_name();
                    println!("🗑️ [VOLUME_DELETE] Deleting RAID bdev: {}", raid_bdev_name);
                    
                    // Get RPC URL for the RAID disk's node
                    let raid_rpc_url = self.driver.get_rpc_url_for_node(&raid_disk.spec.created_on_node).await?;
                    let delete_result = call_spdk_rpc(&raid_rpc_url, &json!({
                        "method": "bdev_raid_delete",
                        "params": { "name": raid_bdev_name }
                    })).await;
                    
                    match delete_result {
                        Ok(_) => println!("✅ [VOLUME_DELETE] Successfully deleted RAID bdev: {}", raid_bdev_name),
                        Err(e) => {
                            if e.to_string().contains("not found") || e.to_string().contains("does not exist") {
                                println!("ℹ️ [VOLUME_DELETE] RAID bdev already deleted: {}", raid_bdev_name);
                            } else {
                                println!("⚠️ [VOLUME_DELETE] Failed to delete RAID bdev {}: {}", raid_bdev_name, e);
                            }
                        }
                    }
                    
                    // Delete the RAID disk CRD
                    println!("🗑️ [VOLUME_DELETE] Deleting RAID disk CRD: {}", raid_disk_ref);
                    if let Err(e) = spdk_raid_disks.delete(raid_disk_ref, &Default::default()).await {
                        println!("⚠️ [VOLUME_DELETE] Failed to delete RAID disk CRD {}: {}", raid_disk_ref, e);
                    } else {
                        println!("✅ [VOLUME_DELETE] Successfully deleted RAID disk CRD: {}", raid_disk_ref);
                    }
                } else {
                    println!("⚠️ [VOLUME_DELETE] RAID disk CRD {} not found", raid_disk_ref);
                }
            } else {
                println!("ℹ️ [VOLUME_DELETE] RAID disk {} is still in use by other volumes", raid_disk_ref);
            }
            }
        }
        
        // Handle legacy replica-based architecture for backward compatibility
        if !volume.spec.replicas.is_empty() {
            println!("🗑️ [VOLUME_DELETE] Legacy replica-based volume: cleaning up replicas");
            
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
        }
        
        println!("✅ [VOLUME_DELETE] Completed volume deletion for: {}", volume.spec.volume_id);
        Ok(())
    }
    
    /// Check if a RAID disk is still in use by other volumes
    async fn is_raid_disk_in_use(&self, raid_disk_ref: &str) -> Result<bool, Status> {
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let volumes = volumes_api.list(&Default::default()).await
            .map_err(|e| Status::internal(format!("Failed to list volumes: {}", e)))?;
        
        for volume in volumes.items {
            match &volume.spec.storage_backend {
                StorageBackend::RaidDisk { raid_disk_ref: vol_raid_ref, .. } => {
                    if vol_raid_ref == raid_disk_ref {
                        println!("ℹ️ [RAID_CHECK] RAID disk {} is still used by volume {}", raid_disk_ref, volume.spec.volume_id);
                        return Ok(true);
                    }
                }
            }
        }
        
        println!("ℹ️ [RAID_CHECK] RAID disk {} is not used by any volumes", raid_disk_ref);
        Ok(false)
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
                
                // SPDK configuration auto-save could be added here if needed
                
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
                
                // Extract PVC name and namespace from volume name for event creation
                let (pvc_name, pvc_namespace) = if volume_name.starts_with("pvc-") {
                    // Standard CSI PVC naming: pvc-{uuid}
                    // Try to find the actual PVC name from parameters or use the volume name
                    let pvc_name = req.parameters.get("csi.storage.k8s.io/pvc/name")
                        .map(|s| s.as_str())
                        .unwrap_or(volume_name.as_str());
                    let pvc_namespace = req.parameters.get("csi.storage.k8s.io/pvc/namespace")
                        .map(|s| s.as_str())
                        .or_else(|| req.parameters.get("csi.storage.k8s.io/pod/namespace").map(|s| s.as_str()))
                        .unwrap_or("default");
                    (pvc_name, pvc_namespace)
                } else {
                    // Extract namespace from parameters if available
                    let default_namespace = req.parameters.get("csi.storage.k8s.io/pvc/namespace")
                        .map(|s| s.as_str())
                        .or_else(|| req.parameters.get("csi.storage.k8s.io/pod/namespace").map(|s| s.as_str()))
                        .unwrap_or("default");
                    (volume_name.as_str(), default_namespace)
                };
                
                // For resource exhaustion errors, provide more detailed context and create PVC events
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
                    } else if status.message().contains("No local NVMe disks or external NVMe-oF endpoints available") {
                        // This indicates userspace binding limitation - create PVC event
                        let event_message = format!(
                            "Storage provisioning failed: {}. This instance type may not support userspace SPDK operations. Required: 1) Kernel driver unbinding capability, 2) Userspace drivers (vfio-pci or uio_pci_generic), 3) Write access to /sys/bus/pci/drivers_probe. Consider using instances that support IOMMU and userspace driver management.",
                            status.message()
                        );
                        
                        // Create event on PVC for user visibility
                        if let Err(e) = self.create_pvc_event(
                            pvc_name,
                            pvc_namespace,
                            "Warning",
                            "UserspaceBindingNotSupported",
                            &event_message
                        ).await {
                            println!("⚠️ [PVC_EVENT] Failed to create PVC event: {}", e);
                        }
                        
                        return Err(Status::resource_exhausted(event_message));
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

        // SPDK configuration auto-save could be added here if needed

        // RAID disk status is maintained by operator; no per-disk status updates here

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
        let raids_api: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let raid_list = raids_api.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list SpdkRaidDisks: {}", e)))?;

        let total_capacity = raid_list.items.iter()
            .filter_map(|raid| raid.status.as_ref())
            .filter(|status| status.state == "online" && !status.degraded)
            .map(|status| status.usable_capacity_bytes - status.used_capacity_bytes)
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
