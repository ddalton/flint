// main.rs - Corrected SPDK CSI Driver with Runtime RAID Creation
use csi_driver::csi::csi::v1::*;
use csi_driver::csi::csi::v1::{
    controller_server::{Controller, ControllerServer},
    identity_server::{Identity, IdentityServer}, 
    node_server::{Node, NodeServer},
    PluginCapability, ProbeResponse,
};
use k8s_openapi::api::core::v1::{Node as k8sNode, Pod};
use kube::{
    api::{Api, ListParams, Patch, PatchParams, PostParams},
    Client, 
};
use reqwest::Client as HttpClient;
use serde_json::json;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;
use tonic::{transport::Server, Request, Response, Status};
use std::path::Path;
use spdk_csi_driver::models::*;
use chrono::Utc;

mod csi_snapshotter;

mod csi_driver {
    pub mod csi {
        tonic::include_proto!("csi");
    }
}

#[derive(Clone)]
struct SpdkCsiDriver {
    node_id: String,
    kube_client: Client,
    spdk_rpc_url: String,
    spdk_node_urls: Arc<Mutex<HashMap<String, String>>>,
    vhost_socket_base_path: String,
}

impl SpdkCsiDriver {
    /// Gets the SPDK RPC URL for a specific node by finding the 'node_agent' pod
    async fn get_rpc_url_for_node(&self, node_name: &str) -> Result<String, Status> {
        let mut cache = self.spdk_node_urls.lock().await;

        if let Some(url) = cache.get(node_name) {
            return Ok(url.clone());
        }

        println!("Discovering spdk-node-agent pod for node '{}'...", node_name);
        let pods_api: Api<Pod> = Api::all(self.kube_client.clone());
        let lp = ListParams::default().labels("app=spdk-node-agent");

        let pods = pods_api.list(&lp).await
            .map_err(|e| Status::internal(format!("Failed to list spdk-node-agent pods: {}", e)))?;

        for pod in pods {
            let pod_node = pod.spec.as_ref().and_then(|s| s.node_name.as_deref());
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref());

            if let (Some(p_node), Some(p_ip)) = (pod_node, pod_ip) {
                let url = format!("http://{}:5260", p_ip);
                cache.insert(p_node.to_string(), url);
            }
        }

