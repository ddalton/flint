// controller.rs - Controller service implementation
use std::sync::Arc;
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

    /// Provision volume with specified number of replicas - enhanced with validation
    async fn provision_volume(
        &self,
        volume_id: &str,
        capacity: i64,
        num_replicas: i32,
    ) -> Result<SpdkVolume, Status> {
        // Validate inputs
        self.validate_volume_request(volume_id, capacity, num_replicas).await?;
        
        let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let available_disks = self.get_available_disks(&disks, capacity).await?;

        // Enhanced validation with better error messages
        self.validate_disk_availability(&available_disks, capacity, num_replicas).await?;

        let selected_disks = self.select_disks_with_node_separation(available_disks, num_replicas as usize)?;
        let replicas = self.create_replicas(&selected_disks, capacity, volume_id).await?;

        let spdk_volume = SpdkVolume::new_with_metadata(
            volume_id,
            SpdkVolumeSpec {
                volume_id: volume_id.to_string(),
                size_bytes: capacity,
                num_replicas,
                replicas: replicas.clone(),
                raid_auto_rebuild: num_replicas > 1,
                nvmeof_transport: Some(self.driver.nvmeof_transport.clone()),
                nvmeof_target_port: Some(self.driver.nvmeof_target_port),
                ..Default::default()
            },
            &self.driver.target_namespace,
        );

        // Create CRD
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        crd_api.create(&PostParams::default(), &spdk_volume).await
            .map_err(|e| Status::internal(format!("Failed to create SpdkVolume CRD: {}", e)))?;

        // Update disk statuses
        self.update_disk_statuses(&disks, &selected_disks, capacity, 1).await?;

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

    /// Validate disk availability and capacity
    async fn validate_disk_availability(
        &self,
        available_disks: &[SpdkDisk],
        capacity: i64,
        num_replicas: i32,
    ) -> Result<(), Status> {
        if available_disks.is_empty() {
            return Err(Status::resource_exhausted(
                "No healthy disks available for volume provisioning"
            ));
        }

        if available_disks.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(
                format!(
                    "Insufficient healthy disks: need {}, found {} available", 
                    num_replicas, 
                    available_disks.len()
                )
            ));
        }

        // Check if any disk has sufficient capacity
        let disks_with_capacity = available_disks.iter()
            .filter(|disk| {
                disk.status.as_ref().map_or(false, |status| status.free_space >= capacity)
            })
            .count();

        if disks_with_capacity < num_replicas as usize {
            return Err(Status::resource_exhausted(
                format!(
                    "Insufficient disk capacity: need {} disks with {}GB each, found {} disks with sufficient space",
                    num_replicas,
                    capacity / (1024 * 1024 * 1024),
                    disks_with_capacity
                )
            ));
        }

        // For multi-replica, validate node distribution is possible
        if num_replicas > 1 {
            let unique_nodes: std::collections::HashSet<_> = available_disks.iter()
                .map(|disk| &disk.spec.node_id)
                .collect();

            if unique_nodes.len() < num_replicas as usize {
                return Err(Status::resource_exhausted(
                    format!(
                        "Cannot achieve node separation: need {} nodes, found {} nodes with available disks",
                        num_replicas,
                        unique_nodes.len()
                    )
                ));
            }
        }

        Ok(())
    }

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

    async fn create_replicas(&self, disks: &[SpdkDisk], capacity: i64, volume_id: &str) -> Result<Vec<Replica>, Status> {
        let mut replicas = Vec::new();

        for (i, disk) in disks.iter().enumerate() {
            let lvol_uuid = self.create_volume_lvol(disk, capacity, volume_id).await
                .map_err(|e| Status::internal(format!("Failed to create lvol: {}", e)))?;

            let node_ip = self.driver.get_node_ip(&disk.spec.node_id).await?;
            let nqn = format!("nqn.2025-05.io.spdk:volume-{}-replica-{}", volume_id, i);

            let replica = Replica {
                node: disk.spec.node_id.clone(),
                replica_type: "lvol".to_string(),
                pcie_addr: Some(disk.spec.pcie_addr.clone()),
                disk_ref: disk.metadata.name.clone().unwrap_or_default(),
                lvol_uuid: Some(lvol_uuid),
                nqn: Some(nqn),
                ip: Some(node_ip),
                port: Some(self.driver.nvmeof_target_port.to_string()),
                raid_member_index: i,
                health_status: ReplicaHealth::Healthy,
                raid_member_state: RaidMemberState::Online,
                ..Default::default()
            };

            replicas.push(replica);
        }

        Ok(replicas)
    }

    async fn create_volume_lvol(
        &self,
        disk: &SpdkDisk,
        size_bytes: i64,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let rpc_url = self.driver.get_rpc_url_for_node(&disk.spec.node_id).await?;
        let http_client = HttpClient::new();
        let lvs_name = format!("lvs_{}", disk.metadata.name.as_ref().unwrap());
        let lvol_name = format!("vol_{}", volume_id);

        let lvol_response = http_client
            .post(&rpc_url)
            .json(&json!({
                "method": "bdev_lvol_create",
                "params": {
                    "lvs_name": lvs_name,
                    "lvol_name": lvol_name,
                    "size": size_bytes,
                    "thin_provision": false,
                    "clear_method": "write_zeroes"
                }
            }))
            .send()
            .await?;

        if !lvol_response.status().is_success() {
            let error_text = lvol_response.text().await?;
            return Err(format!("Failed to create lvol: {}", error_text).into());
        }

        let lvol_info: serde_json::Value = lvol_response.json().await?;
        let lvol_uuid = lvol_info["result"]["uuid"]
            .as_str()
            .ok_or("Failed to get lvol UUID")?
            .to_string();

        Ok(lvol_uuid)
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
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_bdev_name = format!("{}/{}", lvs_name, lvol_uuid);
                
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
        // Check if hostname topology is enabled via environment variable
        let use_hostname_topology = std::env::var("USE_HOSTNAME_TOPOLOGY")
            .unwrap_or_default()
            .to_lowercase() == "true";
            
        let topology_key = if use_hostname_topology {
            "topology.kubernetes.io/hostname"
        } else {
            "flint.csi.storage.io/node"  // Safe for managed clusters
        };
        
        replicas.iter()
            .map(|replica| Topology {
                segments: [(
                    topology_key.to_string(),
                    replica.node.clone(),
                )].into_iter().collect(),
            })
            .collect()
    }

    fn build_volume_context(&self) -> std::collections::HashMap<String, String> {
        [
            ("storageType".to_string(), "spdk-nvmeof".to_string()),
            ("transport".to_string(), self.driver.nvmeof_transport.clone()),
            ("port".to_string(), self.driver.nvmeof_target_port.to_string())
        ].into_iter().collect()
    }
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
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_name = format!("{}/{}", lvs_name, lvol_uuid);

                let response = http_client
                    .post(&rpc_url)
                    .json(&json!({
                        "method": "bdev_lvol_resize",
                        "params": {
                            "name": lvol_name,
                            "size": new_capacity
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
