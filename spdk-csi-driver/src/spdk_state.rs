// spdk_state.rs - Minimal State SPDK Query Layer
// This module provides direct SPDK querying capabilities for gradual migration to minimal state

use std::collections::HashMap;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::driver::SpdkCsiDriver;

/// Represents a disk discovered directly from SPDK (minimal state)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpdkStateDisk {
    pub node_name: String,
    pub pci_address: String,
    pub device_name: String,        // nvme0n1, nvme1n1, etc.
    pub bdev_name: Option<String>,  // kernel_nvme0n1, nvme0, etc.
    pub size_bytes: u64,
    pub healthy: bool,
    pub blobstore_initialized: bool,
    pub free_space: u64,
    pub lvs_name: Option<String>,
    pub lvol_count: u32,
}

/// Represents a volume discovered directly from SPDK (minimal state)  
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpdkStateVolume {
    pub volume_id: String,
    pub size_bytes: u64,
    pub replicas: Vec<SpdkStateReplica>,
    pub raid_level: Option<u32>,
    pub health: String, // "healthy", "degraded", "failed"
}

/// Represents a volume replica discovered directly from SPDK
#[derive(Debug, Clone, Serialize, Deserialize)]  
pub struct SpdkStateReplica {
    pub node_name: String,
    pub disk_pci_address: String,
    pub lvol_uuid: String,
    pub lvol_name: String,      // vol_pvc-abc123
    pub nqn: Option<String>,    // NVMe-oF target NQN
    pub target_ip: Option<String>,
    pub target_port: Option<u16>,
    pub health: String,         // "online", "failed", "rebuilding"
}

/// SPDK State Query Service - provides minimal state queries
/// This works alongside existing CRD logic during migration
pub struct SpdkStateService {
    driver: Arc<SpdkCsiDriver>,
}