        if let Some(url) = cache.get(node_name) {
            Ok(url.clone())
        } else {
            Err(Status::not_found(format!("Could not find spdk-node-agent pod on node '{}'", node_name)))
        }
    }

    /// Create individual lvols on different nodes (no RAID at creation time)
    async fn provision_multi_replica_volume(
        &self,
        volume_id: &str,
        capacity: i64,
        num_replicas: i32,
    ) -> Result<(SpdkVolume, String), Status> {
        let disks: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        let available_disks = disks
            .list(&ListParams::default())
            .await
            .map_err(|e| Status::internal(format!("Failed to list SpdkDisks: {}", e)))?
            .items
            .into_iter()
            .filter(|d| {
                if let Some(status) = &d.status {
                    status.healthy && status.blobstore_initialized && status.free_space >= capacity
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();

        if available_disks.len() < num_replicas as usize {
            return Err(Status::resource_exhausted("Insufficient healthy disks"));
        }

        // Select disks with node separation
        let mut selected_disks = Vec::new();
        let mut used_nodes = std::collections::HashSet::new();
        
        for disk in available_disks {
            if !used_nodes.contains(&disk.spec.node) && selected_disks.len() < num_replicas as usize {
                used_nodes.insert(disk.spec.node.clone());
                selected_disks.push(disk);
            }
        }

        if selected_disks.len() < num_replicas as usize {
            return Err(Status::resource_exhausted("Cannot achieve node separation for replicas"));
        }

        let mut replicas = Vec::new();

        // Create individual lvols on each selected disk
        for (i, disk) in selected_disks.iter().enumerate() {
            let lvol_uuid = self.create_volume_lvol(disk, capacity, volume_id).await
                .map_err(|e| Status::internal(format!("Failed to create lvol: {}", e)))?;

            let replica = Replica {
                node: disk.spec.node.clone(),
                replica_type: "lvol".to_string(),
                pcie_addr: Some(disk.spec.pcie_addr.clone()),
                disk_ref: disk.metadata.name.clone().unwrap_or_default(),
                lvol_uuid: Some(lvol_uuid.clone()),
                raid_member_index: i,
                health_status: ReplicaHealth::Healthy,
                raid_member_state: RaidMemberState::Online,
                ..Default::default()
            };

            replicas.push(replica);
        }

        let spdk_volume = SpdkVolume::new_with_metadata(
            volume_id,
            SpdkVolumeSpec {
                volume_id: volume_id.to_string(),
                size_bytes: capacity,
                num_replicas,
                replicas: replicas.clone(),
                raid_auto_rebuild: true,
                ..Default::default()
            },
        );

        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        crd_api.create(&PostParams::default(), &spdk_volume).await
            .map_err(|e| Status::internal(format!("Failed to create SpdkVolume CRD: {}", e)))?;

        // Update disk statuses
        for (disk, _) in selected_disks.iter().zip(replicas.iter()) {
            let disk_name = disk.metadata.name.clone().unwrap_or_default();
            let mut disk_status = disk.status.clone().unwrap_or_default();
            
            disk_status.free_space -= capacity;
            disk_status.used_space += capacity;
            disk_status.lvol_count += 1;
            disks.patch_status(&disk_name, &PatchParams::default(), 
                             &Patch::Merge(json!({ "status": disk_status })))
                .await
                .map_err(|e| Status::internal(format!("Failed to update SpdkDisk: {}", e)))?;
        }

        Ok((spdk_volume, volume_id.to_string()))
    }

    async fn create_volume_lvol(
        &self,
        disk: &SpdkDisk,
        size_bytes: i64,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let rpc_url = self.get_rpc_url_for_node(&disk.spec.node).await?;
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

    /// Create RAID bdev at staging time (when pod is scheduled)
    async fn create_runtime_raid(
        &self,
        volume_id: &str,
        spdk_volume: &SpdkVolume,
        current_node: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        
        println!("Creating runtime RAID1 for volume {} on node {}", volume_id, current_node);
        
        // Build bdev list for RAID creation
        let mut base_bdevs = Vec::new();
        
        for replica in &spdk_volume.spec.replicas {
            let bdev_name = if replica.node == current_node {
                // Local replica - direct lvol access
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_uuid = replica.lvol_uuid.as_ref()
                    .ok_or("Local replica missing lvol_uuid")?;
                format!("{}/{}", lvs_name, lvol_uuid)
            } else {
                // Remote replica - create NVMe-oF connection
                let nqn = format!("nqn.2025-05.io.spdk:lvol-{}", replica.lvol_uuid.as_ref().unwrap());
                let remote_ip = self.get_node_ip(&replica.node).await?;
                
                // Export from remote node
                self.export_replica_as_nvmf(&replica.node, replica, &nqn).await?;
                
                // Import on current node
                let nvmf_bdev_name = format!("nvmf_{}", replica.node.replace("-", "_"));
                self.import_nvmf_bdev(current_node, &nqn, &remote_ip, &nvmf_bdev_name).await?;
                
                nvmf_bdev_name
            };
            
            base_bdevs.push(bdev_name);
        }
        
        // Create RAID1 bdev on current node
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_create",
                "params": {
                    "name": volume_id,
                    "block_size": 4096,
                    "raid_level": 1,
                    "base_bdevs": base_bdevs,
                    "strip_size": 64,
                    "write_ordering": true,
                    "superblock": true,
                    "uuid": format!("raid-{}", uuid::Uuid::new_v4()),
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Failed to create runtime RAID1: {}", error_text).into());
        }

        println!("Successfully created runtime RAID1 for volume: {}", volume_id);
        Ok(())
    }

    async fn export_replica_as_nvmf(
        &self,
        node_name: &str,
        replica: &Replica,
        nqn: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rpc_url = self.get_rpc_url_for_node(node_name).await?;
        let http_client = HttpClient::new();
        let lvs_name = format!("lvs_{}", replica.disk_ref);
        let lvol_bdev_name = format!("{}/{}", lvs_name, replica.lvol_uuid.as_ref().unwrap());
        let node_ip = self.get_node_ip(node_name).await?;

        // Create NVMe-oF subsystem
        http_client
            .post(&rpc_url)
            .json(&json!({
                "method": "nvmf_create_subsystem",
                "params": {
                    "nqn": nqn,
                    "serial_number": format!("SPDK{}", uuid::Uuid::new_v4()),
                    "allow_any_host": true
                }
            }))
            .send()
            .await?;

        // Add namespace
        http_client
            .post(&rpc_url)
            .json(&json!({
                "method": "nvmf_subsystem_add_ns",
                "params": {
                    "nqn": nqn,
                    "bdev_name": lvol_bdev_name,
                    "nsid": 1
                }
            }))
            .send()
            .await?;

        // Add listener
        http_client
            .post(&rpc_url)
            .json(&json!({
                "method": "nvmf_subsystem_add_listener",
                "params": {
                    "nqn": nqn,
                    "trtype": "tcp",
                    "traddr": node_ip,
                    "trsvcid": "4420"
                }
            }))
            .send()
            .await?;

        Ok(())
    }

    async fn import_nvmf_bdev(
        &self,
        node_name: &str,
        nqn: &str,
        target_ip: &str,
        bdev_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rpc_url = self.get_rpc_url_for_node(node_name).await?;
        let http_client = HttpClient::new();

        let response = http_client
            .post(&rpc_url)
            .json(&json!({
                "method": "bdev_nvme_attach_controller",
                "params": {
                    "name": bdev_name,
                    "trtype": "tcp",
                    "traddr": target_ip,
                    "trsvcid": "4420",
                    "subnqn": nqn
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Failed to import NVMe-oF bdev: {}", error_text).into());
        }

        Ok(())
    }

    /// Cleanup runtime RAID when pod is terminated
    async fn cleanup_runtime_raid(
        &self,
        volume_id: &str,
        spdk_volume: &SpdkVolume,
        current_node: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        
        // Delete RAID bdev
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_delete",
                "params": { "name": volume_id }
            }))
            .send()
            .await
            .ok();

        // Cleanup NVMe-oF connections
        for replica in &spdk_volume.spec.replicas {
            if replica.node != current_node {
                let nqn = format!("nqn.2025-05.io.spdk:lvol-{}", replica.lvol_uuid.as_ref().unwrap());
                
                // Detach from current node
                let nvmf_bdev_name = format!("nvmf_{}", replica.node.replace("-", "_"));
                http_client
                    .post(&self.spdk_rpc_url)
                    .json(&json!({
                        "method": "bdev_nvme_detach_controller",
                        "params": { "name": nvmf_bdev_name }
                    }))
                    .send()
                    .await
                    .ok();

                // Delete subsystem from remote node
                let rpc_url = self.get_rpc_url_for_node(&replica.node).await?;
                http_client
                    .post(&rpc_url)
                    .json(&json!({
                        "method": "nvmf_delete_subsystem",
                        "params": { "nqn": nqn }
                    }))
                    .send()
                    .await
                    .ok();
            }
        }

        println!("Cleaned up runtime RAID1 for volume: {}", volume_id);
        Ok(())
    }

    async fn get_node_ip(&self, node_name: &str) -> Result<String, Status> {
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        
        let node = nodes_api.get(node_name).await
            .map_err(|e| Status::not_found(format!("Node {} not found: {}", node_name, e)))?;

        if let Some(status) = &node.status {
            if let Some(addresses) = &status.addresses {
                for address in addresses {
                    if address.type_ == "InternalIP" {
                        return Ok(address.address.clone());
                    }
                }
            }
        }

        Err(Status::not_found(format!("No IP address found for node {}", node_name)))
    }

    fn get_vhost_socket_path(&self, volume_id: &str) -> String {
        format!("{}/vhost_{}.sock", self.vhost_socket_base_path, volume_id)
    }

    async fn create_vhost_controller(
        &self,
        volume_id: &str,
        bdev_name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        let socket_path = self.get_vhost_socket_path(volume_id);
        let controller_name = format!("vhost_{}", volume_id);

        if let Some(parent) = Path::new(&socket_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Create vhost-nvme controller
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_create_nvme_controller",
                "params": {
                    "ctrlr": controller_name,
                    "io_queues": 4,
                    "cpumask": "0x1",
                    "max_namespaces": 32
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Failed to create vhost-nvme controller: {}", error_text).into());
        }

        // Add namespace
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_nvme_controller_add_ns",
                "params": {
                    "ctrlr": controller_name,
                    "bdev_name": bdev_name
                }
            }))
            .send()
            .await?;

        // Start controller
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_start_controller",
                "params": {
                    "ctrlr": controller_name,
                    "socket": socket_path
                }
            }))
            .send()
            .await?;

        Ok(socket_path)
    }

    async fn delete_vhost_controller(&self, volume_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        let controller_name = format!("vhost_{}", volume_id);
        let socket_path = self.get_vhost_socket_path(volume_id);

        // Remove namespace
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_nvme_controller_remove_ns",
                "params": { 
                    "ctrlr": controller_name,
                    "nsid": 1
                }
            }))
            .send()
            .await
            .ok();

        // Stop controller
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_stop_controller",
                "params": { "ctrlr": controller_name }
            }))
            .send()
            .await
            .ok();

        // Delete controller
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_delete_controller",
                "params": { "ctrlr": controller_name }
            }))
            .send()
            .await
            .ok();

        tokio::fs::remove_file(&socket_path).await.ok();
        Ok(())
    }
}

