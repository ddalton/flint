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
    async fn provision_multi_replica_volume(
        &self,
        volume_id: &str,
        capacity: i64,
        num_replicas: i32,
    ) -> Result<(StorageBackend, String, String), Status> {
        // Find a suitable RAID1 disk
        let raid_disk = self.find_or_create_raid_disk(num_replicas, capacity, "1").await?;
        
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
        let spdk_rpc_url = self.driver.get_rpc_url_for_node(target_node).await?;
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
        raid_level: &str,
    ) -> Result<SpdkRaidDisk, Status> {
        // First, try to find an existing RAID disk that can accommodate the volume
        if let Ok(existing_raid) = self.find_suitable_raid_disk(num_replicas, required_capacity, raid_level).await {
            return Ok(existing_raid);
        }

        // If no suitable RAID disk exists, return resource exhausted
        Err(Status::resource_exhausted(format!(
            "No suitable RAID{} disk found for {} replicas and capacity {} bytes",
            raid_level, num_replicas, required_capacity
        )))
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

    /// Create a new RAID disk from available member disks
    // RAID disk creation removed: controller no longer assembles RAID; it selects existing SpdkRaidDisk

    /// Create the actual RAID bdev on the specified node using SPDK RPC
    // RAID bdev assembly removed from controller

    /// Create LVS on the RAID disk
    // LVS creation on RAID disk removed from controller (expect existing LVS)

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
        
        // Unified RAID-based provisioning: always RAID1
        let desired_raid_level = "1";
        let (storage_backend, lvol_uuid, lvs_name) = {
            let raid_disk = self.find_or_create_raid_disk(num_replicas, capacity, desired_raid_level).await?;
            let lvol_uuid = self.create_volume_lvol_on_raid(&raid_disk, capacity, volume_id).await?;
            let lvs_name = raid_disk.spec.lvs_name();
            let storage_backend = StorageBackend::RaidDisk {
                raid_disk_ref: raid_disk.metadata.name.clone().unwrap_or_default(),
                node_id: raid_disk.spec.created_on_node.clone(),
            };
            (storage_backend, lvol_uuid, lvs_name)
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
                
                // Auto-save SPDK configuration after volume creation (non-blocking)
                spdk_csi_driver::spdk_config_sync::safe_auto_save_spdk_config(
                    &self.driver.kube_client,
                    &self.driver.target_namespace,
                    &self.driver.node_id,
                    &self.driver.spdk_rpc_url,
                    "volume creation",
                ).await;
                
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

        // Auto-save SPDK configuration after volume deletion (non-blocking)
        spdk_csi_driver::spdk_config_sync::safe_auto_save_spdk_config(
            &self.driver.kube_client,
            &self.driver.target_namespace,
            &self.driver.node_id,
            &self.driver.spdk_rpc_url,
            "volume deletion",
        ).await;

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
