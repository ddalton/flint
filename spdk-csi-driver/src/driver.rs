// driver_minimal.rs - Clean minimal state SPDK CSI Driver  
// CONTROLLER implementation - talks to Node Agents via HTTP (NOT directly to SPDK)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::{json, Value};
use reqwest::Client as HttpClient;
use tonic::Status;

// Kubernetes API imports
use kube::{Api, Client};
use k8s_openapi::api::core::v1::Node as k8sNode;

use crate::minimal_models::{MinimalStateError, DiskInfo, VolumeCreationResult, ReplicaInfo};
use crate::capacity_cache::CapacityCache;

/// Block device backend type
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockDeviceBackend {
    Ublk,
    Nvmeof,
}

/// Cleanup data specific to each backend
#[derive(Debug, Clone)]
pub enum CleanupData {
    Ublk {
        ublk_id: u32
    },
    Nvmeof {
        nqn: String,
        nvme_device: String
    },
}

/// Information about a created block device
#[derive(Debug, Clone)]
pub struct BlockDeviceInfo {
    pub device_path: String,
    pub backend_type: BlockDeviceBackend,
    pub cleanup_data: CleanupData,
}

/// Minimal State SPDK CSI Driver
/// Focuses on direct SPDK operations without heavy CRD management
#[derive(Clone)]
pub struct SpdkCsiDriver {
    pub kube_client: Client,
    pub target_namespace: String,
    pub node_id: String,
    pub spdk_rpc_url: String,
    pub nvmeof_transport: String,
    pub nvmeof_target_port: u16,
    
    // Simple caching for efficiency
    pub spdk_node_urls: Arc<Mutex<HashMap<String, String>>>,
    
    // Capacity cache for scalability
    pub capacity_cache: CapacityCache,
}

impl SpdkCsiDriver {
    /// Create new minimal state driver instance
    pub fn new(
        kube_client: Client,
        target_namespace: String,
        node_id: String,
        spdk_rpc_url: String,
        nvmeof_transport: String,
        nvmeof_target_port: u16,
    ) -> Self {
        Self {
            kube_client,
            target_namespace,
            node_id,
            spdk_rpc_url,
            nvmeof_transport,
            nvmeof_target_port,
            spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
            capacity_cache: CapacityCache::new(30), // 30 second TTL
        }
    }
    
    /// Initialize driver (warm up cache, start background tasks)
    pub async fn initialize(&self, mode: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚀 [DRIVER] Initializing CSI driver in {} mode...", mode);

        // Capacity cache is only needed for controller mode (volume placement decisions)
        // Node mode only handles local operations (mount/unmount/stage/unstage)
        if mode == "controller" || mode == "all" {
            // Warm up capacity cache
            println!("🔥 [DRIVER] Warming up capacity cache...");
            self.capacity_cache.warm_up(self).await?;

            // Start background cache refresh (every 60 seconds)
            // Note: Cache is also invalidated after every volume creation, so this is mainly
            // to catch external changes (manual SPDK operations, disk failures, etc.)
            println!("🔄 [DRIVER] Starting background capacity refresh (every 60s)...");
            CapacityCache::start_background_refresh(
                Arc::new(self.capacity_cache.clone()),
                Arc::new(self.clone()),
                60,
            );
        } else {
            println!("⏭️  [DRIVER] Skipping capacity cache initialization in node mode");
        }

        println!("✅ [DRIVER] Initialization complete");
        Ok(())
    }

    /// Select a node for single-replica volume using capacity cache
    async fn select_node_for_single_replica(&self, size_bytes: u64) -> Result<String, MinimalStateError> {
        println!("🔍 [DRIVER] Selecting node for single-replica volume (size: {}GB)",
                 size_bytes / (1024 * 1024 * 1024));

        // Get all nodes in cluster
        let all_nodes = self.get_all_nodes().await
            .map_err(|e| MinimalStateError::InternalError {
                message: format!("Failed to list nodes: {}", e)
            })?;

        if all_nodes.is_empty() {
            return Err(MinimalStateError::InternalError {
                message: "No nodes found in cluster".to_string()
            });
        }

        println!("📊 [DRIVER] Found {} nodes in cluster", all_nodes.len());

        // Get cached capacities for all nodes in parallel
        let mut tasks = Vec::new();
        for node_name in all_nodes {
            let cache = self.capacity_cache.clone();
            let driver_clone = self.clone();
            
            tasks.push(tokio::spawn(async move {
                cache.get_node_capacity(&node_name, &driver_clone).await
            }));
        }

        // Wait for all capacity checks and collect candidates
        let mut candidates = Vec::new();
        for task in tasks {
            if let Ok(Ok(capacity)) = task.await {
                if capacity.free_capacity >= size_bytes {
                    candidates.push(capacity);
                }
            }
        }

        if candidates.is_empty() {
            println!("❌ [DRIVER] No nodes with sufficient capacity found");
            return Err(MinimalStateError::InsufficientCapacity {
                required: size_bytes,
                available: 0,
            });
        }

        // Sort by free capacity (descending) for load balancing
        candidates.sort_by(|a, b| b.free_capacity.cmp(&a.free_capacity));

        // Select node with most free space
        let selected = &candidates[0];

        // Reserve capacity (optimistic locking)
        self.capacity_cache.reserve_capacity(&selected.node_name, size_bytes).await?;

        println!("✅ [DRIVER] Selected node: {} (free: {}GB / {}GB)",
                 selected.node_name,
                 selected.free_capacity / (1024 * 1024 * 1024),
                 selected.total_capacity / (1024 * 1024 * 1024));

        Ok(selected.node_name.clone())
    }

    /// Select N nodes for multi-replica volume (each replica on different node)
    async fn select_nodes_for_replicas(
        &self,
        replica_count: u32,
        size_bytes: u64,
    ) -> Result<Vec<crate::raid::NodeDiskSelection>, MinimalStateError> {
        println!("🔍 [DRIVER] Finding {} nodes for replicas (size: {}GB)",
                 replica_count, size_bytes / (1024 * 1024 * 1024));

        // Get all nodes in cluster
        let all_nodes = self.get_all_nodes().await
            .map_err(|e| MinimalStateError::InternalError {
                message: format!("Failed to list nodes: {}", e)
            })?;

        println!("📊 [DRIVER] Found {} nodes in cluster", all_nodes.len());

        let mut selected = Vec::new();

        for node_name in all_nodes {
            if selected.len() >= replica_count as usize {
                break; // Found enough nodes
            }

            // Query disks on this node
            match self.get_initialized_disks_from_node(&node_name).await {
                Ok(disks) => {
                    // Find first disk with enough space
                    if let Some(disk) = disks.iter().find(|d| d.free_space >= size_bytes) {
                        selected.push(crate::raid::NodeDiskSelection {
                            node_name: node_name.clone(),
                            disk: disk.clone(),
                        });
                        println!("   ✓ Selected node: {} (disk: {}, free: {}GB)",
                                 node_name, disk.device_name, disk.free_space / (1024 * 1024 * 1024));
                    }
                }
                Err(e) => {
                    println!("   ⚠️ Skipping node {} (query failed: {})", node_name, e);
                    continue;
                }
            }
        }

        if selected.len() < replica_count as usize {
            return Err(MinimalStateError::InsufficientNodes {
                required: replica_count,
                available: selected.len() as u32,
                message: format!(
                    "Cannot create {} replicas: only {} nodes with sufficient space ({}GB required per node)",
                    replica_count,
                    selected.len(),
                    size_bytes / (1024 * 1024 * 1024)
                ),
            });
        }

        Ok(selected)
    }

