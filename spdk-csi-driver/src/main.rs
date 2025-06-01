// Updated main.rs with vhost support
use csi_driver::csi::{
    controller_server::Controller, node_server::Node, ControllerGetCapabilitiesResponse,
    ControllerServiceCapability, CreateVolumeRequest, CreateVolumeResponse, DeleteVolumeRequest,
    DeleteVolumeResponse, Identity, NodeGetCapabilitiesResponse, NodePublishVolumeRequest,
    NodePublishVolumeResponse, NodeServiceCapability, NodeStageVolumeRequest,
    NodeStageVolumeResponse, NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse,
    NodeUnstageVolumeRequest, NodeUnstageVolumeResponse, Volume, VolumeCapability,
};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, ListParams, Patch, PatchParams, PostParams, ResourceExt},
    Client, CustomResource,
};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;
use tonic::{transport::Server, Request, Response, Status};
use uuid::Uuid;
use std::path::Path;

mod csi_driver {
    pub mod csi {
        tonic::include_proto!("csi.v1");
    }
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default)]
#[kube(
    group = "csi.spdk.io",
    version = "v1",
    kind = "SpdkVolume",
    plural = "spdkvolumes"
)]
#[kube(namespaced)]
#[kube(status = "SpdkVolumeStatus")]
struct SpdkVolumeSpec {
    volume_id: String,
    size_bytes: i64,
    num_replicas: i32,
    replicas: Vec<Replica>,
    primary_lvol_uuid: Option<String>,
    rebuild_in_progress: Option<ReplicationState>,
    write_ordering_enabled: bool,
    vhost_socket: Option<String>, // Path to vhost socket
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct Replica {
    node: String,
    #[serde(rename = "type")]
    replica_type: String,
    pcie_addr: Option<String>,
    nqn: Option<String>,
    ip: Option<String>,
    port: Option<String>,
    local_pod_scheduled: bool,
    pod_name: Option<String>,
    disk_ref: String,
    lvol_uuid: Option<String>,
    health_status: ReplicaHealth,
    last_io_timestamp: Option<String>,
    write_sequence: u64,
    vhost_socket: Option<String>, // For vhost-based access
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct ReplicationState {
    target_replica_index: usize,
    source_replica_index: usize,
    snapshot_id: String,
    copy_progress: f64,
    phase: String,
    started_at: String,
    catch_write_log: Vec<WriteOperation>,
    write_barrier_active: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct WriteOperation {
    offset: u64,
    length: u64,
    sequence: u64,
    timestamp: String,
    checksum: String,
    data_hash: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
enum ReplicaHealth {
    #[default]
    Healthy,
    Degraded,
    Failed,
    Rebuilding,
    Syncing,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct SpdkVolumeStatus {
    state: String,
    degraded: bool,
    last_checked: String,
    active_replicas: Vec<usize>,
    failed_replicas: Vec<usize>,
    write_sequence: u64,
    last_successful_write: Option<String>,
    vhost_device: Option<String>, // Path to vhost block device
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default)]
#[kube(
    group = "csi.spdk.io",
    version = "v1",
    kind = "SpdkDisk",
    plural = "spdkdisks"
)]
#[kube(namespaced)]
#[kube(status = "SpdkDiskStatus")]
struct SpdkDiskSpec {
    node: String,
    pcie_addr: String,
    capacity: i64,
    blobstore_uuid: Option<String>,
    nvme_controller_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct SpdkDiskStatus {
    total_capacity: i64,
    free_space: i64,
    used_space: i64,
    healthy: bool,
    last_checked: String,
    lvol_count: u32,
    blobstore_initialized: bool,
    io_stats: IoStatistics,
    lvs_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct IoStatistics {
    read_iops: u64,
    write_iops: u64,
    read_latency_us: u64,
    write_latency_us: u64,
    error_count: u64,
}

#[derive(Debug, Clone)]
struct SpdkCsiDriver {
    node_id: String,
    kube_client: Client,
    spdk_rpc_url: String,
    write_sequence_counter: Arc<Mutex<u64>>,
    local_lvol_cache: Arc<Mutex<HashMap<String, String>>>,
    vhost_socket_base_path: String, // Base path for vhost sockets
}

impl SpdkCsiDriver {
    async fn next_write_sequence(&self) -> u64 {
        let mut counter = self.write_sequence_counter.lock().await;
        *counter += 1;
        *counter
    }

    async fn create_volume_lvol(
        &self,
        disk: &SpdkDisk,
        size_bytes: i64,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        let lvs_name = self.ensure_lvol_store_initialized(disk).await?;

        let lvol_name = format!("vol_{}", volume_id);
        let lvol_response = http_client
            .post(&self.spdk_rpc_url)
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

        self.local_lvol_cache
            .lock()
            .await
            .insert(volume_id.to_string(), lvol_uuid.clone());

        Ok(lvol_uuid)
    }

    async fn ensure_lvol_store_initialized(
        &self,
        disk: &SpdkDisk,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        let lvs_name = format!("lvs_{}", disk.metadata.name.as_ref().unwrap());

        let check_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_lvol_get_lvstores"
            }))
            .send()
            .await?;

        let existing_stores: serde_json::Value = check_response.json().await?;
        let store_exists = existing_stores["result"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .any(|store| store["name"].as_str() == Some(&lvs_name));

        if !store_exists {
            let create_response = http_client
                .post(&self.spdk_rpc_url)
                .json(&json!({
                    "method": "bdev_lvol_create_lvstore",
                    "params": {
                        "bdev_name": format!("{}n1", disk.spec.nvme_controller_id.as_ref().unwrap_or(&"nvme0".to_string())),
                        "lvs_name": lvs_name,
                        "cluster_sz": 65536
                    }
                }))
                .send()
                .await?;

            if !create_response.status().is_success() {
                let error_text = create_response.text().await?;
                return Err(format!("Failed to create lvol store: {}", error_text).into());
            }
        }

        Ok(lvs_name)
    }

    async fn setup_write_ordering(
        &self,
        volume_id: &str,
        _lvol_bdev_names: &[String],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_configure_write_ordering",
                "params": {
                    "name": volume_id,
                    "ordering_mode": "strict",
                    "sync_mode": "barrier",
                    "timeout_ms": 5000,
                    "enable_snapshot_consistency": true
                }
            }))
            .send()
            .await?;

