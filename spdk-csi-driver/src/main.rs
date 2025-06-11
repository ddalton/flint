// Updated main.rs leveraging SPDK's native RAID1 capabilities
use csi_driver::csi::csi::v1::*;
use csi_driver::csi::csi::v1::{
    controller_server::{Controller, ControllerServer},
    identity_server::{Identity, IdentityServer}, 
    node_server::{Node, NodeServer},
    PluginCapability, ProbeResponse,
};
use k8s_openapi::api::core::v1::Pod;
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
use uuid::Uuid;
use std::path::Path;
use spdk_csi_driver::models::*;
use chrono::Utc;

mod csi_snapshotter;

mod csi_driver {
    pub mod csi {
        tonic::include_proto!("csi");
    }
}

// Add new scheduling policy enum
#[derive(Debug, Clone, Default)]
pub enum SchedulingPolicy {
    #[default]
    AnyNode,           // Current behavior - maximum availability
    PreferReplicas,    // Prefer replica nodes, fallback to any
    RequireReplicas,   // Only schedule on replica nodes
}

impl std::str::FromStr for SchedulingPolicy {
    type Err = String;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "any" | "any-node" => Ok(SchedulingPolicy::AnyNode),
            "prefer" | "prefer-replicas" => Ok(SchedulingPolicy::PreferReplicas),
            "require" | "require-replicas" => Ok(SchedulingPolicy::RequireReplicas),
            _ => Err(format!("Invalid scheduling policy: {}", s))
        }
    }
}

// Example usage with Storage Classes for different optimization levels
/*
# High Availability Priority (Default)
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: spdk-raid1-ha
provisioner: spdk.csi.storage.io
parameters:
  numReplicas: "2"
  schedulingPolicy: "any-node"    # Maximum availability
  autoRebuild: "true"

---
# Performance Optimized with Locality Preference
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: spdk-raid1-perf
provisioner: spdk.csi.storage.io
parameters:
  numReplicas: "2"
  schedulingPolicy: "prefer-replicas"  # Prefer local replicas
  autoRebuild: "true"
  replicaNodes: "node-a,node-b"       # Optional: specify preferred nodes

---
# Latency Critical (Local Required)
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: spdk-raid1-local
provisioner: spdk.csi.storage.io
parameters:
  numReplicas: "2"
  schedulingPolicy: "require-replicas"  # Must have local replica
  autoRebuild: "true"
*/

