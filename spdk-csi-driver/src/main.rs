// Updated main.rs leveraging SPDK's native RAID1 capabilities
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

mod snapshot;
mod csi_snapshotter;
use snapshot::{SpdkSnapshot, SpdkSnapshotSpec, SpdkSnapshotStatus};

mod csi_driver {
    pub mod csi {
        tonic::include_proto!("csi.v1");
    }
}

// ... (All existing structs and impls up to the Controller trait remain the same) ...

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
    write_ordering_enabled: bool,
    vhost_socket: Option<String>,
    raid_auto_rebuild: bool, // Enable SPDK's automatic rebuild
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
    vhost_socket: Option<String>,
    raid_member_index: usize, // Position in RAID array
    raid_member_state: RaidMemberState,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
enum RaidMemberState {
    #[default]
    Online,
    Degraded,
    Failed,
    Rebuilding,
    Spare,
    Removing,
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
    vhost_device: Option<String>,
    // Native SPDK RAID status
    raid_status: Option<RaidStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct RaidStatus {
    raid_level: u32,
    state: String, // "online", "degraded", "failed"
    num_base_bdevs: u32,
    num_base_bdevs_discovered: u32,
    num_base_bdevs_operational: u32,
    base_bdevs_list: Vec<RaidMember>,
    rebuild_info: Option<RaidRebuildInfo>,
    superblock_version: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct RaidMember {
    name: String,
    state: String, // "online", "failed", "rebuilding"
    slot: u32,
    uuid: Option<String>,
    is_configured: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct RaidRebuildInfo {
    state: String, // "init", "running", "completed", "failed"
    target_slot: u32,
    source_slot: u32,
    blocks_remaining: u64,
    blocks_total: u64,
    progress_percentage: f64,
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
    spdk_node_urls: Arc<Mutex<HashMap<String, String>>>, // Map of node name to its RPC URL
    write_sequence_counter: Arc<Mutex<u64>>,
    local_lvol_cache: Arc<Mutex<HashMap<String, String>>>,
    vhost_socket_base_path: String,
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

    // Updated RAID creation with native SPDK RAID1 features
    async fn create_lvol_raid_with_native_rebuild(
        &self,
        volume_id: &str,
        lvol_bdev_names: &[String],
        enable_auto_rebuild: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        // Create RAID1 with enhanced configuration for native rebuild support
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_create",
                "params": {
                    "name": volume_id,
                    "block_size": 4096,
                    "raid_level": 1,
                    "base_bdevs": lvol_bdev_names,
                    "strip_size": 64, // KB
                    "write_ordering": true,
                    "read_policy": "primary_first",
                    // Native SPDK RAID1 rebuild configuration
                    "rebuild_support": true,
                    "auto_rebuild": enable_auto_rebuild,
                    "rebuild_on_add": true,
                    "rebuild_async": true,
                    "rebuild_verify": true,
                    // Superblock configuration for persistence
                    "superblock": true,
                    "uuid": format!("raid-{}", Uuid::new_v4()),
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Failed to create RAID1: {}", error_text).into());
        }

        // Configure RAID1-specific rebuild parameters
        self.configure_raid_rebuild_parameters(volume_id).await?;

        Ok(())
    }

    async fn configure_raid_rebuild_parameters(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        // Set rebuild throttling and priority
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_set_rebuild_config",
                "params": {
                    "name": volume_id,
                    "rebuild_priority": "high",
                    "rebuild_throttle_iops": 1000, // Limit rebuild I/O impact
                    "rebuild_verify_blocks": true,
                    "rebuild_background": true,
                }
            }))
            .send()
            .await
            .ok(); // This may not be supported in all SPDK versions

        Ok(())
    }

    // Get real-time RAID status from SPDK
    async fn get_raid_status(
        &self,
        volume_id: &str,
    ) -> Result<Option<RaidStatus>, Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_get_bdevs",
                "params": {
                    "category": "all"
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let raid_info: serde_json::Value = response.json().await?;
        
        if let Some(raid_bdevs) = raid_info["result"].as_array() {
            for raid_bdev in raid_bdevs {
                if let Some(name) = raid_bdev["name"].as_str() {
                    if name == volume_id {
                        return Ok(Some(self.parse_raid_status(raid_bdev)?));
                    }
                }
            }
        }
        
        Ok(None)
    }