        Ok(())
    }

    fn get_lvol_bdev_name(&self, lvs_name: &str, lvol_uuid: &str) -> String {
        format!("{}/{}", lvs_name, lvol_uuid)
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

        // Ensure socket directory exists
        if let Some(parent) = Path::new(&socket_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Create vhost-blk controller
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_create_blk_controller",
                "params": {
                    "ctrlr": controller_name,
                    "dev_name": bdev_name,
                    "cpumask": "0x1",
                    "readonly": false,
                    "packed_ring": false
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Failed to create vhost controller: {}", error_text).into());
        }

        // Start the vhost controller with socket path
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

    async fn export_raid_as_vhost(
        &self,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        // For RAID volumes, export the RAID bdev as vhost
        self.create_vhost_controller(volume_id, volume_id).await
    }

    async fn export_lvol_as_nvmf(
        &self,
        lvol_bdev_name: &str,
        nqn: &str,
        ip: &str,
        port: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        // Create NVMe-oF subsystem for remote replicas
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_create_subsystem",
                "params": {
                    "nqn": nqn,
                    "serial_number": format!("SPDK{}", lvol_bdev_name.replace('/', "_")),
                    "allow_any_host": true
                }
            }))
            .send()
            .await?;

        http_client
            .post(&self.spdk_rpc_url)
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

        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_subsystem_add_listener",
                "params": {
                    "nqn": nqn,
                    "trtype": "tcp",
                    "traddr": ip,
                    "trsvcid": port
                }
            }))
            .send()
            .await?;

        Ok(())
    }

    async fn create_lvol_raid(
        &self,
        volume_id: &str,
        lvol_bdev_names: &[String],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_create",
                "params": {
                    "name": volume_id,
                    "block_size": 4096,
                    "raid_level": 1,
                    "base_bdevs": lvol_bdev_names,
                    "write_ordering": true,
                    "read_policy": "primary_first",
                    "rebuild_support": true
                }
            }))
            .send()
            .await?;

        Ok(())
    }

    async fn delete_vhost_controller(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        let controller_name = format!("vhost_{}", volume_id);
        let socket_path = self.get_vhost_socket_path(volume_id);

        // Stop vhost controller
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_stop_controller",
                "params": { "ctrlr": controller_name }
            }))
            .send()
            .await
            .ok();

        // Delete vhost controller
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_delete_controller",
                "params": { "ctrlr": controller_name }
            }))
            .send()
            .await
            .ok();

        // Clean up socket file
        tokio::fs::remove_file(&socket_path).await.ok();

        Ok(())
    }

    async fn delete_lvol_raid(&self, volume_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_delete",
                "params": { "name": volume_id }
            }))
            .send()
            .await?;

        Ok(())
    }

    async fn delete_lvol(
        &self,
        lvol_bdev_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_lvol_delete",
                "params": {
                    "name": lvol_bdev_name
                }
            }))
            .send()
            .await?;

        Ok(())
    }

    async fn unexport_nvmf_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_delete_subsystem",
                "params": { "nqn": nqn }
            }))
            .send()
            .await?;

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
        let name = req.name;
        let capacity = req.capacity_range.map(|cr| cr.required_bytes).unwrap_or(0);
        let params = req.parameters;

        if name.is_empty() || capacity == 0 {
            return Err(Status::invalid_argument("Missing name or capacity"));
        }

        let num_replicas = params
            .get("numReplicas")
            .and_then(|n| n.parse::<i32>().ok())
            .unwrap_or(2);
        let replica_nodes: Vec<String> = params
            .get("replicaNodes")
            .map(|s| s.split(',').map(String::from).collect())
            .unwrap_or_default();
        let write_ordering = params
            .get("writeOrdering")
            .map(|s| s.parse::<bool>().unwrap_or(true))
            .unwrap_or(true);

        if replica_nodes.len() < num_replicas as usize {
            return Err(Status::invalid_argument("Insufficient replica nodes"));
        }

        let disks: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        let available_disks = disks
            .list(&ListParams::default())
            .await
            .map_err(|e| Status::internal(format!("Failed to list SpdkDisks: {}", e)))?
            .items
            .into_iter()
            .filter(|d| {
                if let Some(status) = &d.status {
                    status.healthy 
                        && status.blobstore_initialized 
                        && status.free_space >= capacity
                        && replica_nodes.contains(&d.spec.node)
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();

        if available_disks.len() < num_replicas as usize {
            return Err(Status::resource_exhausted(
                "Insufficient healthy disks with blobstore support",
            ));
        }

        let mut selected_disks = available_disks;
        selected_disks.sort_by(|a, b| {
            let a_score = a.status.as_ref().unwrap().free_space as f64
                - a.status.as_ref().unwrap().io_stats.write_latency_us as f64;
            let b_score = b.status.as_ref().unwrap().free_space as f64
                - b.status.as_ref().unwrap().io_stats.write_latency_us as f64;
            b_score
                .partial_cmp(&a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let selected_disks = selected_disks
            .into_iter()
            .take(num_replicas as usize)
            .collect::<Vec<_>>();

        let volume_id = format!("raid1-{}", Uuid::new_v4());

        let mut lvol_uuids = Vec::new();
        let mut replicas = Vec::new();
        let mut volume_context = HashMap::new();

        for (i, disk) in selected_disks.iter().enumerate() {
            let lvol_uuid = self
                .create_volume_lvol(disk, capacity, &volume_id)
                .await
                .map_err(|e| Status::internal(format!("Failed to create lvol: {}", e)))?;

            lvol_uuids.push(lvol_uuid.clone());

            let node = &disk.spec.node;
            let is_local = node == &self.node_id;
            let lvs_name = format!("lvs_{}", disk.metadata.name.as_ref().unwrap());
            let lvol_bdev_name = self.get_lvol_bdev_name(&lvs_name, &lvol_uuid);

            if is_local {
                let pcie_addr = &disk.spec.pcie_addr;
                volume_context.insert(format!("nvmeAddr{}", i), pcie_addr.clone());
                volume_context.insert(format!("lvolBdev{}", i), lvol_bdev_name.clone());

                replicas.push(Replica {
                    node: node.clone(),
                    replica_type: "lvol".to_string(), // Still lvol - only access method changes
                    pcie_addr: Some(pcie_addr.clone()),
                    disk_ref: disk.metadata.name.clone().unwrap_or_default(),
                    lvol_uuid: Some(lvol_uuid.clone()),
                    health_status: ReplicaHealth::Healthy,
                    last_io_timestamp: Some(chrono::Utc::now().to_rfc3339()),
                    write_sequence: 0,
                    local_pod_scheduled: false,
                    ..Default::default()
                });
            } else {
                let nqn = format!(
                    "nqn.2025-05.io.spdk:lvol-{}",
                    lvol_bdev_name.replace('/', "-")
                );
                let ip = get_node_ip(node)
                    .await
                    .map_err(|e| Status::internal(format!("Failed to get node IP: {}", e)))?;

                self.export_lvol_as_nvmf(&lvol_bdev_name, &nqn, &ip, "4420")
                    .await
                    .map_err(|e| Status::internal(format!("Failed to export lvol: {}", e)))?;

                volume_context.insert(format!("nvmfNQN{}", i), nqn.clone());
                volume_context.insert(format!("nvmfIP{}", i), ip.clone());
                volume_context.insert(format!("nvmfPort{}", i), "4420".to_string());
                volume_context.insert(format!("lvolBdev{}", i), lvol_bdev_name.clone());

                replicas.push(Replica {
                    node: node.clone(),
                    replica_type: "nvmf".to_string(),
                    nqn: Some(nqn),
                    ip: Some(ip),
                    port: Some("4420".to_string()),
                    disk_ref: disk.metadata.name.clone().unwrap_or_default(),
                    lvol_uuid: Some(lvol_uuid.clone()),
                    health_status: ReplicaHealth::Healthy,
                    last_io_timestamp: Some(chrono::Utc::now().to_rfc3339()),
                    write_sequence: 0,
                    local_pod_scheduled: false,
                    ..Default::default()
                });
            }
        }

        let lvol_bdev_names: Vec<String> = selected_disks.iter()
            .zip(lvol_uuids.iter())
            .map(|(disk, uuid)| {
                let lvs_name = format!("lvs_{}", disk.metadata.name.as_ref().unwrap());
                self.get_lvol_bdev_name(&lvs_name, uuid)
            })
            .collect();

        self.create_lvol_raid(&volume_id, &lvol_bdev_names)
            .await
            .map_err(|e| Status::internal(format!("Failed to create RAID: {}", e)))?;

        if write_ordering {
            self.setup_write_ordering(&volume_id, &lvol_bdev_names)
                .await
                .map_err(|e| Status::internal(format!("Failed to setup write ordering: {}", e)))?;
        }

        let spdk_volume = SpdkVolume::new(
            &volume_id,
            SpdkVolumeSpec {
                volume_id: volume_id.clone(),
                size_bytes: capacity,
                num_replicas,
                replicas,
                primary_lvol_uuid: Some(lvol_uuids[0].clone()),
                rebuild_in_progress: None,
                write_ordering_enabled: write_ordering,
                vhost_socket: None, // Will be set during staging
            },
        );

        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        crd_api
            .create(&PostParams::default(), &spdk_volume)
            .await
            .map_err(|e| Status::internal(format!("Failed to create SpdkVolume: {}", e)))?;

        // Update SpdkDisk status
        for (disk, _lvol_uuid) in selected_disks.iter().zip(lvol_uuids.iter()) {
            let disk_name = disk.metadata.name.clone().unwrap_or_default();
            let mut disk_status = disk.status.clone().unwrap_or_default();
            
            disk_status.free_space -= capacity;
            disk_status.used_space += capacity;
            disk_status.lvol_count += 1;
            disk_status.last_checked = chrono::Utc::now().to_rfc3339();

            disks
                .patch_status(
                    &disk_name,
                    &PatchParams::default(),
                    &Patch::Merge(json!({
                        "status": disk_status
                    })),
                )
                .await
                .map_err(|e| Status::internal(format!("Failed to update SpdkDisk: {}", e)))?;
        }

        volume_context.insert("replicaNodes".to_string(), replica_nodes.join(","));
        volume_context.insert("numReplicas".to_string(), num_replicas.to_string());
        volume_context.insert("writeOrdering".to_string(), write_ordering.to_string());
        volume_context.insert("primaryLvolUuid".to_string(), lvol_uuids[0].clone());
        volume_context.insert("storageType".to_string(), "vhost-raid".to_string()); // Access method, not replica type
        volume_context.insert("accessMethod".to_string(), "vhost".to_string());
        volume_context.insert("vhostSocket".to_string(), format!("/var/lib/spdk-csi/sockets/vhost_{}.sock", volume_id));

        Ok(Response::new(CreateVolumeResponse {
            volume: Some(Volume {
                volume_id,
                capacity_bytes: capacity,
                volume_context,
                ..Default::default()
            }),
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
            Err(_) => {
                return Ok(Response::new(DeleteVolumeResponse {}));
            }
        };

        // Delete vhost controller
        self.delete_vhost_controller(&volume_id).await.ok();

        // Delete RAID configuration
        self.delete_lvol_raid(&volume_id).await.ok();

        // Delete lvols from each replica
        for replica in &spdk_volume.spec.replicas {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_bdev_name = self.get_lvol_bdev_name(&lvs_name, lvol_uuid);
                self.delete_lvol(&lvol_bdev_name).await.ok();

                if replica.replica_type == "nvmf" {
                    if let Some(nqn) = &replica.nqn {
                        self.unexport_nvmf_target(nqn).await.ok();
                    }
                }
            }
        }

        // Update SpdkDisk status
        let disks: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        for replica in &spdk_volume.spec.replicas {
            if let Ok(disk) = disks.get(&replica.disk_ref).await {
                let mut disk_status = disk.status.unwrap_or_default();
                disk_status.free_space += spdk_volume.spec.size_bytes;
                disk_status.used_space -= spdk_volume.spec.size_bytes;
                disk_status.lvol_count = disk_status.lvol_count.saturating_sub(1);
                disk_status.last_checked = chrono::Utc::now().to_rfc3339();

                disks
                    .patch_status(
                        &replica.disk_ref,
                        &PatchParams::default(),
                        &Patch::Merge(json!({
                            "status": disk_status
                        })),
                    )
                    .await
                    .ok();
            }
        }

        // Delete SpdkVolume CRD
        crd_api.delete(&volume_id, &Default::default()).await.ok();

        // Remove from local cache
        self.local_lvol_cache.lock().await.remove(&volume_id);

        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn controller_get_capabilities(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        use csi_driver::csi::controller_service_capability;

        Ok(Response::new(ControllerGetCapabilitiesResponse {
            capabilities: vec![
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::CreateDeleteVolume
                                as i32,
                        },
                    )),
                },
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::CreateDeleteSnapshot
                                as i32,
                        },
                    )),
                },
            ],
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
        let volume_capability = req
            .volume_capability
            .ok_or_else(|| Status::invalid_argument("Missing volume capability"))?;

        if volume_id.is_empty() || staging_target_path.is_empty() {
            return Err(Status::invalid_argument("Missing required parameters"));
        }

        let context = req.volume_context;
        let write_ordering_enabled = context
            .get("writeOrdering")
            .and_then(|s| s.parse::<bool>().ok())
            .unwrap_or(true);

        let pod_node = get_pod_node(&self.kube_client)
            .await
            .map_err(|e| Status::internal(format!("Failed to get pod node: {}", e)))?;

        let pod_name = std::env::var("POD_NAME")
            .map_err(|e| Status::internal(format!("Failed to get POD_NAME: {}", e)))?;

        // Update SpdkVolume CRD with pod scheduling info
        self.update_replica_scheduling(&volume_id, &pod_node, &pod_name)
            .await
            .map_err(|e| Status::internal(format!("Failed to update replica scheduling: {}", e)))?;

        // Create vhost controller for RAID volume
        let socket_path = self
            .export_raid_as_vhost(&volume_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to create vhost controller: {}", e)))?;

        // Update volume CRD with vhost socket path
        self.update_volume_vhost_socket(&volume_id, &socket_path)
            .await
            .map_err(|e| Status::internal(format!("Failed to update vhost socket: {}", e)))?;

        // Start QEMU vhost-user-blk device and get device path
        let device_path = self
            .start_vhost_user_blk(&socket_path, &volume_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to start vhost-user-blk: {}", e)))?;

        // Configure I/O path based on write ordering requirements
        if write_ordering_enabled {
            self.configure_ordered_io(&volume_id, &device_path)
                .await
                .map_err(|e| Status::internal(format!("Failed to configure ordered I/O: {}", e)))?;
        }

        // Stage volume based on capability
        if volume_capability.block.is_some() {
            // Block volume - no filesystem, just ensure device is available
            if !Path::new(&device_path).exists() {
                return Err(Status::internal("Vhost block device not found"));
            }
            return Ok(Response::new(NodeStageVolumeResponse {}));
        }

        // Filesystem volume
        let fs_type = volume_capability
            .mount
            .as_ref()
            .map(|m| m.fs_type.clone())
            .unwrap_or("ext4".to_string());

        // Check if already formatted
        if !is_device_formatted(&device_path)? {
            format_device(&device_path, &fs_type)
                .map_err(|e| Status::internal(format!("Failed to format device: {}", e)))?;
        }

        let mount_flags = volume_capability
            .mount
            .as_ref()
            .map(|m| m.mount_flags.clone())
            .unwrap_or_default();

        mount_device(&device_path, &staging_target_path, &fs_type, &mount_flags)
            .map_err(|e| Status::internal(format!("Failed to mount device: {}", e)))?;

        Ok(Response::new(NodeStageVolumeResponse {}))
    }

    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let staging_path = req.staging_target_path;
        let target_path = req.target_path;
        let volume_capability = req
            .volume_capability
            .ok_or_else(|| Status::invalid_argument("Missing volume capability"))?;

        if staging_path.is_empty() || target_path.is_empty() {
            return Err(Status::invalid_argument("Missing required parameters"));
        }

        // Create target directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(&target_path).parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                Status::internal(format!("Failed to create target directory: {}", e))
            })?;
        }

        if volume_capability.block.is_some() {
            // Block volume - create symlink to actual device
            let volume_id = req.volume_id;
            if let Ok(device_path) = self.get_vhost_device_path(&volume_id).await {
                fs::symlink(&device_path, &target_path)
                    .await
                    .map_err(|e| Status::internal(format!("Failed to symlink: {}", e)))?;
            } else {
                return Err(Status::internal("Failed to get vhost device path"));
            }
        } else {
            // Filesystem volume - bind mount
            let mount_flags = volume_capability
                .mount
                .as_ref()
                .map(|m| m.mount_flags.clone())
                .unwrap_or_default();

            let mut args = vec!["--bind"];
            args.extend(mount_flags.iter().map(|s| s.as_str()));
            args.extend([&staging_path, &target_path]);

            Command::new("mount")
                .args(&args)
                .status()
                .map_err(|e| Status::internal(format!("Failed to bind mount: {}", e)))?;
        }

        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let staging_target_path = req.staging_target_path;

        if staging_target_path.is_empty() {
            return Err(Status::invalid_argument("Missing staging path"));
        }

        // Unmount the staging path
        Command::new("umount")
            .arg(&staging_target_path)
            .status()
            .map_err(|e| Status::internal(format!("Failed to unmount: {}", e)))?;

        // Stop vhost-user-blk device
        self.stop_vhost_user_blk(&volume_id).await.ok();

        // Cleanup I/O configuration
        self.cleanup_io_configuration(&volume_id).await.ok();

        // Delete vhost controller
        self.delete_vhost_controller(&volume_id).await.ok();

        // Disconnect NVMe-oF if used
        self.cleanup_nvmf_connections(&volume_id).await.ok();

        Ok(Response::new(NodeUnstageVolumeResponse {}))
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

        // Unmount or remove symlink
        Command::new("umount").arg(&target_path).status().ok();

        fs::remove_file(&target_path).await.ok();
        fs::remove_dir(&target_path).await.ok();

        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<()>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        use csi_driver::csi::node_service_capability;

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
            ],
        }))
    }
}