#[tonic::async_trait]
impl Controller for SpdkCsiDriver {
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_name = req.name.clone();
        let capacity = req.capacity_range.as_ref().map(|cr| cr.required_bytes).unwrap_or(0);
        
        if volume_name.is_empty() || capacity == 0 {
            return Err(Status::invalid_argument("Missing name or capacity"));
        }

        let num_replicas = req.parameters
            .get("numReplicas")
            .and_then(|n| n.parse::<i32>().ok())
            .unwrap_or(1);

        let (spdk_volume, new_volume_id) = if num_replicas > 1 {
            // Create multiple lvols (no RAID yet)
            self.provision_multi_replica_volume(&volume_name, capacity, num_replicas).await?
        } else {
            // Single replica case
            self.provision_single_replica_volume(&volume_name, capacity).await?
        };

        // Set topology to allow scheduling on any replica node
        let accessible_topology = spdk_volume.spec.replicas.iter()
            .map(|replica| Topology {
                segments: [(
                    "topology.kubernetes.io/hostname".to_string(),
                    replica.node.clone(),
                )].into_iter().collect(),
            })
            .collect();

        let volume = Volume {
            volume_id: new_volume_id.clone(),
            capacity_bytes: spdk_volume.spec.size_bytes,
            volume_context: [("storageType".to_string(), "spdk-lvol".to_string())].into_iter().collect(),
            content_source: req.volume_content_source,
            accessible_topology,
            ..Default::default()
        };