impl SpdkStateService {
    pub fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }

    /// Get all disks across the cluster by querying SPDK directly
    pub async fn get_all_disks(&self) -> Result<Vec<SpdkStateDisk>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [SPDK_STATE] Discovering all disks across cluster via SPDK");
        
        let mut all_disks = Vec::new();
        let nodes = self.discover_cluster_nodes().await?;
        
        for (node_name, rpc_url) in nodes {
            match self.get_disks_for_node(&node_name, &rpc_url).await {
                Ok(mut node_disks) => {
                    all_disks.append(&mut node_disks);
                }
                Err(e) => {
                    println!("⚠️ [SPDK_STATE] Failed to get disks from node {}: {}", node_name, e);
                    // Continue with other nodes
                }
            }
        }
        
        println!("✅ [SPDK_STATE] Discovered {} disks across {} nodes", all_disks.len(), nodes_count);
        Ok(all_disks)
    }

    /// Get available disks that meet capacity requirements  
    pub async fn get_available_disks(&self, min_capacity: u64) -> Result<Vec<SpdkStateDisk>, Box<dyn std::error::Error + Send + Sync>> {
        let all_disks = self.get_all_disks().await?;
        
        let available: Vec<_> = all_disks.into_iter()
            .filter(|disk| {
                disk.healthy && 
                disk.blobstore_initialized && 
                disk.free_space >= min_capacity
            })
            .collect();
            
        println!("📊 [SPDK_STATE] Found {} available disks (min capacity: {} bytes)", available.len(), min_capacity);
        Ok(available)
    }

    /// Get all volumes across the cluster by querying SPDK directly
    pub async fn get_all_volumes(&self) -> Result<Vec<SpdkStateVolume>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [SPDK_STATE] Discovering all volumes across cluster via SPDK");
        
        let mut volume_map: HashMap<String, SpdkStateVolume> = HashMap::new();
        let nodes = self.discover_cluster_nodes().await?;
        
        for (node_name, rpc_url) in nodes {
            match self.get_volumes_for_node(&node_name, &rpc_url).await {
                Ok(node_volumes) => {
                    // Merge replicas from different nodes into volume objects
                    for replica in node_volumes {
                        let volume_id = extract_volume_id_from_lvol_name(&replica.lvol_name);
                        
                        volume_map.entry(volume_id.clone())
                            .or_insert_with(|| SpdkStateVolume {
                                volume_id: volume_id.clone(),
                                size_bytes: 0, // Will be set from first replica
                                replicas: Vec::new(),
                                raid_level: None,
                                health: "unknown".to_string(),
                            })
                            .replicas.push(replica);
                    }
                }
                Err(e) => {
                    println!("⚠️ [SPDK_STATE] Failed to get volumes from node {}: {}", node_name, e);
                }
            }
        }
        
        let volumes: Vec<_> = volume_map.into_values().collect();
        println!("✅ [SPDK_STATE] Discovered {} volumes across {} nodes", volumes.len(), nodes_count);
        Ok(volumes)
    }

    /// Find a specific volume by ID
    pub async fn get_volume(&self, volume_id: &str) -> Result<Option<SpdkStateVolume>, Box<dyn std::error::Error + Send + Sync>> {
        let all_volumes = self.get_all_volumes().await?;
        Ok(all_volumes.into_iter().find(|v| v.volume_id == volume_id))
    }

    /// Get disks for a specific node by querying its SPDK instance
    async fn get_disks_for_node(&self, node_name: &str, rpc_url: &str) -> Result<Vec<SpdkStateDisk>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [SPDK_STATE] Querying disks on node: {}", node_name);
        
        // Query SPDK for block devices and LVS stores
        let (bdevs, lvstores) = tokio::try_join!(
            self.call_node_rpc(rpc_url, "bdev_get_bdevs", json!({})),
            self.call_node_rpc(rpc_url, "bdev_lvol_get_lvstores", json!({}))
        )?;
        
        let mut disks = Vec::new();
        
        // Parse bdevs to find NVMe disks
        if let Some(bdev_list) = bdevs["result"].as_array() {
            for bdev in bdev_list {
                if let Some(disk) = self.parse_bdev_to_disk(bdev, node_name, &lvstores).await? {
                    disks.push(disk);
                }
            }
        }
        
        println!("📀 [SPDK_STATE] Found {} disks on node {}", disks.len(), node_name);
        Ok(disks)
    }

    /// Get volume replicas for a specific node
    async fn get_volumes_for_node(&self, node_name: &str, rpc_url: &str) -> Result<Vec<SpdkStateReplica>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [SPDK_STATE] Querying volumes on node: {}", node_name);
        
        // Query SPDK for logical volumes
        let lvols_response = self.call_node_rpc(rpc_url, "bdev_lvol_get_lvols", json!({})).await?;
        
        let mut replicas = Vec::new();
        
        if let Some(lvol_list) = lvols_response["result"].as_array() {
            for lvol in lvol_list {
                if let Some(replica) = self.parse_lvol_to_replica(lvol, node_name, rpc_url).await? {
                    replicas.push(replica);
                }
            }
        }
        
        println!("💾 [SPDK_STATE] Found {} volume replicas on node {}", replicas.len(), node_name);
        Ok(replicas)
    }

    /// Discover all nodes in the cluster (uses existing logic)
    async fn discover_cluster_nodes(&self) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
        // Reuse existing node discovery from spdk_dashboard_backend.rs
        use kube::{Api, api::ListParams};
        use k8s_openapi::api::core::v1::Pod;
        
        let mut nodes = HashMap::new();
        let pods_api: Api<Pod> = Api::all(self.driver.kube_client.clone());
        let lp = ListParams::default().labels("app=flint-csi-node");

        let pods = pods_api.list(&lp).await?;

        for pod in pods.items {
            let node_name = pod.spec.as_ref().and_then(|s| s.node_name.clone());
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());

            if let (Some(name), Some(ip)) = (node_name, pod_ip) {
                let url = format!("http://{}:8081/api/spdk/rpc", ip);
                nodes.insert(name, url);
            }
        }
        
        Ok(nodes)
    }

    /// Call RPC on a specific node (reuses existing infrastructure)
    async fn call_node_rpc(&self, rpc_url: &str, method: &str, params: Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let http_client = reqwest::Client::new();
        let response = http_client
            .post(rpc_url)
            .json(&json!({
                "method": method,
                "params": params,
                "id": 1
            }))
            .send()
            .await?;
            
        if !response.status().is_success() {
            return Err(format!("RPC call to {} failed: {}", rpc_url, response.status()).into());
        }
        
        let result: Value = response.json().await?;
        Ok(result)
    }

    /// Parse SPDK bdev JSON to SpdkStateDisk
    async fn parse_bdev_to_disk(&self, bdev: &Value, node_name: &str, lvstores: &Value) -> Result<Option<SpdkStateDisk>, Box<dyn std::error::Error + Send + Sync>> {
        // Extract key info from bdev JSON
        let name = bdev["name"].as_str().unwrap_or("");
        let block_size = bdev["block_size"].as_u64().unwrap_or(0);
        let num_blocks = bdev["num_blocks"].as_u64().unwrap_or(0);
        let product_name = bdev["product_name"].as_str().unwrap_or("");
        
        // Only process NVMe disks (not logical volumes)
        if !is_physical_nvme_disk(name, product_name) {
            return Ok(None);
        }
        
        let size_bytes = block_size * num_blocks;
        let (lvs_info, free_space) = self.find_lvs_for_bdev(name, lvstores);
        
        Ok(Some(SpdkStateDisk {
            node_name: node_name.to_string(),
            pci_address: extract_pci_from_bdev_name(name),
            device_name: extract_device_name(name),
            bdev_name: Some(name.to_string()),
            size_bytes,
            healthy: true, // TODO: Add health checks
            blobstore_initialized: lvs_info.is_some(),
            free_space,
            lvs_name: lvs_info,
            lvol_count: 0, // TODO: Count lvols
        }))
    }

    /// Parse SPDK lvol JSON to SpdkStateReplica  
    async fn parse_lvol_to_replica(&self, lvol: &Value, node_name: &str, _rpc_url: &str) -> Result<Option<SpdkStateReplica>, Box<dyn std::error::Error + Send + Sync>> {
        let uuid = lvol["uuid"].as_str().unwrap_or("");
        let name = lvol["name"].as_str().unwrap_or("");
        
        // Only process CSI volumes (skip other lvols)
        if !name.starts_with("vol_pvc-") {
            return Ok(None);
        }
        
        // TODO: Query NVMe-oF targets to get NQN/IP/port info
        
        Ok(Some(SpdkStateReplica {
            node_name: node_name.to_string(),
            disk_pci_address: "unknown".to_string(), // TODO: Extract from lvol store info
            lvol_uuid: uuid.to_string(),
            lvol_name: name.to_string(),
            nqn: None, // TODO: Query nvmf_get_subsystems
            target_ip: None,
            target_port: None,
            health: "online".to_string(), // TODO: Add health checks
        }))
    }

    /// Find LVS information for a given bdev
    fn find_lvs_for_bdev(&self, bdev_name: &str, lvstores: &Value) -> (Option<String>, u64) {
        if let Some(lvs_list) = lvstores["result"].as_array() {
            for lvs in lvs_list {
                if let Some(base_bdev) = lvs["base_bdev"].as_str() {
                    if base_bdev == bdev_name {
                        let lvs_name = lvs["name"].as_str().unwrap_or("").to_string();
                        let free_clusters = lvs["free_clusters"].as_u64().unwrap_or(0);
                        let cluster_size = lvs["cluster_size"].as_u64().unwrap_or(0);
                        let free_space = free_clusters * cluster_size;
                        return (Some(lvs_name), free_space);
                    }
                }
            }
        }
        (None, 0)
    }
}

/// Helper functions for parsing SPDK responses

fn is_physical_nvme_disk(name: &str, product_name: &str) -> bool {
    // Skip logical volumes and virtual devices
    if name.contains("/") || name.starts_with("malloc") || name.starts_with("null") {
        return false;
    }
    
    // Look for NVMe characteristics
    name.starts_with("nvme") || name.starts_with("kernel_nvme") || 
    product_name.contains("NVMe") || product_name.contains("SSD")
}

fn extract_pci_from_bdev_name(_name: &str) -> String {
    // TODO: Extract PCI address from bdev name or use other discovery method
    "0000:00:00.0".to_string()
}

fn extract_device_name(bdev_name: &str) -> String {
    // kernel_nvme0n1 -> nvme0n1
    if let Some(stripped) = bdev_name.strip_prefix("kernel_") {
        stripped.to_string()
    } else {
        bdev_name.to_string()
    }
}

fn extract_volume_id_from_lvol_name(lvol_name: &str) -> String {
    // vol_pvc-abc123 -> pvc-abc123
    if let Some(stripped) = lvol_name.strip_prefix("vol_") {
        stripped.to_string()
    } else {
        lvol_name.to_string()
    }
}
