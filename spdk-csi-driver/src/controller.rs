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
        
        // TODO: Create Kubernetes event on PVC for user visibility
        // self.create_pvc_event("Warning", "ProvisioningFailed", &error_msg).await;
        
        Err(Status::resource_exhausted(error_msg))
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
        let available_disks = self.find_available_local_nvme_disks(required_capacity).await?;
        
        if available_disks.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(format!(
                "Need {} disks but only {} local NVMe disks available",
                num_replicas, available_disks.len()
            )));
        }

        // Step 2: Select optimal node for RAID creation (node with most local members)
        let optimal_node = self.select_optimal_raid_node(&available_disks, num_replicas).await?;
        
        // Step 3: Select disks for RAID (prefer disks on optimal node, then nearby nodes)
        let selected_disks = self.select_raid_member_disks(&available_disks, num_replicas, &optimal_node).await?;

        // Step 4: Create the SpdkRaidDisk CRD
        let raid_disk = self.create_raid_disk_crd(&selected_disks, raid_level, &optimal_node).await?;

        // Step 5: Initialize the actual RAID bdev and LVS on the target node
        self.initialize_raid_bdev_and_lvs(&raid_disk).await?;

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
        self.initialize_raid_bdev_and_lvs(&raid_disk).await?;

        println!("✅ [REMOTE_RAID] Successfully created RAID disk from NVMe-oF endpoints");
        Ok(raid_disk)
    }

    /// Initialize RAID bdev and LVS on the target node with comprehensive status updates
    async fn initialize_raid_bdev_and_lvs(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let target_node = &raid_disk.spec.created_on_node;
        let spdk_rpc_url = self.driver.get_rpc_url_for_node(target_node).await?;
        
        println!("🔧 [RAID_INIT] Initializing RAID bdev and LVS on node: {}", target_node);

        // Update status to 'initializing'
        self.update_raid_disk_status_initializing(raid_disk).await?;

        // Step 1: Ensure all member bdevs are available in SPDK
        println!("🔗 [RAID_INIT] Step 1: Ensuring member bdevs are available...");
        for (index, member) in raid_disk.spec.member_disks.iter().enumerate() {
            match self.ensure_member_bdev_available(&spdk_rpc_url, member, index).await {
                Ok(_) => println!("✅ [RAID_INIT] Member {} bdev ready", index),
                Err(e) => {
                    let error_msg = format!("Failed to ensure member {} bdev: {}", index, e);
                    self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                    return Err(e);
                }
            }
        }

        // Step 2: Create RAID bdev
        println!("⚙️ [RAID_INIT] Step 2: Creating RAID bdev...");
        let raid_bdev_name = raid_disk.spec.raid_bdev_name();
        let member_names: Vec<String> = raid_disk.spec.member_disks.iter()
            .map(|m| format!("nvme-{}", m.hardware_id.as_ref().unwrap_or(&"unknown".to_string())))
            .collect();

        let raid_create_result = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid_bdev_name,
                "raid_level": raid_disk.spec.raid_level,
                "base_bdevs": member_names,
                "strip_size_kb": raid_disk.spec.stripe_size_kb
            }
        })).await;

        match raid_create_result {
            Ok(_) => {
                println!("✅ [RAID_INIT] RAID bdev created: {}", raid_bdev_name);
                self.update_raid_disk_status_bdev_created(raid_disk).await?;
            }
            Err(e) => {
                let error_msg = format!("Failed to create RAID bdev: {}", e);
                self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                return Err(Status::internal(error_msg));
            }
        }

        // Step 3: Create LVS on RAID bdev (with thin provisioning)
        println!("💾 [RAID_INIT] Step 3: Creating LVS with thin provisioning...");
        let lvs_name = raid_disk.spec.lvs_name();
        let lvs_create_result = call_spdk_rpc(&spdk_rpc_url, &json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": raid_bdev_name,
                "lvs_name": lvs_name,
                "cluster_sz": 1048576  // 1MB clusters for thin provisioning
            }
        })).await;

        match lvs_create_result {
            Ok(_) => {
                println!("✅ [RAID_INIT] LVS created with thin provisioning: {}", lvs_name);
                
                // Update RAID disk status to indicate it's ready
                self.update_raid_disk_status_to_ready(raid_disk).await?;
                
                Ok(())
            }
            Err(e) => {
                let error_msg = format!("Failed to create LVS: {}", e);
                self.update_raid_disk_status_failed(raid_disk, &error_msg).await?;
                Err(Status::internal(error_msg))
            }
        }
    }

    /// Update RAID disk status to 'initializing'
    async fn update_raid_disk_status_initializing(&self, raid_disk: &SpdkRaidDisk) -> Result<(), Status> {
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        let mut status = raid_disk.status.clone().unwrap_or_default();
        status.state = "initializing".to_string();
        status.health_status = "initializing".to_string();
        status.last_checked = chrono::Utc::now().to_rfc3339();
        
        let patch = json!({ "status": status });
        raids.patch_status(
            &raid_disk.metadata.name.as_ref().unwrap(),
            &PatchParams::default(),
            &Patch::Merge(patch)
        ).await
        .map_err(|e| Status::internal(format!("Failed to update RAID status to initializing: {}", e)))?;

        println!("🔄 [RAID_STATUS] Updated RAID disk {} to 'initializing' status", 
                 raid_disk.metadata.name.as_ref().unwrap());
        Ok(())
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
        println!("🔍 [DISK_DISCOVERY] Searching for available local NVMe disks (min {}GB)", 
                 min_capacity / (1024 * 1024 * 1024));

        // TODO: Implement actual discovery across cluster nodes
        // This would query node agents to get their discovered local disks
        // For now, return placeholder structure
        
        let mut available_disks = Vec::new();
        
        // Query all nodes for available local disks via node agent APIs
        let nodes = self.get_cluster_nodes().await?;
        for node in nodes {
            match self.query_node_available_disks(&node, min_capacity).await {
                Ok(mut node_disks) => available_disks.append(&mut node_disks),
                Err(e) => println!("⚠️ [DISK_DISCOVERY] Failed to query node {}: {}", node, e),
            }
        }

        println!("✅ [DISK_DISCOVERY] Found {} available local NVMe disks", available_disks.len());
        Ok(available_disks)
    }

    /// Select optimal node for RAID creation (node with most local members)
    async fn select_optimal_raid_node(&self, available_disks: &[AvailableNvmeDisk], _num_replicas: i32) -> Result<String, Status> {
        use std::collections::HashMap;

        let mut node_disk_counts: HashMap<String, usize> = HashMap::new();
        for disk in available_disks {
            *node_disk_counts.entry(disk.node_id.clone()).or_insert(0) += 1;
        }

        // Find node with most disks (prefer local storage for performance)
        let optimal_node = node_disk_counts.iter()
            .max_by_key(|(_, count)| *count)
            .map(|(node, _)| node.clone())
            .ok_or_else(|| Status::resource_exhausted("No nodes with available disks found"))?;

        println!("🎯 [NODE_SELECT] Selected optimal node: {} ({} local disks)", 
                 optimal_node, node_disk_counts.get(&optimal_node).unwrap_or(&0));
        
        Ok(optimal_node)
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

        let raid_id = format!("auto-raid-{}", Uuid::new_v4().to_string().split('-').next().unwrap());
        
        let member_disks: Vec<RaidMemberDisk> = selected_disks.iter().enumerate().map(|(i, disk)| {
            RaidMemberDisk {
                member_index: i as u32,
                node_id: disk.node_id.clone(),
                hardware_id: Some(disk.device_path.clone()),
                serial_number: Some(disk.serial_number.clone()),
                wwn: disk.wwn.clone(),
                model: Some(disk.model.clone()),
                vendor: Some(disk.vendor.clone()),
                nvmeof_endpoint: NvmeofEndpoint::default(), // Local disk, no NVMe-oF endpoint needed
                state: RaidMemberState::Online,
                capacity_bytes: disk.capacity,
                connected: true,
                last_health_check: Some(chrono::Utc::now().to_rfc3339()),
            }
        }).collect();

        let spec = SpdkRaidDiskSpec {
            raid_disk_id: raid_id.clone(),
            raid_level: raid_level.to_string(),
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
            state: "creating".to_string(),
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
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let created_raid = raids.create(&PostParams::default(), &raid_disk).await
            .map_err(|e| Status::internal(format!("Failed to create SpdkRaidDisk CRD: {}", e)))?;

        println!("✅ [CRD_CREATE] Created SpdkRaidDisk CRD: {} with initial 'creating' status", raid_id);
        
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
        
        if let Some(bdevs) = bdev_data.as_array() {
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
                    
                    let is_available_storage = !is_claimed && 
                                             supports_read && 
                                             supports_write && 
                                             capacity >= min_capacity as u64 && 
                                             !self.is_bdev_in_use(node, name).await;
                    
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
                
                // Check if bdev already exists
                let check_result = call_spdk_rpc(rpc_url, &json!({
                    "method": "bdev_get_bdevs",
                    "params": { "name": hardware_id }
                })).await;

                match check_result {
                    Ok(_) => {
                        println!("✅ [BDEV_ENSURE] Local bdev already available: {}", hardware_id);
                        Ok(())
                    }
                    Err(_) => {
                        // Try to attach via PCIe if not already attached
                        let attach_result = call_spdk_rpc(rpc_url, &json!({
                            "method": "bdev_nvme_attach_controller",
                            "params": {
                                "name": hardware_id,
                                "trtype": "PCIe",
                                "traddr": hardware_id // Assuming hardware_id is PCI address
                            }
                        })).await;

                        match attach_result {
                            Ok(_) => {
                                println!("✅ [BDEV_ENSURE] Attached local NVMe device: {}", hardware_id);
                                Ok(())
                            }
                            Err(e) => {
                                println!("❌ [BDEV_ENSURE] Failed to attach local device {}: {}", hardware_id, e);
                                Err(Status::internal(format!("Failed to ensure local bdev: {}", e)))
                            }
                        }
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