        Ok(Response::new(CreateVolumeResponse {
            volume: Some(volume),
        }))
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let volume_id = request.into_inner().volume_id;
        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Missing volume ID"));
        }

        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = match crd_api.get(&volume_id).await {
            Ok(vol) => vol,
            Err(_) => return Ok(Response::new(DeleteVolumeResponse {})),
        };

        // Delete individual lvols from each replica
        for replica in &spdk_volume.spec.replicas {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_bdev_name = format!("{}/{}", lvs_name, lvol_uuid);
                
                let rpc_url = self.get_rpc_url_for_node(&replica.node).await?;
                let http_client = HttpClient::new();
                
                http_client
                    .post(&rpc_url)
                    .json(&json!({
                        "method": "bdev_lvol_delete",
                        "params": { "name": lvol_bdev_name }
                    }))
                    .send()
                    .await
                    .ok();
            }
        }

        // Update disk statuses
        let disks: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        for replica in &spdk_volume.spec.replicas {
            if let Ok(disk) = disks.get(&replica.disk_ref).await {
                let mut disk_status = disk.status.unwrap_or_default();
                disk_status.free_space += spdk_volume.spec.size_bytes;
                disk_status.used_space -= spdk_volume.spec.size_bytes;
                disk_status.lvol_count = disk_status.lvol_count.saturating_sub(1);

                disks.patch_status(&replica.disk_ref, &PatchParams::default(), 
                                 &Patch::Merge(json!({ "status": disk_status })))
                    .await
                    .ok();
            }
        }

        // Delete CRD
        crd_api.delete(&volume_id, &Default::default()).await.ok();

        Ok(Response::new(DeleteVolumeResponse {}))
    }

    // Other controller methods remain the same...
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

        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        match volumes_api.get(&volume_id).await {
            Ok(_) => {
                let mut confirmed_capabilities = Vec::new();
                
                for capability in req.volume_capabilities {
                    let supported_access_mode = if let Some(access_mode) = &capability.access_mode {
                        let mode_value = access_mode.mode;
                        mode_value == (volume_capability::access_mode::Mode::SingleNodeWriter as i32) ||
                        mode_value == (volume_capability::access_mode::Mode::SingleNodeReaderOnly as i32) ||
                        mode_value == (volume_capability::access_mode::Mode::SingleNodeSingleWriter as i32)
                    } else {
                        false
                    };

                    let supported_access_type = matches!(
                        capability.access_type,
                        Some(volume_capability::AccessType::Block(_)) |
                        Some(volume_capability::AccessType::Mount(_))
                    );

                    if supported_access_mode && supported_access_type {
                        confirmed_capabilities.push(capability);
                    }
                }

                let is_empty = confirmed_capabilities.is_empty();

                Ok(Response::new(ValidateVolumeCapabilitiesResponse {
                    confirmed: if is_empty {
                        None
                    } else {
                        Some(validate_volume_capabilities_response::Confirmed { 
                            volume_capabilities: confirmed_capabilities,
                            volume_context: req.volume_context,
                            parameters: req.parameters,
                            mutable_parameters: std::collections::HashMap::new(),
                        })
                    },
                    message: if is_empty {
                        "Unsupported volume capabilities".to_string()
                    } else {
                        "Volume capabilities validated successfully".to_string()
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
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume_list = volumes_api.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list volumes: {}", e)))?;

        let entries = volume_list.items.iter().map(|volume| {
            list_volumes_response::Entry {
                volume: Some(Volume {
                    volume_id: volume.spec.volume_id.clone(),
                    capacity_bytes: volume.spec.size_bytes,
                    volume_context: [("storageType".to_string(), "spdk-lvol".to_string())].into_iter().collect(),
                    content_source: None,
                    accessible_topology: volume.spec.replicas.iter()
                        .map(|replica| Topology {
                            segments: [(
                                "topology.kubernetes.io/hostname".to_string(),
                                replica.node.clone(),
                            )].into_iter().collect(),
                        })
                        .collect(),
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
        let disks_api: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
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
        self.create_snapshot_impl(request).await
    }

    async fn delete_snapshot(
        &self,
        request: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        self.delete_snapshot_impl(request).await
    }

    async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        self.list_snapshots_impl(request).await
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

        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;

        if new_capacity <= volume.spec.size_bytes {
            return Err(Status::invalid_argument("New capacity must be larger than current capacity"));
        }

        // Expand lvols on each replica
        let mut failed_replicas = Vec::new();
        for replica in &volume.spec.replicas {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let rpc_url = self.get_rpc_url_for_node(&replica.node).await?;
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

        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume = volumes_api.get(&volume_id).await
            .map_err(|_| Status::not_found(format!("Volume {} not found", volume_id)))?;

        let accessible_topology = volume.spec.replicas.iter()
            .map(|replica| Topology {
                segments: [(
                    "topology.kubernetes.io/hostname".to_string(),
                    replica.node.clone(),
                )].into_iter().collect(),
            })
            .collect();

        let csi_volume = Volume {
            volume_id: volume.spec.volume_id.clone(),
            capacity_bytes: volume.spec.size_bytes,
            volume_context: [("storageType".to_string(), "spdk-lvol".to_string())].into_iter().collect(),
            content_source: None,
            accessible_topology,
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

#[tonic::async_trait]
impl Identity for SpdkCsiDriver {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: "spdk.csi.storage.io".to_string(),
            vendor_version: "1.0.0".to_string(),
            ..Default::default()
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities: vec![
                PluginCapability {
                    r#type: Some(plugin_capability::Type::Service(
                        plugin_capability::Service {
                            r#type: plugin_capability::service::Type::ControllerService as i32,
                        },
                    )),
                },
                PluginCapability {
                    r#type: Some(plugin_capability::Type::Service(
                        plugin_capability::Service {
                            r#type: plugin_capability::service::Type::VolumeAccessibilityConstraints as i32,
                        },
                    )),
                },
            ],
        }))
    }

    async fn probe(&self, _request: Request<ProbeRequest>) -> Result<Response<ProbeResponse>, Status> {
        Ok(Response::new(ProbeResponse {
            ready: Some(true),
        }))
    }
}

#[tonic::async_trait]
impl Node for SpdkCsiDriver {
    async fn node_stage_volume(
        &self,
        request: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let staging_target_path = req.staging_target_path;

        if volume_id.is_empty() || staging_target_path.is_empty() {
            return Err(Status::invalid_argument("Missing required parameters"));
        }

        let pod_node = get_pod_node(&self.kube_client)
            .await
            .map_err(|e| Status::internal(format!("Failed to get pod node: {}", e)))?;

        // Get volume information
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = crd_api.get(&volume_id).await
            .map_err(|_| Status::not_found(format!("SpdkVolume {} not found", volume_id)))?;

        println!("Staging volume {} on node {}", volume_id, pod_node);

        // Determine the bdev to expose
        let bdev_to_expose = if spdk_volume.spec.num_replicas > 1 {
            // Multi-replica: Create runtime RAID1
            self.create_runtime_raid(&volume_id, &spdk_volume, &pod_node).await
                .map_err(|e| Status::internal(format!("Failed to create runtime RAID1: {}", e)))?;
            
            volume_id.clone()
        } else {
            // Single replica: use lvol directly
            let replica = spdk_volume.spec.replicas.first()
                .ok_or_else(|| Status::internal("Volume has no replica information"))?;
            
            if replica.node != pod_node {
                return Err(Status::failed_precondition(
                    format!("Single replica volume can only be used on node {}", replica.node)
                ));
            }
            
            let lvs_name = format!("lvs_{}", replica.disk_ref);
            let lvol_uuid = replica.lvol_uuid.as_ref()
                .ok_or_else(|| Status::internal("Replica is missing lvol_uuid"))?;
            format!("{}/{}", lvs_name, lvol_uuid)
        };

        // Create vhost controller
        let socket_path = self.create_vhost_controller(&volume_id, &bdev_to_expose).await
            .map_err(|e| Status::internal(format!("Failed to create vhost controller: {}", e)))?;

        // Update volume status
        let patch = json!({
            "spec": { "vhost_socket": &socket_path },
            "status": { 
                "state": "Staged", 
                "last_checked": Utc::now().to_rfc3339(),
                "scheduled_node": pod_node,
            }
        });
        crd_api.patch(&volume_id, &PatchParams::default(), &Patch::Merge(patch)).await
            .map_err(|e| Status::internal(format!("Failed to patch SpdkVolume status: {}", e)))?;

        Ok(Response::new(NodeStageVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Missing volume ID"));
        }

        let pod_node = get_pod_node(&self.kube_client)
            .await
            .map_err(|e| Status::internal(format!("Failed to get pod node: {}", e)))?;

        // Get volume information
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = crd_api.get(&volume_id).await
            .map_err(|_| Status::not_found(format!("SpdkVolume {} not found", volume_id)))?;

        println!("Unstaging volume {} from node {}", volume_id, pod_node);

        // Delete vhost controller
        self.delete_vhost_controller(&volume_id).await.ok();

        // Cleanup runtime RAID if multi-replica
        if spdk_volume.spec.num_replicas > 1 {
            self.cleanup_runtime_raid(&volume_id, &spdk_volume, &pod_node).await.ok();
        }

        // Update volume status
        let patch = json!({
            "status": {
                "state": "Available",
                "scheduled_node": null,
                "last_checked": Utc::now().to_rfc3339(),
            }
        });
        crd_api.patch(&volume_id, &PatchParams::default(), &Patch::Merge(patch)).await.ok();

        Ok(Response::new(NodeUnstageVolumeResponse {}))
    }

    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let target_path = req.target_path;

        if volume_id.is_empty() || target_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and target path are required"));
        }

        // Get vhost socket path
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;

        let vhost_socket = volume.spec.vhost_socket
            .unwrap_or_else(|| format!("/var/lib/spdk-csi/sockets/vhost_{}.sock", volume_id));

        // Find vhost device
        let device_path = self.find_vhost_device_path(&vhost_socket).await?;

        // Handle block vs filesystem mounting
        if let Some(volume_capability) = req.volume_capability {
            match volume_capability.access_type {
                Some(volume_capability::AccessType::Block(_)) => {
                    // Create symlink for block device
                    if let Some(parent) = std::path::Path::new(&target_path).parent() {
                        tokio::fs::create_dir_all(parent).await
                            .map_err(|e| Status::internal(format!("Failed to create target directory: {}", e)))?;
                    }
                    
                    if std::path::Path::new(&target_path).exists() {
                        tokio::fs::remove_file(&target_path).await.ok();
                    }
                    
                    tokio::fs::symlink(&device_path, &target_path).await
                        .map_err(|e| Status::internal(format!("Failed to create device symlink: {}", e)))?;
                }
                Some(volume_capability::AccessType::Mount(mount)) => {
                    // Format and mount for filesystem access
                    let fs_type = if mount.fs_type.is_empty() { "ext4" } else { &mount.fs_type };
                    self.format_device_if_needed(&device_path, fs_type).await?;
                    
                    tokio::fs::create_dir_all(&target_path).await
                        .map_err(|e| Status::internal(format!("Failed to create mount point: {}", e)))?;
                    
                    let mut mount_cmd = tokio::process::Command::new("mount");
                    mount_cmd.arg("-t").arg(fs_type);
                    
                    for flag in &mount.mount_flags {
                        mount_cmd.arg("-o").arg(flag);
                    }
                    
                    if req.readonly {
                        mount_cmd.arg("-o").arg("ro");
                    }
                    
                    mount_cmd.arg(&device_path).arg(&target_path);
                    
                    let output = mount_cmd.output().await
                        .map_err(|e| Status::internal(format!("Failed to execute mount: {}", e)))?;
                    
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(Status::internal(format!("Mount failed: {}", stderr)));
                    }
                }
                None => {
                    return Err(Status::invalid_argument("Volume capability access type must be specified"));
                }
            }
        }

        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let target_path = req.target_path;

        if target_path.is_empty() {
            return Err(Status::invalid_argument("Missing target path"));
        }

        Command::new("umount").arg(&target_path).status().ok();
        fs::remove_file(&target_path).await.ok();
        fs::remove_dir(&target_path).await.ok();

        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_volume_stats(
        &self,
        request: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID is required"));
        }

        // Get volume statistics from SPDK
        let http_client = HttpClient::new();
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_get_iostat",
                "params": { "name": volume_id }
            }))
            .send()
            .await
            .map_err(|e| Status::internal(format!("Failed to get volume stats: {}", e)))?;

        let mut usage = Vec::new();
        let mut volume_condition = None;

        if response.status().is_success() {
            if let Ok(iostat) = response.json::<serde_json::Value>().await {
                if let Some(bdev_stats) = iostat["result"].as_array() {
                    for stat in bdev_stats {
                        if stat["name"].as_str() == Some(&volume_id) {
                            let num_blocks = stat["num_blocks"].as_u64().unwrap_or(0);
                            let block_size = stat["block_size"].as_u64().unwrap_or(512);
                            let total_bytes = num_blocks * block_size;

                            usage.push(VolumeUsage {
                                available: total_bytes as i64,
                                total: total_bytes as i64,
                                used: 0, // Could be calculated from I/O stats
                                unit: volume_usage::Unit::Bytes as i32,
                            });

                            let read_ios = stat["read_ios"].as_u64().unwrap_or(0);
                            let write_ios = stat["write_ios"].as_u64().unwrap_or(0);
                            let io_errors = stat["io_error"].as_u64().unwrap_or(0);

                            volume_condition = Some(VolumeCondition {
                                abnormal: io_errors > 0,
                                message: format!("Read I/Os: {}, Write I/Os: {}, Errors: {}", 
                                                read_ios, write_ios, io_errors),
                            });
                        }
                    }
                }
            }
        }

        if usage.is_empty() {
            usage.push(VolumeUsage {
                available: 0,
                total: 0,
                used: 0,
                unit: volume_usage::Unit::Bytes as i32,
            });
        }

        Ok(Response::new(NodeGetVolumeStatsResponse {
            usage,
            volume_condition,
        }))
    }

    async fn node_expand_volume(
        &self,
        request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let volume_path = req.volume_path;

        if volume_id.is_empty() || volume_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and volume path are required"));
        }

        let new_capacity = req.capacity_range
            .as_ref()
            .map(|cr| cr.required_bytes)
            .unwrap_or(0);

        if new_capacity <= 0 {
            return Err(Status::invalid_argument("New capacity must be greater than 0"));
        }

        // Expand filesystem if needed
        if let Some(volume_capability) = req.volume_capability {
            if let Some(volume_capability::AccessType::Mount(mount)) = volume_capability.access_type {
                let fs_type = if mount.fs_type.is_empty() { "ext4" } else { &mount.fs_type };
                
                match fs_type {
                    "ext4" | "ext3" | "ext2" => {
                        let output = tokio::process::Command::new("resize2fs")
                            .arg(&volume_path)
                            .output()
                            .await
                            .map_err(|e| Status::internal(format!("Failed to execute resize2fs: {}", e)))?;
                        
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            return Err(Status::internal(format!("resize2fs failed: {}", stderr)));
                        }
                    }
                    "xfs" => {
                        let output = tokio::process::Command::new("xfs_growfs")
                            .arg(&volume_path)
                            .output()
                            .await
                            .map_err(|e| Status::internal(format!("Failed to execute xfs_growfs: {}", e)))?;
                        
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            return Err(Status::internal(format!("xfs_growfs failed: {}", stderr)));
                        }
                    }
                    _ => {
                        return Err(Status::invalid_argument(
                            format!("Filesystem type {} not supported for expansion", fs_type)
                        ));
                    }
                }
            }
        }

        Ok(Response::new(NodeExpandVolumeResponse {
            capacity_bytes: new_capacity,
        }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        let node_id = std::env::var("NODE_NAME")
            .map_err(|_| Status::internal("NODE_NAME environment variable not set"))?;

        let max_volumes_per_node = std::env::var("MAX_VOLUMES_PER_NODE")
            .unwrap_or("100".to_string())
            .parse::<i64>()
            .unwrap_or(100);

        let accessible_topology = Some(Topology {
            segments: [("topology.kubernetes.io/hostname".to_string(), node_id.clone())]
                .into_iter().collect(),
        });

        Ok(Response::new(NodeGetInfoResponse {
            node_id,
            max_volumes_per_node,
            accessible_topology,
        }))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        Ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: vec![
                NodeServiceCapability {
                    r#type: Some(node_service_capability::Type::Rpc(
                        node_service_capability::Rpc {
                            r#type: node_service_capability::rpc::Type::StageUnstageVolume as i32,
                        },
                    )),
                },
                NodeServiceCapability {
                    r#type: Some(node_service_capability::Type::Rpc(
                        node_service_capability::Rpc {
                            r#type: node_service_capability::rpc::Type::GetVolumeStats as i32,
                        },
                    )),
                },
                NodeServiceCapability {
                    r#type: Some(node_service_capability::Type::Rpc(
                        node_service_capability::Rpc {
                            r#type: node_service_capability::rpc::Type::VolumeCondition as i32,
                        },
                    )),
                },
                NodeServiceCapability {
                    r#type: Some(node_service_capability::Type::Rpc(
                        node_service_capability::Rpc {
                            r#type: node_service_capability::rpc::Type::ExpandVolume as i32,
                        },
                    )),
                },
            ],
        }))
    }
}