impl SpdkCsiDriver {
    async fn update_replica_scheduling(
        &self,
        volume_id: &str,
        pod_node: &str,
        pod_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let mut spdk_volume = crd_api.get(volume_id).await?;
        let mut updated = false;

        for replica in spdk_volume.spec.replicas.iter_mut() {
            let is_local = replica.node == pod_node;
            if replica.local_pod_scheduled != is_local
                || replica.pod_name.as_ref() != Some(pod_name)
            {
                replica.local_pod_scheduled = is_local;
                replica.pod_name = if is_local {
                    Some(pod_name.to_string())
                } else {
                    None
                };
                replica.last_io_timestamp = Some(chrono::Utc::now().to_rfc3339());
                updated = true;
            }
        }

        if updated {
            crd_api
                .patch(
                    volume_id,
                    &PatchParams::default(),
                    &Patch::Merge(&spdk_volume),
                )
                .await?;
        }

        Ok(())
    }

    async fn update_volume_vhost_socket(
        &self,
        volume_id: &str,
        socket_path: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        
        crd_api
            .patch(
                volume_id,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "spec": {
                        "vhost_socket": socket_path
                    },
                    "status": {
                        "vhost_device": format!("/dev/vhost-{}", volume_id)
                    }
                })),
            )
            .await?;

        Ok(())
    }

    async fn start_vhost_user_blk(
        &self,
        socket_path: &str,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        // Create a unique device name
        let device_name = format!("vhost-{}", volume_id);
        let device_path = format!("/dev/{}", device_name);

        // Start vhost-user-blk process using QEMU's vhost-user-blk
        // This creates a userspace block device that interfaces with the vhost socket
        let mut cmd = Command::new("vhost-user-blk");
        cmd.args([
            "--socket-path", socket_path,
            "--blk-file", &device_path,
            "--read-only=off",
            "--num-queues=4",
            "--queue-size=128",
        ]);

        // Start the process in background
        let child = cmd.spawn()?;
        
        // Store process info for later cleanup
        self.store_vhost_process_info(volume_id, child.id()).await?;

        // Wait for device to appear
        let max_wait = 30; // 30 seconds
        for _ in 0..max_wait {
            if Path::new(&device_path).exists() {
                return Ok(device_path);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        Err(format!("Vhost device {} did not appear within {} seconds", device_path, max_wait).into())
    }

    async fn stop_vhost_user_blk(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(pid) = self.get_vhost_process_info(volume_id).await? {
            // Terminate the vhost-user-blk process
            Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()?;

            // Clean up process info
            self.remove_vhost_process_info(volume_id).await?;
        }

        Ok(())
    }

    async fn store_vhost_process_info(
        &self,
        volume_id: &str,
        pid: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Store PID in a file for later cleanup
        let pid_file = format!("/var/run/spdk-csi/vhost-{}.pid", volume_id);
        if let Some(parent) = Path::new(&pid_file).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&pid_file, pid.to_string()).await?;
        Ok(())
    }

    async fn get_vhost_process_info(
        &self,
        volume_id: &str,
    ) -> Result<Option<u32>, Box<dyn std::error::Error>> {
        let pid_file = format!("/var/run/spdk-csi/vhost-{}.pid", volume_id);
        if Path::new(&pid_file).exists() {
            let pid_str = tokio::fs::read_to_string(&pid_file).await?;
            Ok(pid_str.trim().parse().ok())
        } else {
            Ok(None)
        }
    }

    async fn remove_vhost_process_info(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let pid_file = format!("/var/run/spdk-csi/vhost-{}.pid", volume_id);
        tokio::fs::remove_file(&pid_file).await.ok();
        Ok(())
    }

    async fn get_vhost_device_path(
        &self,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = crd_api.get(volume_id).await?;
        
        if let Some(status) = &spdk_volume.status {
            if let Some(device_path) = &status.vhost_device {
                return Ok(device_path.clone());
            }
        }
        
        // Fallback to expected path
        Ok(format!("/dev/vhost-{}", volume_id))
    }

    async fn configure_ordered_io(
        &self,
        volume_id: &str,
        device_path: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        // Enable write ordering for consistency
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_set_qos_limit",
                "params": {
                    "name": volume_id,
                    "rw_ios_per_sec": 10000,
                    "rw_mbytes_per_sec": 100,
                    "write_ordering": true
                }
            }))
            .send()
            .await?;

        // Configure device queue depth for optimal performance
        if let Some(device_name) = std::path::Path::new(device_path).file_name() {
            if let Some(device_str) = device_name.to_str() {
                Command::new("sh")
                    .arg("-c")
                    .arg(format!(
                        "echo 32 > /sys/block/{}/queue/nr_requests",
                        device_str
                    ))
                    .status()
                    .ok();
            }
        }

        Ok(())
    }

    async fn cleanup_io_configuration(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        // Remove QoS limits
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_set_qos_limit",
                "params": {
                    "name": volume_id,
                    "rw_ios_per_sec": 0,
                    "rw_mbytes_per_sec": 0
                }
            }))
            .send()
            .await
            .ok();

        Ok(())
    }

    async fn cleanup_nvmf_connections(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Get volume details to find NVMe-oF connections
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        if let Ok(spdk_volume) = crd_api.get(volume_id).await {
            for replica in &spdk_volume.spec.replicas {
                if replica.replica_type == "nvmf" {
                    if let Some(nqn) = &replica.nqn {
                        disconnect_nvmf(nqn).ok();
                    }
                }
            }
        }

        Ok(())
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
                    return Err(
                        format!("Failed to get pod after {} attempts: {}", attempt + 1, e).into(),
                    );
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }

    Err("Pod node not assigned after retries".into())
}