    fn parse_raid_status(&self, raid_bdev: &serde_json::Value) -> Result<RaidStatus, Box<dyn std::error::Error>> {
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
        })
    }

    // Trigger manual rebuild using SPDK's native capabilities
    async fn trigger_raid_rebuild(
        &self,
        volume_id: &str,
        failed_slot: u32,
        replacement_bdev: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        // Remove failed member
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_remove_base_bdev",
                "params": {
                    "name": volume_id,
                    "slot": failed_slot
                }
            }))
            .send()
            .await?;

        // Add replacement - SPDK will automatically start rebuild
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_add_base_bdev",
                "params": {
                    "name": volume_id,
                    "base_bdev": replacement_bdev,
                    "slot": failed_slot
                }
            }))
            .send()
            .await?;

        // Explicitly start rebuild if needed (may auto-start)
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_start_rebuild",
                "params": {
                    "name": volume_id,
                    "slot": failed_slot
                }
            }))
            .send()
            .await
            .ok(); // May fail if rebuild auto-started

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

        if let Some(parent) = Path::new(&socket_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Create vhost-nvme controller (instead of vhost-blk for better performance)
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

        // Add namespace to the vhost-nvme controller
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "vhost_nvme_controller_add_ns",
                "params": {
                    "ctrlr": controller_name,
                    "bdev_name": bdev_name // Use RAID bdev as the namespace
                }
            }))
            .send()
            .await?;

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
        // For RAID volumes, export the RAID bdev as vhost-nvme
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

    async fn delete_vhost_controller(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        let controller_name = format!("vhost_{}", volume_id);
        let socket_path = self.get_vhost_socket_path(volume_id);

        // Remove namespace from vhost-nvme controller
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

    // --- Start of New Code ---
    /// Helper function to provision a new, empty RAID volume.
    /// This function contains the logic for creating lvols and the RAID bdev.
    /// It is used for both creating an empty volume and for creating the destination volume for a clone.
    async fn provision_new_raid_volume(
        &self,
        volume_id: &str,
        capacity: i64,
        params: &HashMap<String, String>,
    ) -> Result<(SpdkVolume, String), Status> {
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
        let auto_rebuild = params
            .get("autoRebuild")
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

        let new_volume_id = format!("raid1-{}", Uuid::new_v4());

        let mut lvol_uuids = Vec::new();
        let mut replicas = Vec::new();

        for (i, disk) in selected_disks.iter().enumerate() {
            let lvol_uuid = self
                .create_volume_lvol(disk, capacity, &new_volume_id)
                .await
                .map_err(|e| Status::internal(format!("Failed to create lvol: {}", e)))?;

            lvol_uuids.push(lvol_uuid.clone());

            let node = &disk.spec.node;
            let is_local = node == &self.node_id;
            let lvs_name = format!("lvs_{}", disk.metadata.name.as_ref().unwrap());
            let lvol_bdev_name = self.get_lvol_bdev_name(&lvs_name, &lvol_uuid);

            if is_local {
                replicas.push(Replica {
                    node: node.clone(),
                    replica_type: "lvol".to_string(),
                    pcie_addr: Some(disk.spec.pcie_addr.clone()),
                    disk_ref: disk.metadata.name.clone().unwrap_or_default(),
                    lvol_uuid: Some(lvol_uuid.clone()),
                    raid_member_index: i,
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

                replicas.push(Replica {
                    node: node.clone(),
                    replica_type: "nvmf".to_string(),
                    nqn: Some(nqn),
                    ip: Some(ip),
                    port: Some("4420".to_string()),
                    disk_ref: disk.metadata.name.clone().unwrap_or_default(),
                    lvol_uuid: Some(lvol_uuid.clone()),
                    raid_member_index: i,
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

        self.create_lvol_raid_with_native_rebuild(&new_volume_id, &lvol_bdev_names, auto_rebuild)
            .await
            .map_err(|e| Status::internal(format!("Failed to create RAID: {}", e)))?;

        let spdk_volume = SpdkVolume::new(
            &new_volume_id,
            SpdkVolumeSpec {
                volume_id: new_volume_id.clone(),
                size_bytes: capacity,
                num_replicas,
                replicas,
                primary_lvol_uuid: Some(lvol_uuids[0].clone()),
                write_ordering_enabled: write_ordering,
                vhost_socket: None,
                raid_auto_rebuild: auto_rebuild,
            },
        );

        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        crd_api
            .create(&PostParams::default(), &spdk_volume)
            .await
            .map_err(|e| Status::internal(format!("Failed to create SpdkVolume CRD: {}", e)))?;

        for (disk, _) in selected_disks.iter().zip(lvol_uuids.iter()) {
            let disk_name = disk.metadata.name.clone().unwrap_or_default();
            let mut disk_status = disk.status.clone().unwrap_or_default();
            
            disk_status.free_space -= capacity;
            disk_status.used_space += capacity;
            disk_status.lvol_count += 1;
            disks
                .patch_status(
                    &disk_name,
                    &PatchParams::default(),
                    &Patch::Merge(json!({ "status": disk_status })),
                )
                .await
                .map_err(|e| Status::internal(format!("Failed to update SpdkDisk: {}", e)))?;
        }

        Ok((spdk_volume, new_volume_id))
    }
}
// --- End of New Code ---

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

        let (spdk_volume, new_volume_id) = if let Some(source) = req.volume_content_source {
            // --- MODIFIED: Handle Create Volume From Decentralized Snapshot ---
            if let Some(snapshot_source) = source.snapshot {
                let snapshot_id = snapshot_source.snapshot_id;
                
                // 1. Get source SpdkSnapshot CRD
                let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), "default");
                let snapshot_crd = snapshots_api.get(&snapshot_id).await
                    .map_err(|_| Status::not_found(format!("Source snapshot {} not found", snapshot_id)))?;

                // 2. Select a source replica snapshot to clone from. Any healthy one will do.
                let source_replica_snapshot = snapshot_crd.spec.replica_snapshots.first()
                    .ok_or_else(|| Status::not_found(format!("No available replica snapshots found for snapshot {}", snapshot_id)))?;
                
                let source_bdev_name = &source_replica_snapshot.spdk_snapshot_lvol;
                let source_node_name = &source_replica_snapshot.node_name;
                
                // 3. Provision the new destination RAID volume scaffolding
                let (dest_spdk_volume, dest_raid_bdev_name) = self.provision_new_raid_volume(&volume_name, capacity, &req.parameters).await?;
                
                // 4. Perform the copy from the snapshot bdev to the new RAID bdev.
                // The copy command should be sent to the node where the source snapshot bdev exists.
                println!("Cloning from snapshot bdev '{}' on node '{}' to new volume '{}'", source_bdev_name, source_node_name, dest_raid_bdev_name);
                let source_node_rpc_url = self.get_rpc_url_for_node(source_node_name).await?;
                let http_client = HttpClient::new();
                let copy_response = http_client.post(&source_node_rpc_url)
                    .json(&json!({
                        "method": "bdev_copy",
                        "params": {
                            "src_bdev": source_bdev_name,
                            "dst_bdev": dest_raid_bdev_name,
                        }
                    }))
                    .send()
                    .await
                    .map_err(|e| Status::internal(format!("SPDK bdev_copy RPC call failed: {}", e)))?;

                if !copy_response.status().is_success() {
                    let err_text = copy_response.text().await.unwrap_or_default();
                    // Attempt to clean up the newly created volume if copy fails
                    self.delete_volume(Request::new(DeleteVolumeRequest{ volume_id: dest_spdk_volume.spec.volume_id.clone() })).await.ok();
                    return Err(Status::internal(format!("Failed to copy data from snapshot: {}", err_text)));
                }

                (dest_spdk_volume, dest_spdk_volume.spec.volume_id.clone())
            } else {
                return Err(Status::invalid_argument("Unsupported volume content source type"));
            }
        } else {
            // --- Original Path: Create an empty volume ---
            self.provision_new_raid_volume(&volume_name, capacity, &req.parameters).await?
        };

        let mut volume_context = HashMap::new();
        volume_context.insert("storageType".to_string(), "vhost-raid".to_string());

        Ok(Response::new(CreateVolumeResponse {
            volume: Some(Volume {
                volume_id: new_volume_id,
                capacity_bytes: spdk_volume.spec.size_bytes,
                volume_context,
                content_source: req.volume_content_source,
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
                // ADD a capability for creating and deleting snapshots
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::CreateDeleteSnapshot
                                as i32,
                        },
                    )),
                },
                // ADD a capability for listing snapshots
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::ListSnapshots
                                as i32,
                        },
                    )),
                },
                // Add capability for cloning
                ControllerServiceCapability {
                    r#type: Some(controller_service_capability::Type::Rpc(
                        controller_service_capability::Rpc {
                            r#type: controller_service_capability::rpc::Type::CloneVolume
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
        let auto_rebuild = context
            .get("autoRebuild")
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

        // Create vhost-nvme controller for RAID volume
        let socket_path = self
            .export_raid_as_vhost(&volume_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to create vhost controller: {}", e)))?;

        // Update volume CRD with vhost socket path and current RAID status
        self.update_volume_with_raid_status(&volume_id, &socket_path)
            .await
            .map_err(|e| Status::internal(format!("Failed to update volume status: {}", e)))?;

        // Start QEMU vhost-user-nvme device and get device path
        let device_path = self
            .start_vhost_user_nvme(&socket_path, &volume_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to start vhost-user-nvme: {}", e)))?;

        // Stage volume based on capability
        if volume_capability.block.is_some() {
            if !Path::new(&device_path).exists() {
                return Err(Status::internal("Vhost NVMe device not found"));
            }
            return Ok(Response::new(NodeStageVolumeResponse {}));
        }

        // Filesystem volume
        let fs_type = volume_capability
            .mount
            .as_ref()
            .map(|m| m.fs_type.clone())
            .unwrap_or("ext4".to_string());

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

        if let Some(parent) = std::path::Path::new(&target_path).parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                Status::internal(format!("Failed to create target directory: {}", e))
            })?;
        }

        if volume_capability.block.is_some() {
            let volume_id = req.volume_id;
            if let Ok(device_path) = self.get_vhost_device_path(&volume_id).await {
                fs::symlink(&device_path, &target_path)
                    .await
                    .map_err(|e| Status::internal(format!("Failed to symlink: {}", e)))?;
            } else {
                return Err(Status::internal("Failed to get vhost device path"));
            }
        } else {
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

        Command::new("umount")
            .arg(&staging_target_path)
            .status()
            .map_err(|e| Status::internal(format!("Failed to unmount: {}", e)))?;

        self.stop_vhost_user_nvme(&volume_id).await.ok();
        self.delete_vhost_controller(&volume_id).await.ok();
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
    /// Gets the SPDK RPC URL for a specific node by finding the 'node_agent' pod
    /// running on that node and returning its IP-based URL.
    /// It uses a cache to avoid repeated lookups.
    pub async fn get_rpc_url_for_node(&self, node_name: &str) -> Result<String, Status> {
        let mut cache = self.spdk_node_urls.lock().await;

        // 1. Check cache first
        if let Some(url) = cache.get(node_name) {
            return Ok(url.clone());
        }

        // 2. If not in cache, query the Kubernetes API.
        // Assumes node_agent pods are labeled with 'app=spdk-node-agent'.
        println!("Cache miss for node '{}'. Discovering spdk-node-agent pod...", node_name);
        let pods_api: Api<Pod> = Api::all(self.kube_client.clone());
        let lp = ListParams::default().labels("app=spdk-node-agent");

        let pods = pods_api.list(&lp).await
            .map_err(|e| Status::internal(format!("Failed to list spdk-node-agent pods: {}", e)))?;

        for pod in pods {
            let pod_node = pod.spec.as_ref().and_then(|s| s.node_name.as_deref());
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref());

            if let (Some(p_node), Some(p_ip)) = (pod_node, pod_ip) {
                let url = format!("http://{}:5260", p_ip);
                // Update cache for the found pod
                cache.insert(p_node.to_string(), url);
            }
        }

        // 3. Try the cache again after discovery
        if let Some(url) = cache.get(node_name) {
            Ok(url.clone())
        } else {
            Err(Status::not_found(format!("Could not find a running spdk-node-agent pod on node '{}'", node_name)))
        }
    }

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

    async fn update_volume_with_raid_status(
        &self,
        volume_id: &str,
        socket_path: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        
        // Get current RAID status from SPDK
        let raid_status = self.get_raid_status(volume_id).await?;
        
        let mut patch_data = json!({
            "spec": {
                "vhost_socket": socket_path
            },
            "status": {
                "vhost_device": format!("/dev/nvme-vhost-{}", volume_id),
                "last_checked": chrono::Utc::now().to_rfc3339()
            }
        });

        // Include RAID status if available
        if let Some(raid_info) = raid_status {
            patch_data["status"]["raid_status"] = json!(raid_info);
            
            // Update volume state based on RAID status
            let volume_state = match raid_info.state.as_str() {
                "online" => "Healthy",
                "degraded" => "Degraded", 
                "failed" | "broken" => "Failed",
                _ => "Unknown",
            };
            patch_data["status"]["state"] = json!(volume_state);
            patch_data["status"]["degraded"] = json!(raid_info.state == "degraded");
        }
        
        crd_api
            .patch(
                volume_id,
                &PatchParams::default(),
                &Patch::Merge(patch_data),
            )
            .await?;

        Ok(())
    }

    async fn start_vhost_user_nvme(
        &self,
        socket_path: &str,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let device_name = format!("nvme-vhost-{}", volume_id);
        let device_path = format!("/dev/{}", device_name);

        // Start vhost-user-nvme process using QEMU's vhost-user-nvme
        let mut cmd = Command::new("vhost-user-nvme");
        cmd.args([
            "--socket-path", socket_path,
            "--nvme-device", &device_path,
            "--read-only=off",
            "--num-queues=4",
            "--queue-size=256",
            "--max-ioqpairs=4",
        ]);

        let child = cmd.spawn()?;
        self.store_vhost_process_info(volume_id, child.id()).await?;

        // Wait for NVMe device to appear
        let max_wait = 30;
        for _ in 0..max_wait {
            if Path::new(&device_path).exists() {
                return Ok(device_path);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        Err(format!("Vhost NVMe device {} did not appear within {} seconds", device_path, max_wait).into())
    }

    async fn stop_vhost_user_nvme(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(pid) = self.get_vhost_process_info(volume_id).await? {
            Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()?;

            self.remove_vhost_process_info(volume_id).await?;
        }

        Ok(())
    }

    async fn store_vhost_process_info(
        &self,
        volume_id: &str,
        pid: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
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
        
        Ok(format!("/dev/nvme-vhost-{}", volume_id))
    }

    async fn cleanup_nvmf_connections(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
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
    match node {
        "node-a" => Ok("192.168.1.100".to_string()),
        "node-b" => Ok("192.168.1.101".to_string()),
        "node-c" => Ok("192.168.1.102".to_string()),
        _ => Ok("192.168.1.100".to_string()),
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
        _ => "mkfs.ext4",
    };

    let mut cmd = Command::new(format_cmd);
    cmd.arg("-F").arg(device);

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
        spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
        write_sequence_counter: Arc::new(Mutex::new(0)),
        local_lvol_cache: Arc::new(Mutex::new(HashMap::new())),
        vhost_socket_base_path,
    };

    let addr = std::env::var("CSI_ENDPOINT")
        .unwrap_or("[::1]:50051".to_string())
        .parse()?;

    println!(
        "Starting SPDK CSI Driver with native RAID1 support on {} for node {}",
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