impl SpdkCsiDriver {
    async fn find_vhost_device_path(&self, socket_path: &str) -> Result<String, Status> {
        let max_wait = std::time::Duration::from_secs(30);
        let start = std::time::Instant::now();

        while start.elapsed() < max_wait {
            if let Ok(entries) = tokio::fs::read_dir("/dev").await {
                let mut entries = entries;
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("nvme") && self.is_vhost_device(&path, socket_path).await {
                            return Ok(path.to_string_lossy().to_string());
                        }
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        Err(Status::deadline_exceeded(
            format!("Vhost device for socket {} did not appear within timeout", socket_path)
        ))
    }

    async fn is_vhost_device(&self, device_path: &std::path::Path, _socket_path: &str) -> bool {
        if let Ok(output) = tokio::process::Command::new("readlink")
            .arg("-f")
            .arg(device_path)
            .output()
            .await
        {
            let real_path = String::from_utf8_lossy(&output.stdout);
            return real_path.contains("vhost");
        }
        false
    }

    async fn format_device_if_needed(&self, device_path: &str, fs_type: &str) -> Result<(), Status> {
        // Check if device is already formatted
        let output = tokio::process::Command::new("blkid")
            .arg(device_path)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to check device format: {}", e)))?;

        if output.status.success() {
            return Ok(()); // Already formatted
        }

        // Format the device
        let format_cmd = match fs_type {
            "ext4" => vec!["mkfs.ext4", "-F", device_path],
            "ext3" => vec!["mkfs.ext3", "-F", device_path],
            "xfs" => vec!["mkfs.xfs", "-f", device_path],
            _ => return Err(Status::invalid_argument(format!("Unsupported filesystem type: {}", fs_type))),
        };

        let output = tokio::process::Command::new(format_cmd[0])
            .args(&format_cmd[1..])
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to format device: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Status::internal(format!("Device formatting failed: {}", stderr)));
        }

        Ok(())
    }