async fn get_node_ip(node: &str) -> Result<String, Box<dyn std::error::Error>> {
    // In a real implementation, this would query the Kubernetes API
    // to get the node's internal IP address
    match node {
        "node-a" => Ok("192.168.1.100".to_string()),
        "node-b" => Ok("192.168.1.101".to_string()),
        "node-c" => Ok("192.168.1.102".to_string()),
        _ => Ok("192.168.1.100".to_string()), // Fallback
    }
}

fn disconnect_nvmf(nqn: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new("nvme")
        .args(["disconnect", "-n", nqn])
        .status()?;
    Ok(())
}

fn is_device_formatted(device: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let output = Command::new("blkid").arg(device).output()?;
    Ok(output.status.success() && !output.stdout.is_empty())
}

fn format_device(device: &str, fs_type: &str) -> Result<(), Box<dyn std::error::Error>> {
    let format_cmd = match fs_type {
        "ext4" => "mkfs.ext4",
        "xfs" => "mkfs.xfs",
        "btrfs" => "mkfs.btrfs",
        _ => "mkfs.ext4", // Default
    };

    let mut cmd = Command::new(format_cmd);
    cmd.arg("-F").arg(device); // -F to force formatting

    if fs_type == "ext4" {
        cmd.args(["-E", "lazy_itable_init=0,lazy_journal_init=0"]);
    }

    cmd.status()?;
    Ok(())
}

fn mount_device(
    device: &str,
    target: &str,
    fs_type: &str,
    mount_flags: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    // Create mount point if it doesn't exist
    std::fs::create_dir_all(target)?;

    let mut args = vec!["-t", fs_type];
    args.extend(mount_flags.iter().map(|s| s.as_str()));
    args.extend([device, target]);

    Command::new("mount").args(&args).status()?;
    Ok(())
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
        node_id,
        kube_client,
        spdk_rpc_url: std::env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        write_sequence_counter: Arc::new(Mutex::new(0)),
        local_lvol_cache: Arc::new(Mutex::new(HashMap::new())),
        vhost_socket_base_path,
    };

    let addr = std::env::var("CSI_ENDPOINT")
        .unwrap_or("[::1]:50051".to_string())
        .parse()?;

    println!(
        "Starting SPDK CSI Driver with vhost support on {} for node {}",
        addr, driver.node_id
    );

    Server::builder()
        .add_service(csi_driver::csi::node_server::NodeServer::new(
            driver.clone(),
        ))
        .add_service(csi_driver::csi::controller_server::ControllerServer::new(
            driver,
        ))
        .serve(addr)
        .await?;

    Ok(())
}