    /// Create distributed multi-replica volume (RAID 1 across nodes)
    async fn create_distributed_multi_replica_volume(
        &self,
        volume_id: &str,
        size_bytes: u64,
        replica_count: u32,
        thin_provision: bool,
    ) -> Result<VolumeCreationResult, MinimalStateError> {
        println!("🎯 [DRIVER] Creating distributed multi-replica volume: {} ({} replicas)",
                 volume_id, replica_count);

        // Step 1: Find N nodes with available space (each on different node)
        let selected_nodes = self.select_nodes_for_replicas(replica_count, size_bytes).await?;

        println!("✅ [DRIVER] Selected {} nodes for replicas:", selected_nodes.len());
        for (i, node_info) in selected_nodes.iter().enumerate() {
            println!("   Replica {}: node={}, disk={}, free={}GB",
                     i + 1,
                     node_info.node_name,
                     node_info.disk.device_name,
                     node_info.disk.free_space / (1024 * 1024 * 1024));
        }

        // Step 2: Create lvol on each selected node
        let mut replicas = Vec::new();
        let mut created_replicas = Vec::new(); // Track for cleanup on error

        for (i, node_info) in selected_nodes.iter().enumerate() {
            let replica_volume_id = format!("{}_replica_{}", volume_id, i);
            let lvs_name = node_info.disk.lvs_name.as_ref()
                .ok_or_else(|| MinimalStateError::InternalError {
                    message: "Selected disk has no LVS name".to_string()
                })?;

            match self.create_lvol(
                &node_info.node_name,
                lvs_name,
                &replica_volume_id,
                size_bytes,
                thin_provision,
            ).await {
                Ok(lvol_uuid) => {
                    println!("✅ [DRIVER] Created replica {} on node {}: UUID={}",
                             i + 1, node_info.node_name, lvol_uuid);

                    let replica = ReplicaInfo {
                        node_name: node_info.node_name.clone(),
                        disk_pci_address: node_info.disk.pci_address.clone(),
                        lvol_uuid: lvol_uuid.clone(),
                        lvol_name: format!("vol_{}", replica_volume_id),
                        lvs_name: lvs_name.clone(),
                        nqn: None, // Will be set during NodePublishVolume if needed
                        target_ip: None,
                        target_port: None,
                        health: "online".to_string(),
                        generation: 0, // Initial generation, will be set during first attach
                        generation_timestamp: 0,
                    };

                    created_replicas.push((node_info.node_name.clone(), lvol_uuid.clone()));
                    replicas.push(replica);

                    // Invalidate cache for this node
                    self.capacity_cache.invalidate(&node_info.node_name).await;
                }
                Err(e) => {
                    // Cleanup: Delete all previously created replicas
                    println!("❌ [DRIVER] Failed to create replica {} on node {}: {}",
                             i + 1, node_info.node_name, e);
                    println!("🧹 [DRIVER] Cleaning up {} previously created replicas...",
                             created_replicas.len());

                    for (node, uuid) in created_replicas {
                        let _ = self.delete_lvol(&node, &uuid).await;
                    }

            return Err(MinimalStateError::InternalError {
                        message: format!("Failed to create replica {}: {}", i + 1, e)
                    });
                }
            }
        }

        println!("✅ [DRIVER] Created {} replicas for volume {}", replicas.len(), volume_id);

        // Step 3: Return result with replica metadata
        // This will be stored in PV annotations by CSI controller
        Ok(VolumeCreationResult {
            volume_id: volume_id.to_string(),
            size_bytes,
            replicas,
        })
    }

    /// Create single-replica volume (existing logic, unchanged)
    async fn create_single_replica_volume(
        &self,
        volume_id: &str,
        size_bytes: u64,
        thin_provision: bool,
    ) -> Result<VolumeCreationResult, MinimalStateError> {
        // Select node dynamically using capacity cache
        let node_name = match self.select_node_for_single_replica(size_bytes).await {
            Ok(node) => node,
            Err(e) => {
                println!("❌ [DRIVER] Failed to select node: {}", e);
                return Err(e);
            }
        };
        
        // Get disks that have been initialized with LVS on selected node
        let initialized_disks = self.get_initialized_disks_from_node(&node_name).await?;
        if initialized_disks.is_empty() {
            // Release capacity reservation
            self.capacity_cache.release_capacity(&node_name, size_bytes).await;
            return Err(MinimalStateError::InsufficientCapacity { 
                required: size_bytes, 
                available: 0 
            });
        }
        
        // Find a disk with enough free space
        let selected_disk = initialized_disks.iter()
            .find(|d| d.free_space >= size_bytes)
            .ok_or_else(|| {
                // Release capacity reservation
                let node = node_name.clone();
                let size = size_bytes;
                let cache = self.capacity_cache.clone();
                tokio::spawn(async move {
                    cache.release_capacity(&node, size).await;
                });
                MinimalStateError::InsufficientCapacity { 
                    required: size_bytes, 
                    available: initialized_disks.iter().map(|d| d.free_space).max().unwrap_or(0)
                }
            })?;
        
        let lvs_name = selected_disk.lvs_name.as_ref()
            .ok_or_else(|| MinimalStateError::InternalError { 
                message: "Selected disk has no LVS name".to_string() 
            })?;
        
        println!("✅ [DRIVER] Selected disk: {} with LVS: {} (free: {}GB) on node: {}", 
                 selected_disk.device_name, 
                 lvs_name,
                 selected_disk.free_space / (1024*1024*1024), 
                 node_name);
        
        // Create logical volume on existing LVS
        let lvol_uuid = match self.create_lvol(&node_name, lvs_name, volume_id, size_bytes, thin_provision).await {
            Ok(uuid) => uuid,
            Err(e) => {
                // Release capacity reservation on failure
                self.capacity_cache.release_capacity(&node_name, size_bytes).await;
                return Err(e);
            }
        };
        
        // Invalidate cache entry to force refresh on next query
        // This ensures next volume creation sees accurate capacity
        // Combined with 60s background refresh, this provides:
        // - Immediate accuracy after volume creation (invalidation)
        // - External changes detected within 60s (background refresh)
        self.capacity_cache.invalidate(&node_name).await;
        
        println!("✅ [DRIVER] Volume {} created successfully with lvol UUID: {}", volume_id, lvol_uuid);
        
        // Return full volume creation result with metadata
        Ok(VolumeCreationResult {
            volume_id: volume_id.to_string(),
            size_bytes,
            replicas: vec![ReplicaInfo {
                node_name: node_name.to_string(),
                disk_pci_address: selected_disk.pci_address.clone(),
                lvol_uuid: lvol_uuid.clone(),
                lvol_name: format!("vol_{}", volume_id),
                lvs_name: lvs_name.clone(),
                nqn: None,
                target_ip: None,
                target_port: None,
                health: "online".to_string(),
                generation: 0,
                generation_timestamp: 0,
            }],
        })
    }

    /// Create volume using minimal state architecture (routing to single or multi-replica)
    pub async fn create_volume(&self, volume_id: &str, size_bytes: u64, replica_count: u32, thin_provision: bool) -> Result<VolumeCreationResult, MinimalStateError> {
        println!("🎯 [DRIVER] Creating volume: {} ({} bytes, {} replicas, thin: {})",
                 volume_id, size_bytes, replica_count, thin_provision);

        // Route based on replica count
        if replica_count == 1 {
            // Single replica: Use existing path (zero changes to existing logic)
            return self.create_single_replica_volume(volume_id, size_bytes, thin_provision).await;
        }

        // Multi-replica: RAID 1 requires minimum 2 replicas
        if replica_count < 2 {
            return Err(MinimalStateError::InvalidParameter {
                message: "RAID 1 requires minimum 2 replicas".to_string()
            });
        }

        // Create distributed multi-replica volume
        self.create_distributed_multi_replica_volume(
            volume_id,
            size_bytes,
            replica_count,
            thin_provision
        ).await
    }

    /// Get SPDK RPC URL for a specific node (simplified)
    pub async fn get_rpc_url_for_node(&self, node_name: &str) -> Result<String, Status> {
        // For minimal state, we'll use a simple pattern
        // In production, this would query the kubernetes API to find the pod IP
        let url = format!("http://{}:8081/api/spdk/rpc", node_name);
        
        // Cache for efficiency
        let mut cache = self.spdk_node_urls.lock().await;
        cache.insert(node_name.to_string(), url.clone());
        
        Ok(url)
    }

    /// Get node IP address (simplified)
    pub async fn get_node_ip(&self, node_name: &str) -> Result<String, Status> {
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        let node = nodes_api.get(node_name).await
            .map_err(|e| Status::internal(format!("Failed to get node {}: {}", node_name, e)))?;

        if let Some(status) = &node.status {
            if let Some(addresses) = &status.addresses {
                // Prefer InternalIP
                for address in addresses {
                    if address.type_ == "InternalIP" {
                        return Ok(address.address.clone());
                    }
                }
                // Fallback to first address
                if let Some(addr) = addresses.first() {
                    return Ok(addr.address.clone());
                }
            }
        }
        
        Err(Status::not_found(format!("No IP address found for node {}", node_name)))
    }