    /// Provision single replica volume (simplified case)
    async fn provision_single_replica_volume(
        &self,
        volume_name: &str,
        capacity: i64,
    ) -> Result<(SpdkVolume, String), Status> {
        let disks: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        
        let selected_disk = disks.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list SpdkDisks: {}", e)))?
            .items
            .into_iter()
            .find(|d| {
                d.status.as_ref().map_or(false, |s| 
                    s.healthy && s.blobstore_initialized && s.free_space >= capacity)
            })
            .ok_or_else(|| Status::resource_exhausted("No suitable disk found for single-replica volume"))?;

        let new_volume_id = volume_name.to_string();
        let lvol_uuid = self.create_volume_lvol(&selected_disk, capacity, &new_volume_id).await
            .map_err(|e| Status::internal(format!("Failed to create lvol: {}", e)))?;

        let replica = Replica {
            node: selected_disk.spec.node.clone(),
            replica_type: "lvol".to_string(),
            disk_ref: selected_disk.metadata.name.clone().unwrap_or_default(),
            lvol_uuid: Some(lvol_uuid.clone()),
            health_status: ReplicaHealth::Healthy,
            raid_member_state: RaidMemberState::Online,
            ..Default::default()
        };

        let spdk_volume = SpdkVolume::new_with_metadata(
            &new_volume_id,
            SpdkVolumeSpec {
                volume_id: new_volume_id.clone(),
                size_bytes: capacity,
                num_replicas: 1,
                replicas: vec![replica],
                ..Default::default()
            },
        );

        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        crd_api.create(&PostParams::default(), &spdk_volume).await
            .map_err(|e| Status::internal(format!("Failed to create SpdkVolume CRD: {}", e)))?;

        // Update disk status
        let disk_name = selected_disk.metadata.name.as_ref().unwrap();
        let mut disk_status = selected_disk.status.clone().unwrap_or_default();
        disk_status.free_space -= capacity;
        disk_status.used_space += capacity;
        disk_status.lvol_count += 1;
        disks.patch_status(disk_name, &PatchParams::default(), 
                          &Patch::Merge(json!({ "status": disk_status })))
            .await
            .map_err(|e| Status::internal(format!("Failed to update SpdkDisk status: {}", e)))?;

        Ok((spdk_volume, new_volume_id))
    }
}