#[derive(Clone)]
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

    // Enhanced RAID creation with local replica optimization
    async fn create_lvol_raid_with_local_optimization(
        &self,
        volume_id: &str,
        spdk_volume: &SpdkVolume,
        current_node: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        
        println!("Creating RAID1 with local optimization for volume {} on node {}", volume_id, current_node);
        
        // Get optimal replica ordering for read performance
        let (ordered_bdevs, read_policy, local_replica_slot) = self.get_optimal_replica_ordering(
            &spdk_volume.spec.replicas,
            current_node,
        ).await?;
        
        self.log_volume_scheduling_info(volume_id, current_node, &spdk_volume.spec.replicas).await;
        
        println!("RAID1 bdev order: {:?}", ordered_bdevs);
        println!("Using read policy: {} (local replica at slot: {:?})", read_policy, local_replica_slot);
        
        // Create RAID1 with enhanced configuration for native rebuild support
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_create",
                "params": {
                    "name": volume_id,
                    "block_size": 4096,
                    "raid_level": 1,
                    "base_bdevs": ordered_bdevs,  // Local replica first!
                    "strip_size": 64, // KB
                    "write_ordering": true,
                    "read_policy": read_policy,   // Optimized for locality
                    // Native SPDK RAID1 rebuild configuration
                    "rebuild_support": true,
                    "auto_rebuild": spdk_volume.spec.raid_auto_rebuild,
                    "rebuild_on_add": true,
                    "rebuild_async": true,
                    "rebuild_verify": true,
                    // Superblock configuration for persistence
                    "superblock": true,
                    "uuid": format!("raid-{}", uuid::Uuid::new_v4()),
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Failed to create optimized RAID1: {}", error_text).into());
        }

        // Configure RAID1-specific rebuild parameters
        self.configure_raid_rebuild_parameters(volume_id).await?;

        println!("Successfully created locality-optimized RAID1 for volume: {}", volume_id);
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

    // Enhanced method to get optimal replica ordering for RAID creation
    async fn get_optimal_replica_ordering(
        &self,
        replicas: &[Replica],
        current_node: &str,
    ) -> Result<(Vec<String>, String, Option<u32>), Box<dyn std::error::Error>> {
        let mut local_bdevs = Vec::new();
        let mut remote_bdevs = Vec::new();
        let mut local_replica_slot = None;
        
        for (index, replica) in replicas.iter().enumerate() {
            let bdev_name = if replica.node == current_node {
                // Local replica - direct lvol access
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_uuid = replica.lvol_uuid.as_ref()
                    .ok_or("Local replica missing lvol_uuid")?;
                local_replica_slot = Some(index as u32);
                format!("{}/{}", lvs_name, lvol_uuid)
            } else {
                // Remote replica - construct NVMe-oF bdev name
                let lvol_uuid = replica.lvol_uuid.as_ref()
                    .ok_or("Remote replica missing lvol_uuid")?;
                format!("nvmf_{}_{}", replica.node.replace("-", "_"), lvol_uuid)
            };
            
            if replica.node == current_node {
                println!("Found local replica: {} on node {}", bdev_name, current_node);
                local_bdevs.push(bdev_name);
            } else {
                println!("Found remote replica: {} on node {}", bdev_name, replica.node);
                remote_bdevs.push(bdev_name);
            }
        }
        
        // Order: local replica first (for primary_first read optimization)
        let mut ordered_bdevs = local_bdevs;
        ordered_bdevs.extend(remote_bdevs);
        
        // Choose optimal read policy based on replica locality
        let read_policy = if local_replica_slot.is_some() {
            "primary_first".to_string()  // Local replica will be primary - all reads go local!
        } else {
            "queue_depth".to_string()    // No local replica, use load balancing
        };
        
        Ok((ordered_bdevs, read_policy, local_replica_slot))
    }

    // Update RAID read policy dynamically when pod moves
    async fn update_raid_read_policy(
        &self,
        volume_id: &str,
        current_node: &str,
        replicas: &[Replica],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        
        let has_local_replica = replicas.iter().any(|r| r.node == current_node);
        
        let optimal_policy = if has_local_replica {
            "primary_first"  // Favor local replica for reads
        } else {
            "queue_depth"    // Load balance remote replicas
        };
        
        println!("Updating RAID read policy for volume {} to: {} (local_replica: {})", 
                 volume_id, optimal_policy, has_local_replica);
        
        // Update RAID configuration
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_set_options",
                "params": {
                    "name": volume_id,
                    "read_policy": optimal_policy
                }
            }))
            .send()
            .await;
            
        match response {
            Ok(resp) if resp.status().is_success() => {
                println!("Successfully updated read policy for volume: {}", volume_id);
                Ok(())
            }
            Ok(resp) => {
                let error_text = resp.text().await.unwrap_or_default();
                println!("Warning: Failed to update read policy: {}", error_text);
                // Don't fail the operation for read policy update failures
                Ok(())
            }
            Err(e) => {
                println!("Warning: Error updating read policy: {}", e);
                // Don't fail the operation for read policy update failures
                Ok(())
            }
        }
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
                        return Ok(Some(RaidStatus::from_spdk_response(raid_bdev)?));
                    }
                }
            }
        }
        
        Ok(None)
    }

    // Enhanced volume status update with locality information
    async fn update_volume_with_raid_status_and_locality(
        &self,
        volume_id: &str,
        socket_path: &str,
        current_node: &str,
        replicas: &[Replica],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        
        // Get current RAID status from SPDK
        let raid_status = self.get_raid_status(volume_id).await?;
        
        let has_local_replica = replicas.iter().any(|r| r.node == current_node);
        let replica_nodes: Vec<String> = replicas.iter().map(|r| r.node.clone()).collect();
        
        // Get performance metrics for local replica optimization
        let performance_metrics = self.calculate_local_replica_performance(volume_id, has_local_replica).await;
        
        let mut patch_data = json!({
            "spec": {
                "vhost_socket": socket_path
            },
            "status": {
                "vhost_device": format!("/dev/nvme-vhost-{}", volume_id),
                "last_checked": chrono::Utc::now().to_rfc3339(),
                "scheduled_node": current_node,
                "has_local_replica": has_local_replica,
                "replica_nodes": replica_nodes,
                "read_optimized": has_local_replica,
                "local_replica_performance": performance_metrics,
            }
        });

        // Include RAID status if available
        if let Some(raid_info) = raid_status {
            patch_data["status"]["raid_status"] = json!(raid_info);
            patch_data["status"]["read_policy"] = json!(raid_info.read_policy);
            
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

    // Calculate local replica performance metrics
    async fn calculate_local_replica_performance(
        &self,
        volume_id: &str,
        has_local_replica: bool,
    ) -> Option<LocalReplicaMetrics> {
        if !has_local_replica {
            return None;
        }

        // Get I/O statistics from SPDK
        let http_client = HttpClient::new();
        if let Ok(response) = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_get_iostat",
                "params": {
                    "name": volume_id
                }
            }))
            .send()
            .await
        {
            if let Ok(iostat) = response.json::<serde_json::Value>().await {
                if let Some(bdev_stats) = iostat["result"].as_array() {
                    for stat in bdev_stats {
                        if stat["name"].as_str() == Some(volume_id) {
                            let local_read_latency = stat["read_latency_ticks"].as_u64().unwrap_or(0) / 1000;
                            let remote_read_latency = local_read_latency * 3; // Estimate 3x latency for remote
                            
                            return Some(LocalReplicaMetrics {
                                local_read_percentage: 95.0, // Estimate with primary_first policy
                                local_read_latency_avg: local_read_latency,
                                remote_read_latency_avg: remote_read_latency,
                                optimization_ratio: remote_read_latency as f64 / local_read_latency.max(1) as f64,
                                last_updated: Utc::now().to_rfc3339(),
                            });
                        }
                    }
                }
            }
        }

        None
    }

    // Enhanced logging for volume scheduling information
    async fn log_volume_scheduling_info(
        &self,
        volume_id: &str,
        current_node: &str,
        replicas: &[Replica],
    ) {
        let local_replicas: Vec<&str> = replicas.iter()
            .filter(|r| r.node == current_node)
            .map(|r| r.node.as_str())
            .collect();
            
        let remote_replicas: Vec<&str> = replicas.iter()
            .filter(|r| r.node != current_node)
            .map(|r| r.node.as_str())
            .collect();

        println!("=== Volume Scheduling Info ===");
        println!("Volume ID: {}", volume_id);
        println!("Scheduled Node: {}", current_node);
        println!("Local Replicas: {:?}", local_replicas);
        println!("Remote Replicas: {:?}", remote_replicas);
        println!("Read Optimization: {}", !local_replicas.is_empty());
        println!("==============================");
    }

    // Method to monitor and report read performance metrics
    async fn report_read_performance_metrics(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        
        // Get RAID I/O statistics
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_get_iostat",
                "params": {
                    "name": volume_id
                }
            }))
            .send()
            .await?;

        if response.status().is_success() {
            let iostat: serde_json::Value = response.json().await?;
            
            if let Some(bdev_stats) = iostat["result"].as_array() {
                for stat in bdev_stats {
                    if stat["name"].as_str() == Some(volume_id) {
                        let read_latency = stat["read_latency_ticks"].as_u64().unwrap_or(0);
                        let read_iops = stat["read_ios"].as_u64().unwrap_or(0);
                        
                        println!("Volume {} Read Metrics - Latency: {}μs, IOPS: {}", 
                                 volume_id, read_latency / 1000, read_iops);
                    }
                }
            }
        }
        
        Ok(())
    }

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

    async fn export_bdev_as_vhost(
        &self,
        volume_id: &str,
        bdev_name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        // The implementation remains the same, as create_vhost_controller
        // already takes a generic bdev_name.
        self.create_vhost_controller(volume_id, bdev_name).await
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

    /// Helper function to provision a new, empty RAID volume.
    /// This function contains the logic for creating lvols and the RAID bdev.
    /// Enhanced with scheduling policy support and local optimization.
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
        
        // Parse scheduling policy from storage class parameters
        let scheduling_policy = params
            .get("schedulingPolicy")
            .and_then(|p| p.parse::<SchedulingPolicy>().ok())
            .unwrap_or_default();
            
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
                scheduling_policy: Some(format!("{:?}", scheduling_policy)),
                preferred_nodes: if replica_nodes.is_empty() { None } else { Some(replica_nodes) },
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
    async fn controller_publish_volume(
        &self,
        _request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        // SPDK CSI driver handles attachment at the node level during staging
        // No controller-level attachment is needed
        Ok(Response::new(ControllerPublishVolumeResponse {
            publish_context: std::collections::HashMap::new(),
        }))
    }

    async fn controller_unpublish_volume(
        &self,
        _request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        // SPDK CSI driver handles detachment at the node level during unstaging
        // No controller-level detachment is needed
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

        // Check if volume exists
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        match volumes_api.get(&volume_id).await {
            Ok(_) => {
                // Validate each capability
                let mut confirmed_capabilities = Vec::new();
                
                for capability in req.volume_capabilities {
                    // Check access mode
                    let supported_access_mode = if let Some(access_mode) = &capability.access_mode {
                        let mode_value = access_mode.mode;
                        mode_value == (volume_capability::access_mode::Mode::SingleNodeWriter as i32) ||
                        mode_value == (volume_capability::access_mode::Mode::SingleNodeReaderOnly as i32) ||
                        mode_value == (volume_capability::access_mode::Mode::SingleNodeSingleWriter as i32)
                    } else {
                        false
                    };

                    // Check access type (block or mount)
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
        request: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        let req = request.into_inner();
        let max_entries = req.max_entries as usize;
        let starting_token = req.starting_token;

        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume_list = volumes_api.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list volumes: {}", e)))?;

        let mut entries = Vec::new();
        let mut start_index = 0;

        // Handle pagination
        if !starting_token.is_empty() {
            start_index = starting_token.parse().unwrap_or(0);
        }

        let volumes_slice = if max_entries > 0 {
            volume_list.items.iter()
                .skip(start_index)
                .take(max_entries)
                .collect::<Vec<_>>()
        } else {
            volume_list.items.iter().skip(start_index).collect::<Vec<_>>()
        };

        for volume in volumes_slice {
            let volume_context = std::collections::HashMap::from([
                ("storageType".to_string(), if volume.spec.num_replicas > 1 { 
                    "vhost-raid".to_string() 
                } else { 
                    "vhost-lvol".to_string() 
                }),
                ("schedulingPolicy".to_string(), 
                 volume.spec.scheduling_policy.clone().unwrap_or("AnyNode".to_string())),
            ]);

            // Create topology based on replica nodes
            let accessible_topology = volume.spec.replicas.iter()
                .map(|replica| Topology {
                    segments: [(
                        "topology.kubernetes.io/hostname".to_string(),
                        replica.node.clone(),
                    )].into_iter().collect(),
                })
                .collect();

            let entry = list_volumes_response::Entry {
                volume: Some(Volume {
                    volume_id: volume.spec.volume_id.clone(),
                    capacity_bytes: volume.spec.size_bytes,
                    volume_context,
                    content_source: None,
                    accessible_topology,
                }),
                status: volume.status.as_ref().map(|s| list_volumes_response::VolumeStatus {
                    published_node_ids: if let Some(scheduled_node) = &s.scheduled_node {
                        vec![scheduled_node.clone()]
                    } else {
                        vec![]
                    },
                    volume_condition: if s.degraded {
                        Some(VolumeCondition {
                            abnormal: true,
                            message: "Volume is in degraded state".to_string(),
                        })
                    } else {
                        None
                    },
                }),
            };

            entries.push(entry);
        }

        // Calculate next token
        let next_token = if max_entries > 0 && entries.len() == max_entries {
            (start_index + max_entries).to_string()
        } else {
            String::new()
        };

        Ok(Response::new(ListVolumesResponse {
            entries,
            next_token,
        }))
    }

    async fn get_capacity(
        &self,
        request: Request<GetCapacityRequest>,
    ) -> Result<Response<GetCapacityResponse>, Status> {
        let req = request.into_inner();
        
        // Parse topology requirements if specified
        let mut target_nodes = Vec::new();
        if let Some(topology) = req.accessible_topology {
            if let Some(hostname) = topology.segments.get("topology.kubernetes.io/hostname") {
                target_nodes.push(hostname.clone());
            }
        }

        let disks_api: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        let disk_list = disks_api.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list disks: {}", e)))?;

        let total_capacity = disk_list.items.iter()
            .filter(|disk| {
                // Filter by topology if specified
                if !target_nodes.is_empty() && !target_nodes.contains(&disk.spec.node) {
                    return false;
                }
                
                // Only count healthy disks with initialized blobstores
                disk.status.as_ref().map_or(false, |s| 
                    s.healthy && s.blobstore_initialized
                )
            })
            .map(|disk| disk.status.as_ref().unwrap().free_space)
            .sum::<i64>();

        Ok(Response::new(GetCapacityResponse {
            available_capacity: total_capacity,
            maximum_volume_size: Some(total_capacity), // Single volume can use all available space
            minimum_volume_size: Some(1024 * 1024 * 1024), // 1GB minimum
        }))
    }

    async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        // Delegate to the implementation in csi_snapshotter.rs
        self.create_snapshot_impl(request).await
    }

    async fn delete_snapshot(
        &self,
        request: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        // Delegate to the implementation in csi_snapshotter.rs
        self.delete_snapshot_impl(request).await
    }

    async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        // Delegate to the implementation in csi_snapshotter.rs
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

        // Get the volume
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;

        if new_capacity <= volume.spec.size_bytes {
            return Err(Status::invalid_argument("New capacity must be larger than current capacity"));
        }

        // Check if all target disks have enough free space
        let disks_api: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        let capacity_increase = new_capacity - volume.spec.size_bytes;

        for replica in &volume.spec.replicas {
            if let Ok(disk) = disks_api.get(&replica.disk_ref).await {
                if let Some(status) = &disk.status {
                    if status.free_space < capacity_increase {
                        return Err(Status::resource_exhausted(
                            format!("Insufficient space on disk {} (node: {})", 
                                   replica.disk_ref, replica.node)
                        ));
                    }
                }
            }
        }

        // Expand lvols on each replica
        let http_client = reqwest::Client::new();
        let mut failed_replicas = Vec::new();

        for replica in &volume.spec.replicas {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let rpc_url = self.get_rpc_url_for_node(&replica.node).await?;
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_name = format!("{}/{}", lvs_name, lvol_uuid);

                let response = http_client
                    .post(&rpc_url)
                    .json(&serde_json::json!({
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

        // Update volume spec with new capacity
        let patch = serde_json::json!({
            "spec": {
                "size_bytes": new_capacity
            }
        });
        volumes_api.patch(&volume_id, &PatchParams::default(), &Patch::Merge(patch)).await
            .map_err(|e| Status::internal(format!("Failed to update volume spec: {}", e)))?;

        // Update disk statuses
        for replica in &volume.spec.replicas {
            if let Ok(disk) = disks_api.get(&replica.disk_ref).await {
                let mut status = disk.status.unwrap_or_default();
                status.free_space -= capacity_increase;
                status.used_space += capacity_increase;
                status.last_checked = chrono::Utc::now().to_rfc3339();

                disks_api
                    .patch_status(&replica.disk_ref, &PatchParams::default(), 
                                 &Patch::Merge(serde_json::json!({"status": status})))
                    .await
                    .ok(); // Ignore disk status update errors
            }
        }

        Ok(Response::new(ControllerExpandVolumeResponse {
            capacity_bytes: new_capacity,
            node_expansion_required: true, // Always require node expansion for filesystem resize
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

        let volume_context = std::collections::HashMap::from([
            ("storageType".to_string(), if volume.spec.num_replicas > 1 { 
                "vhost-raid".to_string() 
            } else { 
                "vhost-lvol".to_string() 
            }),
            ("schedulingPolicy".to_string(), 
             volume.spec.scheduling_policy.clone().unwrap_or("AnyNode".to_string())),
        ]);

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
            volume_context,
            content_source: None,
            accessible_topology,
        };

        let status = if let Some(vol_status) = &volume.status {
            Some(controller_get_volume_response::VolumeStatus {
                published_node_ids: if let Some(scheduled_node) = &vol_status.scheduled_node {
                    vec![scheduled_node.clone()]
                } else {
                    vec![]
                },
                volume_condition: if vol_status.degraded {
                    Some(VolumeCondition {
                        abnormal: true,
                        message: format!("Volume state: {}", vol_status.state),
                    })
                } else {
                    None
                },
            })
        } else {
            None
        };

        Ok(Response::new(ControllerGetVolumeResponse {
            volume: Some(csi_volume),
            status,
        }))
    }

    async fn controller_modify_volume(
        &self,
        _request: Request<ControllerModifyVolumeRequest>,
    ) -> Result<Response<ControllerModifyVolumeResponse>, Status> {
        // Volume modification is not supported in this implementation
        // This could be extended to support changing volume parameters
        Err(Status::unimplemented("Volume modification is not supported"))
    }

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

        // Parse scheduling policy from storage class parameters
        let scheduling_policy = req.parameters
            .get("schedulingPolicy")
            .and_then(|p| p.parse::<SchedulingPolicy>().ok())
            .unwrap_or_default();

        let num_replicas = req.parameters
            .get("numReplicas")
            .and_then(|n| n.parse::<i32>().ok())
            .unwrap_or(1);

        let (spdk_volume, new_volume_id) = if let Some(source) = &req.volume_content_source {
            // Handle volume content source - check the type field
            match &source.r#type {
                Some(volume_content_source::Type::Snapshot(snapshot_source)) => {
                    let snapshot_id = &snapshot_source.snapshot_id;
                    
                    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), "default");
                    let snapshot_crd = snapshots_api.get(snapshot_id).await
                        .map_err(|_| Status::not_found(format!("Source snapshot {} not found", snapshot_id)))?;

                    let source_replica_snapshot = snapshot_crd.spec.replica_snapshots.first()
                        .ok_or_else(|| Status::not_found(format!("No replica snapshots in {}", snapshot_id)))?;
                    
                    // Provision the destination volume (RAID or single lvol)
                    let (dest_spdk_volume, dest_bdev_name) = if num_replicas > 1 {
                        self.provision_new_raid_volume(&volume_name, capacity, &req.parameters).await?
                    } else {
                        self.provision_single_lvol_volume(&volume_name, capacity, &req.parameters).await?
                    };

                    // Perform the copy/clone
                    let http_client = HttpClient::new();
                    let source_node_rpc_url = self.get_rpc_url_for_node(&source_replica_snapshot.node_name).await?;
                    let copy_response = http_client.post(&source_node_rpc_url)
                        .json(&json!({
                            "method": "bdev_copy",
                            "params": {
                                "src_bdev": &source_replica_snapshot.spdk_snapshot_lvol,
                                "dst_bdev": &dest_bdev_name,
                            }
                        }))
                        .send().await.map_err(|e| Status::internal(format!("SPDK bdev_copy RPC failed: {}", e)))?;

                    if !copy_response.status().is_success() {
                        let err_text = copy_response.text().await.unwrap_or_default();
                        self.delete_volume(Request::new(DeleteVolumeRequest { 
                            volume_id: dest_spdk_volume.spec.volume_id.clone(),
                            secrets: std::collections::HashMap::new(),
                        })).await.ok();
                        return Err(Status::internal(format!("Failed to copy data: {}", err_text)));
                    }

                    (dest_spdk_volume, dest_bdev_name)
                }
                Some(volume_content_source::Type::Volume(volume_source)) => {
                    // Handle volume cloning
                    let source_volume_id = &volume_source.volume_id;
                    // Implement volume cloning logic here if needed
                    return Err(Status::unimplemented("Volume cloning not yet implemented"));
                }
                None => {
                    return Err(Status::invalid_argument("Volume content source type not specified"));
                }
            }
        } else {
            // Create an empty volume (RAID or single lvol)
            if num_replicas > 1 {
                self.provision_new_raid_volume(&volume_name, capacity, &req.parameters).await?
            } else {
                self.provision_single_lvol_volume(&volume_name, capacity, &req.parameters).await?
            }
        };

        // Enhanced topology logic based on replica count and scheduling policy
        let mut volume_context = HashMap::new();
        let mut accessible_topology = vec![];
        let mut preferred_topology = vec![]; // NEW: Preferred topology list

        if num_replicas > 1 {
            volume_context.insert("storageType".to_string(), "vhost-raid".to_string());
            
            // Implement preferred topology for RAID volumes
            match scheduling_policy {
                SchedulingPolicy::AnyNode => {
                    // Current behavior: no topology constraints
                    // Pod can be scheduled anywhere
                }
                
                SchedulingPolicy::PreferReplicas => {
                    // Set preferred topology to replica nodes
                    preferred_topology = spdk_volume.spec.replicas.iter()
                        .map(|replica| Topology {
                            segments: [(
                                "topology.kubernetes.io/hostname".to_string(),
                                replica.node.clone(),
                            )].into_iter().collect(),
                        })
                        .collect();
                }
                
                SchedulingPolicy::RequireReplicas => {
                    // Require scheduling on replica nodes
                    accessible_topology = spdk_volume.spec.replicas.iter()
                        .map(|replica| Topology {
                            segments: [(
                                "topology.kubernetes.io/hostname".to_string(),
                                replica.node.clone(),
                            )].into_iter().collect(),
                        })
                        .collect();
                }
            }
        } else {
            // Single-replica volumes: always require specific node
            volume_context.insert("storageType".to_string(), "vhost-lvol".to_string());
            if let Some(replica) = spdk_volume.spec.replicas.first() {
                let topology = Topology {
                    segments: [(
                        "topology.kubernetes.io/hostname".to_string(),
                        replica.node.clone(),
                    )].into_iter().collect(),
                };
                accessible_topology.push(topology);
            }
        }

        // Add scheduling policy to volume context for debugging
        volume_context.insert("schedulingPolicy".to_string(), format!("{:?}", scheduling_policy));

        // Create the CSI Volume response with enhanced topology
        let mut volume = Volume {
            volume_id: new_volume_id.clone(),
            capacity_bytes: spdk_volume.spec.size_bytes,
            volume_context: volume_context.clone(),
            content_source: req.volume_content_source,
            accessible_topology,
            ..Default::default()
        };

        // Add preferred topology information to volume context for scheduler hints
        if !preferred_topology.is_empty() {
            volume.volume_context.insert(
                "preferredNodes".to_string(),
                preferred_topology.iter()
                    .filter_map(|t| t.segments.get("topology.kubernetes.io/hostname"))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            );
            
            // Also add as annotation for external scheduling tools
            volume.volume_context.insert(
                "spdk.io/replica-nodes".to_string(),
                spdk_volume.spec.replicas.iter()
                    .map(|r| r.node.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }

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
            Err(_) => {
                return Ok(Response::new(DeleteVolumeResponse {}));
            }
        };

        // Delete vhost controller
        self.delete_vhost_controller(&volume_id).await.ok();

        // Delete RAID configuration
        if spdk_volume.spec.num_replicas > 1 {
            self.delete_lvol_raid(&volume_id).await.ok();
        }

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
        _request: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {

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
impl Identity for SpdkCsiDriver {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: "flint.csi.storage.io".to_string(),
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

// Helper structs for SPDK statistics
#[derive(Debug, Default)]
struct SpdkVolumeStatsDetailed {
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
    total_clusters: u64,
    used_clusters: u64,
    available_clusters: u64,
    cluster_size: u64,
    read_iops: u64,
    write_iops: u64,
    read_bytes: u64,
    write_bytes: u64,
    read_latency_us: u64,
    write_latency_us: u64,
    io_error_count: u64,
    is_healthy: bool,
    thin_provisioned: bool,
    raid_level: Option<u64>,
    raid_state: Option<String>,
    operational_members: Option<u64>,
    total_members: Option<u64>,
}

#[derive(Debug)]
struct BdevInfo {
    name: String,
    num_blocks: u64,
    block_size: u64,
    claimed: bool,
    driver_name: String,
}

#[derive(Debug)]
struct BdevIoStat {
    read_ios: u64,
    write_ios: u64,
    read_bytes: u64,
    write_bytes: u64,
    read_latency_ticks: u64,
    write_latency_ticks: u64,
    io_error: u64,
}

#[derive(Debug)]
struct RaidBdevStat {
    raid_level: u64,
    state: String,
    num_base_bdevs: u64,
    num_base_bdevs_operational: u64,
    rebuild_progress: f64,
}

#[derive(Debug)]
struct LvolStat {
    total_data_clusters: u64,
    free_clusters: u64,
    num_allocated_clusters: u64,
    cluster_size: u64,
    thin_provision: bool,
}

#[tonic::async_trait]
impl Node for SpdkCsiDriver {
    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let target_path = req.target_path;
        let staging_target_path = req.staging_target_path;

        if volume_id.is_empty() || target_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and target path are required"));
        }

        // Get the SpdkVolume CR to find vhost socket path
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;

        // Check if volume has a local replica on this node
        let node_name = std::env::var("NODE_NAME")
            .map_err(|_| Status::internal("NODE_NAME environment variable not set"))?;

        let local_replica = volume.spec.replicas.iter()
            .find(|r| r.node == node_name && r.replica_type == "lvol")
            .ok_or_else(|| Status::failed_precondition(
                format!("No local replica found for volume {} on node {}", volume_id, node_name)
            ))?;

        // Get vhost socket path from volume spec or construct default
        let vhost_socket = volume.spec.vhost_socket
            .unwrap_or_else(|| format!("/var/lib/spdk-csi/sockets/vhost_{}.sock", volume_id));

        // Ensure vhost controller exists and is active
        self.ensure_vhost_controller_active(&volume_id, &vhost_socket).await?;

        // Create target directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(&target_path).parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| Status::internal(format!("Failed to create target directory: {}", e)))?;
        }

        // For block volumes, create a symlink to the vhost device
        if let Some(volume_capability) = req.volume_capability {
            match volume_capability.access_type {
                Some(volume_capability::AccessType::Block(_)) => {
                    // Find the vhost-nvme device path
                    let device_path = self.find_vhost_device_path(&vhost_socket).await?;
                    
                    // Create symlink from target_path to the actual device
                    if std::path::Path::new(&target_path).exists() {
                        tokio::fs::remove_file(&target_path).await.ok();
                    }
                    
                    tokio::fs::symlink(&device_path, &target_path).await
                        .map_err(|e| Status::internal(format!("Failed to create device symlink: {}", e)))?;
                }
                Some(volume_capability::AccessType::Mount(mount)) => {
                    // For filesystem access, we need to format and mount the device
                    let device_path = self.find_vhost_device_path(&vhost_socket).await?;
                    
                    // Format the device if needed
                    let fs_type = if mount.fs_type.is_empty() { "ext4" } else { &mount.fs_type };
                    self.format_device_if_needed(&device_path, fs_type).await?;
                    
                    // Create target directory
                    tokio::fs::create_dir_all(&target_path).await
                        .map_err(|e| Status::internal(format!("Failed to create mount point: {}", e)))?;
                    
                    // Mount the device
                    let mut mount_cmd = tokio::process::Command::new("mount");
                    mount_cmd.arg("-t").arg(fs_type);
                    
                    // Add mount flags if specified
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

        // Update the replica status to indicate it's published
        self.update_replica_published_status(&volume_id, &node_name, true).await?;

        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_get_volume_stats(
        &self,
        request: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let volume_path = req.volume_path;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID is required"));
        }

        let mut usage = Vec::new();
        let mut volume_condition = None;

        // Get comprehensive SPDK statistics
        match self.get_pure_spdk_volume_stats(&volume_id).await {
            Ok(stats) => {
                // Add block-level statistics (always available for SPDK volumes)
                usage.push(VolumeUsage {
                    available: stats.available_bytes as i64,
                    total: stats.total_bytes as i64,
                    used: stats.used_bytes as i64,
                    unit: volume_usage::Unit::Bytes as i32,
                });

                // For lvol-based volumes, we can also provide cluster statistics
                if stats.total_clusters > 0 {
                    usage.push(VolumeUsage {
                        available: stats.available_clusters as i64,
                        total: stats.total_clusters as i64,
                        used: stats.used_clusters as i64,
                        unit: volume_usage::Unit::Inodes as i32, // Repurpose inodes for clusters
                    });
                }

                // Determine volume health based on SPDK metrics
                let is_healthy = stats.is_healthy && stats.available_bytes > 0;
                let mut health_message = String::new();

                if !stats.is_healthy {
                    health_message.push_str("SPDK bdev reports unhealthy state. ");
                }
                if stats.io_error_count > 0 {
                    health_message.push_str(&format!("I/O errors detected: {}. ", stats.io_error_count));
                }
                if stats.available_bytes == 0 {
                    health_message.push_str("Volume is full. ");
                }
                if stats.read_latency_us > 100000 || stats.write_latency_us > 100000 {
                    health_message.push_str("High I/O latency detected. ");
                }

                if health_message.is_empty() {
                    health_message = format!(
                        "Volume healthy. IOPS: {}/{} (R/W), Latency: {}μs/{}μs (R/W)",
                        stats.read_iops, stats.write_iops,
                        stats.read_latency_us, stats.write_latency_us
                    );
                }

                volume_condition = Some(VolumeCondition {
                    abnormal: !is_healthy,
                    message: health_message,
                });
            }
            Err(e) => {
                volume_condition = Some(VolumeCondition {
                    abnormal: true,
                    message: format!("Failed to get SPDK volume statistics: {}", e),
                });

                // Return minimal stats if we can't get detailed info
                usage.push(VolumeUsage {
                    available: 0,
                    total: 0,
                    used: 0,
                    unit: volume_usage::Unit::Bytes as i32,
                });
            }
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

        // Get the new capacity from the request
        let new_capacity = req.capacity_range
            .as_ref()
            .map(|cr| cr.required_bytes)
            .unwrap_or(0);

        if new_capacity <= 0 {
            return Err(Status::invalid_argument("New capacity must be greater than 0"));
        }

        // Get current node name
        let node_name = std::env::var("NODE_NAME")
            .map_err(|_| Status::internal("NODE_NAME environment variable not set"))?;

        // Get the SpdkVolume CR
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;

        // Find local replica
        let local_replica = volume.spec.replicas.iter()
            .find(|r| r.node == node_name && r.replica_type == "lvol")
            .ok_or_else(|| Status::failed_precondition(
                format!("No local replica found for volume {} on node {}", volume_id, node_name)
            ))?;

        // Expand the underlying SPDK lvol first
        if let Some(lvol_uuid) = &local_replica.lvol_uuid {
            let lvs_name = format!("lvs_{}", local_replica.disk_ref);
            let lvol_name = format!("{}/{}", lvs_name, lvol_uuid);
            
            let http_client = reqwest::Client::new();
            let response = http_client
                .post(&self.spdk_rpc_url)
                .json(&serde_json::json!({
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
                return Err(Status::internal(format!("Failed to resize lvol: {}", error_text)));
            }
        }

        // For RAID volumes, we need to resize all replicas
        if volume.spec.num_replicas > 1 {
            // The RAID bdev should automatically detect the size change
            // We can verify this by checking the RAID bdev size
            let http_client = reqwest::Client::new();
            let response = http_client
                .post(&self.spdk_rpc_url)
                .json(&serde_json::json!({
                    "method": "bdev_get_bdevs",
                    "params": { "name": volume_id }
                }))
                .send()
                .await
                .map_err(|e| Status::internal(format!("Failed to get RAID bdev info: {}", e)))?;

            if response.status().is_success() {
                let bdev_info: serde_json::Value = response.json().await
                    .map_err(|e| Status::internal(format!("Failed to parse RAID bdev response: {}", e)))?;
                
                if let Some(bdev_array) = bdev_info["result"].as_array() {
                    if let Some(bdev) = bdev_array.first() {
                        let actual_size = bdev["num_blocks"].as_u64().unwrap_or(0) * 
                                        bdev["block_size"].as_u64().unwrap_or(512);
                        
                        if actual_size < new_capacity as u64 {
                            return Err(Status::internal(
                                "RAID bdev size did not update after lvol resize"
                            ));
                        }
                    }
                }
            }
        }

        // Expand the filesystem if this is a mount volume
        if let Some(volume_capability) = req.volume_capability {
            if let Some(volume_capability::AccessType::Mount(mount)) = volume_capability.access_type {
                let fs_type = if mount.fs_type.is_empty() { "ext4" } else { &mount.fs_type };
                
                // Find the underlying device
                let device_path = self.find_device_for_mount(&volume_path).await?;
                
                // Resize the filesystem
                match fs_type {
                    "ext4" | "ext3" | "ext2" => {
                        let output = tokio::process::Command::new("resize2fs")
                            .arg(&device_path)
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
        // Get node name from environment
        let node_id = std::env::var("NODE_NAME")
            .map_err(|_| Status::internal("NODE_NAME environment variable not set"))?;

        // Get maximum volumes per node from environment or use default
        let max_volumes_per_node = std::env::var("MAX_VOLUMES_PER_NODE")
            .unwrap_or("100".to_string())
            .parse::<i64>()
            .unwrap_or(100);

        // Get topology information from environment
        let mut topology_segments = std::collections::HashMap::new();
        
        // Add node-specific topology
        topology_segments.insert("topology.spdk.io/node".to_string(), node_id.clone());
        
        // Add zone/region if available
        if let Ok(zone) = std::env::var("NODE_ZONE") {
            topology_segments.insert("topology.spdk.io/zone".to_string(), zone);
        }
        
        if let Ok(region) = std::env::var("NODE_REGION") {
            topology_segments.insert("topology.spdk.io/region".to_string(), region);
        }

        // Add rack information if available
        if let Ok(rack) = std::env::var("NODE_RACK") {
            topology_segments.insert("topology.spdk.io/rack".to_string(), rack);
        }

        // Check SPDK availability on this node
        match self.check_spdk_health().await {
            Ok(true) => {
                topology_segments.insert("spdk.io/available".to_string(), "true".to_string());
            }
            Ok(false) => {
                topology_segments.insert("spdk.io/available".to_string(), "false".to_string());
            }
            Err(_) => {
                topology_segments.insert("spdk.io/available".to_string(), "unknown".to_string());
            }
        }

        // Get NVMe device count if available
        if let Ok(device_count) = self.get_nvme_device_count().await {
            topology_segments.insert("spdk.io/nvme-devices".to_string(), device_count.to_string());
        }

        let accessible_topology = if !topology_segments.is_empty() {
            Some(Topology {
                segments: topology_segments,
            })
        } else {
            None
        };

        Ok(Response::new(NodeGetInfoResponse {
            node_id,
            max_volumes_per_node,
            accessible_topology,
        }))
    }

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

        let pod_node = get_pod_node(&self.kube_client)
            .await
            .map_err(|e| Status::internal(format!("Failed to get pod node: {}", e)))?;

        let pod_name = std::env::var("POD_NAME")
            .map_err(|e| Status::internal(format!("Failed to get POD_NAME: {}", e)))?;

        // Update SpdkVolume CRD with pod scheduling info
        self.update_replica_scheduling(&volume_id, &pod_node, &pod_name)
            .await
            .map_err(|e| Status::internal(format!("Failed to update replica scheduling: {}", e)))?;

        // Get volume information
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = crd_api.get(&volume_id).await
            .map_err(|_| Status::not_found(format!("SpdkVolume {} not found", volume_id)))?;

        // Check replica locality for optimization
        let has_local_replica = spdk_volume.spec.replicas.iter()
            .any(|r| r.node == pod_node);
        
        println!("Volume {} staging on node {}, has_local_replica: {}, replicas: {:?}", 
                 volume_id, pod_node, has_local_replica,
                 spdk_volume.spec.replicas.iter().map(|r| &r.node).collect::<Vec<_>>());

        // Determine the correct bdev to expose based on replica count and locality
        let bdev_to_expose = if spdk_volume.spec.num_replicas > 1 {
            // Multi-replica: Create locality-optimized RAID1
            self.create_lvol_raid_with_local_optimization(
                &volume_id,
                &spdk_volume,
                &pod_node,
            ).await
            .map_err(|e| Status::internal(format!("Failed to create optimized RAID1: {}", e)))?;
            
            volume_id.clone()
        } else {
            // Single replica: construct the lvol's bdev name
            let replica = spdk_volume.spec.replicas.first()
                .ok_or_else(|| Status::internal("Volume has no replica information"))?;
            let lvs_name = format!("lvs_{}", replica.disk_ref);
            let lvol_uuid = replica.lvol_uuid.as_ref()
                .ok_or_else(|| Status::internal("Replica is missing lvol_uuid"))?;
            self.get_lvol_bdev_name(&lvs_name, lvol_uuid)
        };

        // Create the vhost-nvme controller for the bdev
        let socket_path = self
            .export_bdev_as_vhost(&volume_id, &bdev_to_expose)
            .await
            .map_err(|e| Status::internal(format!("Failed to create vhost controller: {}", e)))?;

        // Update volume status with locality and RAID information
        if spdk_volume.spec.num_replicas > 1 {
            self.update_volume_with_raid_status_and_locality(
                &volume_id, 
                &socket_path,
                &pod_node,
                &spdk_volume.spec.replicas,
            ).await
            .map_err(|e| Status::internal(format!("Failed to update volume status: {}", e)))?;
        } else {
            // Single replica status update
            let patch = json!({
                "spec": { "vhost_socket": &socket_path },
                "status": { 
                    "state": "Staged", 
                    "last_checked": Utc::now().to_rfc3339(),
                    "scheduled_node": pod_node,
                    "has_local_replica": has_local_replica,
                }
            });
            crd_api.patch(&volume_id, &PatchParams::default(), &Patch::Merge(patch)).await
                .map_err(|e| Status::internal(format!("Failed to patch SpdkVolume status: {}", e)))?;
        }

        // Start QEMU vhost-user-nvme device and get the local device path
        let device_path = self
            .start_vhost_user_nvme(&socket_path, &volume_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to start vhost-user-nvme: {}", e)))?;

        // Handle block vs filesystem mounting - check the access_type field
        match &volume_capability.access_type {
            Some(volume_capability::AccessType::Block(_)) => {
                // Block device mode
                if !Path::new(&device_path).exists() {
                    return Err(Status::internal("Vhost NVMe device not found after starting process"));
                }
                return Ok(Response::new(NodeStageVolumeResponse {}));
            }
            Some(volume_capability::AccessType::Mount(mount_volume)) => {
                // Mount mode - fix the fs_type access
                let fs_type = if mount_volume.fs_type.is_empty() {
                    "ext4".to_string()
                } else {
                    mount_volume.fs_type.clone()
                };

                // Fix the error conversion
                if !is_device_formatted(&device_path)
                    .map_err(|e| Status::internal(format!("Failed to check device format: {}", e)))? 
                {
                    format_device(&device_path, &fs_type)
                        .map_err(|e| Status::internal(format!("Failed to format device: {}", e)))?;
                }

                let mount_flags = mount_volume.mount_flags.clone();
                mount_device(&device_path, &staging_target_path, &fs_type, &mount_flags)
                    .map_err(|e| Status::internal(format!("Failed to mount device: {}", e)))?;

                Ok(Response::new(NodeStageVolumeResponse {}))
            }
            None => {
                Err(Status::invalid_argument("Volume capability access type not specified"))
            }
        }

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

        // Standard unmount
        Command::new("umount")
            .arg(&staging_target_path)
            .status()
            .map_err(|e| Status::internal(format!("Failed to unmount: {}", e)))?;

        // Stop vhost processes and cleanup
        self.stop_vhost_user_nvme(&volume_id).await.ok();
        self.delete_vhost_controller(&volume_id).await.ok();
        self.cleanup_nvmf_connections(&volume_id).await.ok();

        // Reset replica scheduling status
        let crd_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let patch = json!({
            "status": {
                "scheduled_node": null,
                "has_local_replica": false,
                "read_optimized": false,
                "state": "Available",
                "last_checked": Utc::now().to_rfc3339(),
            }
        });
        crd_api.patch(&volume_id, &PatchParams::default(), &Patch::Merge(patch)).await.ok();

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
                // Add volume condition capability if you want to report volume health
                NodeServiceCapability {
                    r#type: Some(node_service_capability::Type::Rpc(
                        node_service_capability::Rpc {
                            r#type: node_service_capability::rpc::Type::VolumeCondition as i32,
                        },
                    )),
                },
                // Add expand volume capability if you support online volume expansion
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
    async fn ensure_vhost_controller_active(
        &self,
        volume_id: &str,
        socket_path: &str,
    ) -> Result<(), Status> {
        let http_client = reqwest::Client::new();
        let controller_name = format!("vhost_{}", volume_id);

        // Check if controller exists and is active
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&serde_json::json!({
                "method": "vhost_get_controllers"
            }))
            .send()
            .await
            .map_err(|e| Status::internal(format!("Failed to check vhost controllers: {}", e)))?;

        if let Ok(controllers_info) = response.json::<serde_json::Value>().await {
            if let Some(controllers) = controllers_info["result"].as_array() {
                for controller in controllers {
                    if controller["ctrlr"].as_str() == Some(&controller_name) {
                        if controller["active"].as_bool() == Some(true) {
                            return Ok(()); // Controller is active
                        }
                    }
                }
            }
        }

        Err(Status::failed_precondition(
            format!("Vhost controller {} is not active", controller_name)
        ))
    }

    async fn find_vhost_device_path(&self, socket_path: &str) -> Result<String, Status> {
        // Wait for the vhost device to appear (up to 30 seconds)
        let max_wait = std::time::Duration::from_secs(30);
        let start = std::time::Instant::now();

        while start.elapsed() < max_wait {
            // Look for vhost-nvme devices in /dev
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
        // Check if this NVMe device is backed by vhost
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
        // Check if the device is already formatted
        let output = tokio::process::Command::new("blkid")
            .arg(device_path)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to check device format: {}", e)))?;

        if output.status.success() {
            // Device is already formatted
            return Ok(());
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

    async fn update_replica_published_status(
        &self,
        volume_id: &str,
        node_name: &str,
        published: bool,
    ) -> Result<(), Status> {
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        
        // This would update the replica's published status in the SpdkVolume CR
        // Implementation depends on your specific CRD structure
        println!("Updated replica published status for volume {} on node {} to {}", 
                volume_id, node_name, published);
        
        Ok(())
    }

    async fn find_device_for_mount(&self, mount_path: &str) -> Result<String, Status> {
        // Use findmnt to get the source device for the mount
        let output = tokio::process::Command::new("findmnt")
            .arg("-n")
            .arg("-o")
            .arg("SOURCE")
            .arg(mount_path)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to find mount device: {}", e)))?;

        if output.status.success() {
            let device = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(device)
        } else {
            Err(Status::not_found("Could not find device for mount point"))
        }
    }

    async fn check_spdk_health(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let http_client = reqwest::Client::new();
        
        match http_client
            .post(&self.spdk_rpc_url)
            .json(&serde_json::json!({
                "method": "spdk_get_version"
            }))
            .send()
            .await
        {
            Ok(response) => Ok(response.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    async fn get_nvme_device_count(&self) -> Result<u32, Box<dyn std::error::Error>> {
        let http_client = reqwest::Client::new();
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&serde_json::json!({
                "method": "bdev_get_bdevs"
            }))
            .send()
            .await?;

        let bdevs: serde_json::Value = response.json().await?;
        
        if let Some(bdev_list) = bdevs["result"].as_array() {
            let nvme_count = bdev_list.iter()
                .filter(|bdev| bdev["product_name"].as_str().unwrap_or("").contains("NVMe"))
                .count();
            Ok(nvme_count as u32)
        } else {
            Ok(0)
        }
    }

    async fn get_pure_spdk_volume_stats(&self, volume_id: &str) -> Result<SpdkVolumeStatsDetailed, Box<dyn std::error::Error>> {
        let http_client = reqwest::Client::new();
        let mut stats = SpdkVolumeStatsDetailed::default();

        // 1. Get basic bdev information
        let bdev_info = self.get_bdev_info(&http_client, volume_id).await?;
        stats.total_bytes = bdev_info.num_blocks * bdev_info.block_size;
        stats.is_healthy = bdev_info.claimed;

        // 2. Get I/O statistics
        if let Ok(io_stats) = self.get_bdev_iostat(&http_client, volume_id).await {
            stats.read_iops = io_stats.read_ios;
            stats.write_iops = io_stats.write_ios;
            stats.read_bytes = io_stats.read_bytes;
            stats.write_bytes = io_stats.write_bytes;
            stats.read_latency_us = io_stats.read_latency_ticks / 1000; // Convert to microseconds
            stats.write_latency_us = io_stats.write_latency_ticks / 1000;
            stats.io_error_count = io_stats.io_error;
        }

        // 3. For RAID volumes, get RAID-specific statistics
        if let Ok(raid_stats) = self.get_raid_bdev_stats(&http_client, volume_id).await {
            stats.raid_level = Some(raid_stats.raid_level);
            
            // Check health before moving the state
            let is_raid_online = raid_stats.state == "online";
            stats.raid_state = Some(raid_stats.state);
            stats.operational_members = Some(raid_stats.num_base_bdevs_operational);
            stats.total_members = Some(raid_stats.num_base_bdevs);
            
            // RAID volume health depends on member health
            stats.is_healthy = stats.is_healthy && is_raid_online;
        }

        // 4. For lvol volumes, get lvol-specific information
        if let Ok(lvol_stats) = self.get_lvol_stats(&http_client, volume_id).await {
            stats.total_clusters = lvol_stats.total_data_clusters;
            stats.used_clusters = lvol_stats.num_allocated_clusters;
            stats.available_clusters = stats.total_clusters.saturating_sub(stats.used_clusters);
            stats.cluster_size = lvol_stats.cluster_size;
            
            // More accurate byte calculations for lvols
            stats.used_bytes = stats.used_clusters * stats.cluster_size;
            stats.available_bytes = stats.available_clusters * stats.cluster_size;
            
            // Check thin provisioning status
            stats.thin_provisioned = lvol_stats.thin_provision;
        } else {
            // Fallback: estimate usage based on I/O if lvol info not available
            stats.used_bytes = stats.read_bytes + stats.write_bytes;
            stats.available_bytes = stats.total_bytes.saturating_sub(stats.used_bytes);
        }

        Ok(stats)
    }

    async fn get_bdev_info(&self, http_client: &reqwest::Client, volume_id: &str) -> Result<BdevInfo, Box<dyn std::error::Error>> {
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&serde_json::json!({
                "method": "bdev_get_bdevs",
                "params": { "name": volume_id }
            }))
            .send()
            .await?;

        let bdev_data: serde_json::Value = response.json().await?;
        
        if let Some(bdev_array) = bdev_data["result"].as_array() {
            if let Some(bdev) = bdev_array.first() {
                return Ok(BdevInfo {
                    name: bdev["name"].as_str().unwrap_or("").to_string(),
                    num_blocks: bdev["num_blocks"].as_u64().unwrap_or(0),
                    block_size: bdev["block_size"].as_u64().unwrap_or(512),
                    claimed: bdev["claimed"].as_bool().unwrap_or(false),
                    driver_name: bdev["product_name"].as_str().unwrap_or("").to_string(),
                });
            }
        }
        
        Err(format!("Bdev {} not found", volume_id).into())
    }

    async fn get_bdev_iostat(&self, http_client: &reqwest::Client, volume_id: &str) -> Result<BdevIoStat, Box<dyn std::error::Error>> {
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&serde_json::json!({
                "method": "bdev_get_iostat",
                "params": { "name": volume_id }
            }))
            .send()
            .await?;

        let iostat_data: serde_json::Value = response.json().await?;
        
        if let Some(iostat_array) = iostat_data["result"].as_array() {
            if let Some(iostat) = iostat_array.first() {
                return Ok(BdevIoStat {
                    read_ios: iostat["read_ios"].as_u64().unwrap_or(0),
                    write_ios: iostat["write_ios"].as_u64().unwrap_or(0),
                    read_bytes: iostat["read_bytes"].as_u64().unwrap_or(0),
                    write_bytes: iostat["write_bytes"].as_u64().unwrap_or(0),
                    read_latency_ticks: iostat["read_latency_ticks"].as_u64().unwrap_or(0),
                    write_latency_ticks: iostat["write_latency_ticks"].as_u64().unwrap_or(0),
                    io_error: iostat["io_error"].as_u64().unwrap_or(0),
                });
            }
        }
        
        Err(format!("I/O stats for {} not found", volume_id).into())
    }

    async fn get_raid_bdev_stats(&self, http_client: &reqwest::Client, volume_id: &str) -> Result<RaidBdevStat, Box<dyn std::error::Error>> {
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&serde_json::json!({
                "method": "bdev_raid_get_bdevs",
                "params": { "category": "all" }
            }))
            .send()
            .await?;

        let raid_data: serde_json::Value = response.json().await?;
        
        if let Some(raid_array) = raid_data["result"].as_array() {
            for raid_bdev in raid_array {
                if raid_bdev["name"].as_str() == Some(volume_id) {
                    return Ok(RaidBdevStat {
                        raid_level: raid_bdev["raid_level"].as_u64().unwrap_or(1),
                        state: raid_bdev["state"].as_str().unwrap_or("unknown").to_string(),
                        num_base_bdevs: raid_bdev["num_base_bdevs"].as_u64().unwrap_or(0),
                        num_base_bdevs_operational: raid_bdev["num_base_bdevs_operational"].as_u64().unwrap_or(0),
                        rebuild_progress: raid_bdev.get("rebuild_info")
                            .and_then(|ri| ri.get("progress_percentage"))
                            .and_then(|p| p.as_f64())
                            .unwrap_or(0.0),
                    });
                }
            }
        }
        
        Err(format!("RAID bdev {} not found", volume_id).into())
    }

    async fn get_lvol_stats(&self, http_client: &reqwest::Client, volume_id: &str) -> Result<LvolStat, Box<dyn std::error::Error>> {
        // First, get all lvol stores to find our volume
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&serde_json::json!({
                "method": "bdev_lvol_get_lvstores"
            }))
            .send()
            .await?;

        let lvstores_data: serde_json::Value = response.json().await?;
        
        if let Some(lvstore_array) = lvstores_data["result"].as_array() {
            for lvstore in lvstore_array {
                if let Some(lvols) = lvstore["lvols"].as_array() {
                    for lvol in lvols {
                        let lvol_name = format!("{}/{}", 
                            lvstore["name"].as_str().unwrap_or(""),
                            lvol["name"].as_str().unwrap_or("")
                        );
                        
                        if lvol_name == volume_id || lvol["name"].as_str() == Some(volume_id) {
                            return Ok(LvolStat {
                                total_data_clusters: lvstore["total_data_clusters"].as_u64().unwrap_or(0),
                                free_clusters: lvstore["free_clusters"].as_u64().unwrap_or(0),
                                num_allocated_clusters: lvol["num_allocated_clusters"].as_u64().unwrap_or(0),
                                cluster_size: lvstore["cluster_size"].as_u64().unwrap_or(4096),
                                thin_provision: lvol["thin_provision"].as_bool().unwrap_or(false),
                            });
                        }
                    }
                }
            }
        }
        
        Err(format!("Lvol {} not found in any lvstore", volume_id).into())
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
                || replica.pod_name.as_ref() != Some(&pod_name.to_string())
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
        // Extract all the async work first
        let pid = match self.get_vhost_process_info(volume_id).await? {
            Some(pid) => pid,
            None => return Ok(()), // No process to stop
        };
        
        // Do synchronous kill
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()?;
        
        // Clean up async
        self.remove_vhost_process_info(volume_id).await?;
        
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

    /// Provisions a new volume consisting of a single lvol without RAID.
    /// Returns the SpdkVolume CRD and the name of the bdev to be used for I/O.
    async fn provision_single_lvol_volume(
        &self,
        volume_name: &str,
        capacity: i64,
        params: &HashMap<String, String>,
    ) -> Result<(SpdkVolume, String), Status> {
        let disks: Api<SpdkDisk> = Api::namespaced(self.kube_client.clone(), "default");
        
        // In a real implementation, you would use accessibility requirements
        // to select the correct node and disk.
        let selected_disk = disks.list(&ListParams::default()).await
            .map_err(|e| Status::internal(format!("Failed to list SpdkDisks: {}", e)))?
            .items
            .into_iter()
            .find(|d| {
                d.status.as_ref().map_or(false, |s| s.healthy && s.blobstore_initialized && s.free_space >= capacity)
            })
            .ok_or_else(|| Status::resource_exhausted("No suitable disk found for single-replica volume"))?;

        let new_volume_id = format!("lvol-{}", Uuid::new_v4());
        let lvol_uuid = self
            .create_volume_lvol(&selected_disk, capacity, &new_volume_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to create lvol: {}", e)))?;

        let replica = Replica {
            node: selected_disk.spec.node.clone(),
            replica_type: "lvol".to_string(),
            disk_ref: selected_disk.metadata.name.clone().unwrap_or_default(),
            lvol_uuid: Some(lvol_uuid.clone()),
            health_status: ReplicaHealth::Healthy,
            ..Default::default()
        };

        let spdk_volume = SpdkVolume::new(
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
        disks.patch_status(disk_name, &PatchParams::default(), &Patch::Merge(json!({ "status": disk_status }))).await
            .map_err(|e| Status::internal(format!("Failed to update SpdkDisk status: {}", e)))?;

        // The bdev to expose is the lvol itself.
        let lvs_name = format!("lvs_{}", selected_disk.metadata.name.as_ref().unwrap());
        let bdev_name = self.get_lvol_bdev_name(&lvs_name, &lvol_uuid);

        Ok((spdk_volume, bdev_name))
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
        node_id: node_id.clone(),
        kube_client,
        spdk_rpc_url: std::env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
        write_sequence_counter: Arc::new(Mutex::new(0)),
        local_lvol_cache: Arc::new(Mutex::new(HashMap::new())),
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
