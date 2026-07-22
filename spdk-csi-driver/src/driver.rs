// driver_minimal.rs - Clean minimal state SPDK CSI Driver
// CONTROLLER implementation - talks to Node Agents via HTTP (NOT directly to SPDK)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::{json, Value};
use reqwest::Client as HttpClient;
use tonic::Status;
use tracing::{debug, info, warn, error};

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

    // Whether the local SPDK supports `bdev_raid_delete clear_sb` (v26.05+).
    // Probed once via spdk_get_version and cached for the process lifetime.
    clear_sb_support: Arc<tokio::sync::OnceCell<bool>>,

    // The ONE role resolver (identity-unification Phase 1): every RPC that
    // must classify a bare handle (RWO block consumer vs RWX/ROX NFS
    // client) goes through this instead of site-local heuristics.
    pub role_resolver: crate::identity::RoleResolver,
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
            role_resolver: crate::identity::RoleResolver::new(kube_client.clone()),
            kube_client,
            target_namespace,
            node_id,
            spdk_rpc_url,
            nvmeof_transport,
            nvmeof_target_port,
            spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
            capacity_cache: CapacityCache::new(30), // 30 second TTL
            clear_sb_support: Arc::new(tokio::sync::OnceCell::new()),
        }
    }
    
    /// Initialize driver (warm up cache, start background tasks)
    pub async fn initialize(&self, mode: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(mode, "[DRIVER] Initializing CSI driver");

        // Capacity cache is only needed for controller mode (volume placement decisions)
        // Node mode only handles local operations (mount/unmount/stage/unstage)
        if mode == "controller" || mode == "all" {
            // Warm up capacity cache
            info!("[DRIVER] Warming up capacity cache");
            self.capacity_cache.warm_up(self).await?;

            // Start background cache refresh (every 60 seconds)
            // Note: Cache is also invalidated after every volume creation, so this is mainly
            // to catch external changes (manual SPDK operations, disk failures, etc.)
            debug!("[DRIVER] Starting background capacity refresh (every 60s)");
            CapacityCache::start_background_refresh(
                Arc::new(self.capacity_cache.clone()),
                Arc::new(self.clone()),
                60,
            );
        } else {
            debug!("[DRIVER] Skipping capacity cache initialization in node mode");
        }

        info!("[DRIVER] Initialization complete");
        Ok(())
    }

    /// Select a node for single-replica volume using capacity cache.
    ///
    /// `preferred_nodes` is the ordered list of node names from the CSI
    /// request's topology hint (see [`crate::TOPOLOGY_NODE_KEY`]). When a
    /// preferred node has enough free capacity it wins; otherwise (empty
    /// hint, or no preferred node has room) placement falls back to the
    /// historical max-free choice. Passing `&[]` is byte-identical to the
    /// pre-topology behaviour.
    async fn select_node_for_single_replica(
        &self,
        size_bytes: u64,
        preferred_nodes: &[String],
    ) -> Result<String, MinimalStateError> {
        let size_gb = size_bytes / (1024 * 1024 * 1024);
        debug!(size_gb, "[DRIVER] Selecting node for single-replica volume");

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

        let node_count = all_nodes.len();
        debug!(node_count, "[DRIVER] Found nodes in cluster");

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
            error!("[DRIVER] No nodes with sufficient capacity found");
            return Err(MinimalStateError::InsufficientCapacity {
                required: size_bytes,
                available: 0,
            });
        }

        // Choose: first preferred node that has capacity, else max-free.
        let candidate_pairs: Vec<(String, u64)> = candidates
            .iter()
            .map(|c| (c.node_name.clone(), c.free_capacity))
            .collect();
        let chosen_name = pick_placement_node(&candidate_pairs, preferred_nodes)
            .expect("candidates is non-empty (checked above)");
        let honored_preferred = preferred_nodes.iter().any(|p| p == &chosen_name);
        let selected = candidates
            .iter()
            .find(|c| c.node_name == chosen_name)
            .expect("chosen node came from candidates");

        // Reserve capacity (optimistic locking)
        self.capacity_cache.reserve_capacity(&selected.node_name, size_bytes).await?;

        let node_name = &selected.node_name;
        let free_gb = selected.free_capacity / (1024 * 1024 * 1024);
        let total_gb = selected.total_capacity / (1024 * 1024 * 1024);
        info!(node_name, free_gb, total_gb, honored_preferred, "[DRIVER] Selected node");

        Ok(selected.node_name.clone())
    }

    /// Select N nodes for multi-replica volume (each replica on different node)
    async fn select_nodes_for_replicas(
        &self,
        replica_count: u32,
        size_bytes: u64,
    ) -> Result<Vec<crate::raid::NodeDiskSelection>, MinimalStateError> {
        let size_gb = size_bytes / (1024 * 1024 * 1024);
        debug!(replica_count, size_gb, "[DRIVER] Finding nodes for replicas");

        // Get all nodes in cluster
        let all_nodes = self.get_all_nodes().await
            .map_err(|e| MinimalStateError::InternalError {
                message: format!("Failed to list nodes: {}", e)
            })?;

        let node_count = all_nodes.len();
        debug!(node_count, "[DRIVER] Found nodes in cluster");

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
                        let device_name = &disk.device_name;
                        let free_gb = disk.free_space / (1024 * 1024 * 1024);
                        debug!(node_name, device_name, free_gb, "[DRIVER] Selected node for replica");
                    }
                }
                Err(e) => {
                    let err = e.to_string();
                    warn!(node_name, err, "[DRIVER] Skipping node, query failed");
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
        debug!(volume_id, replica_count, "[DRIVER] Creating distributed multi-replica volume");

        // Step 1: Find N nodes with available space (each on different node)
        let selected_nodes = self.select_nodes_for_replicas(replica_count, size_bytes).await?;

        let count = selected_nodes.len();
        info!(count, "[DRIVER] Selected nodes for replicas");
        for (i, node_info) in selected_nodes.iter().enumerate() {
            let replica_num = i + 1;
            let node_name = &node_info.node_name;
            let device_name = &node_info.disk.device_name;
            let free_gb = node_info.disk.free_space / (1024 * 1024 * 1024);
            debug!(replica_num, node_name, device_name, free_gb, "[DRIVER] Replica placement");
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

            // Fetch node UID for replica tracking
            let node_uid = match self.get_node_uid(&node_info.node_name).await {
                Ok(uid) => uid,
                Err(e) => {
                    let node_name = &node_info.node_name;
                    let err = e.to_string();
                    warn!(node_name, err, "[DRIVER] Failed to get node UID, using empty");
                    String::new()
                }
            };

            match self.create_lvol(
                &node_info.node_name,
                lvs_name,
                &replica_volume_id,
                size_bytes,
                thin_provision,
            ).await {
                Ok(lvol_uuid) => {
                    let replica_num = i + 1;
                    let node_name = &node_info.node_name;
                    info!(replica_num, node_name, lvol_uuid, "[DRIVER] Created replica");

                    let replica = ReplicaInfo {
                        node_name: node_info.node_name.clone(),
                        node_uid: node_uid.clone(),
                        disk_pci_address: node_info.disk.pci_address.clone(),
                        lvol_uuid: lvol_uuid.clone(),
                        lvol_name: crate::identity::lvol_name(&replica_volume_id),
                        lvs_name: lvs_name.clone(),
                        nqn: None, // Will be set during NodePublishVolume if needed
                        target_ip: None,
                        target_port: None,
                        health: "online".to_string(),
                    };

                    created_replicas.push((node_info.node_name.clone(), lvol_uuid.clone()));
                    replicas.push(replica);

                    // Invalidate cache for this node
                    self.capacity_cache.invalidate(&node_info.node_name).await;
                }
                Err(e) => {
                    // Cleanup: Delete all previously created replicas
                    let replica_num = i + 1;
                    let node_name = &node_info.node_name;
                    let err = e.to_string();
                    error!(replica_num, node_name, err, "[DRIVER] Failed to create replica");
                    let cleanup_count = created_replicas.len();
                    debug!(cleanup_count, "[DRIVER] Cleaning up previously created replicas");

                    for (node, uuid) in created_replicas {
                        let _ = self.delete_lvol(&node, &uuid).await;
                    }

            return Err(MinimalStateError::InternalError {
                        message: format!("Failed to create replica {}: {}", i + 1, e)
                    });
                }
            }
        }

        let replica_count = replicas.len();
        info!(replica_count, volume_id, "[DRIVER] Created replicas for volume");

        // Step 3: Return result with replica metadata
        // This will be stored in PV annotations by CSI controller
        Ok(VolumeCreationResult {
            volume_id: volume_id.to_string(),
            size_bytes,
            replicas,
        })
    }

    /// Create single-replica volume. `preferred_nodes` steers placement
    /// toward the CSI topology hint (WaitForFirstConsumer's selected node)
    /// when it has capacity; `&[]` preserves the historical max-free pick.
    async fn create_single_replica_volume(
        &self,
        volume_id: &str,
        size_bytes: u64,
        thin_provision: bool,
        preferred_nodes: &[String],
    ) -> Result<VolumeCreationResult, MinimalStateError> {
        // Select node dynamically using capacity cache
        let node_name = match self.select_node_for_single_replica(size_bytes, preferred_nodes).await {
            Ok(node) => node,
            Err(e) => {
                let err = e.to_string();
                error!(err, "[DRIVER] Failed to select node");
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
        
        let device_name = &selected_disk.device_name;
        let free_gb = selected_disk.free_space / (1024*1024*1024);
        info!(device_name, lvs_name, free_gb, node_name, "[DRIVER] Selected disk");
        
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

        // Fetch node UID for replica tracking
        let node_uid = match self.get_node_uid(&node_name).await {
            Ok(uid) => uid,
            Err(e) => {
                let err = e.to_string();
                warn!(node_name, err, "[DRIVER] Failed to get node UID, using empty");
                String::new()
            }
        };

        info!(volume_id, lvol_uuid, "[DRIVER] Volume created successfully");

        // Return full volume creation result with metadata
        Ok(VolumeCreationResult {
            volume_id: volume_id.to_string(),
            size_bytes,
            replicas: vec![ReplicaInfo {
                node_name: node_name.to_string(),
                node_uid,
                disk_pci_address: selected_disk.pci_address.clone(),
                lvol_uuid: lvol_uuid.clone(),
                lvol_name: crate::identity::lvol_name(volume_id),
                lvs_name: lvs_name.clone(),
                nqn: None,
                target_ip: None,
                target_port: None,
                health: "online".to_string(),
            }],
        })
    }

    /// Create volume using minimal state architecture (routing to single or multi-replica).
    ///
    /// `preferred_nodes` is the CSI topology hint (WaitForFirstConsumer's
    /// selected node); it only steers single-replica placement. Multi-replica
    /// (RAID) placement already spreads replicas across distinct nodes and
    /// ignores the hint. Pass `&[]` for topology-unaware callers.
    pub async fn create_volume(&self, volume_id: &str, size_bytes: u64, replica_count: u32, thin_provision: bool, preferred_nodes: &[String]) -> Result<VolumeCreationResult, MinimalStateError> {
        debug!(volume_id, size_bytes, replica_count, thin_provision, "[DRIVER] Creating volume");

        // Route based on replica count
        if replica_count == 1 {
            // Single replica: Use existing path (zero changes to existing logic)
            return self.create_single_replica_volume(volume_id, size_bytes, thin_provision, preferred_nodes).await;
        }

        // Multi-replica: RAID 1 requires minimum 2 replicas
        if replica_count < 2 {
            return Err(MinimalStateError::InvalidParameter {
                message: "RAID 1 requires minimum 2 replicas".to_string()
            });
        }

        // Create distributed multi-replica volume
        let result = self.create_distributed_multi_replica_volume(
            volume_id,
            size_bytes,
            replica_count,
            thin_provision
        ).await?;

        // Add replica node labels to PV for node-based discovery on restart
        // This enables nodes to find which volumes have local replicas when they come back online
        if let Err(e) = self.add_replica_node_labels(volume_id, &result.replicas).await {
            // Log but don't fail - labels are for optimization, not critical path
            let err = e.to_string();
            warn!(err, "[DRIVER] Failed to add replica node labels (non-fatal)");
        }

        Ok(result)
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

    /// Get node UID from Kubernetes API
    /// Used to create unique labels for replica tracking on PVs
    pub async fn get_node_uid(&self, node_name: &str) -> Result<String, Status> {
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        let node = nodes_api.get(node_name).await
            .map_err(|e| Status::internal(format!("Failed to get node {}: {}", node_name, e)))?;

        node.metadata.uid
            .ok_or_else(|| Status::not_found(format!("No UID found for node {}", node_name)))
    }

    /// Add replica node labels to PV for node-based discovery on restart
    /// Labels format: flint.csi.storage.io/replica-{node_uid}=true
    /// Also seeds the per-replica sync-state annotation (incremental-rebuild
    /// phase 1) — all replicas in_sync at creation. Best effort either way:
    /// the node agent's reconcile/monitor lazily rebuilds both.
    pub async fn add_replica_node_labels(
        &self,
        volume_id: &str,
        replicas: &[ReplicaInfo],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::api::{Patch, PatchParams};

        debug!(volume_id, "[DRIVER] Adding replica node labels to PV");

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());

        // Build labels map with node UIDs
        let mut labels = serde_json::Map::new();
        for replica in replicas {
            if !replica.node_uid.is_empty() {
                let label_key = format!("flint.csi.storage.io/replica-{}", replica.node_uid);
                labels.insert(label_key, serde_json::json!("true"));
                let node_name = &replica.node_name;
                let node_uid = &replica.node_uid;
                debug!(node_name, node_uid, "[DRIVER] Adding label for node");
            }
        }

        if labels.is_empty() {
            warn!("[DRIVER] No valid node UIDs found, skipping label patching");
            return Ok(());
        }

        let sync_record = crate::replica_sync::VolumeSyncRecord::initial(replicas);
        let patch = serde_json::json!({
            "metadata": {
                "labels": labels,
                "annotations": {
                    crate::replica_sync::SYNC_STATE_ANNOTATION: sync_record.to_annotation()
                }
            }
        });

        // Retry with backoff - PV may not exist yet if external-provisioner hasn't created it
        for attempt in 1..=3 {
            match pvs.patch(volume_id, &PatchParams::default(), &Patch::Merge(&patch)).await {
                Ok(_) => {
                    let label_count = labels.len();
                    info!(volume_id, label_count, "[DRIVER] Replica node labels added to PV");
                    return Ok(());
                }
                Err(e) if attempt < 3 => {
                    let err = e.to_string();
                    warn!(attempt, err, "[DRIVER] Failed to patch PV, retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Err(e) => {
                    let err = e.to_string();
                    error!(attempt, err, "[DRIVER] Failed to add replica labels after retries");
                    return Err(format!("Failed to add replica labels: {}", e).into());
                }
            }
        }

        Ok(())
    }

    /// Get current node IP (cached)
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let node_id = &self.node_id;
        debug!(node_id, "[MINIMAL_DRIVER] Getting IP for node");
        Ok(self.get_node_ip(&self.node_id).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?)
    }

    /// Call Node Agent HTTP API (CONTROLLER pattern - not direct SPDK)
    /// GET twin of [`call_node_agent`] for the agent's read-only routes
    /// (`/api/snapshots/list` is `warp::get()` — POSTing it 405s, and the
    /// callers' per-node `continue` swallowed that silently: the
    /// name-sweep delete leaked the lvol it reported deleting. Found live
    /// 2026-07-06 on runn.)
    pub async fn get_node_agent(&self, node_name: &str, endpoint: &str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        debug!(node_name, endpoint, "[CONTROLLER_HTTP] GET node agent");
        let node_agent_url = self.get_node_agent_url(node_name).await?;
        let full_url = format!("{}{}", node_agent_url, endpoint);
        let response = HttpClient::new().get(&full_url).send().await?;
        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Node agent HTTP GET failed: {}", error_text).into());
        }
        Ok(response.json().await?)
    }

    pub async fn call_node_agent(&self, node_name: &str, endpoint: &str, payload: &Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        debug!(node_name, endpoint, "[CONTROLLER_HTTP] Calling node agent");
        
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
                    debug!(node_name, pod_ip, "[CONTROLLER_HTTP] Found node agent");
                    return Ok(pod_ip);
                }
            }
        }
        
        Err(format!("No node agent pod found for node {}", node_name).into())
    }

    /// Initialize blobstore on a disk (CONTROLLER calls Node Agent via HTTP)
    pub async fn initialize_blobstore(&self, node_name: &str, disk_pci_address: &str) -> Result<String, MinimalStateError> {
        debug!(node_name, disk_pci_address, "[CONTROLLER] Requesting blobstore initialization");

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

        info!(lvs_name, "[CONTROLLER] Blobstore initialized via node agent");
        Ok(lvs_name)
    }

    /// Create logical volume (CONTROLLER calls Node Agent via HTTP)  
    pub async fn create_lvol(&self, node_name: &str, lvs_name: &str, volume_id: &str, size_bytes: u64, thin_provision: bool) -> Result<String, MinimalStateError> {
        debug!(node_name, lvs_name, volume_id, thin_provision, "[CONTROLLER] Requesting lvol creation");
        
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

        info!(lvol_uuid, "[CONTROLLER] Lvol created via node agent");
        Ok(lvol_uuid)
    }

    /// Delete logical volume (CONTROLLER calls Node Agent via HTTP)
    pub async fn delete_lvol(&self, node_name: &str, lvol_uuid: &str) -> Result<(), MinimalStateError> {
        debug!(node_name, lvol_uuid, "[CONTROLLER] Requesting lvol deletion");

        let payload = json!({
            "lvol_uuid": lvol_uuid
        });

        self.call_node_agent(node_name, "/api/volumes/delete_lvol", &payload).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to delete lvol via node agent: {}", e) 
            })?;

        info!(lvol_uuid, "[CONTROLLER] Lvol deleted via node agent");
        Ok(())
    }

    /// Check if backing storage exists on a node (for graceful deletion)
    /// Returns Ok(true) if storage exists, Ok(false) if not found or node unreachable
    /// This is used during volume deletion to handle cases where:
    /// - Memory disk was destroyed (SPDK pod restart)
    /// - NVMe disk failed or was removed
    /// - Node is offline
    pub async fn check_backing_storage_exists(&self, node_name: &str, lvol_uuid: &str) -> Result<bool, MinimalStateError> {
        debug!(node_name, lvol_uuid, "[CONTROLLER] Checking if backing storage exists");

        let payload = json!({
            "lvol_uuid": lvol_uuid
        });

        match self.call_node_agent(node_name, "/api/volumes/check_exists", &payload).await {
            Ok(response) => {
                // Parse the response
                let exists = response["exists"].as_bool().unwrap_or(false);

                if let Some(warning) = response["warning"].as_str() {
                    warn!(warning, "[CONTROLLER] Storage check warning");
                }

                let status = if exists { "exists" } else { "not found" };
                debug!(lvol_uuid, node_name, status, "[CONTROLLER] Backing storage check result");

                Ok(exists)
            }
            Err(e) => {
                // Node agent unreachable - treat as storage gone
                // This handles the case where:
                // - Node is offline
                // - Node agent pod is down
                // - Network partition
                let err = e.to_string();
                warn!(node_name, err, "[CONTROLLER] Could not reach node to check storage");
                debug!("[CONTROLLER] Treating as storage unavailable (node unreachable)");
                Ok(false)
            }
        }
    }

    /// Check health of backing storage on a node
    /// Returns detailed health information for volume monitoring
    pub async fn check_backing_storage_health(&self, node_name: &str, lvol_uuid: &str) -> Result<BackingStorageHealth, MinimalStateError> {
        debug!(node_name, lvol_uuid, "[CONTROLLER] Checking backing storage health");

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

                let exists = health.exists;
                let healthy = health.healthy;
                let message = &health.message;
                if healthy {
                    debug!(exists, healthy, message, "[CONTROLLER] Storage health check result");
                } else {
                    warn!(exists, healthy, message, "[CONTROLLER] Storage health check result");
                }

                Ok(health)
            }
            Err(e) => {
                // Node agent unreachable
                let err = e.to_string();
                error!(node_name, err, "[CONTROLLER] Could not reach node for health check");
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
        debug!(volume_id, node_name, "[DEFENSIVE] Checking if volume is still staged");
        
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
                        info!("[DEFENSIVE] Volume was staged - successfully unstaged");
                    } else {
                        debug!("[DEFENSIVE] Volume was not staged - no action needed");
                    }
                }
                Ok(())
            }
            Err(e) => {
                let err = e.to_string();
                warn!(err, "[DEFENSIVE] Force unstage check failed");
                // Don't fail - this is best effort
                Ok(())
            }
        }
    }

    /// Aggressive cleanup for stuck volumes (last resort)
    pub async fn force_cleanup_volume(&self, node_name: &str, volume_id: &str, ublk_id: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(volume_id, node_name, "[AGGRESSIVE] Force cleaning up volume");
        
        let payload = json!({
            "volume_id": volume_id,
            "ublk_id": ublk_id,
            "force": true  // Force cleanup even if errors
        });
        
        self.call_node_agent(node_name, "/api/volumes/force_unstage", &payload).await?;
        
        info!("[AGGRESSIVE] Force cleanup completed");
        Ok(())
    }

    /// Create NVMe-oF target (minimal implementation - will be enhanced later)
    pub async fn create_nvmeof_target(&self, bdev_name: &str, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(bdev_name, nqn, "[MINIMAL_NVMEOF] Creating NVMe-oF target");
        
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
            Ok(_) => info!(nqn, "[MINIMAL_NVMEOF] Subsystem created"),
            Err(e) if e.to_string().contains("already exists") => {
                debug!(nqn, "[MINIMAL_NVMEOF] Subsystem already exists");
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
            Ok(_) => info!(bdev_name, "[MINIMAL_NVMEOF] Namespace added for bdev"),
            Err(e) if e.to_string().contains("already exists") => {
                debug!(bdev_name, "[MINIMAL_NVMEOF] Namespace already exists for bdev");
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
            Ok(_) => {
                let port = self.nvmeof_target_port;
                info!(node_ip, port, "[MINIMAL_NVMEOF] Listener added");
            }
            Err(e) if e.to_string().contains("already exists") => {
                let port = self.nvmeof_target_port;
                debug!(node_ip, port, "[MINIMAL_NVMEOF] Listener already exists");
            }
            Err(e) => return Err(e),
        }

        info!(nqn, "[MINIMAL_NVMEOF] NVMe-oF target setup completed");
        Ok(())
    }

    /// Cleanup NVMe-oF target (minimal implementation)
    pub async fn cleanup_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(nqn, "[MINIMAL_NVMEOF] Cleaning up NVMe-oF target");

        let delete_params = json!({
            "method": "nvmf_delete_subsystem",
            "params": {
                "nqn": nqn
            }
        });

        match self.call_node_agent(&self.node_id, "/api/nvmeof/delete_subsystem", &delete_params).await {
            Ok(_) => info!(nqn, "[MINIMAL_NVMEOF] Successfully deleted subsystem"),
            Err(e) => {
                let err = e.to_string();
                warn!(nqn, err, "[MINIMAL_NVMEOF] Failed to delete subsystem (may not exist)");
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
    async fn create_ublk_block_device(&self, bdev_name: &str, _volume_id: &str, ublk_id: u32) -> Result<BlockDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        debug!(bdev_name, ublk_id, "[MINIMAL_UBLK] Creating ublk device");

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

        let response = self.call_node_agent(&self.node_id, "/api/ublk/create", &ublk_params).await?;

        // The agent allocates the REAL id — the kernel bounds ADD_DEV ids
        // to ublks_max (default 64), so the hashed id above is only a
        // legacy hint. The actual id rides back in the response and is
        // what lands in the PV annotation for unstage/rehydration.
        let ublk_id = response
            .get("ublk_id")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(ublk_id);
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

    /// Stop whatever ublk disk serves `bdev_name` (unstage fallback for
    /// volumes whose PV annotation is missing — F32-class stages from
    /// before the fix). The agent resolves the serving id from live SPDK
    /// state; the legacy volume-id hash is never a valid delete key (ids
    /// are agent-allocated, kernel-bounded to ublks_max).
    pub async fn delete_ublk_device_by_bdev(&self, bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [MINIMAL_UBLK] Deleting ublk device by backing bdev: {}", bdev_name);

        let delete_params = json!({
            "method": "ublk_stop_disk",
            "params": {
                "bdev_name": bdev_name
            }
        });

        match self.call_node_agent(&self.node_id, "/api/ublk/delete", &delete_params).await {
            Ok(_) => println!("✅ [MINIMAL_UBLK] Stopped ublk disk serving bdev: {}", bdev_name),
            Err(e) => {
                println!("⚠️ [MINIMAL_UBLK] Failed to stop disk by bdev (may not exist): {}", e);
                // Don't fail - cleanup is best effort
            }
        }

        Ok(())
    }

    /// Create NVMe-oF block device (internal implementation)
    async fn create_nvmeof_block_device(&self, bdev_name: &str, volume_id: &str) -> Result<BlockDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [NVMEOF_BLOCK] Creating NVMe-oF block device for bdev: {}", bdev_name);

        let nqn = crate::identity::volume_nqn(volume_id);
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
        use kube::api::{Patch, PatchParams};

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());

        // Annotation merge-patch instead of a full-object replace: the node
        // SA only holds the patch verb (phase 0 fix — the old replace was
        // rejected with 403, so unstage could never find the device info),
        // and patch avoids clobbering concurrent PV updates.
        let mut annotations = serde_json::Map::new();
        annotations.insert(
            "flint.io/block-device-backend".to_string(),
            json!(format!("{:?}", device_info.backend_type)),
        );
        match &device_info.cleanup_data {
            CleanupData::Ublk { ublk_id } => {
                annotations.insert("flint.io/ublk-id".to_string(), json!(ublk_id.to_string()));
            }
            CleanupData::Nvmeof { nqn, nvme_device } => {
                annotations.insert("flint.io/nvmeof-nqn".to_string(), json!(nqn));
                annotations.insert("flint.io/nvme-device".to_string(), json!(nvme_device));
            }
        }
        let patch = json!({ "metadata": { "annotations": annotations } });

        // F32: resolve the PV NAME from the handle. Backing volumes
        // (`nfs-server-…`) live on a PV named `flint-nfs-pv-…`; patching
        // by the raw handle 404'd, the error was swallowed as non-fatal,
        // and rehydration later re-minted the ublk device under the hash
        // fallback id — orphaning the nfs pod's mount (drill 3.3b).
        let pv_name = crate::identity::pv_name_of_handle(volume_id);
        pvs.patch(&pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await?;

        Ok(())
    }

    /// Retrieve block device info from PV annotations
    pub async fn get_block_device_info(&self, volume_id: &str) -> Result<BlockDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        // Same F32 name resolution as store_block_device_info: backing
        // handles must read the synthetic PV, not a PV named by the handle.
        let pv = pvs.get(&crate::identity::pv_name_of_handle(volume_id)).await?;

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
                        // Gate on the replicas attribute itself, not the
                        // count: a volume restored from a multi-replica
                        // snapshot carries ONE replica entry but must
                        // still stage through the raid path — its clone
                        // holds the source raid's superblock, so the
                        // filesystem sits at the raid data offset (§11;
                        // live-cluster regression 2026-06-12). Legacy
                        // single-replica volumes never have this field.
                        // Override-aware (U11): a re-placed identity in the
                        // annotation supersedes the immutable attribute.
                        if let Some(replicas_json) =
                            crate::replica_sync::raw_replicas_json(&pv)
                        {
                            let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)?;
                            return Ok(Some(replicas));
                        }

                        if csi.volume_attributes.is_some() {
                            // No replicas field: legacy bare-lvol volume
                            return Ok(None);
                        }
                    }
                }
            }
        }
        
        Err("PV not found".into())
    }

    /// Get volume info from PV volumeAttributes (fast path)
    /// Access modes of the PV named `pv_name` (empty when unset). Used by
    /// NodeUnstage to classify shared (RWX/ROX = NFS-consumer) volumes by
    /// the API's authority instead of sniffing mount state.
    pub async fn pv_access_modes(
        &self,
        pv_name: &str,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::Api;
        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        let pv = pvs.get(pv_name).await?;
        Ok(pv
            .spec
            .as_ref()
            .and_then(|s| s.access_modes.clone())
            .unwrap_or_default())
    }

    /// Whether the PV backing `volume_id` is a pNFS volume (tagged with
    /// pnfs.flint.io/* volumeAttributes at create time). Used by the
    /// context-free node RPCs (NodeUnstage) to classify without a
    /// volume_context. Any API failure reads as `false`: the caller
    /// falls through to the block-volume path, which has its own
    /// PV-read fallbacks.
    pub async fn pv_is_pnfs(&self, volume_id: &str) -> bool {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::Api;
        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        let pv_name = crate::identity::storage_id_of_handle(volume_id);
        match pvs.get(pv_name).await {
            Ok(pv) => pv
                .spec
                .as_ref()
                .and_then(|s| s.csi.as_ref())
                .and_then(|csi| csi.volume_attributes.as_ref())
                .map(|attrs| crate::snapshot::snapshot_csi::is_pnfs_volume_attrs(attrs))
                .unwrap_or(false),
            Err(_) => false,
        }
    }

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

        // Resolve to the storage id: the synthetic NFS backing volume's
        // handle (nfs-server-pvc-X) names no PV — annotating by handle
        // silently failed, so every RWX restage saw "never formatted" and
        // wipefs'd the live filesystem (data loss, found 2026-06-12). The
        // marker lives on the user PV, like every other volume-scoped record.
        let pv_name = crate::identity::storage_id_of_handle(volume_id);
        pvs.patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await?;

        Ok(())
    }

    /// Check if PV has filesystem-initialized annotation
    pub async fn check_pv_filesystem_initialized(&self, volume_id: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::Api;

        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());

        match pvs.get(crate::identity::storage_id_of_handle(volume_id)).await {
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
    pub(crate) fn parse_quantity(quantity_str: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
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

    /// Create RAID 1 bdev from replicas with mixed local/remote access
    ///
    /// This function handles graceful degradation: if some replicas are unavailable,
    /// it will create a degraded RAID with the available replicas (minimum 2 required).
    /// Unavailable replicas can be added later when their nodes recover.
    pub async fn create_raid_from_replicas(
        &self,
        volume_id: &str,
        replicas: &[ReplicaInfo],
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use crate::catchup::{self, CatchupStore};
        use crate::replica_sync::SyncState;

        let current_node = &self.node_id;
        // RWX volumes stage under the synthetic "nfs-server-<vol>" handle;
        // the sync record (and epoch namespace) lives on the user PV.
        let record_volume_id = crate::identity::storage_id_of_handle(volume_id).to_string();

        println!("🔧 [DRIVER] Creating RAID 1 on node: {}", current_node);
        println!("🔧 [DRIVER] Processing {} replicas...", replicas.len());

        // Staging is sync-state-aware (incremental-rebuild phase 4). The
        // record is consulted best-effort, and enforced only when the volume
        // has epoch history — without epochs the catch-up machinery cannot
        // heal an excluded replica, so legacy behavior (attach everything,
        // warn on stale admission) is the lesser hazard.
        let store = catchup::KubeStore { client: self.kube_client.clone() };
        let record = match store.load(&record_volume_id).await {
            Ok(r) => r,
            Err(e) => {
                println!(
                    "⚠️ [DRIVER] Cannot load replica sync record for {} (staging without it): {}",
                    record_volume_id, e
                );
                None
            }
        };
        let enforce = record.as_ref().map(|r| !r.epochs.is_empty()).unwrap_or(false);

        let mut base_bdevs = Vec::new();
        let mut attached_in_sync: Vec<String> = Vec::new();
        let mut unavailable_replicas: Vec<(&ReplicaInfo, String)> = Vec::new();
        let mut deferred_standbys: Vec<&ReplicaInfo> = Vec::new();
        let mut excluded_stale: Vec<(usize, &ReplicaInfo)> = Vec::new();

        for (i, replica) in replicas.iter().enumerate() {
            let rec = record.as_ref().and_then(|r| r.get(&replica.lvol_uuid));
            let state = rec.map(|r| r.sync_state).unwrap_or(SyncState::InSync);
            // After a catch-up revert the live head has a new uuid; the
            // identity uuid in volumeAttributes addresses nothing.
            let live_uuid = rec
                .map(|r| r.live_lvol_uuid().to_string())
                .unwrap_or_else(|| replica.lvol_uuid.clone());

            // Tier-2 7b: a hot-rejoin-marked replica is revert-first at
            // reassembly — its head may be an esnap clone whose external
            // parent no longer exists; the marker-driven reconciler owns
            // it. Never admit it directly, whatever its sync_state.
            if enforce && rec.map(|r| r.hot_rejoin.is_some()).unwrap_or(false) {
                println!(
                    "   Replica {}: HOT-REJOIN in progress on {} — excluded from assembly \
                     (revert-first; the hot-rejoin reconciler resolves it)",
                    i + 1,
                    replica.node_name
                );
                excluded_stale.push((i, replica));
                continue;
            }
            if enforce && state == SyncState::Standby {
                println!(
                    "   Replica {}: STANDBY on {} — deferred to final-delta admission",
                    i + 1,
                    replica.node_name
                );
                deferred_standbys.push(replica);
                continue;
            }
            if enforce && state == SyncState::Stale {
                println!(
                    "   Replica {}: STALE on {} — excluded from assembly (missed writes; \
                     the catch-up orchestrator heals it in the background)",
                    i + 1,
                    replica.node_name
                );
                excluded_stale.push((i, replica));
                continue;
            }

            match self.attach_replica_base(volume_id, i, replica, &live_uuid).await {
                Ok(bdev) => {
                    base_bdevs.push(bdev);
                    attached_in_sync.push(replica.lvol_uuid.clone());
                }
                Err(reason) => {
                    println!(
                        "   ✗ Replica {} on node {} UNAVAILABLE: {}",
                        i + 1,
                        replica.node_name,
                        reason
                    );
                    unavailable_replicas.push((replica, reason));
                }
            }
        }

        // Phase 4 (§6 rejoin-at-assembly): every survivor that attached is
        // now fenced to this node — no writer exists — so standbys can run
        // the final delta and join the create as in-sync members. Deferral
        // is contained per-standby (the replica stays a chasing standby).
        let mut admitted_standbys: Vec<catchup::AdmittedStandby> = Vec::new();
        let raid_name = crate::identity::raid_name(volume_id);
        if !deferred_standbys.is_empty() {
            let stage_cfg = catchup::StageConfig::from_env();
            admitted_standbys = catchup::admit_standbys_at_stage(
                self,
                &store,
                &record_volume_id,
                &raid_name,
                replicas,
                current_node,
                &attached_in_sync,
                &stage_cfg,
            )
            .await;
            for a in &admitted_standbys {
                println!(
                    "✅ [DRIVER] Standby replica on {} admitted in_sync at {} — base bdev {}",
                    a.node_name, a.final_epoch, a.bdev
                );
                base_bdevs.push(a.bdev.clone());
            }
        }

        // ── F36c: last-writer-set freshness gate ────────────────────────
        // (docs/f36c-assembly-freshness-gate.md). For a synchronous raid1,
        // every acked write lives on every leg of the LAST serving assembly
        // — so assembling without one of those legs while it is only
        // TRANSIENTLY unavailable serves a trailing lineage: the 6-write
        // tail lost in drill 3.6 run 3. A PERMANENTLY lost writer must
        // never manufacture an outage (drill 2.4): serve the survivor and
        // surface the bounded risk. Ordering is load-bearing: the gate runs
        // BEFORE the forced-stale fallback below, which is only legal once
        // the gate has ruled the missing writers gone (or the defer
        // deadline passed).
        let gate_cfg = crate::freshness_gate::GateConfig::from_env();
        const F36C_DEFER_KEY: &str = "flint.io/f36c-defer";
        const F36C_RISK_KEY: &str = "flint.io/acked-tail-risk";
        if gate_cfg.enabled {
            use crate::freshness_gate::{self as gate, GateDecision, LegAvailability, MissingWriter};

            // Recorded writers + claim-block corroboration: a leg whose
            // attach failed claim-shaped WAS in the previous assembly
            // whatever the record says — the record write may have lost the
            // race with the node death that stranded the claim.
            let mut writer_uuids: Vec<String> = record
                .as_ref()
                .map(|r| r.writer_uuids().to_vec())
                .unwrap_or_default();
            for (r, reason) in &unavailable_replicas {
                if gate::is_claim_blocked(reason) && !writer_uuids.contains(&r.lvol_uuid) {
                    writer_uuids.push(r.lvol_uuid.clone());
                }
            }

            let attached_now: Vec<&str> = attached_in_sync
                .iter()
                .map(|s| s.as_str())
                .chain(admitted_standbys.iter().map(|a| a.lvol_uuid.as_str()))
                .collect();
            let mut missing: Vec<MissingWriter> = Vec::new();
            for uuid in writer_uuids.iter().filter(|u| !attached_now.contains(&u.as_str())) {
                // Only legs still in the authoritative membership can be
                // probed or waited for; reconcile_membership prunes the
                // record on its next write.
                let Some(rep) = replicas.iter().find(|r| r.lvol_uuid == *uuid) else {
                    continue;
                };
                let availability = match unavailable_replicas
                    .iter()
                    .find(|(r, _)| r.lvol_uuid == *uuid)
                {
                    Some((_, reason)) if gate::is_claim_blocked(reason) => {
                        LegAvailability::ClaimBlocked
                    }
                    _ => self.node_availability(&rep.node_name).await,
                };
                missing.push(MissingWriter {
                    lvol_uuid: uuid.clone(),
                    node_name: rep.node_name.clone(),
                    availability,
                });
            }

            if missing.is_empty() {
                // Full writer set attached (or gate inert on this volume):
                // clear the transient defer marker. The RISK marker is NOT
                // cleared on this evidence — a ServeWithRisk assembly stamps
                // the survivor as the sole writer, so "all writers attached"
                // is true one tick after the loss was flagged (rc1's ~90s
                // amnesia, found live on runaa 3.6c). It clears only when
                // every flagged leg rejoined the writer set or left the
                // membership; the clear is evented as AckedTailResolved so
                // the loss history survives the annotation.
                let annos = self
                    .get_pv_annotations(&record_volume_id)
                    .await
                    .unwrap_or_default();
                if annos.contains_key(F36C_DEFER_KEY) {
                    self.set_pv_annotation(&record_volume_id, F36C_DEFER_KEY, None).await;
                }
                if let Some(marker) = annos.get(F36C_RISK_KEY) {
                    let member_uuids: Vec<String> =
                        replicas.iter().map(|r| r.lvol_uuid.clone()).collect();
                    if gate::risk_marker_resolved(marker, &writer_uuids, &member_uuids) {
                        self.set_pv_annotation(&record_volume_id, F36C_RISK_KEY, None).await;
                        crate::replica_sync::emit_pv_event(
                            &self.kube_client,
                            current_node,
                            &record_volume_id,
                            "Warning",
                            "AckedTailResolved",
                            &format!(
                                "F36c: acked-tail-risk marker resolved on {} — the flagged \
                                 leg(s) rejoined the serving lineage or were replaced; any \
                                 divergent tail is now a recorded loss, not a live fork \
                                 (marker was: {})",
                                current_node, marker
                            ),
                        )
                        .await;
                    }
                }
            } else {
                let now = crate::replica_sync::now_rfc3339();
                let mut missing_uuids: Vec<String> =
                    missing.iter().map(|m| m.lvol_uuid.clone()).collect();
                missing_uuids.sort();
                let annos = self
                    .get_pv_annotations(&record_volume_id)
                    .await
                    .unwrap_or_default();
                // Wall-clock defer bound, persisted so kubelet's retry
                // cadence can't stretch it; re-armed when the missing set
                // changes (partial progress is new evidence).
                let deadline_passed = match annos
                    .get(F36C_DEFER_KEY)
                    .and_then(|v| gate::parse_defer_marker(v))
                {
                    Some((deadline, prev)) if prev == missing_uuids => {
                        gate::deadline_passed(&deadline, &now)
                    }
                    _ => {
                        let deadline = gate::deadline_from(&now, gate_cfg.defer_secs);
                        self.set_pv_annotation(
                            &record_volume_id,
                            F36C_DEFER_KEY,
                            Some(&gate::encode_defer_marker(&deadline, &missing_uuids)),
                        )
                        .await;
                        false
                    }
                };
                let detail = gate::describe_missing(&missing);
                match gate::evaluate(&missing, deadline_passed, &gate_cfg) {
                    GateDecision::Proceed => {}
                    GateDecision::Defer => {
                        println!(
                            "⏳ [DRIVER] F36C DEFER: writer-set leg(s) transiently unavailable — \
                             refusing to assemble from a possibly-trailing leg: {}",
                            detail
                        );
                        crate::replica_sync::emit_pv_event(
                            &self.kube_client,
                            current_node,
                            &record_volume_id,
                            "Warning",
                            "AssemblyDeferred",
                            &format!(
                                "F36c: deferring degraded assembly on {} — last-writer leg(s) \
                                 transiently unavailable ({}); bound {}s, then serve-with-risk",
                                current_node, detail, gate_cfg.defer_secs
                            ),
                        )
                        .await;
                        return Err(format!(
                            "F36c freshness gate: last-writer leg(s) transiently unavailable ({}); \
                             deferring assembly so the freshest leg can rejoin (bound {}s)",
                            detail, gate_cfg.defer_secs
                        )
                        .into());
                    }
                    GateDecision::ServeWithRisk => {
                        println!(
                            "⚠️ [DRIVER] F36C SERVE-WITH-RISK: writer-set leg(s) permanently \
                             unavailable (or defer bound expired) — serving reachable legs: {}",
                            detail
                        );
                        self.set_pv_annotation(
                            &record_volume_id,
                            F36C_RISK_KEY,
                            Some(&format!("{}|{}", now, missing_uuids.join(","))),
                        )
                        .await;
                        self.set_pv_annotation(&record_volume_id, F36C_DEFER_KEY, None).await;
                        crate::replica_sync::emit_pv_event(
                            &self.kube_client,
                            current_node,
                            &record_volume_id,
                            "Warning",
                            "AckedTailRisk",
                            &format!(
                                "F36c: serving without last-writer leg(s) {} on {} — writes acked \
                                 after the last common point may be missing until the leg(s) \
                                 return or are replaced",
                                detail, current_node
                            ),
                        )
                        .await;
                    }
                }
            }
        }

        // Last-resort fallback: if exclusions left us below the 2-base
        // minimum, admit stale replicas rather than brick the volume —
        // exactly the pre-phase-4 behavior, still surfaced loudly via the
        // StaleReplicaAdmitted event below.
        let mut forced_stale: Vec<String> = Vec::new();
        if base_bdevs.len() < 2 && !excluded_stale.is_empty() {
            println!(
                "⚠️ [DRIVER] Below the 2-base minimum with stale replicas excluded — \
                 forced stale admission (divergence hazard, evented)"
            );
            for (i, replica) in &excluded_stale {
                if base_bdevs.len() >= 2 {
                    break;
                }
                let live_uuid = record
                    .as_ref()
                    .and_then(|r| r.get(&replica.lvol_uuid))
                    .map(|r| r.live_lvol_uuid().to_string())
                    .unwrap_or_else(|| replica.lvol_uuid.clone());
                match self.attach_replica_base(volume_id, *i, replica, &live_uuid).await {
                    Ok(bdev) => {
                        base_bdevs.push(bdev);
                        forced_stale.push(replica.lvol_uuid.clone());
                    }
                    Err(reason) => {
                        println!(
                            "   ✗ Replica {} on node {} UNAVAILABLE: {}",
                            i + 1,
                            replica.node_name,
                            reason
                        );
                        unavailable_replicas.push((replica, reason));
                    }
                }
            }
        }

        // Degraded assembly floor. This was total.min(2), which vetoed
        // staging after a PERMANENT node loss (2u/2.4: r2 + terminated
        // node = "need minimum 2 replicas, only 1 available" forever —
        // the exact outage replicas exist to survive) while a LIVE raid
        // losing the same leg keeps serving 1/2. Staleness is policed
        // explicitly above (Stale excluded when epoch history exists,
        // standbys deferred, hot-rejoin markers excluded); a
        // merely-UNAVAILABLE replica contributes no data, so refusing to
        // serve without it protects nothing. Assemble with whatever
        // in-sync legs attached; refuse only at zero. SPDK raid1 accepts
        // a single-base create.
        let total_replicas = replicas.len();
        let available_count = base_bdevs.len();
        let unavailable_count = unavailable_replicas.len();
        let min_required = 1;

        if available_count < min_required {
            // Cannot create RAID with fewer than 2 replicas
            let unavailable_nodes: Vec<String> = unavailable_replicas
                .iter()
                .map(|(r, reason)| format!("{} ({})", r.node_name, reason))
                .collect();

            return Err(format!(
                "Cannot create RAID 1: need minimum {} replicas, only {} available \
                 ({} standby deferred, {} stale excluded). Unavailable: [{}]",
                min_required,
                available_count,
                deferred_standbys.len() - admitted_standbys.len(),
                excluded_stale.len() - forced_stale.len(),
                unavailable_nodes.join(", ")
            ).into());
        }

        // Log degraded status if applicable
        if unavailable_count > 0 {
            println!("⚠️ [DRIVER] DEGRADED MODE: {}/{} replicas available",
                     available_count, total_replicas);
            for (replica, reason) in &unavailable_replicas {
                println!("   - {} on node {}: {}",
                         replica.lvol_uuid, replica.node_name, reason);
            }
            println!("   Note: Unavailable replicas will be re-added when nodes recover");
        }

        // Replicas deliberately left out in a healable state (standby or
        // stale-with-catch-up-pending) must not be re-marked stale or
        // counted as silently-admitted by the bookkeeping below.
        let skipped: Vec<String> = deferred_standbys
            .iter()
            .map(|r| r.lvol_uuid.clone())
            .filter(|u| !admitted_standbys.iter().any(|a| a.lvol_uuid == *u))
            .chain(
                excluded_stale
                    .iter()
                    .map(|(_, r)| r.lvol_uuid.clone())
                    .filter(|u| !forced_stale.contains(u)),
            )
            .collect();

        // F36c record-before-writes: persist the new serving membership
        // (writer set + exclusion staleness) BEFORE the assembly can take a
        // write. A crash between this write and raid-online leaves a record
        // that is at worst conservative — a too-early stale mark or a
        // too-large writer set only defers a later assembly, never loses an
        // acked write.
        self.record_assembly_sync_state(volume_id, replicas, &unavailable_replicas, &skipped)
            .await;

        // The attaches above registered new bdevs; any that carry an old raid
        // superblock will spawn a phantom raid from the asynchronous examine
        // hook. Settle examine before looking at raid state (§3 discipline).
        if let Err(e) = self.wait_for_examine().await {
            println!("⚠️ [DRIVER] bdev_wait_for_examine failed (continuing): {}", e);
        }

        // SPDK v26.05 raid1 refuses a single-base create (EINVAL, verified
        // live) — so a multi-replica volume down to ONE in-sync leg after a
        // permanent node loss (2u/2.4) is served DIRECT on that leg, no
        // raid layer, exactly like an r1 volume. The PV is annotated so the
        // consumer-blindness monitor doesn't strike-and-repair against the
        // missing raid; the annotation clears on the next >=2-leg assembly
        // (replica re-placement is the follow-up that restores redundancy).
        // The consumer-blindness monitor reads the degraded-direct exemption
        // from the PV it examines: the BACKING PV for RWX (it skips RWX user
        // PVs via is_rwx_pv), the user PV for RWO. rc1/rc2 wrote it only to
        // the user PV, so an RWX single-survivor direct serve entered the
        // monitor's destructive repair loop within 3 strikes — the F38
        // enabler found live on runaa (docs/f38-reentry-export-drop.md).
        // Write where the monitor reads; keep the user-PV copy for
        // operators/dashboards.
        let monitor_pv = match crate::identity::parse_backing_handle(volume_id) {
            Some(storage_id) => crate::identity::backing_pv_name(storage_id),
            None => record_volume_id.clone(),
        };
        let raid_bdev_name = if base_bdevs.len() == 1 && total_replicas > 1 {
            let direct = base_bdevs.into_iter().next().unwrap();
            println!(
                "⚠️ [DRIVER] SINGLE-SURVIVOR DIRECT SERVE: {} on {} (no raid layer; \
                 redundancy lost until a replacement replica is provisioned)",
                direct, current_node
            );
            self.set_pv_annotation(&record_volume_id, "flint.io/degraded-direct", Some(current_node))
                .await;
            if monitor_pv != record_volume_id {
                self.set_pv_annotation(&monitor_pv, "flint.io/degraded-direct", Some(current_node))
                    .await;
            }
            crate::replica_sync::emit_pv_event(
                &self.kube_client,
                current_node,
                &record_volume_id,
                "Warning",
                "DegradedDirectServe",
                &format!(
                    "Serving the single surviving replica DIRECT on {} (1 of {} legs, no raid \
                     layer); redundancy restores via replica re-placement + catch-up",
                    current_node, total_replicas
                ),
            )
            .await;
            direct
        } else {
            // Create RAID 1 bdev with available replicas
            println!("🔧 [DRIVER] Creating RAID 1 bdev: {} with {} base bdevs",
                     raid_name, base_bdevs.len());

            let name = self.ensure_raid1_bdev(&raid_name, base_bdevs).await?;

            if unavailable_count > 0 {
                println!("⚠️ [DRIVER] RAID 1 bdev created in DEGRADED mode: {}", name);
                crate::replica_sync::emit_pv_event(
                    &self.kube_client,
                    current_node,
                    &record_volume_id,
                    "Warning",
                    "DegradedAssembly",
                    &format!(
                        "RAID1 assembled DEGRADED on {}: {} of {} legs ({} unavailable); \
                         missing legs heal via catch-up or re-placement",
                        current_node, available_count, total_replicas, unavailable_count
                    ),
                )
                .await;
            } else {
                println!("✅ [DRIVER] RAID 1 bdev created: {}", name);
            }
            self.set_pv_annotation(&record_volume_id, "flint.io/degraded-direct", None)
                .await;
            if monitor_pv != record_volume_id {
                self.set_pv_annotation(&monitor_pv, "flint.io/degraded-direct", None).await;
            }
            name
        };

        Ok(raid_bdev_name)
    }

    /// Delete every flint-owned NVMe-oF subsystem on this node that exports
    /// `bdev` (see `flint_subsystems_exporting_bdev` for the match rules).
    /// Used at NodeStage before a consumer-local lvol is claimed as a raid
    /// base; live-cluster regression 2026-06-12 (stage-2 e2e on runf).
    async fn drop_stale_local_exports(
        &self,
        bdev: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let payload = serde_json::json!({"method": "nvmf_get_subsystems", "params": {}});
        let response = self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await?;
        let empty = serde_json::json!([]);
        let subsystems = response.get("result").unwrap_or(&empty);
        for nqn in flint_subsystems_exporting_bdev(subsystems, bdev) {
            println!("   Dropping stale local export {} of {}", nqn, bdev);
            let del = serde_json::json!({
                "method": "nvmf_delete_subsystem",
                "params": {"nqn": nqn}
            });
            self.call_node_agent(&self.node_id, "/api/spdk/rpc", &del).await?;
        }
        Ok(())
    }

    /// Attach one replica as a raid base on this node: local lvols verify
    /// directly, remote ones go through the fenced NVMe-oF export + attach.
    /// `bdev_name` is the live head (the identity uuid unless a catch-up
    /// revert recorded an `active_lvol_uuid` override).
    async fn attach_replica_base(
        &self,
        volume_id: &str,
        replica_index: usize,
        replica: &ReplicaInfo,
        bdev_name: &str,
    ) -> Result<String, String> {
        if replica.node_name == self.node_id {
            println!("   Replica {}: LOCAL access (lvol: {})", replica_index + 1, bdev_name);
            // A surviving NVMe-oF export of this lvol (created while a
            // previous consumer ran on another node) holds a write-mode
            // open, so the raid module's exclusive claim of the local base
            // fails with EPERM at bdev_raid_create. This node is the
            // volume's new exclusive consumer — the fence whitelist already
            // admits only us — so any subsystem exporting the lvol is
            // stale; drop it before claiming.
            if let Err(e) = self.drop_stale_local_exports(bdev_name).await {
                println!("   ⚠ Stale local export cleanup failed (continuing): {}", e);
            }
            match self.verify_local_lvol_exists(bdev_name).await {
                Ok(bdev) => {
                    println!("   ✓ Local replica verified: {}", bdev);
                    Ok(bdev)
                }
                Err(e) => Err(format!("Local lvol not found: {}", e)),
            }
        } else {
            println!(
                "   Replica {}: REMOTE access (node: {}, setting up NVMe-oF...)",
                replica_index + 1,
                replica.node_name
            );
            match self
                .try_attach_remote_replica(volume_id, replica, replica_index, bdev_name)
                .await
            {
                Ok(nvme_bdev) => {
                    println!("   ✓ Attached remote replica as: {}", nvme_bdev);
                    Ok(nvme_bdev)
                }
                Err(e) => Err(format!("NVMe-oF connection failed: {}", e)),
            }
        }
    }

    /// Persist the membership outcome of a raid assembly on the PV
    /// (incremental-rebuild phase 1, §9-1). Called BEFORE the assembly is
    /// created (F36c record-before-writes): a replica excluded here stops
    /// receiving writes the moment the degraded raid goes online — that is
    /// the in_sync → stale transition — and the included legs become the
    /// volume's writer set, stamped before they can take a write. A stale
    /// replica that was force-admitted (phase-4 below-minimum fallback) is
    /// surfaced as a StaleReplicaAdmitted event. `skipped` are replicas
    /// deliberately left out in a healable state (deferred standbys,
    /// excluded stale): they are neither re-marked stale (a standby must
    /// keep chasing) nor counted as admitted. Best effort: the node agent's
    /// health monitor converges the record.
    async fn record_assembly_sync_state(
        &self,
        volume_id: &str,
        replicas: &[ReplicaInfo],
        unavailable_replicas: &[(&ReplicaInfo, String)],
        skipped: &[String],
    ) {
        use crate::replica_sync::{self, SyncState};

        let excluded: Vec<(String, String)> = unavailable_replicas
            .iter()
            .map(|(r, reason)| (r.lvol_uuid.clone(), reason.clone()))
            .collect();
        let included: Vec<String> = replicas
            .iter()
            .map(|r| r.lvol_uuid.clone())
            .filter(|uuid| !excluded.iter().any(|(e, _)| e == uuid))
            .filter(|uuid| !skipped.contains(uuid))
            .collect();

        let now = replica_sync::now_rfc3339();
        let current_node = self.node_id.clone();
        let mut newly_stale: Vec<String> = Vec::new();
        let mut admitted_stale: Vec<String> = Vec::new();
        let result = replica_sync::update_sync_record(&self.kube_client, volume_id, |record| {
            // The closure may run more than once on write conflicts.
            newly_stale.clear();
            admitted_stale.clear();
            admitted_stale.extend(
                included
                    .iter()
                    .filter(|uuid| {
                        record
                            .get(uuid)
                            .map(|r| r.sync_state != SyncState::InSync)
                            .unwrap_or(false)
                    })
                    .cloned(),
            );
            for (uuid, reason) in &excluded {
                let why = format!(
                    "excluded from raid assembly on {}: {}",
                    current_node, reason
                );
                if record.mark_stale(uuid, &why, &now) {
                    newly_stale.push(uuid.clone());
                }
            }
            // F36c: `included` is exactly the membership of the assembly
            // about to be created (in-sync attachers, stage-admitted
            // standbys, forced-stale admissions) — stamp it as the writer
            // set before it can take a write.
            record.set_writer_set(&included, &now);
        })
        .await;

        match result {
            Ok(Some(_)) => {
                for uuid in &newly_stale {
                    let node = replicas
                        .iter()
                        .find(|r| r.lvol_uuid == *uuid)
                        .map(|r| r.node_name.as_str())
                        .unwrap_or("unknown");
                    replica_sync::emit_pv_event(
                        &self.kube_client,
                        &self.node_id,
                        volume_id,
                        "Warning",
                        "ReplicaStale",
                        &format!(
                            "Replica {} on node {} excluded from raid assembly on {} — it is no longer receiving writes",
                            uuid, node, self.node_id
                        ),
                    )
                    .await;
                }
                for uuid in &admitted_stale {
                    let node = replicas
                        .iter()
                        .find(|r| r.lvol_uuid == *uuid)
                        .map(|r| r.node_name.as_str())
                        .unwrap_or("unknown");
                    println!(
                        "⚠️ [DRIVER] STALE replica {} (node {}) admitted to raid for {} without catch-up — \
                         reads may return stale data for writes it missed (incremental rebuild lands in phase 3/4)",
                        uuid, node, volume_id
                    );
                    replica_sync::emit_pv_event(
                        &self.kube_client,
                        &self.node_id,
                        volume_id,
                        "Warning",
                        "StaleReplicaAdmitted",
                        &format!(
                            "Stale replica {} on node {} was admitted to the raid on {} without catch-up; \
                             it may serve stale reads for the writes it missed",
                            uuid, node, self.node_id
                        ),
                    )
                    .await;
                }
            }
            Ok(None) => {} // single-replica volume: no sync record applies
            Err(e) => {
                println!(
                    "⚠️ [DRIVER] Failed to update replica sync record for {} (non-fatal): {}",
                    volume_id, e
                );
            }
        }
    }

    /// Try to attach a remote replica via NVMe-oF
    /// Returns Ok(bdev_name) on success, Err on failure (node down, connection refused, etc.)
    /// `bdev_name` is the bdev to export — the replica's live head uuid
    /// (post-revert override aware, incremental-rebuild phase 4).
    async fn try_attach_remote_replica(
        &self,
        volume_id: &str,
        replica: &ReplicaInfo,
        replica_index: usize,
        bdev_name: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Create NVMe-oF target on remote node
        let conn_info = self.setup_nvmeof_target_on_node(
            &replica.node_name,
            bdev_name,
            &format!("{}_{}", volume_id, replica_index),
            &self.node_id,
        ).await?;

        // Attach NVMe-oF target from current node
        let nvme_bdev = self.connect_to_nvmeof_target(&conn_info).await?;
        Ok(nvme_bdev)
    }

    /// Verify that a local lvol exists and return its bdev name
    async fn verify_local_lvol_exists(
        &self,
        lvol_uuid: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Query SPDK to verify the lvol exists
        let payload = serde_json::json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": lvol_uuid
            }
        });

        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await {
            Ok(response) => {
                // Check if response contains the bdev
                if let Some(result) = response.get("result") {
                    if let Some(bdevs) = result.as_array() {
                        if !bdevs.is_empty() {
                            // Return the bdev name (could be UUID or lvs/name format)
                            if let Some(name) = bdevs[0].get("name").and_then(|n| n.as_str()) {
                                return Ok(name.to_string());
                            }
                            return Ok(lvol_uuid.to_string());
                        }
                    }
                }
                Err(format!("Lvol {} not found in SPDK", lvol_uuid).into())
            }
            Err(e) => Err(format!("Failed to query lvol {}: {}", lvol_uuid, e).into())
        }
    }

    /// Whether the node-local SPDK supports `bdev_raid_delete` with
    /// `clear_sb` (added in SPDK v26.05). Without it, deleting a phantom
    /// raid leaves the on-disk superblocks behind and they re-arm the §3
    /// examine hazard on the next bdev registration.
    pub async fn spdk_supports_clear_sb(&self) -> bool {
        *self
            .clear_sb_support
            .get_or_init(|| async {
                let payload = json!({ "method": "spdk_get_version" });
                match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await {
                    Ok(resp) => {
                        let major = resp["result"]["fields"]["major"].as_i64().unwrap_or(0);
                        let minor = resp["result"]["fields"]["minor"].as_i64().unwrap_or(0);
                        let supported = major > 26 || (major == 26 && minor >= 5);
                        println!(
                            "ℹ️ [DRIVER] SPDK v{}.{:02} — bdev_raid_delete clear_sb {}",
                            major,
                            minor,
                            if supported { "supported" } else { "NOT supported (phantom deletes leave superblocks)" }
                        );
                        supported
                    }
                    Err(_) => false,
                }
            })
            .await
    }

    /// Best-effort PV annotation set/clear (merge patch; None removes).
    /// Used for operational state markers like `flint.io/degraded-direct` —
    /// a failed patch must never fail the data-path operation it decorates.
    pub async fn set_pv_annotation(&self, pv_name: &str, key: &str, value: Option<&str>) {
        use k8s_openapi::api::core::v1::PersistentVolume;
        use kube::api::{Api, Patch, PatchParams};
        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        let patch = serde_json::json!({ "metadata": { "annotations": { key: value } } });
        if let Err(e) = pvs
            .patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            println!(
                "⚠️ [DRIVER] PV annotation patch failed ({}={:?} on {}): {}",
                key, value, pv_name, e
            );
        }
    }

    /// Read a PV's annotations. Best effort: None on API error or missing
    /// PV — callers treat that as "no markers".
    pub async fn get_pv_annotations(
        &self,
        pv_name: &str,
    ) -> Option<std::collections::BTreeMap<String, String>> {
        use k8s_openapi::api::core::v1::PersistentVolume;
        let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
        pvs.get_opt(pv_name)
            .await
            .ok()
            .flatten()
            .and_then(|pv| pv.metadata.annotations)
    }

    /// F36c permanent-vs-transient evidence for a missing writer's node.
    /// An API blip reads as NodeReady (transient): deferring while blind is
    /// the bounded-safe direction — the defer deadline caps it. F33 caveat
    /// (Ready node, dead tgt) is likewise absorbed by the deadline, not by
    /// trusting Ready as proof of anything beyond "not permanently gone".
    async fn node_availability(&self, node_name: &str) -> crate::freshness_gate::LegAvailability {
        use crate::freshness_gate::LegAvailability;
        let nodes: Api<k8sNode> = Api::all(self.kube_client.clone());
        match nodes.get_opt(node_name).await {
            Err(_) => LegAvailability::NodeReady,
            Ok(None) => LegAvailability::NodeGone,
            Ok(Some(node)) => {
                let ready = node
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .and_then(|cs| cs.iter().find(|c| c.type_ == "Ready"));
                match ready {
                    Some(c) if c.status == "True" => LegAvailability::NodeReady,
                    Some(c) => {
                        let not_ready_secs = c
                            .last_transition_time
                            .as_ref()
                            .map(|t| {
                                (chrono::Utc::now().timestamp() - t.0.as_second()).max(0) as u64
                            })
                            .unwrap_or(0);
                        LegAvailability::NodeNotReady { not_ready_secs }
                    }
                    // No Ready condition at all (node just registered):
                    // treat as freshly NotReady — transient, deadline-bounded.
                    None => LegAvailability::NodeNotReady { not_ready_secs: 0 },
                }
            }
        }
    }

    /// Wait for all in-flight bdev examine callbacks to finish. Examine is
    /// asynchronous: a bdev registered a moment ago (nvme attach, lvolstore
    /// load) may still be spawning a phantom raid. Settle before inspecting.
    async fn wait_for_examine(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let payload = json!({ "method": "bdev_wait_for_examine" });
        self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await?;
        Ok(())
    }

    /// Fetch the raid bdev record by name on the local SPDK, if present.
    async fn get_raid_bdev(
        &self,
        raid_name: &str,
    ) -> Result<Option<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let payload = json!({
            "method": "bdev_raid_get_bdevs",
            "params": { "category": "all" }
        });
        let response = self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await?;
        let raid = response
            .get("result")
            .and_then(|r| r.as_array())
            .and_then(|raids| {
                raids
                    .iter()
                    .find(|r| r.get("name").and_then(|n| n.as_str()) == Some(raid_name))
            })
            .cloned();
        Ok(raid)
    }

    /// Delete a raid bdev, clearing on-disk superblocks when SPDK supports it.
    async fn delete_raid_bdev(
        &self,
        raid_name: &str,
        clear_sb: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut params = json!({ "name": raid_name });
        if clear_sb {
            params["clear_sb"] = json!(true);
        }
        let payload = json!({ "method": "bdev_raid_delete", "params": params });
        self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await?;
        Ok(())
    }

    /// Create-or-converge the RAID 1 bdev (phase 0 fix).
    ///
    /// The §3 examine hook auto-assembles a phantom raid under this very name
    /// the moment an attached base carrying a superblock registers, and a
    /// previous partially-failed NodeStage may have left a live raid behind —
    /// a blind `bdev_raid_create` then fails `-EEXIST` forever. Converge
    /// instead: reuse an ONLINE raid (idempotent restage), delete anything
    /// else (phantoms are CONFIGURING; clear_sb when available so the
    /// superblocks can't respawn them) and create, retrying once around races
    /// with in-flight examine.
    async fn ensure_raid1_bdev(
        &self,
        raid_name: &str,
        base_bdevs: Vec<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        const MAX_ATTEMPTS: usize = 3;

        for attempt in 1..=MAX_ATTEMPTS {
            if let Some(raid) = self.get_raid_bdev(raid_name).await? {
                let state = raid.get("state").and_then(|s| s.as_str()).unwrap_or("unknown");
                if state == "online" {
                    let configured = raid
                        .get("base_bdevs_list")
                        .and_then(|b| b.as_array())
                        .map(|bases| {
                            bases
                                .iter()
                                .filter(|b| b.get("is_configured").and_then(|c| c.as_bool()).unwrap_or(false))
                                .count()
                        })
                        .unwrap_or(0);
                    println!(
                        "♻️ [DRIVER] RAID {} already ONLINE ({} base(s) configured) — reusing",
                        raid_name, configured
                    );
                    return Ok(raid_name.to_string());
                }

                // CONFIGURING (phantom from examine) or offline leftover:
                // remove it so our create can proceed.
                let clear_sb = self.spdk_supports_clear_sb().await;
                println!(
                    "🧹 [DRIVER] RAID {} exists in state '{}' (phantom/stale) — deleting (clear_sb: {})",
                    raid_name, state, clear_sb
                );
                self.delete_raid_bdev(raid_name, clear_sb).await?;
                // Examine may still be in flight for another base; let it
                // settle so the phantom can't re-create itself between our
                // delete and create.
                let _ = self.wait_for_examine().await;
            }

            // superblock:false — flint never uses SPDK's examine-based
            // auto-reassembly (raids are ephemeral, re-created here at every
            // NodeStage from the PV replica record), so the superblock only
            // ever hurt: it is the root of the §3 phantom-assembly hazard
            // class, and it shifted the filesystem 1 MiB into every base
            // lvol, which made snapshots/clones unmountable raw and silently
            // formatted volumes restored from multi-replica snapshots
            // (live-cluster regression, 2026-06-12). Without it the base
            // lvols carry the filesystem at LBA 0, identical to bare
            // single-replica volumes. Layout change: lvols written by
            // superblocked builds (≤1.2.0-rc4, pre-release only) are
            // incompatible — recreate, don't upgrade in place.
            let payload = json!({
                "method": "bdev_raid_create",
                "params": {
                    "name": raid_name,
                    "raid_level": "1",
                    "base_bdevs": base_bdevs,
                    "superblock": false,
                }
            });

            match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await {
                Ok(_) => {
                    println!("✅ [DRIVER] RAID 1 bdev created: {}", raid_name);
                    return Ok(raid_name.to_string());
                }
                Err(e) => {
                    let msg = e.to_string();
                    let eexist = msg.contains("File exists") || msg.contains("Code=-17");
                    if eexist && attempt < MAX_ATTEMPTS {
                        println!(
                            "🔄 [DRIVER] bdev_raid_create hit EEXIST (attempt {}/{}) — phantom re-appeared, re-converging",
                            attempt, MAX_ATTEMPTS
                        );
                        let _ = self.wait_for_examine().await;
                        continue;
                    }
                    return Err(format!("Failed to create RAID bdev {}: {}", raid_name, e).into());
                }
            }
        }

        Err(format!(
            "RAID bdev {} did not converge after {} attempts (phantom kept re-appearing)",
            raid_name, MAX_ATTEMPTS
        )
        .into())
    }

    /// Setup NVMe-oF target and return connection info. `consumer_node` is
    /// the node whose kernel/SPDK initiator will connect — the fencing
    /// whitelist admits exactly that host, so the caller must pass the real
    /// consumer (ControllerPublish runs in the controller process, where
    /// `self.node_id` is the controller pod, not the consumer).
    pub async fn setup_nvmeof_target_on_node(&self, node_name: &str, bdev_name: &str, volume_id: &str, consumer_node: &str) -> Result<NvmeofConnectionInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🌐 [DRIVER] Setting up NVMe-oF target on node: {} for bdev: {}", node_name, bdev_name);

        let nqn = crate::identity::volume_nqn(volume_id);

        let node_ip = self.get_node_ip(node_name).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?;

        // Convergent export: inspects subsystem/namespace/listener state and
        // only issues the RPCs that are missing, so a partially-created export
        // from an earlier failed attempt is completed instead of failing on
        // duplicates (phase 0 fix; see docs/phase0-hazard-repro-2026-06-10.md).
        // Fencing: the consumer doing the staging is the single admitted
        // host — staging IS the fence flip that locks out a previous
        // consumer (doc §3).
        let allowed = vec![crate::nvmeof_export::flint_host_nqn(consumer_node)];
        let transport = crate::nvmeof_export::NodeAgentTransport { driver: self, node_name };
        let spec = crate::nvmeof_export::ExportSpec {
            nqn: &nqn,
            bdev_name,
            bdev_aliases: &[],
            trtype: &self.nvmeof_transport,
            traddr: &node_ip,
            trsvcid: self.nvmeof_target_port,
            allowed_hosts: crate::nvmeof_export::fencing_enabled().then_some(allowed.as_slice()),
            ns_identity: None,
        };
        crate::nvmeof_export::ensure_export(&transport, &spec).await?;

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

        let expected_bdev = format!("{}n1", controller_name);

        // Pre-check (phase 0 fix): an earlier stage attempt, a kubelet retry,
        // or a previous consumer of this volume on this node may have left
        // this controller behind. Reuse it while it serves a bdev; replace it
        // when it is dead weight (e.g. state=failed after the remote rebooted
        // — exactly what the live repro observed). The old code only reacted
        // to an "already exists" error string SPDK does not reliably emit.
        if self.nvme_controller_exists(&controller_name).await {
            if self.verify_bdev_exists(&expected_bdev).await.is_ok() {
                println!("♻️ [DRIVER] NVMe controller {} already attached with bdev {} — reusing",
                         controller_name, expected_bdev);
                return Ok(expected_bdev);
            }
            println!("🧹 [DRIVER] NVMe controller {} exists without usable bdev (stale/failed) — detaching",
                     controller_name);
            if let Err(e) = self.delete_nvme_controller(&controller_name).await {
                println!("⚠️ [DRIVER] Failed to delete stale controller (continuing): {}", e);
            }
        }

        let attach_params = json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": controller_name,
                "trtype": conn_info.transport.to_uppercase(),
                "traddr": conn_info.target_ip,
                "trsvcid": conn_info.target_port.to_string(),
                "subnqn": conn_info.nqn,
                "adrfam": "IPv4",
                // Stable per-node identity so the target's host fencing can
                // admit exactly this consumer (doc §3). Default initiator
                // NQNs are random, which makes host filtering impossible.
                "hostnqn": crate::nvmeof_export::flint_host_nqn(&self.node_id),
                // Survivable reconnect (chaos drill 1u/B3): without these,
                // a storage-node outage of ~a minute makes the initiator
                // DROP the bdev, which cascades into the ublk disk being
                // stopped and the consumer mount destroyed — the SPDK-
                // initiator mirror of the kernel-side ctrl-loss-tmo that
                // v1.15.0 graceful-recovery #2 added to the loopback path.
                // -1 = retry forever; I/O queues until the target returns.
                "ctrlr_loss_timeout_sec": -1,
                "reconnect_delay_sec": 2
            }
        });

        println!("📡 [DRIVER] Calling bdev_nvme_attach_controller...");
        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &attach_params).await {
            Ok(response) => {
                // The response should contain the bdev names created
                if let Some(bdev_name) = response
                    .get("result")
                    .and_then(|r| r.as_array())
                    .and_then(|names| names.first())
                    .and_then(|b| b.as_str())
                {
                    println!("✅ [DRIVER] Connected to NVMe-oF target, bdev created: {}", bdev_name);
                    return Ok(bdev_name.to_string());
                }
                // Fallback - construct expected bdev name
                println!("✅ [DRIVER] Connected to NVMe-oF target, expected bdev: {}", expected_bdev);
                Ok(expected_bdev)
            }
            Err(e) => {
                // Attach failed — possibly a race with a concurrent attach of
                // the same controller. Verify state before giving up.
                if self.verify_bdev_exists(&expected_bdev).await.is_ok() {
                    println!("♻️ [DRIVER] Attach raced but bdev {} exists — reusing", expected_bdev);
                    return Ok(expected_bdev);
                }
                println!("❌ [DRIVER] bdev_nvme_attach_controller failed: {}", e);
                Err(format!("Failed to attach NVMe controller: {}", e).into())
            }
        }
    }

    /// Whether an NVMe-oF initiator controller with this name exists locally.
    async fn nvme_controller_exists(&self, controller_name: &str) -> bool {
        let payload = json!({
            "method": "bdev_nvme_get_controllers",
            "params": { "name": controller_name }
        });
        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &payload).await {
            Ok(response) => response
                .get("result")
                .and_then(|r| r.as_array())
                .map(|c| !c.is_empty())
                .unwrap_or(false),
            // Missing controller surfaces as an RPC error; treat lookup
            // failures as absent and let the attach surface real problems.
            Err(_) => false,
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

    /// Inspect the node-local raid bdev for a multi-replica volume.
    /// Returns None when no raid exists locally (single-replica volume or
    /// not staged here); otherwise (degraded, message).
    /// Phase 0 fix: leg failure used to be invisible — the control plane kept
    /// reporting both replicas online while the array ran un-redundant.
    pub async fn check_local_raid_health(&self, volume_id: &str) -> Option<(bool, String)> {
        let raid_name = crate::identity::raid_name(volume_id);
        let raid = self.get_raid_bdev(&raid_name).await.ok()??;

        let state = raid.get("state").and_then(|s| s.as_str()).unwrap_or("unknown");
        let total = raid.get("num_base_bdevs").and_then(|n| n.as_u64()).unwrap_or(0);
        let bases = raid
            .get("base_bdevs_list")
            .and_then(|b| b.as_array())
            .cloned()
            .unwrap_or_default();
        let configured = bases
            .iter()
            .filter(|b| b.get("is_configured").and_then(|c| c.as_bool()).unwrap_or(false))
            .count() as u64;

        let degraded = state != "online" || configured < total;
        let message = if degraded {
            format!(
                "RAID {} state={} with {}/{} base bdevs configured — volume is running without full redundancy",
                raid_name, state, configured, total
            )
        } else {
            format!("RAID {} online with {}/{} base bdevs", raid_name, configured, total)
        };
        Some((degraded, message))
    }

    /// Tear down all node-local SPDK state for a volume at NodeUnstage
    /// (phase 0 fix). The shipped driver left the raid bdev ONLINE after
    /// unstage; its exclusive claims on the local replica lvol and the
    /// remote-replica bdevs then blocked every later export/stage of the
    /// volume (docs/phase0-hazard-repro-2026-06-10.md, bug 1).
    ///
    /// Order matters: stop serving (loopback subsystem) → delete the raid
    /// (clear_sb while the remote legs are still attached, so the wipe
    /// reaches all replicas) → detach the per-replica initiator controllers.
    ///
    /// Raid-delete failure is returned as an error so kubelet retries
    /// unstage — a live raid's claims cannot be converged around later.
    /// Subsystem and controller cleanup are best-effort.
    pub async fn teardown_volume_spdk_state(&self, volume_id: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // 1. Loopback subsystem (idempotent; absent for never-staged volumes).
        // Through the GUARDED endpoint, not a raw nvmf_delete_subsystem: this
        // runs during NodeUnstage, where the F9 guard must decide whether the
        // subsystem is still serving a cross-node consumer (a stale unstage
        // after force-detach + re-attach would otherwise kill the live
        // export — the raw RPC here bypassed the guard placed on the
        // delete_nvmeof path).
        let loopback_nqn = crate::identity::volume_nqn(volume_id);
        let delete_body = json!({ "nqn": loopback_nqn });
        match self.call_node_agent(&self.node_id, "/api/blockdev/delete_nvmeof", &delete_body).await {
            Ok(_) => println!("✅ [DRIVER] Loopback subsystem cleanup done (guarded): {}", loopback_nqn),
            Err(e) => println!("ℹ️ [DRIVER] Loopback subsystem not deleted (may not exist): {}", e),
        }

        // 2. Raid bdev — frees the exclusive_write claims
        let raid_name = crate::identity::raid_name(volume_id);
        if self.get_raid_bdev(&raid_name).await?.is_some() {
            let clear_sb = self.spdk_supports_clear_sb().await;
            self.delete_raid_bdev(&raid_name, clear_sb)
                .await
                .map_err(|e| format!("Failed to delete raid {} at unstage: {}", raid_name, e))?;
            println!("✅ [DRIVER] RAID deleted at unstage: {} (clear_sb: {})", raid_name, clear_sb);
        }

        // 3. Per-replica initiator controllers (nvme_..._volume_{vol}_{i})
        let per_replica_prefix = format!(
            "nvme_{}_",
            crate::identity::volume_nqn(volume_id)
                .replace(":", "_")
                .replace(".", "_")
        );
        let list = json!({ "method": "bdev_nvme_get_controllers" });
        if let Ok(response) = self.call_node_agent(&self.node_id, "/api/spdk/rpc", &list).await {
            if let Some(controllers) = response.get("result").and_then(|r| r.as_array()) {
                for ctrlr in controllers {
                    if let Some(name) = ctrlr.get("name").and_then(|n| n.as_str()) {
                        if name.starts_with(&per_replica_prefix) {
                            match self.delete_nvme_controller(name).await {
                                Ok(_) => println!("✅ [DRIVER] Detached replica controller: {}", name),
                                Err(e) => println!("⚠️ [DRIVER] Failed to detach controller {} (continuing): {}", name, e),
                            }
                        }
                    }
                }
            }
        }

        Ok(())
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

    /// Node currently attached to this volume per its VolumeAttachment, if
    /// any (controller-side helper; the node agent keeps an equivalent).
    /// Used to keep snapshot-restore copy reads off the data path (§11).
    pub async fn get_attached_node(&self, volume_id: &str) -> Option<String> {
        use k8s_openapi::api::storage::v1::VolumeAttachment;
        use kube::api::ListParams;

        let vas: Api<VolumeAttachment> = Api::all(self.kube_client.clone());
        let list = vas.list(&ListParams::default()).await.ok()?;
        list.items.into_iter().find_map(|va| {
            let source_pv = va.spec.source.persistent_volume_name.as_deref();
            let attached = va.status.as_ref().map(|s| s.attached).unwrap_or(false);
            if source_pv == Some(volume_id) && attached {
                Some(va.spec.node_name)
            } else {
                None
            }
        })
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

/// Pick the placement node for a single-replica volume: the first node in
/// `preferred` (topology hint order) that is also a viable candidate, else
/// the candidate with the most free capacity.
///
/// `candidates` is `(node_name, free_capacity)` for every node already known
/// to have enough room (any order). Pure and total so it can be unit-tested
/// without a cluster; the async caller wraps it with capacity discovery and
/// reservation. Returns `None` only when `candidates` is empty. An empty
/// `preferred` (or one naming only nodes without capacity) yields the
/// max-free node — byte-identical to the pre-topology behaviour.
pub fn pick_placement_node(candidates: &[(String, u64)], preferred: &[String]) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    // Honor the first preferred node that is a viable candidate.
    for want in preferred {
        if let Some((name, _)) = candidates.iter().find(|(name, _)| name == want) {
            return Some(name.clone());
        }
    }
    // Fallback: most free capacity. max_by is stable on the first max seen;
    // matches the previous sort-desc-then-[0] selection.
    candidates
        .iter()
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(name, _)| name.clone())
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

/// NQNs of flint-owned NVMe-oF subsystems (`:volume:` NQNs) that export
/// `bdev`, matched against each namespace's `bdev_name`, `uuid` or `name`.
/// Matching by namespace — not by reconstructing the NQN — covers every
/// historical NQN shape and the `active_lvol_uuid` override after a
/// catch-up revert. The raid loopback subsystem exports the raid bdev, not
/// the lvol, so it can never match.
fn flint_subsystems_exporting_bdev(subsystems: &Value, bdev: &str) -> Vec<String> {
    let mut nqns = Vec::new();
    let Some(list) = subsystems.as_array() else {
        return nqns;
    };
    for s in list {
        let nqn = s.get("nqn").and_then(|v| v.as_str()).unwrap_or("");
        if !nqn.contains(":volume:") {
            continue;
        }
        let exports_bdev = s
            .get("namespaces")
            .and_then(|v| v.as_array())
            .is_some_and(|ns| {
                ns.iter().any(|n| {
                    ["bdev_name", "uuid", "name"]
                        .iter()
                        .any(|k| n.get(*k).and_then(|v| v.as_str()) == Some(bdev))
                })
            });
        if exports_bdev {
            nqns.push(nqn.to_string());
        }
    }
    nqns
}

#[cfg(test)]
mod local_export_tests {
    use super::flint_subsystems_exporting_bdev;

    fn subsystems() -> serde_json::Value {
        serde_json::json!([
            {
                "nqn": "nqn.2014-08.org.nvmexpress.discovery",
                "namespaces": []
            },
            {
                "nqn": "nqn.2024-11.com.flint:volume:pvc-aaa_1",
                "namespaces": [{"nsid": 1, "bdev_name": "6f21eb70-63fb-4dbe-9caa-ef3af2253592",
                                "uuid": "6f21eb70-63fb-4dbe-9caa-ef3af2253592"}]
            },
            {
                "nqn": "nqn.2024-11.com.flint:volume:pvc-aaa",
                "namespaces": [{"nsid": 1, "bdev_name": "raid_pvc-aaa"}]
            },
            {
                "nqn": "nqn.2024-11.com.flint:volume:pvc-bbb_0",
                "namespaces": [{"nsid": 1, "bdev_name": "lvs/vol_pvc-bbb_replica_0",
                                "uuid": "11111111-2222-3333-4444-555555555555"}]
            }
        ])
    }

    #[test]
    fn matches_only_subsystems_exporting_the_bdev() {
        // Live-cluster repro (runf, 2026-06-12): consumer re-staged onto the
        // replica-hosting node; the replica export (here pvc-aaa_1) blocked
        // bdev_raid_create with EPERM and had to be dropped.
        let nqns = flint_subsystems_exporting_bdev(
            &subsystems(),
            "6f21eb70-63fb-4dbe-9caa-ef3af2253592",
        );
        assert_eq!(nqns, vec!["nqn.2024-11.com.flint:volume:pvc-aaa_1".to_string()]);
    }

    #[test]
    fn matches_namespace_uuid_when_bdev_name_is_an_alias() {
        let nqns = flint_subsystems_exporting_bdev(
            &subsystems(),
            "11111111-2222-3333-4444-555555555555",
        );
        assert_eq!(nqns, vec!["nqn.2024-11.com.flint:volume:pvc-bbb_0".to_string()]);
    }

    #[test]
    fn ignores_raid_loopback_and_foreign_subsystems() {
        // The volume-level loopback exports the raid bdev — never the lvol —
        // and non-flint NQNs are out of scope entirely.
        assert!(flint_subsystems_exporting_bdev(&subsystems(), "raid_pvc-zzz").is_empty());
        let nqns = flint_subsystems_exporting_bdev(&subsystems(), "raid_pvc-aaa");
        assert_eq!(nqns, vec!["nqn.2024-11.com.flint:volume:pvc-aaa".to_string()]);
    }

    #[test]
    fn tolerates_malformed_subsystem_lists() {
        assert!(flint_subsystems_exporting_bdev(&serde_json::json!(null), "x").is_empty());
        assert!(flint_subsystems_exporting_bdev(&serde_json::json!({}), "x").is_empty());
        assert!(flint_subsystems_exporting_bdev(&serde_json::json!([{"no_nqn": true}]), "x").is_empty());
    }
}

#[cfg(test)]
mod placement_tests {
    use super::pick_placement_node;

    fn c(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|(n, f)| (n.to_string(), *f)).collect()
    }

    #[test]
    fn empty_candidates_is_none() {
        assert_eq!(pick_placement_node(&[], &["a".to_string()]), None);
    }

    #[test]
    fn empty_preferred_picks_max_free() {
        // No topology hint => historical behaviour: most free capacity wins.
        let cands = c(&[("a", 10), ("b", 100), ("cc", 50)]);
        assert_eq!(pick_placement_node(&cands, &[]).as_deref(), Some("b"));
    }

    #[test]
    fn preferred_with_capacity_wins_over_max_free() {
        // "a" is preferred and viable even though "b" has far more room.
        let cands = c(&[("a", 10), ("b", 100)]);
        assert_eq!(
            pick_placement_node(&cands, &["a".to_string()]).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn preferred_without_capacity_falls_back_to_max_free() {
        // "z" is preferred but not a candidate (no capacity) => max-free "b".
        let cands = c(&[("a", 10), ("b", 100)]);
        assert_eq!(
            pick_placement_node(&cands, &["z".to_string()]).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn first_viable_preferred_wins_in_hint_order() {
        // Ordered hint: "z" has no capacity, so the next preferred "a" wins
        // even though "b" is also preferred and has more room.
        let cands = c(&[("a", 10), ("b", 100)]);
        let preferred = vec!["z".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(pick_placement_node(&cands, &preferred).as_deref(), Some("a"));
    }

    #[test]
    fn single_candidate_returned_regardless_of_hint() {
        let cands = c(&[("only", 5)]);
        assert_eq!(pick_placement_node(&cands, &[]).as_deref(), Some("only"));
        assert_eq!(
            pick_placement_node(&cands, &["other".to_string()]).as_deref(),
            Some("only")
        );
    }
}