// Helper functions
async fn get_pod_node(client: &Client) -> Result<String, Box<dyn std::error::Error>> {
    let pod_name = std::env::var("POD_NAME")?;
    let namespace = std::env::var("POD_NAMESPACE").unwrap_or("default".to_string());
    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);

    for attempt in 0..5 {
        match pods.get(&pod_name).await {
            Ok(pod) => {
                if let Some(node_name) = pod.spec.and_then(|s| s.node_name) {
                    return Ok(node_name);
                }
            }
            Err(e) => {
                if attempt == 4 {
                    return Err(format!("Failed to get pod after {} attempts: {}", attempt + 1, e).into());
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }

    Err("Pod node not assigned after retries".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kube_client = Client::try_default().await?;
    let node_id = std::env::var("NODE_ID")
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or("unknown-node".to_string()));
    let vhost_socket_base_path = std::env::var("VHOST_SOCKET_PATH")
        .unwrap_or("/var/lib/spdk-csi/sockets".to_string());
    
    // Ensure vhost socket directory exists
    tokio::fs::create_dir_all(&vhost_socket_base_path).await?;
    
    let driver = SpdkCsiDriver {
        node_id: node_id.clone(),
        kube_client,
        spdk_rpc_url: std::env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
        vhost_socket_base_path,
    };
    
    let mode = std::env::var("CSI_MODE").unwrap_or("all".to_string());
    let endpoint = std::env::var("CSI_ENDPOINT")
        .unwrap_or("unix:///csi/csi.sock".to_string());
    
    // Build the router with services
    let mut router = Server::builder()
        .add_service(IdentityServer::new(driver.clone()));
    
    if mode == "controller" || mode == "all" {
        println!("Starting in Controller mode...");
        router = router.add_service(ControllerServer::new(driver.clone()));
    }
    
    if mode == "node" || mode == "all" {
        println!("Starting in Node mode...");
        router = router.add_service(NodeServer::new(driver.clone()));
    }
    
    println!(
        "SPDK CSI Driver ('{}' mode) starting on {} for node {}",
        mode, endpoint, node_id
    );
    
    // Handle different endpoint types
    if endpoint.starts_with("unix://") {
        let socket_path = endpoint.trim_start_matches("unix://");
        
        // Remove existing socket file if it exists
        if std::path::Path::new(socket_path).exists() {
            std::fs::remove_file(socket_path)?;
        }
        
        // Create parent directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        // Use UnixListener for Unix domain socket
        use tokio::net::UnixListener;
        use tokio_stream::wrappers::UnixListenerStream;
        
        let listener = UnixListener::bind(socket_path)?;
        let stream = UnixListenerStream::new(listener);
        
        println!("Listening on unix socket: {}", socket_path);
        router.serve_with_incoming(stream).await?;
        
    } else if endpoint.starts_with("tcp://") {
        // Handle tcp:// prefix
        let addr = endpoint.trim_start_matches("tcp://").parse()?;
        router.serve(addr).await?;
        
    } else {
        // Assume it's a direct address (e.g., "0.0.0.0:50051")
        let addr = endpoint.parse()?;
        router.serve(addr).await?;
    }
    
    Ok(())
}