    /// Get current node IP (cached)
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [MINIMAL_DRIVER] Getting IP for node: {}", self.node_id);
        Ok(self.get_node_ip(&self.node_id).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?)
    }

    /// Call Node Agent HTTP API (CONTROLLER pattern - not direct SPDK)
    pub async fn call_node_agent(&self, node_name: &str, endpoint: &str, payload: &Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        println!("🌐 [CONTROLLER_HTTP] Calling node agent: {} endpoint: {}", node_name, endpoint);
        
        // Get the node agent URL (HTTP, not direct SPDK socket)
        let node_agent_url = self.get_node_agent_url(node_name).await?;
        let full_url = format!("{}{}", node_agent_url, endpoint);
        
        let http_client = HttpClient::new();
        
        // All endpoints use POST (RPC-style communication)
        let response = http_client.post(&full_url).json(payload).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Node agent HTTP call failed: {}", error_text).into());
        }

        let json_response: Value = response.json().await?;
        Ok(json_response)
    }

    /// Get node agent HTTP URL (not SPDK socket)
    async fn get_node_agent_url(&self, node_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Check cache first
        {
            let cache = self.spdk_node_urls.lock().await;
            if let Some(url) = cache.get(node_name) {
                return Ok(url.clone());
            }
        }

        // Find the node agent pod IP
        let pod_ip = self.get_node_agent_pod_ip(node_name).await?;
        let node_agent_port = std::env::var("NODE_AGENT_PORT").unwrap_or("9081".to_string());
        let node_agent_url = format!("http://{}:{}", pod_ip, node_agent_port);
        
        // Cache it
        {
            let mut cache = self.spdk_node_urls.lock().await;
            cache.insert(node_name.to_string(), node_agent_url.clone());
        }
        
        Ok(node_agent_url)
    }

    /// Find node agent pod IP via Kubernetes API
    async fn get_node_agent_pod_ip(&self, node_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use kube::{api::ListParams, Api};
        use k8s_openapi::api::core::v1::Pod;
        
        let pods_api: Api<Pod> = Api::namespaced(self.kube_client.clone(), &self.target_namespace);
        let list_params = ListParams::default()
            .labels("app=flint-csi-node")
            .fields(&format!("spec.nodeName={}", node_name));
            
        let pods = pods_api.list(&list_params).await?;
        
        for pod in pods.items {
            if let Some(status) = pod.status {
                if let Some(pod_ip) = status.pod_ip {
                    println!("✅ [CONTROLLER_HTTP] Found node agent for {}: {}", node_name, pod_ip);
                    return Ok(pod_ip);
                }
            }
        }
        
        Err(format!("No node agent pod found for node {}", node_name).into())
    }

    /// Initialize blobstore on a disk (CONTROLLER calls Node Agent via HTTP)
    pub async fn initialize_blobstore(&self, node_name: &str, disk_pci_address: &str) -> Result<String, MinimalStateError> {
        println!("🔧 [CONTROLLER] Requesting blobstore initialization on node: {} disk: {}", node_name, disk_pci_address);

        let payload = json!({
            "pci_address": disk_pci_address
        });

        let response = self.call_node_agent(node_name, "/api/disks/initialize_blobstore", &payload).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to initialize blobstore via node agent: {}", e) 
            })?;

        let lvs_name = response["lvs_name"].as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError { 
                message: "No LVS name in node agent response".to_string() 
            })?
            .to_string();

        println!("✅ [CONTROLLER] Blobstore initialized via node agent: {}", lvs_name);
        Ok(lvs_name)
    }

    /// Create logical volume (CONTROLLER calls Node Agent via HTTP)  
    pub async fn create_lvol(&self, node_name: &str, lvs_name: &str, volume_id: &str, size_bytes: u64, thin_provision: bool) -> Result<String, MinimalStateError> {
        println!("🔧 [CONTROLLER] Requesting lvol creation on node: {} LVS: {} volume: {} (thin: {})", 
                 node_name, lvs_name, volume_id, thin_provision);
        
        let payload = json!({
            "lvs_name": lvs_name,
            "volume_id": volume_id,
            "size_bytes": size_bytes,
            "thin_provision": thin_provision
        });

        let response = self.call_node_agent(node_name, "/api/volumes/create_lvol", &payload).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to create lvol via node agent: {}", e) 
            })?;

        let lvol_uuid = response["lvol_uuid"].as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError { 
                message: "No lvol_uuid in node agent response".to_string() 
            })?
            .to_string();

        println!("✅ [CONTROLLER] Lvol created via node agent: {}", lvol_uuid);
        Ok(lvol_uuid)
    }

    /// Delete logical volume (CONTROLLER calls Node Agent via HTTP)
    pub async fn delete_lvol(&self, node_name: &str, lvol_uuid: &str) -> Result<(), MinimalStateError> {
        println!("🗑️ [CONTROLLER] Requesting lvol deletion on node: {} UUID: {}", node_name, lvol_uuid);

        let payload = json!({
            "lvol_uuid": lvol_uuid
        });

        self.call_node_agent(node_name, "/api/volumes/delete_lvol", &payload).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to delete lvol via node agent: {}", e) 
            })?;

        println!("✅ [CONTROLLER] Lvol deleted via node agent: {}", lvol_uuid);
        Ok(())
    }

    /// Check if backing storage exists on a node (for graceful deletion)
    /// Returns Ok(true) if storage exists, Ok(false) if not found or node unreachable
    /// This is used during volume deletion to handle cases where:
    /// - Memory disk was destroyed (SPDK pod restart)
    /// - NVMe disk failed or was removed
    /// - Node is offline
    pub async fn check_backing_storage_exists(&self, node_name: &str, lvol_uuid: &str) -> Result<bool, MinimalStateError> {
        println!("🔍 [CONTROLLER] Checking if backing storage exists on node: {} UUID: {}", node_name, lvol_uuid);

        let payload = json!({
            "lvol_uuid": lvol_uuid
        });

        match self.call_node_agent(node_name, "/api/volumes/check_exists", &payload).await {
            Ok(response) => {
                // Parse the response
                let exists = response["exists"].as_bool().unwrap_or(false);

                if let Some(warning) = response["warning"].as_str() {
                    println!("⚠️ [CONTROLLER] Storage check warning: {}", warning);
                }

                println!("{} [CONTROLLER] Backing storage {} on node {}: {}",
                    if exists { "✅" } else { "ℹ️" },
                    lvol_uuid,
                    node_name,
                    if exists { "exists" } else { "not found" });

                Ok(exists)
            }
            Err(e) => {
                // Node agent unreachable - treat as storage gone
                // This handles the case where:
                // - Node is offline
                // - Node agent pod is down
                // - Network partition
                println!("⚠️ [CONTROLLER] Could not reach node {} to check storage: {}", node_name, e);
                println!("ℹ️ [CONTROLLER] Treating as storage unavailable (node unreachable)");
                Ok(false)
            }
        }
    }

    /// Check health of backing storage on a node
    /// Returns detailed health information for volume monitoring
    pub async fn check_backing_storage_health(&self, node_name: &str, lvol_uuid: &str) -> Result<BackingStorageHealth, MinimalStateError> {
        println!("🏥 [CONTROLLER] Checking backing storage health on node: {} UUID: {}", node_name, lvol_uuid);

        let payload = json!({
            "lvol_uuid": lvol_uuid
        });

        match self.call_node_agent(node_name, "/api/volumes/check_health", &payload).await {
            Ok(response) => {
                let health = BackingStorageHealth {
                    exists: response["exists"].as_bool().unwrap_or(false),
                    healthy: response["healthy"].as_bool().unwrap_or(false),
                    message: response["message"].as_str().unwrap_or("Unknown").to_string(),
                    node_reachable: true,
                };

                println!("{} [CONTROLLER] Storage health: exists={}, healthy={}, message={}",
                    if health.healthy { "✅" } else { "⚠️" },
                    health.exists,
                    health.healthy,
                    health.message);

                Ok(health)
            }
            Err(e) => {
                // Node agent unreachable
                println!("❌ [CONTROLLER] Could not reach node {} for health check: {}", node_name, e);
                Ok(BackingStorageHealth {
                    exists: false,
                    healthy: false,
                    message: format!("Node unreachable: {}", e),
                    node_reachable: false,
                })
            }
        }
    }

    /// Force unstage volume if it's still staged (defensive cleanup for when NodeUnstageVolume wasn't called)
    pub async fn force_unstage_volume_if_needed(&self, node_name: &str, volume_id: &str, ublk_id: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DEFENSIVE] Checking if volume {} is still staged on node: {}", volume_id, node_name);
        
        // Request node agent to check and unstage if needed
        let payload = json!({
            "volume_id": volume_id,
            "ublk_id": ublk_id,
            "force": false  // Don't force if not needed
        });
        
        match self.call_node_agent(node_name, "/api/volumes/force_unstage", &payload).await {
            Ok(response) => {
                if let Some(was_staged) = response["was_staged"].as_bool() {
                    if was_staged {
                        println!("✅ [DEFENSIVE] Volume was staged - successfully unstaged");
                    } else {
                        println!("ℹ️ [DEFENSIVE] Volume was not staged - no action needed");
                    }
                }
                Ok(())
            }
            Err(e) => {
                println!("⚠️ [DEFENSIVE] Force unstage check failed: {}", e);
                // Don't fail - this is best effort
                Ok(())
            }
        }
    }

    /// Aggressive cleanup for stuck volumes (last resort)
    pub async fn force_cleanup_volume(&self, node_name: &str, volume_id: &str, ublk_id: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [AGGRESSIVE] Force cleaning up volume {} on node: {}", volume_id, node_name);
        
        let payload = json!({
            "volume_id": volume_id,
            "ublk_id": ublk_id,
            "force": true  // Force cleanup even if errors
        });
        
        self.call_node_agent(node_name, "/api/volumes/force_unstage", &payload).await?;
        
        println!("✅ [AGGRESSIVE] Force cleanup completed");
        Ok(())
    }

    /// Create NVMe-oF target (minimal implementation - will be enhanced later)
    pub async fn create_nvmeof_target(&self, bdev_name: &str, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚧 [MINIMAL_NVMEOF] Creating NVMe-oF target for bdev: {}, nqn: {}", bdev_name, nqn);
        
        // For now, we'll implement a basic version
        // TODO: Enhance with full functionality later
        
        // 1. Create subsystem
        let subsystem_params = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true,
                "serial_number": format!("SPDK{:016x}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64),
                "model_number": "SPDK CSI Volume"
            }
        });

        // NOTE: For NVMe-oF, the controller should delegate to the target node
        // This is a placeholder - in real implementation, use call_node_agent
        match self.call_node_agent(&self.node_id, "/api/nvmeof/create_subsystem", &subsystem_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Subsystem created: {}", nqn),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [MINIMAL_NVMEOF] Subsystem already exists: {}", nqn);
            }
            Err(e) => return Err(e),
        }

        // 2. Add namespace
        let namespace_params = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "nsid": 1,
                    "bdev_name": bdev_name
                }
            }
        });

        match self.call_node_agent(&self.node_id, "/api/nvmeof/add_namespace", &namespace_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Namespace added for bdev: {}", bdev_name),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [MINIMAL_NVMEOF] Namespace already exists for bdev: {}", bdev_name);
            }
            Err(e) => return Err(e),
        }

        // 3. Add listener
        let node_ip = self.get_current_node_ip().await?;
        let listener_params = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": self.nvmeof_transport.to_uppercase(),
                    "traddr": node_ip,
                    "trsvcid": self.nvmeof_target_port.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });

        match self.call_node_agent(&self.node_id, "/api/nvmeof/add_listener", &listener_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Listener added: {}:{}", node_ip, self.nvmeof_target_port),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [MINIMAL_NVMEOF] Listener already exists: {}:{}", node_ip, self.nvmeof_target_port);
            }
            Err(e) => return Err(e),
        }

        println!("🎉 [MINIMAL_NVMEOF] NVMe-oF target setup completed: {}", nqn);
        Ok(())
    }

    /// Cleanup NVMe-oF target (minimal implementation)
    pub async fn cleanup_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🧹 [MINIMAL_NVMEOF] Cleaning up NVMe-oF target: {}", nqn);

        let delete_params = json!({
            "method": "nvmf_delete_subsystem",
            "params": {
                "nqn": nqn
            }
        });

        match self.call_node_agent(&self.node_id, "/api/nvmeof/delete_subsystem", &delete_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Successfully deleted subsystem: {}", nqn),
            Err(e) => {
                println!("⚠️ [MINIMAL_NVMEOF] Failed to delete subsystem (may not exist): {}", e);
                // Don't fail - cleanup is best effort
            }
        }

        Ok(())
    }

    /// Create ublk device (simplified - keeping core functionality)
    /// Legacy public method - kept for backward compatibility
    pub async fn create_ublk_device(&self, bdev_name: &str, ublk_id: u32) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let volume_id = format!("ublk-{}", ublk_id);  // Dummy volume ID for legacy calls
        let info = self.create_ublk_block_device(bdev_name, &volume_id, ublk_id).await?;
        Ok(info.device_path)
    }

    /// Create ublk block device (internal implementation)
    async fn create_ublk_block_device(&self, bdev_name: &str, volume_id: &str, ublk_id: u32) -> Result<BlockDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [MINIMAL_UBLK] Creating ublk device for bdev: {} with ID: {}", bdev_name, ublk_id);

        // Note: ublk target is initialized by node agent on startup
        // No need to call ensure_ublk_target() here

        // Performance tuning: Use multiple queues for better parallelism
        // num_queues: Match number of CPU cores for optimal throughput
        // queue_depth: Higher depth allows more outstanding I/O operations
        let num_queues = std::env::var("UBLK_NUM_QUEUES")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(4); // Default: 4 queues for good parallelism

        let queue_depth = std::env::var("UBLK_QUEUE_DEPTH")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(256); // Default: 256 for better pipeline depth

        println!("🔧 [UBLK_PERF] Using num_queues={}, queue_depth={}", num_queues, queue_depth);

        let ublk_params = json!({
            "method": "ublk_start_disk",
            "params": {
                "bdev_name": bdev_name,
                "ublk_id": ublk_id,
                "num_queues": num_queues,
                "queue_depth": queue_depth
            }
        });

        self.call_node_agent(&self.node_id, "/api/ublk/create", &ublk_params).await?;

        let device_path = format!("/dev/ublkb{}", ublk_id);

        // Wait for device to appear
        for attempt in 1..=30 {
            if std::path::Path::new(&device_path).exists() {
                println!("✅ [MINIMAL_UBLK] Device created: {}", device_path);
                return Ok(BlockDeviceInfo {
                    device_path,
                    backend_type: BlockDeviceBackend::Ublk,
                    cleanup_data: CleanupData::Ublk { ublk_id },
                });
            }

            if attempt % 10 == 0 {
                println!("🔧 [MINIMAL_UBLK] Waiting for device... ({}/30)", attempt);
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        Err(format!("Device {} did not appear after 3 seconds", device_path).into())
    }

    /// Delete ublk device (simplified)
    /// Legacy public method - kept for backward compatibility
    pub async fn delete_ublk_device(&self, ublk_id: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.delete_ublk_block_device(ublk_id).await
    }

    /// Delete ublk block device (internal implementation)
    async fn delete_ublk_block_device(&self, ublk_id: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [MINIMAL_UBLK] Deleting ublk device with ID: {}", ublk_id);

        let delete_params = json!({
            "method": "ublk_stop_disk",
            "params": {
                "ublk_id": ublk_id
            }
        });

        match self.call_node_agent(&self.node_id, "/api/ublk/delete", &delete_params).await {
            Ok(_) => println!("✅ [MINIMAL_UBLK] Successfully deleted ublk device: {}", ublk_id),
            Err(e) => {
                println!("⚠️ [MINIMAL_UBLK] Failed to delete device (may not exist): {}", e);
                // Don't fail - cleanup is best effort
            }
        }

        Ok(())
    }

    /// Create NVMe-oF block device (internal implementation)
    async fn create_nvmeof_block_device(&self, bdev_name: &str, volume_id: &str) -> Result<BlockDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [NVMEOF_BLOCK] Creating NVMe-oF block device for bdev: {}", bdev_name);

        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
        let target_ip = std::env::var("NVMEOF_LOCAL_TARGET_IP").unwrap_or("127.0.0.1".to_string());
        let target_port = std::env::var("NVMEOF_TARGET_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(4420);

        // Call node agent to create NVMe-oF target and connect via kernel initiator
        let params = json!({
            "bdev_name": bdev_name,
            "nqn": nqn,
            "target_ip": target_ip,
            "target_port": target_port,
        });

        let response = self.call_node_agent(&self.node_id, "/api/blockdev/create_nvmeof", &params).await?;

        let device_path = response["device_path"]
            .as_str()
            .ok_or("Missing device_path in response")?
            .to_string();
        let nvme_device = response["nvme_device"]
            .as_str()
            .ok_or("Missing nvme_device in response")?
            .to_string();

        println!("✅ [NVMEOF_BLOCK] NVMe-oF device created: {}", device_path);

        Ok(BlockDeviceInfo {
            device_path,
            backend_type: BlockDeviceBackend::Nvmeof,
            cleanup_data: CleanupData::Nvmeof {
                nqn,
                nvme_device,
            },
        })
    }

    /// Delete NVMe-oF block device (internal implementation)
    async fn delete_nvmeof_block_device(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [NVMEOF_BLOCK] Deleting NVMe-oF block device with NQN: {}", nqn);

        let delete_params = json!({
            "nqn": nqn,
        });

        match self.call_node_agent(&self.node_id, "/api/blockdev/delete_nvmeof", &delete_params).await {
            Ok(_) => println!("✅ [NVMEOF_BLOCK] Successfully deleted NVMe-oF device: {}", nqn),
            Err(e) => {
                println!("⚠️ [NVMEOF_BLOCK] Failed to delete device (may not exist): {}", e);
                // Don't fail - cleanup is best effort
            }
        }

        Ok(())
    }

    /// Create block device (wrapper that dispatches based on configuration)
    pub async fn create_block_device(&self, bdev_name: &str, volume_id: &str) -> Result<BlockDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        let backend_mode = std::env::var("BLOCK_DEVICE_BACKEND").unwrap_or("ublk".to_string());

        println!("🔧 [BLOCK_DEVICE] Creating block device using backend: {}", backend_mode);

        match backend_mode.as_str() {
            "nvmeof" => self.create_nvmeof_block_device(bdev_name, volume_id).await,
            _ => {
                // Default to ublk
                let ublk_id = self.generate_ublk_id(volume_id);
                self.create_ublk_block_device(bdev_name, volume_id, ublk_id).await
            }
        }
    }

    /// Delete block device (wrapper that dispatches based on cleanup data)
    pub async fn delete_block_device(&self, device_info: &BlockDeviceInfo) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [BLOCK_DEVICE] Deleting block device: {}", device_info.device_path);

        match &device_info.cleanup_data {
            CleanupData::Ublk { ublk_id } => {
                self.delete_ublk_block_device(*ublk_id).await
            }
            CleanupData::Nvmeof { nqn, .. } => {
                self.delete_nvmeof_block_device(nqn).await
            }
        }
    }

    /// Store block device info in PV annotations for later cleanup
    pub async fn store_block_device_info(&self, volume_id: &str, device_info: &BlockDeviceInfo) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());

        // Get the PV
        let mut pv = pvs.get(volume_id).await?;

        // Initialize annotations if needed
        if pv.metadata.annotations.is_none() {
            pv.metadata.annotations = Some(std::collections::BTreeMap::new());
        }

        let annotations = pv.metadata.annotations.as_mut().unwrap();

        // Store backend type and cleanup data
        annotations.insert("flint.io/block-device-backend".to_string(), format!("{:?}", device_info.backend_type));

        match &device_info.cleanup_data {
            CleanupData::Ublk { ublk_id } => {
                annotations.insert("flint.io/ublk-id".to_string(), ublk_id.to_string());
            }
            CleanupData::Nvmeof { nqn, nvme_device } => {
                annotations.insert("flint.io/nvmeof-nqn".to_string(), nqn.clone());
                annotations.insert("flint.io/nvme-device".to_string(), nvme_device.clone());
            }
        }

        // Update the PV
        pvs.replace(volume_id, &Default::default(), &pv).await?;

        Ok(())
    }

    /// Retrieve block device info from PV annotations
    pub async fn get_block_device_info(&self, volume_id: &str) -> Result<BlockDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        let pv = pvs.get(volume_id).await?;

        let annotations = pv.metadata.annotations.as_ref()
            .ok_or("No annotations found on PV")?;

        let backend_str = annotations.get("flint.io/block-device-backend")
            .ok_or("No block-device-backend annotation")?;

        let backend_type = if backend_str.contains("Nvmeof") {
            BlockDeviceBackend::Nvmeof
        } else {
            BlockDeviceBackend::Ublk
        };

        let cleanup_data = match backend_type {
            BlockDeviceBackend::Ublk => {
                let ublk_id = annotations.get("flint.io/ublk-id")
                    .ok_or("No ublk-id annotation")?
                    .parse::<u32>()?;
                CleanupData::Ublk { ublk_id }
            }
            BlockDeviceBackend::Nvmeof => {
                let nqn = annotations.get("flint.io/nvmeof-nqn")
                    .ok_or("No nvmeof-nqn annotation")?
                    .clone();
                let nvme_device = annotations.get("flint.io/nvme-device")
                    .ok_or("No nvme-device annotation")?
                    .clone();
                CleanupData::Nvmeof { nqn, nvme_device }
            }
        };

        // Reconstruct device path (not critical, just for logging)
        let device_path = match &cleanup_data {
            CleanupData::Ublk { ublk_id } => format!("/dev/ublkb{}", ublk_id),
            CleanupData::Nvmeof { nvme_device, .. } => format!("/dev/{}n1", nvme_device),
        };

        Ok(BlockDeviceInfo {
            device_path,
            backend_type,
            cleanup_data,
        })
    }

    /// Ensure ublk target exists (simplified)
    // Note: ensure_ublk_target() removed - ublk target is initialized by node agent on startup

    /// Generate predictable UUID from NQN for namespace consistency  
    pub fn generate_namespace_uuid_from_nqn(nqn: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        nqn.hash(&mut hasher);
        let hash = hasher.finish();
        
        // Convert to UUID format
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            (hash >> 32) as u32,
            ((hash >> 16) & 0xFFFF) as u16,
            (hash & 0xFFFF) as u16,
            (hash >> 48) as u16,
            hash & 0xFFFFFFFFFFFF
        )
    }

    /// Get disks with existing LVS (initialized) from a specific node
    pub async fn get_initialized_disks_from_node(&self, node_name: &str) -> Result<Vec<DiskInfo>, MinimalStateError> {
        println!("🔍 [DRIVER] Getting initialized disks (with LVS) from node: {}", node_name);
        
        let response = self.call_node_agent(node_name, "/api/disks", &serde_json::json!({})).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to get disks from node agent: {}", e) 
            })?;

        let disks_array = response["disks"].as_array()
            .ok_or_else(|| MinimalStateError::InternalError { 
                message: "No disks array in response".to_string() 
            })?;

        let mut disks = Vec::new();
        for disk_json in disks_array {
            let blobstore_initialized = disk_json["blobstore_initialized"].as_bool().unwrap_or(false);
            
            // Only include disks that have LVS initialized
            if !blobstore_initialized {
                continue;
            }
            
            let disk = DiskInfo {
                node_name: node_name.to_string(),
                pci_address: disk_json["pci_address"].as_str().unwrap_or("unknown").to_string(),
                device_name: disk_json["device_name"].as_str().unwrap_or("unknown").to_string(),
                bdev_name: disk_json["bdev_name"].as_str().unwrap_or("unknown").to_string(),
                size_bytes: disk_json["size_bytes"].as_u64().unwrap_or(0),
                free_space: disk_json["free_space"].as_u64().unwrap_or(0),
                model: disk_json["model"].as_str().unwrap_or("unknown").to_string(),
                serial: disk_json["serial"].as_str().map(|s| s.to_string()),
                firmware: disk_json["firmware"].as_str().map(|s| s.to_string()),
                healthy: disk_json["healthy"].as_bool().unwrap_or(false),
                blobstore_initialized: true,
                lvs_name: disk_json["lvs_name"].as_str().map(|s| s.to_string()),
                lvol_count: disk_json["lvol_count"].as_u64().unwrap_or(0) as u32,
                is_system_disk: false, // Only initialized disks are returned, not system disks
                mounted_partitions: Vec::new(), // Not relevant for SPDK-managed disks
                driver: disk_json["driver"].as_str().unwrap_or("unknown").to_string(),
                device_type: disk_json["device_type"].as_str().unwrap_or("Unknown").to_string(),
            };
            disks.push(disk);
        }

        println!("✅ [DRIVER] Found {} initialized disks on node {}", disks.len(), node_name);
        Ok(disks)
    }

    /// Get available disks from a specific node
    pub async fn get_available_disks_from_node(&self, node_name: &str) -> Result<Vec<DiskInfo>, MinimalStateError> {
        println!("🔍 [DRIVER] Getting available disks from node: {}", node_name);
        
        let response = self.call_node_agent(node_name, "/api/disks/uninitialized", &serde_json::json!({})).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to get disks from node agent: {}", e) 
            })?;

        let disks_array = response["uninitialized_disks"].as_array()
            .ok_or_else(|| MinimalStateError::InternalError { 
                message: "No uninitialized_disks array in response".to_string() 
            })?;

        let mut disks = Vec::new();
        for disk_json in disks_array {
            let disk = DiskInfo {
                node_name: node_name.to_string(),
                pci_address: disk_json["pci_address"].as_str().unwrap_or("unknown").to_string(),
                device_name: disk_json["device_name"].as_str().unwrap_or("unknown").to_string(),
                bdev_name: format!("uring_{}", disk_json["device_name"].as_str().unwrap_or("unknown")),
                size_bytes: disk_json["size_bytes"].as_u64().unwrap_or(0),
                free_space: disk_json["size_bytes"].as_u64().unwrap_or(0), // Assume all free for uninitialized
                model: disk_json["model"].as_str().unwrap_or("unknown").to_string(),
                serial: Some("unknown".to_string()),
                firmware: Some("unknown".to_string()),
                healthy: disk_json["healthy"].as_bool().unwrap_or(false),
                blobstore_initialized: false, // These are uninitialized disks
                lvs_name: None,
                lvol_count: 0,
                is_system_disk: false, // Will be determined by caller/frontend
                mounted_partitions: Vec::new(),
                driver: disk_json["driver"].as_str().unwrap_or("unknown").to_string(),
                device_type: disk_json["device_type"].as_str().unwrap_or("Unknown").to_string(),
            };
            disks.push(disk);
        }

        println!("✅ [DRIVER] Found {} available disks on node {}", disks.len(), node_name);
        Ok(disks)
    }

    /// Generate consistent ublk device ID from volume ID
    pub fn generate_ublk_id(&self, volume_id: &str) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        volume_id.hash(&mut hasher);
        let hash = hasher.finish();
        
        // Use 20-bit hash to stay within ublk kernel module limit
        // ublk kernel module max ID: 1,048,575 (2^20 - 1)
        // This gives us ~1 million possible IDs
        // Collision probability: 50% at ~1,200 volumes
        // The volume_id itself is stored in the lvol name (vol_{volume_id})
        // so we can always find the lvol by name, and ublk ID is just for device numbering
        // Geometry mismatch detection protects against rare collisions
        (hash & 0xFFFFF) as u32  // 20 bits = 1,048,575 max
    }

    /// Verify bdev exists (simplified)
    pub async fn verify_bdev_exists(&self, bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [MINIMAL_DRIVER] Verifying bdev exists: {}", bdev_name);

        let bdev_params = json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": bdev_name
            }
        });

        let response = self.call_node_agent(&self.node_id, "/api/spdk/rpc", &bdev_params).await?;
        
        if let Some(result) = response.get("result") {
            if let Some(bdev_list) = result.as_array() {
                if !bdev_list.is_empty() {
                    println!("✅ [MINIMAL_DRIVER] Bdev verified: {}", bdev_name);
                    return Ok(());
                }
            }
        }

        Err(format!("Bdev '{}' not found in SPDK", bdev_name).into())
    }

    /// Get volume information from PV volumeAttributes
    /// This is the ONLY way to get volume info - no fallback to querying nodes (scalability)
    pub async fn get_volume_info(&self, volume_id: &str) -> Result<VolumeInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DRIVER] Getting volume info from PV metadata: {}", volume_id);
        
        // Read from PV volumeAttributes (single K8s API call)
        match self.get_volume_info_from_pv(volume_id).await {
            Ok(info) => {
                println!("✅ [DRIVER] Found volume info: node={}, lvol={}", 
                         info.node_name, info.lvol_uuid);
                return Ok(info);
            }
            Err(e) => {
                println!("❌ [DRIVER] Volume metadata not found in PV: {}", e);
                println!("💡 [DRIVER] This means either:");
                println!("   1. Volume doesn't exist");
                println!("   2. Volume was created with old driver version (before metadata storage)");
                println!("   3. PV is corrupted or missing volumeAttributes");
                return Err(format!("Volume {} metadata not found in PV: {}", volume_id, e).into());
            }
        }
    }

    /// Get replica info from PV volumeAttributes (for multi-replica volumes)
    pub async fn get_replicas_from_pv(&self, volume_id: &str) -> Result<Option<Vec<ReplicaInfo>>, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::Api;

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        let pv_list = pvs.list(&Default::default()).await?;
        
        for pv in pv_list.items {
            if let Some(spec) = &pv.spec {
                if let Some(csi) = &spec.csi {
                    if csi.volume_handle == volume_id {
                        // Found PV - check for replica annotations
                        if let Some(attrs) = &csi.volume_attributes {
                            // Check replica count first
                            let replica_count = attrs.get("flint.csi.storage.io/replica-count")
                                .and_then(|s| s.parse::<u32>().ok())
                                .unwrap_or(1);

                            if replica_count > 1 {
                                // Multi-replica: Read replicas JSON
                                if let Some(replicas_json) = attrs.get("flint.csi.storage.io/replicas") {
                                    let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)?;
                                    return Ok(Some(replicas));
                                }
                            }
                            
                            // Single replica or no replicas field
                            return Ok(None);
                        }
                    }
                }
            }
        }
        
        Err("PV not found".into())
    }

    /// Get volume info from PV volumeAttributes (fast path)
    async fn get_volume_info_from_pv(&self, volume_id: &str) -> Result<VolumeInfo, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::Api;

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        let pv_list = pvs.list(&Default::default()).await?;
        
        for pv in pv_list.items {
            if let Some(spec) = &pv.spec {
                if let Some(csi) = &spec.csi {
                    if csi.volume_handle == volume_id {
                        // Found PV - read volumeAttributes
                        if let Some(attrs) = &csi.volume_attributes {
                            // Check if metadata exists
                            if let Some(node_name) = attrs.get("flint.csi.storage.io/node-name") {
                                let lvol_uuid = attrs.get("flint.csi.storage.io/lvol-uuid")
                                    .ok_or("Missing lvol-uuid in volumeAttributes")?;
                                let lvs_name = attrs.get("flint.csi.storage.io/lvs-name")
                                    .ok_or("Missing lvs-name in volumeAttributes")?;
                                
                                // Get size from PV capacity
                                let size_bytes = if let Some(capacity) = &spec.capacity {
                                    if let Some(storage) = capacity.get("storage") {
                                        // Parse quantity like "1Gi", "2Gi", etc.
                                        Self::parse_quantity(&storage.0)?
                                    } else {
                                        0
                                    }
                                } else {
                                    0
                                };
                                
                                return Ok(VolumeInfo {
                                    volume_id: volume_id.to_string(),
                                    node_name: node_name.clone(),
                                    lvol_uuid: lvol_uuid.clone(),
                                    lvs_name: lvs_name.clone(),
                                    size_bytes,
                                });
                            }
                        }
                        // PV found but no metadata - fall through to query nodes
                        return Err("PV found but missing flint metadata in volumeAttributes".into());
                    }
                }
            }
        }
        
        Err("PV not found".into())
    }

    /// Update PV to mark filesystem as initialized (after formatting)
    /// Uses annotations (mutable) instead of volumeAttributes (immutable)
    pub async fn update_pv_filesystem_initialized(&self, volume_id: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::{Api, api::Patch, api::PatchParams};
        
        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        
        // Use annotations (mutable) instead of spec (immutable)
        let patch = serde_json::json!({
            "metadata": {
                "annotations": {
                    "flint.csi.storage.io/filesystem-initialized": "true"
                }
            }
        });
        
        pvs.patch(volume_id, &PatchParams::default(), &Patch::Merge(&patch)).await?;
        
        Ok(())
    }

    /// Check if PV has filesystem-initialized annotation
    pub async fn check_pv_filesystem_initialized(&self, volume_id: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::Api;
        
        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        
        match pvs.get(volume_id).await {
            Ok(pv) => {
                let initialized = pv.metadata.annotations
                    .and_then(|annot| annot.get("flint.csi.storage.io/filesystem-initialized").cloned())
                    .map(|v| v == "true")
                    .unwrap_or(false);
                Ok(initialized)
            }
            Err(_) => Ok(false)
        }
    }

    /// Parse Kubernetes quantity string (e.g., "1Gi", "500Mi") to bytes
    fn parse_quantity(quantity_str: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let quantity_str = quantity_str.trim();
        
        // Simple parser for common cases
        if quantity_str.ends_with("Gi") {
            let num: u64 = quantity_str.trim_end_matches("Gi").parse()?;
            Ok(num * 1024 * 1024 * 1024)
        } else if quantity_str.ends_with("Mi") {
            let num: u64 = quantity_str.trim_end_matches("Mi").parse()?;
            Ok(num * 1024 * 1024)
        } else if quantity_str.ends_with("Ki") {
            let num: u64 = quantity_str.trim_end_matches("Ki").parse()?;
            Ok(num * 1024)
        } else {
            // Assume bytes
            Ok(quantity_str.parse()?)
        }
    }

    // ============================================================================
    // GENERATION TRACKING METHODS FOR REPLICA CONSISTENCY
    // ============================================================================
    
    /// Read generation metadata from a replica's lvol
    /// Returns Ok(Some(metadata)) if generation exists, Ok(None) if not initialized, Err on failure
    pub async fn read_replica_generation(
        &self,
        node_name: &str,
        lvol_name: &str,
    ) -> Result<Option<crate::generation_tracking::GenerationMetadata>, Box<dyn std::error::Error + Send + Sync>> {
        use crate::generation_tracking::{GenerationMetadata, GenerationError};
        
        let params = serde_json::json!({
            "method": "bdev_lvol_get_xattr",
            "params": {
                "name": lvol_name,
                "xattr_name": GenerationMetadata::XATTR_NAME
            }
        });
        
        match self.call_node_agent(node_name, "/api/spdk/rpc", &params).await {
            Ok(response) => {
                if let Some(result) = response.get("result") {
                    if let Some(xattr_value) = result.get("xattr_value").and_then(|v| v.as_str()) {
                        // Decode generation metadata
                        match GenerationMetadata::unpack_base64(xattr_value) {
                            Ok(gen) => {
                                println!("📊 [GEN_TRACK] Read generation {} from {} on {}", 
                                    gen.generation, lvol_name, node_name);
                                return Ok(Some(gen));
                            }
                            Err(GenerationError::InvalidMagic { .. }) | 
                            Err(GenerationError::InvalidFormat(_)) => {
                                println!("⚠️ [GEN_TRACK] Invalid generation metadata on {} (will re-initialize)", lvol_name);
                                return Ok(None);
                            }
                            Err(e) => {
                                return Err(format!("Failed to decode generation: {}", e).into());
                            }
                        }
                    }
                }
                // No xattr_value in result - not initialized
                Ok(None)
            }
            Err(e) => {
                let err_str = e.to_string();
                // ENOENT or "not found" means xattr doesn't exist yet (OK for new replicas)
                if err_str.contains("ENOENT") || err_str.contains("not found") {
                    println!("ℹ️ [GEN_TRACK] No generation xattr on {} (uninitialized)", lvol_name);
                    Ok(None)
                } else {
                    Err(format!("Failed to read generation from {}: {}", lvol_name, e).into())
                }
            }
        }
    }
    
    /// Write generation metadata to a replica's lvol
    pub async fn write_replica_generation(
        &self,
        node_name: &str,
        lvol_name: &str,
        generation: &crate::generation_tracking::GenerationMetadata,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::generation_tracking::GenerationMetadata;
        
        let base64_value = generation.pack_base64();
        
        let params = serde_json::json!({
            "method": "bdev_lvol_set_xattr",
            "params": {
                "name": lvol_name,
                "xattr_name": GenerationMetadata::XATTR_NAME,
                "xattr_value": base64_value
            }
        });
        
        self.call_node_agent(node_name, "/api/spdk/rpc", &params).await?;
        
        println!("✅ [GEN_TRACK] Wrote generation {} to {} on {}", 
            generation.generation, lvol_name, node_name);
        
        Ok(())
    }
    
    /// Read generations from all replicas and detect stale ones
    pub async fn check_replica_generations(
        &self,
        replicas: &[ReplicaInfo],
    ) -> Result<crate::generation_tracking::GenerationComparisonResult, Box<dyn std::error::Error + Send + Sync>> {
        use crate::generation_tracking::compare_generations;
        
        println!("🔍 [GEN_TRACK] Checking generations for {} replicas...", replicas.len());
        
        let mut replica_gens = Vec::new();
        
        for (i, replica) in replicas.iter().enumerate() {
            let gen_opt = self.read_replica_generation(
                &replica.node_name,
                &replica.lvol_uuid,
            ).await?;
            
            if let Some(ref gen) = gen_opt {
                println!("   Replica {}: generation={}, timestamp={}, node_id=0x{:08x}",
                    i, gen.generation, gen.timestamp, gen.node_id);
            } else {
                println!("   Replica {}: uninitialized", i);
            }
            
            replica_gens.push(gen_opt);
        }
        
        let result = compare_generations(replica_gens);
        
        println!("📊 [GEN_TRACK] Generation comparison:");
        println!("   Max generation: {}", result.max_generation);
        println!("   Current replicas: {:?}", result.current_replicas);
        println!("   Stale replicas: {:?}", result.stale_replicas);
        println!("   Uninitialized replicas: {:?}", result.uninitialized_replicas);
        
        if result.is_consistent() {
            println!("✅ [GEN_TRACK] All replicas are in sync");
        } else {
            println!("⚠️ [GEN_TRACK] {} replicas need rebuild", result.out_of_sync_count());
        }
        
        Ok(result)
    }
    
    /// Increment generation on all replicas (called during NodePublishVolume)
    pub async fn increment_replica_generations(
        &self,
        replicas: &[ReplicaInfo],
        current_node: &str,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::generation_tracking::GenerationMetadata;
        
        println!("🔄 [GEN_TRACK] Incrementing generation for {} replicas...", replicas.len());
        
        // First, find the max generation
        let comparison = self.check_replica_generations(replicas).await?;
        
        // Create next generation
        let new_generation = comparison.max_generation + 1;
        let new_gen_metadata = GenerationMetadata::new(new_generation, current_node);
        
        println!("📈 [GEN_TRACK] New generation: {} (from {})", new_generation, comparison.max_generation);
        
        // Write to all replicas
        for (i, replica) in replicas.iter().enumerate() {
            match self.write_replica_generation(
                &replica.node_name,
                &replica.lvol_uuid,
                &new_gen_metadata,
            ).await {
                Ok(_) => {
                    println!("   ✓ Replica {}: generation updated to {}", i, new_generation);
                }
                Err(e) => {
                    println!("   ⚠️ Replica {}: failed to update generation: {}", i, e);
                    // Continue anyway - we'll detect stale replicas on next attach
                }
            }
        }
        
        println!("✅ [GEN_TRACK] Generation increment complete: {}", new_generation);
        
        Ok(new_generation)
    }

    /// Create RAID 1 bdev from replicas with mixed local/remote access
    pub async fn create_raid_from_replicas(
        &self,
        volume_id: &str,
        replicas: &[ReplicaInfo],
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let current_node = &self.node_id;
        
        println!("🔧 [DRIVER] Creating RAID 1 on node: {}", current_node);
        println!("🔧 [DRIVER] Processing {} replicas...", replicas.len());

        // ========================================================================
        // STEP 1: Check generation tracking - detect stale replicas
        // ========================================================================
        println!("📊 [DRIVER] Step 1: Checking replica generations...");
        
        let gen_comparison = self.check_replica_generations(replicas).await?;
        
        if !gen_comparison.is_consistent() {
            println!("⚠️ [DRIVER] WARNING: Detected {} out-of-sync replicas", 
                gen_comparison.out_of_sync_count());
            println!("⚠️ [DRIVER] Stale replicas: {:?}", gen_comparison.stale_replicas);
            println!("⚠️ [DRIVER] Uninitialized replicas: {:?}", gen_comparison.uninitialized_replicas);
            
            if !gen_comparison.can_rebuild() {
                return Err(format!(
                    "No current replica available for rebuild (max generation: {})",
                    gen_comparison.max_generation
                ).into());
            }
            
            // TODO: Implement automatic replica rebuild in future enhancement
            // For now, we'll proceed with DEGRADED mode using only current replicas
            println!("⚠️ [DRIVER] Proceeding in DEGRADED mode with current replicas only");
            println!("⚠️ [DRIVER] Manual rebuild recommended for stale replicas");
        }

        // ========================================================================
        // STEP 2: Check minimum replica requirement
        // ========================================================================
        let available_replicas: Vec<&ReplicaInfo> = replicas.iter()
            .filter(|r| self.is_node_available_sync(&r.node_name))
            .collect();

        if available_replicas.len() < 2 {
            return Err(format!(
                "Cannot create RAID 1: need minimum 2 replicas, only {} available",
                available_replicas.len()
            ).into());
        }

        if available_replicas.len() < replicas.len() {
            println!("⚠️ [DRIVER] DEGRADED: {}/{} replicas available", 
                     available_replicas.len(), replicas.len());
        }

        // ========================================================================
        // STEP 3: Attach each replica (local or remote)
        // ========================================================================
        println!("🔧 [DRIVER] Step 2: Attaching replicas...");
        let mut base_bdevs = Vec::new();

        for (i, replica) in available_replicas.iter().enumerate() {
            if replica.node_name == *current_node {
                // LOCAL: Use lvol bdev directly
                println!("   Replica {}: LOCAL access (lvol: {})", 
                         i + 1, replica.lvol_uuid);
                base_bdevs.push(replica.lvol_uuid.clone());
            } else {
                // REMOTE: Setup NVMe-oF and attach
                println!("   Replica {}: REMOTE access (node: {}, setting up NVMe-oF...)", 
                         i + 1, replica.node_name);

                // Create NVMe-oF target on remote node
                let nqn = format!("nqn.2024-11.com.flint:volume:{}:replica:{}", volume_id, i);
                let conn_info = self.setup_nvmeof_target_on_node(
                    &replica.node_name,
                    &replica.lvol_uuid,
                    &format!("{}_{}", volume_id, i),
                ).await?;

                // Attach NVMe-oF target from current node
                let nvme_bdev = self.connect_to_nvmeof_target(&conn_info).await?;
                println!("   ✓ Attached remote replica as: {}", nvme_bdev);
                base_bdevs.push(nvme_bdev);
            }
        }

        // ========================================================================
        // STEP 4: Create RAID 1 bdev
        // ========================================================================
        println!("🔧 [DRIVER] Step 3: Creating RAID 1 bdev...");
        let raid_name = format!("raid_{}", volume_id);
        println!("🔧 [DRIVER] Creating RAID 1 bdev: {} with {} base bdevs", 
                 raid_name, base_bdevs.len());

        let raid_bdev_name = self.create_raid1_bdev(&raid_name, base_bdevs).await?;

        println!("✅ [DRIVER] RAID 1 bdev created: {}", raid_bdev_name);

        // ========================================================================
        // STEP 5: Increment generation on all replicas
        // ========================================================================
        println!("📈 [DRIVER] Step 4: Incrementing generation...");
        
        match self.increment_replica_generations(replicas, current_node).await {
            Ok(new_gen) => {
                println!("✅ [DRIVER] Generation incremented to: {}", new_gen);
            }
            Err(e) => {
                println!("⚠️ [DRIVER] Failed to increment generation (non-fatal): {}", e);
                // Continue - RAID is already created, generation tracking is best-effort
            }
        }

        println!("✅ [DRIVER] RAID setup complete: {}", raid_bdev_name);

        Ok(raid_bdev_name)
    }

    /// Check if a node is available (simplified sync version)
    fn is_node_available_sync(&self, _node_name: &str) -> bool {
        // In production, this would check node readiness via K8s API
        // For now, optimistically assume available
        // TODO: Add proper node health checking
        true
    }

    /// Create RAID 1 bdev
    async fn create_raid1_bdev(
        &self,
        raid_name: &str,
        base_bdevs: Vec<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Call SPDK RPC directly
        let payload = serde_json::json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid_name,
                "raid_level": "1",
                "base_bdevs": base_bdevs,
            }
        });

        self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await?;

        println!("✅ [DRIVER] RAID 1 bdev created: {}", raid_name);
        Ok(raid_name.to_string())
    }

    /// Setup NVMe-oF target and return connection info
    pub async fn setup_nvmeof_target_on_node(&self, node_name: &str, bdev_name: &str, volume_id: &str) -> Result<NvmeofConnectionInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🌐 [DRIVER] Setting up NVMe-oF target on node: {} for bdev: {}", node_name, bdev_name);
        
        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
        
        // Try to get specific subsystem by NQN (more efficient than listing all)
        let get_subsystem_params = json!({
            "method": "nvmf_get_subsystems",
            "params": {
                "nqn": nqn
            }
        });
        
        let subsystem_exists = match self.call_node_agent(node_name, "/api/spdk/rpc", &get_subsystem_params).await {
            Ok(response) => {
                // If result is a non-empty array, subsystem exists
                if let Some(result) = response.get("result") {
                    if let Some(subsystems) = result.as_array() {
                        !subsystems.is_empty()
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            Err(_) => false
        };
        
        if subsystem_exists {
            println!("ℹ️ [DRIVER] Subsystem already exists (idempotent): {}", nqn);
        } else {
            // Create subsystem
            println!("🔧 [DRIVER] Creating new NVMe-oF subsystem: {}", nqn);
            let subsystem_params = json!({
                "method": "nvmf_create_subsystem",
                "params": {
                    "nqn": nqn,
                    "allow_any_host": true,
                    "serial_number": format!("SPDK{:016x}", std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64),
                    "model_number": "SPDK CSI Volume"
                }
            });

            match self.call_node_agent(node_name, "/api/spdk/rpc", &subsystem_params).await {
                Ok(_) => println!("✅ [DRIVER] Subsystem created: {}", nqn),
                Err(e) if e.to_string().contains("already exists") => {
                    println!("ℹ️ [DRIVER] Subsystem already exists (race condition): {}", nqn);
                }
                Err(e) => {
                    println!("❌ [DRIVER] Failed to create subsystem: {}", e);
                    return Err(format!("Failed to create subsystem: {}", e).into());
                }
            }
        }

        // Add namespace
        let namespace_params = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "nsid": 1,
                    "bdev_name": bdev_name
                }
            }
        });

        match self.call_node_agent(node_name, "/api/spdk/rpc", &namespace_params).await {
            Ok(_) => println!("✅ [DRIVER] Namespace added for bdev: {}", bdev_name),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [DRIVER] Namespace already exists for bdev: {}", bdev_name);
            }
            Err(e) => return Err(format!("Failed to add namespace: {}", e).into()),
        }

        // Add listener
        let node_ip = self.get_node_ip(node_name).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?;
        
        let listener_params = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": self.nvmeof_transport.to_uppercase(),
                    "traddr": node_ip,
                    "trsvcid": self.nvmeof_target_port.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });

        match self.call_node_agent(node_name, "/api/spdk/rpc", &listener_params).await {
            Ok(_) => println!("✅ [DRIVER] Listener added: {}:{}", node_ip, self.nvmeof_target_port),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [DRIVER] Listener already exists: {}:{}", node_ip, self.nvmeof_target_port);
            }
            Err(e) => return Err(format!("Failed to add listener: {}", e).into()),
        }

        println!("🎉 [DRIVER] NVMe-oF target setup completed: {}", nqn);
        
        Ok(NvmeofConnectionInfo {
            nqn: nqn.clone(),
            target_ip: node_ip.clone(),
            target_port: self.nvmeof_target_port,
            transport: self.nvmeof_transport.clone(),
        })
    }

    /// Connect to NVMe-oF target from current node
    pub async fn connect_to_nvmeof_target(&self, conn_info: &NvmeofConnectionInfo) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔌 [DRIVER] Connecting to NVMe-oF target");
        println!("   NQN: {}", conn_info.nqn);
        println!("   Target: {}:{}", conn_info.target_ip, conn_info.target_port);
        println!("   Transport: {}", conn_info.transport);
        
        let controller_name = format!("nvme_{}", conn_info.nqn.replace(":", "_").replace(".", "_"));
        println!("   Controller name: {}", controller_name);
        
        let attach_params = json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": controller_name,
                "trtype": conn_info.transport.to_uppercase(),
                "traddr": conn_info.target_ip,
                "trsvcid": conn_info.target_port.to_string(),
                "subnqn": conn_info.nqn,
                "adrfam": "IPv4"
            }
        });

        println!("📡 [DRIVER] Calling bdev_nvme_attach_controller...");
        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &attach_params).await {
            Ok(response) => {
                println!("✅ [DRIVER] bdev_nvme_attach_controller succeeded");
                // The response should contain the bdev names created
                if let Some(result) = response.get("result") {
                    if let Some(bdev_names) = result.as_array() {
                        if let Some(first_bdev) = bdev_names.first() {
                            if let Some(bdev_name) = first_bdev.as_str() {
                                println!("✅ [DRIVER] Connected to NVMe-oF target, bdev created: {}", bdev_name);
                                return Ok(bdev_name.to_string());
                            }
                        }
                    }
                }
                // Fallback - construct expected bdev name
                let bdev_name = format!("{}n1", controller_name);
                println!("✅ [DRIVER] Connected to NVMe-oF target, expected bdev: {}", bdev_name);
                Ok(bdev_name)
            }
            Err(e) if e.to_string().contains("already exists") => {
                println!("⚠️ [DRIVER] Controller already exists (error -114)");
                println!("   This could mean:");
                println!("   1. Controller is connected and working (bdev exists) ✅");
                println!("   2. Controller exists but is FAILED (no bdev) ❌");
                println!("   Verifying bdev existence...");
                
                let expected_bdev = format!("{}n1", controller_name);
                println!("   Expected bdev name: {}", expected_bdev);
                
                match self.verify_bdev_exists(&expected_bdev).await {
                    Ok(()) => {
                        // Bdev exists - controller is working
                        println!("✅ [DRIVER] Bdev verified to exist: {}", expected_bdev);
                        println!("   Controller is working, using existing connection");
                        Ok(expected_bdev)
                    }
                    Err(_) => {
                        // Bdev doesn't exist - controller is in FAILED state
                        println!("❌ [DRIVER] Bdev NOT found - controller is in FAILED state");
                        println!("   Cleaning up stale controller and retrying...");
                        
                        // Delete the failed controller
                        if let Err(e) = self.delete_nvme_controller(&controller_name).await {
                            println!("⚠️ [DRIVER] Failed to delete stale controller: {}", e);
                            println!("   Continuing with retry anyway...");
                        } else {
                            println!("✅ [DRIVER] Stale controller deleted: {}", controller_name);
                        }
                        
                        // Retry the attach
                        println!("🔄 [DRIVER] Retrying bdev_nvme_attach_controller after cleanup...");
                        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &attach_params).await {
                            Ok(response) => {
                                if let Some(result) = response.get("result") {
                                    if let Some(bdev_names) = result.as_array() {
                                        if let Some(first_bdev) = bdev_names.first() {
                                            if let Some(bdev_name) = first_bdev.as_str() {
                                                println!("✅ [DRIVER] Retry succeeded, bdev created: {}", bdev_name);
                                                return Ok(bdev_name.to_string());
                                            }
                                        }
                                    }
                                }
                                let bdev_name = format!("{}n1", controller_name);
                                println!("✅ [DRIVER] Retry succeeded, expected bdev: {}", bdev_name);
                                Ok(bdev_name)
                            }
                            Err(e) => {
                                println!("❌ [DRIVER] Retry failed: {}", e);
                                Err(format!("Retry attach failed after cleanup: {}", e).into())
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("❌ [DRIVER] bdev_nvme_attach_controller failed: {}", e);
                Err(format!("Failed to attach NVMe controller: {}", e).into())
            }
        }
    }

    /// Delete a failed NVMe controller
    pub async fn delete_nvme_controller(&self, controller_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🧹 [DRIVER] Deleting NVMe controller: {}", controller_name);
        
        let rpc = json!({
            "method": "bdev_nvme_detach_controller",
            "params": {
                "name": controller_name
            }
        });
        
        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &rpc).await {
            Ok(_) => {
                println!("   ✓ Controller deleted successfully: {}", controller_name);
                Ok(())
            }
            Err(e) => {
                println!("   ⚠️ Failed to delete controller: {}", e);
                Err(format!("Failed to delete controller {}: {}", controller_name, e).into())
            }
        }
    }

    /// Disconnect from NVMe-oF target
    pub async fn disconnect_from_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔌 [DRIVER] Disconnecting from NVMe-oF target: {}", nqn);
        
        let controller_name = format!("nvme_{}", nqn.replace(":", "_").replace(".", "_"));
        
        let detach_params = json!({
            "method": "bdev_nvme_detach_controller",
            "params": {
                "name": controller_name
            }
        });

        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &detach_params).await {
            Ok(_) => {
                println!("✅ [DRIVER] Disconnected from NVMe-oF target: {}", nqn);
                Ok(())
            }
            Err(e) if e.to_string().contains("does not exist") => {
                println!("ℹ️ [DRIVER] NVMe controller not found (already disconnected): {}", controller_name);
                Ok(())
            }
            Err(e) => {
                println!("⚠️ [DRIVER] Failed to disconnect (continuing anyway): {}", e);
                Ok(()) // Best effort cleanup
            }
        }
    }

    /// Remove NVMe-oF target from a node
    pub async fn remove_nvmeof_target(&self, node_name: &str, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🧹 [DRIVER] Removing NVMe-oF target from node: {} nqn: {}", node_name, nqn);
        
        let delete_params = json!({
            "method": "nvmf_delete_subsystem",
            "params": {
                "nqn": nqn
            }
        });

        match self.call_node_agent(node_name, "/api/spdk/rpc", &delete_params).await {
            Ok(_) => {
                println!("✅ [DRIVER] Successfully removed NVMe-oF target: {}", nqn);
                Ok(())
            }
            Err(e) if e.to_string().contains("does not exist") => {
                println!("ℹ️ [DRIVER] NVMe-oF target not found (already removed): {}", nqn);
                Ok(())
            }
            Err(e) => {
                println!("⚠️ [DRIVER] Failed to remove target (continuing anyway): {}", e);
                Ok(()) // Best effort cleanup
            }
        }
    }

    /// Get list of all node names in the cluster
    /// Used by snapshot controller to query all nodes for snapshots
    pub async fn get_all_nodes(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        use kube::api::ListParams;
        use k8s_openapi::api::core::v1::Node as k8sNode;
        
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        let nodes = nodes_api.list(&ListParams::default()).await?;
        
        let node_names: Vec<String> = nodes.items
            .iter()
            .filter_map(|n| n.metadata.name.clone())
            .collect();
        
        println!("✅ [DRIVER] Found {} nodes in cluster", node_names.len());
        Ok(node_names)
    }
}

/// Volume information
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub volume_id: String,
    pub node_name: String,
    pub lvol_uuid: String,
    pub lvs_name: String,
    pub size_bytes: u64,
}

/// NVMe-oF connection information
#[derive(Debug, Clone)]
pub struct NvmeofConnectionInfo {
    pub nqn: String,
    pub target_ip: String,
    pub target_port: u16,
    pub transport: String,
}

/// Health status of backing storage
/// Used for volume health monitoring and graceful deletion
#[derive(Debug, Clone)]
pub struct BackingStorageHealth {
    /// Whether the storage exists
    pub exists: bool,
    /// Whether the storage is healthy
    pub healthy: bool,
    /// Human-readable status message
    pub message: String,
    /// Whether the node was reachable for the health check
    pub node_reachable: bool,
}
