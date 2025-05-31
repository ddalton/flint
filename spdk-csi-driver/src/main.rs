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
    primary_lvol_uuid: Option<String>, // Primary logical volume UUID
    rebuild_in_progress: Option<ReplicationState>,
    write_ordering_enabled: bool,
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
    lvol_uuid: Option<String>, // Logical volume UUID for this replica
    health_status: ReplicaHealth,
    last_io_timestamp: Option<String>,
    write_sequence: u64,
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
    local_lvol_cache: Arc<Mutex<HashMap<String, String>>>, // volume_id -> lvol_uuid
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

        // Create logical volume
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

        // Cache the lvol UUID locally
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

        // Check if lvol store exists
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
            // Create new lvol store on the disk
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

        // Configure write ordering and consistency guarantees for lvol-based RAID
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

        // Parse StorageClass parameters
        let num_replicas = params
            .get("numReplicas")
            .and_then(|n| n.parse::<i32>().ok())
            .unwrap_or(2);
        let replica_nodes: Vec<String> = params
            .get("replicaNodes")
            .map(|s| s.split(',').map(String::from).collect())
            .unwrap_or_default();
        let _protocol = params.get("protocol").cloned().unwrap_or("tcp".to_string());
        let write_ordering = params
            .get("writeOrdering")
            .map(|s| s.parse::<bool>().unwrap_or(true))
            .unwrap_or(true);

        if replica_nodes.len() < num_replicas as usize {
            return Err(Status::invalid_argument("Insufficient replica nodes"));
        }

        // Select disks with sufficient free space and verify blobstore support
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

        // Sort by available space and I/O performance
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

        // Generate unique volume ID
        let volume_id = format!("raid1-{}", Uuid::new_v4());

        // Create lvols on each selected disk
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

            // Configure replica based on location
            if is_local {
                let pcie_addr = &disk.spec.pcie_addr;
                volume_context.insert(format!("nvmeAddr{}", i), pcie_addr.clone());
                volume_context.insert(format!("lvolBdev{}", i), lvol_bdev_name.clone());

                replicas.push(Replica {
                    node: node.clone(),
                    replica_type: "lvol".to_string(),
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

                // Export lvol as NVMe-oF target
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

        // Create RAID-1 configuration with lvol-based bdevs
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

        // Configure write ordering if enabled
        if write_ordering {
            self.setup_write_ordering(&volume_id, &lvol_bdev_names)
                .await
                .map_err(|e| Status::internal(format!("Failed to setup write ordering: {}", e)))?;
        }

        // Create SpdkVolume CRD
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
            },
        );

        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        crd_api
            .create(&PostParams::default(), &spdk_volume)
            .await
            .map_err(|e| Status::internal(format!("Failed to create SpdkVolume: {}", e)))?;

        // Update SpdkDisk status
        for (disk, lvol_uuid) in selected_disks.iter().zip(lvol_uuids.iter()) {
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

        // Add metadata to volume context
        volume_context.insert("replicaNodes".to_string(), replica_nodes.join(","));
        volume_context.insert("numReplicas".to_string(), num_replicas.to_string());
        volume_context.insert("writeOrdering".to_string(), write_ordering.to_string());
        volume_context.insert("primaryLvolUuid".to_string(), lvol_uuids[0].clone());
        volume_context.insert("storageType".to_string(), "lvol".to_string());

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

        // Get volume details from CRD
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = match crd_api.get(&volume_id).await {
            Ok(vol) => vol,
            Err(_) => {
                // Volume already deleted, return success
                return Ok(Response::new(DeleteVolumeResponse {}));
            }
        };

        // Delete RAID configuration
        self.delete_lvol_raid(&volume_id).await.ok();

        // Delete lvols from each replica
        for replica in &spdk_volume.spec.replicas {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_bdev_name = self.get_lvol_bdev_name(&lvs_name, lvol_uuid);
                self.delete_lvol(&lvol_bdev_name).await.ok();

                // Remove NVMe-oF export if applicable
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

impl SpdkCsiDriver {
    async fn export_lvol_as_nvmf(
        &self,
        lvol_bdev_name: &str,
        nqn: &str,
        ip: &str,
        port: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        // Create NVMe-oF subsystem
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

        // Add lvol bdev as namespace
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

        // Add listener
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

        // Create RAID-1 with lvol bdevs
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

        // Delete the logical volume
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

        // Parse volume context
        let context = req.volume_context;
        let write_ordering_enabled = context
            .get("writeOrdering")
            .and_then(|s| s.parse::<bool>().ok())
            .unwrap_or(true);

        // Get pod's node assignment
        let pod_node = get_pod_node(&self.kube_client)
            .await
            .map_err(|e| Status::internal(format!("Failed to get pod node: {}", e)))?;

        let pod_name = std::env::var("POD_NAME")
            .map_err(|e| Status::internal(format!("Failed to get POD_NAME: {}", e)))?;

        // Update SpdkVolume CRD with pod scheduling info
        self.update_replica_scheduling(&volume_id, &pod_node, &pod_name)
            .await
            .map_err(|e| Status::internal(format!("Failed to update replica scheduling: {}", e)))?;

        // Determine optimal device path
        let device_path = self
            .get_optimal_device_path(&volume_id, &pod_node, &context)
            .await
            .map_err(|e| Status::internal(format!("Failed to get device path: {}", e)))?;

        // Configure I/O path based on write ordering requirements
        if write_ordering_enabled {
            self.configure_ordered_io(&volume_id, &device_path)
                .await
                .map_err(|e| Status::internal(format!("Failed to configure ordered I/O: {}", e)))?;
        }

        // Stage volume based on capability
        if volume_capability.block.is_some() {
            // Block volume - no filesystem
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
            // Block volume - create symlink
            fs::symlink(&staging_path, &target_path)
                .await
                .map_err(|e| Status::internal(format!("Failed to symlink: {}", e)))?;
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

        // Cleanup I/O configuration
        self.cleanup_io_configuration(&volume_id).await.ok();

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

    async fn get_optimal_device_path(
        &self,
        volume_id: &str,
        pod_node: &str,
        context: &HashMap<String, String>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let storage_type = context.get("storageType").unwrap_or(&"lvol".to_string());

        // Check for local replica first (best performance)
        for i in 0..10 {
            // Support up to 10 replicas
            if let Some(nvme_addr) = context.get(&format!("nvmeAddr{}", i)) {
                if let Some(bdev_identifier) = context.get(&format!("{}Bdev{}", storage_type, i)) {
                    // Check if this replica is on the current node
                    if self.is_replica_local(volume_id, i, pod_node).await? {
                        // For local access, use the RAID device directly
                        return Ok(format!("/dev/spdk/{}", volume_id));
                    }
                }
            }
        }

        // Fall back to NVMe-oF access
        for i in 0..10 {
            if let (Some(nqn), Some(ip), Some(port)) = (
                context.get(&format!("nvmfNQN{}", i)),
                context.get(&format!("nvmfIP{}", i)),
                context.get(&format!("nvmfPort{}", i)),
            ) {
                connect_nvmf(nqn, ip, port)?;
                return find_nvmf_device(nqn);
            }
        }

        // Last resort: Use RAID device directly
        Ok(format!("/dev/spdk/{}", volume_id))
    }

    async fn is_replica_local(
        &self,
        volume_id: &str,
        replica_index: usize,
        pod_node: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = crd_api.get(volume_id).await?;
        
        if let Some(replica) = spdk_volume.spec.replicas.get(replica_index) {
            Ok(replica.node == pod_node)
        } else {
            Ok(false)
        }
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

fn connect_nvmf(nqn: &str, ip: &str, port: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new("nvme")
        .args(["connect", "-t", "tcp", "-n", nqn, "-a", ip, "-s", port])
        .status()?;

    // Wait for connection to establish
    std::thread::sleep(std::time::Duration::from_secs(2));
    Ok(())
}

fn disconnect_nvmf(nqn: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new("nvme")
        .args(["disconnect", "-n", nqn])
        .status()?;
    Ok(())
}

fn find_nvmf_device(nqn: &str) -> Result<String, Box<dyn std::error::Error>> {
    // Find NVMe-oF device by checking subsystem NQN
    for entry in std::fs::read_dir("/dev")? {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("nvme") && name.ends_with("n1") {
                let device_num = name.trim_start_matches("nvme").trim_end_matches("n1");
                let subsys_path = format!("/sys/class/nvme/nvme{}/subsysnqn", device_num);

                if let Ok(device_nqn) = std::fs::read_to_string(&subsys_path) {
                    if device_nqn.trim() == nqn {
                        return Ok(path.to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    Err("NVMe-oF device not found".into())
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

    let driver = SpdkCsiDriver {
        node_id,
        kube_client,
        spdk_rpc_url: std::env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        write_sequence_counter: Arc::new(Mutex::new(0)),
        local_lvol_cache: Arc::new(Mutex::new(HashMap::new())),
    };

    let addr = std::env::var("CSI_ENDPOINT")
        .unwrap_or("[::1]:50051".to_string())
        .parse()?;

    println!(
        "Starting SPDK CSI Driver on {} for node {}",
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