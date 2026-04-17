// node_agent_minimal.rs - Clean Minimal State Node Agent
// FOR NODE AGENTS ONLY - Uses direct Unix socket communication with SPDK
// Replaces all CRD operations with MinimalDiskService

use std::sync::Arc;
use tokio::time::{Duration, interval};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use warp::Filter;
use warp::{Rejection, Reply};
use warp::http::StatusCode;
use tracing::{debug, info, warn, error};

use crate::minimal_disk_service::MinimalDiskService;
use crate::driver::SpdkCsiDriver;
use crate::minimal_models::ReplicaInfo;

// Kubernetes API imports for node UID and PV queries
use kube::Api;
use kube::api::ListParams;
use k8s_openapi::api::core::v1::PersistentVolume;

/// Minimal State Node Agent - Uses direct SPDK queries instead of CRDs
pub struct NodeAgent {
    pub node_name: String,
    pub node_uid: Arc<tokio::sync::RwLock<String>>, // Kubernetes node UID for PV label-based replica discovery
    pub spdk_socket_path: String,
    pub disk_service: MinimalDiskService,
    pub driver: Arc<SpdkCsiDriver>,
}

impl NodeAgent {
    pub fn new(
        node_name: String,
        spdk_socket_path: String,
        driver: Arc<SpdkCsiDriver>,
    ) -> Self {
        let disk_service = MinimalDiskService::new(node_name.clone(), spdk_socket_path.clone());

        Self {
            node_name,
            node_uid: Arc::new(tokio::sync::RwLock::new(String::new())), // Will be fetched asynchronously in start()
            spdk_socket_path,
            disk_service,
            driver,
        }
    }

    /// Create a new NodeAgent with reserved devices loaded
    pub async fn new_with_reserved_devices(
        node_name: String,
        spdk_socket_path: String,
        driver: Arc<SpdkCsiDriver>,
    ) -> Self {
        let disk_service = MinimalDiskService::new_with_reserved_devices(
            node_name.clone(),
            spdk_socket_path.clone()
        ).await;

        Self {
            node_name,
            node_uid: Arc::new(tokio::sync::RwLock::new(String::new())), // Will be fetched asynchronously in start()
            spdk_socket_path,
            disk_service,
            driver,
        }
    }

    /// Start the minimal node agent with HTTP API
    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(node_name = %self.node_name, "[NODE_AGENT] Starting minimal state node agent");

        // Fetch node UID from Kubernetes API for replica discovery
        debug!("[NODE_AGENT] Fetching node UID from Kubernetes API");
        match self.driver.get_node_uid(&self.node_name).await {
            Ok(uid) => {
                *self.node_uid.write().await = uid.clone();
                info!(node_uid = %uid, "[NODE_AGENT] Node UID fetched");
            }
            Err(e) => {
                warn!(error = %e, "[NODE_AGENT] Failed to fetch node UID - replica reconciliation will be disabled");
            }
        }

        // Initialize ublk subsystem on startup
        debug!("[NODE_AGENT] Initializing ublk subsystem");
        match self.disk_service.call_spdk_rpc(&json!({
            "method": "ublk_create_target",
            "params": {}
        })).await {
            Ok(_) => info!("[NODE_AGENT] ublk subsystem initialized"),
            Err(e) if e.to_string().contains("Method not found") => {
                info!("[NODE_AGENT] SPDK doesn't support ublk - skipping initialization");
            }
            Err(e) if e.to_string().contains("already exists") => {
                debug!("[NODE_AGENT] ublk subsystem already initialized");
            }
            Err(e) => {
                warn!(error = %e, "[NODE_AGENT] ublk initialization failed (continuing anyway)");
            }
        }

        // Run initial disk discovery with auto-recovery at startup
        // This creates bdevs and loads existing LVS stores
        debug!("[NODE_AGENT] Running initial disk discovery with auto-recovery");
        match self.disk_service.discover_local_disks().await {
            Ok(disks) => {
                info!(disk_count = disks.len(), "[NODE_AGENT] Initial discovery completed");
                for disk in &disks {
                    if disk.blobstore_initialized {
                        debug!(
                            device = %disk.device_name,
                            pci = %disk.pci_address,
                            lvs = disk.lvs_name.as_ref().unwrap_or(&"unknown".to_string()),
                            "[NODE_AGENT] LVS initialized"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "[NODE_AGENT] Initial discovery failed (continuing anyway)");
            }
        }

        // Reconcile replica targets for node recovery
        // This sets up NVMe-oF targets for any local replicas so remote RAID members can reconnect
        let node_uid = self.node_uid.read().await.clone();
        if !node_uid.is_empty() {
            debug!("[NODE_AGENT] Running replica target reconciliation");
            if let Err(e) = self.reconcile_replica_targets().await {
                warn!(error = %e, "[NODE_AGENT] Replica reconciliation failed (non-fatal)");
            }
        } else {
            debug!("[NODE_AGENT] Skipping replica reconciliation (no node UID)");
        }

        // Start disk discovery loop (use FAST mode to avoid expensive auto-recovery every 30s)
        // Auto-recovery already ran above during startup
        let disk_service = self.disk_service.clone();
        let discovery_task = tokio::spawn(async move {
            let mut discovery_interval = interval(Duration::from_secs(30));
            loop {
                discovery_interval.tick().await;
                // Use FAST discovery (no auto-recovery) to reduce SPDK RPC spam
                // Auto-recovery is expensive (400+ RPC calls) and only needed at startup
                match disk_service.discover_local_disks_fast().await {
                    Ok(disks) => {
                        debug!(disk_count = disks.len(), "[DISK_DISCOVERY] Found disks on node");
                        for disk in &disks {
                            debug!(
                                device = %disk.device_name,
                                pci = %disk.pci_address,
                                size_bytes = disk.size_bytes,
                                healthy = disk.healthy,
                                initialized = disk.blobstore_initialized,
                                "[DISK_DISCOVERY] Disk status"
                            );
                        }
                    }
                    Err(e) => error!(error = %e, "[DISK_DISCOVERY] Error"),
                }
            }
        });

        // Setup HTTP API routes
        let routes = self.setup_routes();

        // Read node agent port from environment variable (default: 9081)
        // Changed from 8081 to 9081 to avoid conflicts with nginx ingress controllers
        let node_agent_port: u16 = std::env::var("NODE_AGENT_PORT")
            .unwrap_or("9081".to_string())
            .parse()
            .unwrap_or(9081);

        info!(port = node_agent_port, "[NODE_AGENT] Starting HTTP server");

        // Start both the HTTP server and discovery loop
        tokio::select! {
            _ = warp::serve(routes).run(([0, 0, 0, 0], node_agent_port)) => {
                info!("[NODE_AGENT] HTTP server stopped");
            }
            _ = discovery_task => {
                info!("[NODE_AGENT] Discovery task stopped");
            }
        }

        Ok(())
    }

    /// Setup HTTP API routes for controller communication
    fn setup_routes(&self) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
        let node_agent = Arc::new(self.clone());

        // GET /api/disks - List all disks
        let list_disks = warp::path!("api" / "disks")
            .and(warp::get())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_list_disks);

        // POST /api/disks - List all disks (RPC-style for controller)
        let list_disks_post = warp::path!("api" / "disks")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_list_disks_post);

        // POST /api/disks/uninitialized - List uninitialized disks (RPC-style)
        let list_uninitialized = warp::path!("api" / "disks" / "uninitialized")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_get_uninitialized_disks);

        // POST /api/disks/status - Get disk status (RPC-style)
        let disk_status = warp::path!("api" / "disks" / "status")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_get_disk_status);

        // POST /api/disks/initialize_blobstore - Initialize blobstore on disk
        let init_blobstore = warp::path!("api" / "disks" / "initialize_blobstore")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_initialize_blobstore);

        // POST /api/disks/setup - Setup multiple disks (dashboard compatibility)
        let setup_disks = warp::path!("api" / "disks" / "setup")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_setup_disks);

        // POST /api/disks/initialize - Initialize disks (dashboard compatibility)
        let initialize_disks = warp::path!("api" / "disks" / "initialize")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_initialize_disks);

        // POST /api/disks/reset - Reset disk configuration (dashboard compatibility)
        let reset_disks = warp::path!("api" / "disks" / "reset")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_reset_disks);

        // POST /api/memory_disks/create - Create a memory (malloc) disk
        let create_memory_disk = warp::path!("api" / "memory_disks" / "create")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_create_memory_disk);

        // POST /api/memory_disks/delete - Delete a memory (malloc) disk
        let delete_memory_disk = warp::path!("api" / "memory_disks" / "delete")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_delete_memory_disk);

        // POST /api/volumes/create_lvol - Create logical volume
        let create_lvol = warp::path!("api" / "volumes" / "create_lvol")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_create_lvol);

        // POST /api/volumes/delete_lvol - Delete logical volume  
        let delete_lvol = warp::path!("api" / "volumes" / "delete_lvol")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_delete_lvol);

        // POST /api/volumes/resize_lvol - Resize logical volume
        let resize_lvol = warp::path!("api" / "volumes" / "resize_lvol")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_resize_lvol);

        // POST /api/spdk/rpc - Generic SPDK RPC proxy
        let spdk_rpc = warp::path!("api" / "spdk" / "rpc")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_spdk_rpc);

        // POST /api/ublk/create_target - Create ublk target
        let ublk_create_target = warp::path!("api" / "ublk" / "create_target")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_ublk_create_target);

        // POST /api/ublk/create - Create ublk device
        let ublk_create = warp::path!("api" / "ublk" / "create")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_ublk_create);

        // POST /api/ublk/delete - Delete ublk device
        let ublk_delete = warp::path!("api" / "ublk" / "delete")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_ublk_delete);

        // POST /api/blockdev/create_nvmeof - Create NVMe-oF block device
        let blockdev_create_nvmeof = warp::path!("api" / "blockdev" / "create_nvmeof")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_blockdev_create_nvmeof);

        // POST /api/blockdev/delete_nvmeof - Delete NVMe-oF block device
        let blockdev_delete_nvmeof = warp::path!("api" / "blockdev" / "delete_nvmeof")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_blockdev_delete_nvmeof);

        // POST /api/volumes/get_info - Get volume information
        let get_volume_info = warp::path!("api" / "volumes" / "get_info")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_get_volume_info);

        // POST /api/volumes/force_unstage - Force unstage volume (defensive cleanup)
        let force_unstage = warp::path!("api" / "volumes" / "force_unstage")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_force_unstage);

        // POST /api/volumes/check_health - Check if volume backing storage exists and is healthy
        let check_volume_health = warp::path!("api" / "volumes" / "check_health")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_check_volume_health);

        // POST /api/volumes/check_exists - Check if lvol exists (lightweight existence check)
        let check_volume_exists = warp::path!("api" / "volumes" / "check_exists")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_check_volume_exists);

        // GET /api/system/memory - Get node memory information
        let system_memory = warp::path!("api" / "system" / "memory")
            .and(warp::get())
            .and_then(Self::handle_system_memory);

        // ============= SNAPSHOT MODULE INTEGRATION =============
        // Register snapshot routes (isolated module - no changes to existing routes)
        use crate::snapshot::{SnapshotService, register_snapshot_routes};
        let snapshot_service = Arc::new(SnapshotService::new(
            self.node_name.clone(),
            self.disk_service.spdk_socket_path.clone(),
        ));
        let snapshot_routes = register_snapshot_routes(snapshot_service);
        // ============= END SNAPSHOT INTEGRATION =============

        // Combine all routes (boxed to avoid type overflow with deep .or() nesting)
        let routes = list_disks
            .or(list_disks_post)
            .or(list_uninitialized)
            .or(disk_status)
            .or(init_blobstore)
            .or(setup_disks)
            .or(initialize_disks)
            .or(reset_disks)
            .or(create_memory_disk)
            .or(delete_memory_disk)
            .or(create_lvol)
            .or(delete_lvol)
            .or(resize_lvol)
            .boxed()
            .or(spdk_rpc)
            .or(ublk_create_target)
            .or(ublk_create)
            .or(ublk_delete)
            .or(blockdev_create_nvmeof)
            .or(blockdev_delete_nvmeof)
            .or(get_volume_info)
            .or(force_unstage)
            .or(check_volume_health)
            .or(check_volume_exists)
            .or(system_memory)
            .or(snapshot_routes);

        routes.with(warp::cors().allow_any_origin())
    }

    fn with_node_agent(&self, node_agent: Arc<NodeAgent>) -> impl Filter<Extract = (Arc<NodeAgent>,), Error = std::convert::Infallible> + Clone {
        warp::any().map(move || node_agent.clone())
    }

    /// Handle GET /api/disks
    async fn handle_list_disks(node_agent: Arc<NodeAgent>) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling list disks request");
        
        match node_agent.disk_service.discover_local_disks().await {
            Ok(disks) => {
                let response = json!({
                    "status": "success",
                    "disks": disks
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = json!({
                    "status": "error",
                    "message": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/disks (RPC-style for controller)
    async fn handle_list_disks_post(
        _request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        use std::time::Instant;
        let start = Instant::now();
        let request_id = format!("{:08x}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u32);
        
        debug!(request_id, "[HTTP_API] POST /api/disks - Starting FAST disk discovery");
        
        match node_agent.disk_service.discover_local_disks_fast().await {
            Ok(disks) => {
                let elapsed = start.elapsed();
                debug!(request_id, ?elapsed, disk_count = disks.len(), "[HTTP_API] Discovery completed");

                let response = json!({
                    "disks": disks
                });

                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let elapsed = start.elapsed();
                error!(request_id, ?elapsed, error = %e, "[HTTP_API] Discovery failed");

                let error_response = json!({
                    "success": false,
                    "error": e.to_string()
                });

                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/disks/initialize_blobstore
    async fn handle_initialize_blobstore(
        request: InitializeBlobstoreRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!(pci_address = %request.pci_address, "[HTTP_API] Handling initialize blobstore request");
        
        match node_agent.disk_service.initialize_blobstore(&request.pci_address).await {
            Ok(lvs_name) => {
                let response = json!({
                    "status": "success",
                    "lvs_name": lvs_name
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = json!({
                    "status": "error", 
                    "message": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/volumes/create_lvol
    async fn handle_create_lvol(
        request: CreateLvolRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!(volume_id = %request.volume_id, thin_provision = request.thin_provision, "[HTTP_API] Handling create lvol request");
        
        match node_agent.disk_service.create_lvol(&request.lvs_name, &request.volume_id, request.size_bytes, request.thin_provision).await {
            Ok(lvol_uuid) => {
                let response = json!({
                    "status": "success",
                    "lvol_uuid": lvol_uuid
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = json!({
                    "status": "error",
                    "message": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/volumes/delete_lvol
    async fn handle_delete_lvol(
        request: DeleteLvolRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!(lvol_uuid = %request.lvol_uuid, "[HTTP_API] Handling delete lvol request");
        
        match node_agent.disk_service.delete_lvol(&request.lvol_uuid).await {
            Ok(_) => {
                let response = json!({
                    "status": "success"
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = json!({
                    "status": "error",
                    "message": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/volumes/resize_lvol - Resize logical volume
    async fn handle_resize_lvol(
        request: ResizeLvolRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!(lvol_uuid = %request.lvol_uuid, new_size_bytes = request.new_size_bytes, "[HTTP_API] Handling resize lvol request");
        
        match node_agent.disk_service.resize_lvol(&request.lvol_uuid, request.new_size_bytes).await {
            Ok(_) => {
                let response = json!({
                    "status": "success",
                    "lvol_uuid": request.lvol_uuid,
                    "new_size_bytes": request.new_size_bytes
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = json!({
                    "status": "error",
                    "message": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/disks/uninitialized - List uninitialized disks (RPC-style)
    async fn handle_get_uninitialized_disks(_request: Value, node_agent: Arc<NodeAgent>) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling get uninitialized disks request (fast mode)");
        
        // Use fast discovery to avoid timeout - no LVS auto-recovery
        match node_agent.disk_service.discover_local_disks_fast().await {
            Ok(disks) => {
                // Return ALL disks with complete fields so frontend can categorize them
                let all_disks: Vec<_> = disks.iter()
                    .filter(|d| d.healthy)
                    .map(|d| json!({
                        "pci_address": d.pci_address,
                        "device_name": d.device_name,
                        "size_bytes": d.size_bytes,
                        "model": d.model,
                        "healthy": d.healthy,
                        // Additional fields expected by frontend
                        "vendor_id": "0x0000",
                        "device_id": "0x0000",
                        "subsystem_vendor_id": "0x0000",
                        "subsystem_device_id": "0x0000",
                        "numa_node": 0,
                        "driver": if d.blobstore_initialized { "vfio-pci" } else { "kernel" },
                        "serial": "",
                        "firmware_version": "",
                        "namespace_id": 1,
                        "mounted_partitions": &d.mounted_partitions,
                        "filesystem_type": null,
                        "is_system_disk": d.is_system_disk,
                        "spdk_ready": d.blobstore_initialized,  // LVS initialized = ready
                        // Memory disks (malloc) are always driver_ready, physical disks need LVS
                        "driver_ready": d.pci_address.starts_with("memory:") || d.blobstore_initialized,
                        "blobstore_initialized": d.blobstore_initialized,
                        "discovered_at": chrono::Utc::now().to_rfc3339()
                    }))
                    .collect();
                
                let response = json!({
                    "success": true,
                    "node": node_agent.node_name,
                    "uninitialized_disks": all_disks,  // Keep same field name for compatibility
                    "count": all_disks.len()
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = json!({
                    "success": false,
                    "error": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/disks/status - Get disk status (RPC-style)
    /// Returns ALL disks with complete fields for Disk Setup tab
    async fn handle_get_disk_status(_request: Value, node_agent: Arc<NodeAgent>) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling get disk status request (fast mode)");
        
        // Use fast discovery to avoid timeout - no LVS auto-recovery
        match node_agent.disk_service.discover_local_disks_fast().await {
            Ok(disks) => {
                // Return all disks with complete fields for frontend filtering
                let disk_statuses: Vec<_> = disks.iter()
                    .filter(|d| d.healthy)
                    .map(|d| json!({
                        "pci_address": d.pci_address,
                        "device_name": d.device_name,
                        "size_bytes": d.size_bytes,
                        "model": d.model,
                        "healthy": d.healthy,
                        // Additional fields expected by frontend
                        "vendor_id": "0x0000",
                        "device_id": "0x0000",
                        "subsystem_vendor_id": "0x0000",
                        "subsystem_device_id": "0x0000",
                        "numa_node": 0,
                        "driver": d.driver,
                        "serial": "",
                        "firmware_version": "",
                        "namespace_id": 1,
                        "mounted_partitions": &d.mounted_partitions,
                        "filesystem_type": null,
                        "is_system_disk": d.is_system_disk,
                        "spdk_ready": d.blobstore_initialized,
                        // Memory disks (malloc) are always driver_ready, physical disks need LVS
                        "driver_ready": d.pci_address.starts_with("memory:") || d.blobstore_initialized,
                        "blobstore_initialized": d.blobstore_initialized,
                        "discovered_at": chrono::Utc::now().to_rfc3339(),
                        "free_space": d.free_space,
                        "temperature": null,
                        "error_count": 0
                    }))
                    .collect();
                
                let response = json!({
                    "node": node_agent.node_name,
                    "disks": disk_statuses,
                    "last_updated": chrono::Utc::now().to_rfc3339()
                });
                
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = json!({
                    "success": false,
                    "error": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/disks/setup - Setup multiple disks
    async fn handle_setup_disks(
        request: DiskSetupRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let disks = request.get_disks();
        debug!(disk_count = disks.len(), "[HTTP_API] Handling setup disks request");
        
        let mut setup_disks = Vec::new();
        let mut failed_disks = Vec::new();
        let mut warnings = Vec::new();
        
        for pci_address in &disks {
            match node_agent.disk_service.initialize_blobstore(pci_address).await {
                Ok(_lvs_name) => {
                    setup_disks.push(pci_address.clone());
                }
                Err(e) => {
                    failed_disks.push(pci_address.clone());
                    warnings.push(format!("Failed to setup {}: {}", pci_address, e));
                }
            }
        }
        
        let response = json!({
            "success": failed_disks.is_empty(),
            "setup_disks": setup_disks,
            "failed_disks": failed_disks,
            "warnings": warnings,
            "completed_at": chrono::Utc::now().to_rfc3339()
        });
        
        Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
    }

    /// Handle POST /api/disks/initialize - Alternative name for setup
    async fn handle_initialize_disks(
        request: DiskSetupRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        Self::handle_setup_disks(request, node_agent).await
    }

    /// Handle POST /api/disks/reset - Reset disk configuration
    async fn handle_reset_disks(
        request: DiskSetupRequest,
        _node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let disks = request.get_disks();
        debug!(disk_count = disks.len(), "[HTTP_API] Handling reset disks request");

        // TODO: Implement actual disk reset
        let response = json!({
            "success": false,
            "setup_disks": [],
            "failed_disks": disks,
            "warnings": ["Disk reset not yet implemented in minimal state"],
            "completed_at": chrono::Utc::now().to_rfc3339()
        });

        Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::NOT_IMPLEMENTED))
    }

    /// Handle POST /api/memory_disks/create - Create a memory (malloc) disk
    async fn handle_create_memory_disk(
        request: CreateMemoryDiskRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!(name = %request.name, size_mb = request.size_mb, "[HTTP_API] Handling create memory disk request");

        match node_agent.disk_service.create_memory_disk(
            &request.name,
            request.size_mb,
            request.block_size
        ).await {
            Ok(bdev_name) => {
                let response = json!({
                    "success": true,
                    "bdev_name": bdev_name,
                    "message": format!("Memory disk '{}' created successfully ({}MB)", bdev_name, request.size_mb),
                    "completed_at": chrono::Utc::now().to_rfc3339()
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let response = json!({
                    "success": false,
                    "error": format!("Failed to create memory disk: {}", e),
                    "completed_at": chrono::Utc::now().to_rfc3339()
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/memory_disks/delete - Delete a memory (malloc) disk
    async fn handle_delete_memory_disk(
        request: DeleteMemoryDiskRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!(name = %request.name, "[HTTP_API] Handling delete memory disk request");

        match node_agent.disk_service.delete_memory_disk(&request.name).await {
            Ok(()) => {
                let response = json!({
                    "success": true,
                    "message": format!("Memory disk '{}' deleted successfully", request.name),
                    "completed_at": chrono::Utc::now().to_rfc3339()
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let response = json!({
                    "success": false,
                    "error": format!("Failed to delete memory disk: {}", e),
                    "completed_at": chrono::Utc::now().to_rfc3339()
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle GET /api/system/memory - Get node memory information
    async fn handle_system_memory() -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling system memory request");

        match tokio::fs::read_to_string("/proc/meminfo").await {
            Ok(meminfo) => {
                let mut mem_total_kb = 0u64;
                let mut mem_available_kb = 0u64;

                for line in meminfo.lines() {
                    if line.starts_with("MemTotal:") {
                        mem_total_kb = line.split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                    } else if line.starts_with("MemAvailable:") {
                        mem_available_kb = line.split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                    }
                }

                let mem_total_mb = mem_total_kb / 1024;
                let mem_available_mb = mem_available_kb / 1024;

                let response = json!({
                    "total_mb": mem_total_mb,
                    "available_mb": mem_available_mb,
                    "total_kb": mem_total_kb,
                    "available_kb": mem_available_kb
                });

                debug!(total_mb = mem_total_mb, available_mb = mem_available_mb, "[HTTP_API] Memory info");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                error!(error = %e, "[HTTP_API] Failed to read /proc/meminfo");
                let error_response = json!({
                    "error": format!("Failed to read memory info: {}", e)
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/spdk/rpc - Generic SPDK RPC proxy
    async fn handle_spdk_rpc(
        rpc_request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let method = rpc_request["method"].as_str().unwrap_or("unknown");
        debug!(method, "[HTTP_API] Handling SPDK RPC request");

        // Proxy the RPC request directly to SPDK
        match node_agent.disk_service.call_spdk_rpc(&rpc_request).await {
            Ok(response) => {
                debug!(method, "[HTTP_API] SPDK RPC succeeded");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                error!(method, error = %e, "[HTTP_API] SPDK RPC failed");
                let error_response = json!({
                    "success": false,
                    "error": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/ublk/create_target - Create ublk target
    async fn handle_ublk_create_target(
        _request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling ublk target creation");

        let ublk_target_rpc = json!({
            "method": "ublk_create_target",
            "params": {}
        });

        match node_agent.disk_service.call_spdk_rpc(&ublk_target_rpc).await {
            Ok(response) => {
                info!("[HTTP_API] ublk target created successfully");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) if e.to_string().contains("Method not found") => {
                // SPDK doesn't support ublk - not an error
                let warning_response = json!({
                    "success": true,
                    "message": "SPDK doesn't support ublk - skipping"
                });
                info!("[HTTP_API] SPDK doesn't support ublk - returning success");
                Ok(warp::reply::with_status(warp::reply::json(&warning_response), StatusCode::OK))
            }
            Err(e) => {
                error!(error = %e, "[HTTP_API] ublk target creation failed");
                let error_response = json!({
                    "success": false,
                    "error": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/ublk/create - Create ublk device
    async fn handle_ublk_create(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling ublk device creation");

        // Extract method and params from request
        let method = request["method"].as_str().unwrap_or("ublk_start_disk");
        let params = &request["params"];

        // Check if ublk device already exists (idempotency for ROX/RWX)
        if let Some(ublk_id) = params["ublk_id"].as_u64() {
            let device_path = format!("/dev/ublkb{}", ublk_id);
            if std::path::Path::new(&device_path).exists() {
                debug!(device_path, "[HTTP_API] ublk device already exists (idempotent)");
                let success_response = json!({
                    "result": device_path
                });
                return Ok(warp::reply::with_status(warp::reply::json(&success_response), StatusCode::OK));
            }
        }
        
        let ublk_rpc = json!({
            "method": method,
            "params": params
        });
        
        match node_agent.disk_service.call_spdk_rpc(&ublk_rpc).await {
            Ok(response) => {
                debug!("[HTTP_API] ublk device created successfully");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                error!(error = %e, "[HTTP_API] ublk device creation failed");
                let error_response = json!({
                    "success": false,
                    "error": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/ublk/delete - Delete ublk device
    async fn handle_ublk_delete(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling ublk device deletion");

        // Extract method and params from request
        let method = request["method"].as_str().unwrap_or("ublk_stop_disk");
        let params = &request["params"];

        let ublk_rpc = json!({
            "method": method,
            "params": params
        });

        match node_agent.disk_service.call_spdk_rpc(&ublk_rpc).await {
            Ok(response) => {
                debug!("[HTTP_API] ublk device deleted successfully");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                warn!(error = %e, "[HTTP_API] ublk device deletion failed (may not exist)");
                // For deletion, we return success even if it fails (cleanup is best effort)
                let success_response = json!({
                    "success": true,
                    "message": "Device deleted or did not exist"
                });
                Ok(warp::reply::with_status(warp::reply::json(&success_response), StatusCode::OK))
            }
        }
    }

    /// Handle POST /api/blockdev/create_nvmeof - Create NVMe-oF block device
    async fn handle_blockdev_create_nvmeof(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling NVMe-oF block device creation");

        let bdev_name = match request["bdev_name"].as_str() {
            Some(name) => name,
            None => {
                let error_response = json!({
                    "success": false,
                    "error": "Missing bdev_name"
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::BAD_REQUEST));
            }
        };
        let nqn = match request["nqn"].as_str() {
            Some(n) => n,
            None => {
                let error_response = json!({
                    "success": false,
                    "error": "Missing nqn"
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::BAD_REQUEST));
            }
        };
        let target_ip = request["target_ip"].as_str().unwrap_or("127.0.0.1");
        let target_port = request["target_port"].as_u64().unwrap_or(4420) as u16;

        // 1. Check if already connected (idempotency)
        if let Ok(existing_device) = Self::find_nvme_device_by_nqn(nqn).await {
            debug!(device = %existing_device, "[HTTP_API] NVMe device already exists (idempotent)");
            let response = json!({
                "device_path": existing_device,
                "nvme_device": Self::extract_nvme_controller(&existing_device),
                "nqn": nqn
            });
            return Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK));
        }

        // 2. Create NVMe-oF target on SPDK
        match Self::create_nvmeof_target_local(&node_agent, bdev_name, nqn, target_ip, target_port).await {
            Ok(_) => debug!("[HTTP_API] NVMe-oF target created"),
            Err(e) if e.to_string().contains("already exists") => {
                debug!("[HTTP_API] NVMe-oF target already exists");
            }
            Err(e) => {
                let error_response = json!({
                    "success": false,
                    "error": format!("Failed to create NVMe-oF target: {}", e)
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR));
            }
        }

        // 3. Execute kernel nvme connect
        if let Err(e) = Self::kernel_nvme_connect(nqn, target_ip, target_port).await {
            let error_response = json!({
                "success": false,
                "error": format!("Failed to connect NVMe device: {}", e)
            });
            return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR));
        }

        // 4. Wait for device to appear and discover it
        let mut device_path = String::new();
        for attempt in 1..=30 {
            if let Ok(path) = Self::find_nvme_device_by_nqn(nqn).await {
                device_path = path;
                break;
            }
            if attempt % 10 == 0 {
                debug!(attempt, "[HTTP_API] Waiting for NVMe device");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        if device_path.is_empty() {
            let error_response = json!({
                "success": false,
                "error": "NVMe device did not appear after 3 seconds"
            });
            return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR));
        }

        let nvme_device = Self::extract_nvme_controller(&device_path);
        debug!(device_path, "[HTTP_API] NVMe-oF block device created");

        let response = json!({
            "device_path": device_path,
            "nvme_device": nvme_device,
            "nqn": nqn
        });
        Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
    }

    /// Handle POST /api/blockdev/delete_nvmeof - Delete NVMe-oF block device
    async fn handle_blockdev_delete_nvmeof(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!("[HTTP_API] Handling NVMe-oF block device deletion");

        let nqn = match request["nqn"].as_str() {
            Some(n) => n,
            None => {
                let error_response = json!({
                    "success": false,
                    "error": "Missing nqn"
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::BAD_REQUEST));
            }
        };

        // 1. Execute nvme disconnect
        if let Err(e) = Self::kernel_nvme_disconnect(nqn).await {
            warn!(error = %e, "[HTTP_API] Failed to disconnect NVMe (may not exist)");
            // Don't fail - continue to cleanup target
        }

        // 2. Remove SPDK target
        let delete_params = json!({
            "method": "nvmf_delete_subsystem",
            "params": {
                "nqn": nqn
            }
        });

        match node_agent.disk_service.call_spdk_rpc(&delete_params).await {
            Ok(_) => debug!("[HTTP_API] NVMe-oF subsystem deleted"),
            Err(e) => warn!(error = %e, "[HTTP_API] Failed to delete subsystem (may not exist)"),
        }

        let response = json!({
            "success": true,
            "message": "NVMe-oF device deleted"
        });
        Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
    }

    /// Helper: Create NVMe-oF target on local SPDK
    async fn create_nvmeof_target_local(
        node_agent: &Arc<NodeAgent>,
        bdev_name: &str,
        nqn: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Step 0: Check if subsystem already exists (idempotency + safety)
        // This handles the case where NodeUnstageVolume wasn't called (e.g., pod completed vs deleted)
        let check_params = json!({
            "method": "nvmf_get_subsystems",
        });

        if let Ok(result) = node_agent.disk_service.call_spdk_rpc(&check_params).await {
            if let Some(subsystems) = result["result"].as_array() {
                for subsystem in subsystems {
                    if let Some(existing_nqn) = subsystem["nqn"].as_str() {
                        if existing_nqn == nqn {
                            // SAFETY CHECK: Only delete if no active namespaces
                            let namespace_count = subsystem["namespaces"]
                                .as_array()
                                .map(|ns| ns.len())
                                .unwrap_or(0);

                            if namespace_count == 0 {
                                // Stale subsystem with no active connections - safe to cleanup
                                debug!(nqn, "[NVMEOF] Subsystem exists with 0 namespaces (stale), cleaning up");
                                let delete_params = json!({
                                    "method": "nvmf_delete_subsystem",
                                    "params": {
                                        "nqn": nqn
                                    }
                                });
                                // Best effort delete - ignore errors
                                let _ = node_agent.disk_service.call_spdk_rpc(&delete_params).await;
                            } else {
                                // Active subsystem with connections - DO NOT DELETE
                                // This handles RWX/ROX or concurrent pod scenarios
                                debug!(namespace_count, nqn, "[NVMEOF] Subsystem exists with active namespaces, reusing");
                                // Skip creation steps - subsystem is already properly configured
                                return Ok(());
                            }
                            break;
                        }
                    }
                }
            }
        }

        // Step 1: Create subsystem
        let subsystem_params = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true,
                "serial_number": format!("SPDK{}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs()),
                "model_number": "SPDK CSI Volume"
            }
        });
        node_agent.disk_service.call_spdk_rpc(&subsystem_params).await?;

        // Step 2: Add namespace
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
        node_agent.disk_service.call_spdk_rpc(&namespace_params).await?;

        // Step 3: Add listener
        let listener_params = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": "TCP",
                    "traddr": target_ip,
                    "trsvcid": target_port.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });
        node_agent.disk_service.call_spdk_rpc(&listener_params).await?;

        Ok(())
    }

    /// Helper: Execute kernel nvme connect
    async fn kernel_nvme_connect(
        nqn: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let output = tokio::process::Command::new("nvme")
            .args(&[
                "connect",
                "-t", "tcp",
                "-a", target_ip,
                "-s", &target_port.to_string(),
                "-n", nqn,
            ])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "already connected" errors
            if !stderr.contains("already connected") {
                return Err(format!("nvme connect failed: {}", stderr).into());
            }
        }

        Ok(())
    }

    /// Helper: Execute kernel nvme disconnect
    async fn kernel_nvme_disconnect(nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let output = tokio::process::Command::new("nvme")
            .args(&["disconnect", "-n", nqn])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("nvme disconnect failed: {}", stderr).into());
        }

        Ok(())
    }

    /// Helper: Find NVMe device by NQN via sysfs
    async fn find_nvme_device_by_nqn(nqn: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let nvme_path = std::path::Path::new("/sys/class/nvme");
        if !nvme_path.exists() {
            return Err("NVMe sysfs path does not exist".into());
        }

        for entry in std::fs::read_dir(nvme_path)? {
            let entry = entry?;
            let subsysnqn_path = entry.path().join("subsysnqn");

            if let Ok(subsys_nqn) = std::fs::read_to_string(&subsysnqn_path) {
                if subsys_nqn.trim() == nqn {
                    let ctrl_name = entry.file_name().to_string_lossy().to_string();
                    // Find namespace (usually nvme0n1)
                    let device_path = format!("/dev/{}n1", ctrl_name);
                    if std::path::Path::new(&device_path).exists() {
                        return Ok(device_path);
                    }
                }
            }
        }

        Err(format!("No NVMe device found for NQN: {}", nqn).into())
    }

    /// Helper: Extract NVMe controller name from device path
    fn extract_nvme_controller(device_path: &str) -> String {
        // Extract nvme0 from /dev/nvme0n1
        if let Some(name) = device_path.strip_prefix("/dev/") {
            if let Some(ctrl) = name.strip_suffix("n1") {
                return ctrl.to_string();
            }
        }
        "unknown".to_string()
    }

    /// Handle POST /api/volumes/get_info - Get volume information
    async fn handle_get_volume_info(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let volume_id = match request["volume_id"].as_str() {
            Some(id) => id,
            None => {
                let error_response = json!({
                    "success": false,
                    "error": "Missing volume_id in request"
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::BAD_REQUEST));
            }
        };

        debug!(volume_id, "[HTTP_API] Getting info for volume");

        // Query all lvstores to find the volume
        let lvstores_response = match node_agent.disk_service.call_spdk_rpc(&json!({
            "method": "bdev_lvol_get_lvstores"
        })).await {
            Ok(resp) => resp,
            Err(e) => {
                let error_response = json!({
                    "success": false,
                    "error": format!("Failed to query lvstores: {}", e)
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR));
            }
        };

        // Look for the volume in all lvstores using bdev_lvol_get_lvols
        if let Some(lvstores) = lvstores_response["result"].as_array() {
            for lvstore in lvstores {
                let lvs_name = lvstore["name"].as_str().unwrap_or("");
                
                // Get all lvols in this lvstore
                let lvols_response = match node_agent.disk_service.call_spdk_rpc(&json!({
                    "method": "bdev_lvol_get_lvols",
                    "params": {
                        "lvs_name": lvs_name
                    }
                })).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        warn!(lvs_name, error = %e, "[HTTP_API] Failed to get lvols for LVS");
                        continue;
                    }
                };

                if let Some(lvols) = lvols_response["result"].as_array() {
                    for lvol in lvols {
                        let lvol_name = lvol["name"].as_str().unwrap_or("");
                        
                        // Check if this lvol matches our volume
                        if lvol_name == format!("vol_{}", volume_id) {
                            let size_bytes = lvol["num_blocks"].as_u64().unwrap_or(0) * 
                                           lvol["block_size"].as_u64().unwrap_or(512);
                            let lvol_uuid = lvol["uuid"].as_str().unwrap_or("").to_string();
                            
                            let response = json!({
                                "success": true,
                                "volume_id": volume_id,
                                "lvol_uuid": lvol_uuid,
                                "lvs_name": lvs_name,
                                "size_bytes": size_bytes
                            });
                            
                            debug!(volume_id, lvol_name, lvol_uuid, "[HTTP_API] Found volume");
                            return Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK));
                        }
                    }
                }
            }
        }

        // Volume not found
        let error_response = json!({
            "success": false,
            "error": format!("Volume {} not found", volume_id)
        });
        warn!(volume_id, "[HTTP_API] Volume not found");
        Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::NOT_FOUND))
    }

    /// Handle POST /api/volumes/force_unstage - Force unstage volume (defensive cleanup)
    /// This is called by DeleteVolume when NodeUnstageVolume wasn't called by kubelet
    async fn handle_force_unstage(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let volume_id = match request["volume_id"].as_str() {
            Some(id) => id,
            None => {
                let error_response = json!({
                    "success": false,
                    "error": "Missing volume_id in request"
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::BAD_REQUEST));
            }
        };

        let ublk_id = request["ublk_id"].as_u64().unwrap_or(0) as u32;
        let force = request["force"].as_bool().unwrap_or(false);

        debug!(volume_id, ublk_id, force, "[HTTP_API] Force unstage request");

        let mut was_staged = false;
        let mut operations_performed = Vec::new();

        // Step 1: Find and unmount all staging paths for this volume
        let staging_base = "/var/lib/kubelet/plugins/kubernetes.io/csi/flint.csi.storage.io";
        
        if let Ok(entries) = std::fs::read_dir(staging_base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let globalmount = path.join("globalmount");
                    
                    if globalmount.exists() {
                        // Check if this path is mounted
                        let is_mounted = std::process::Command::new("mountpoint")
                            .arg("-q")
                            .arg(&globalmount)
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false);
                        
                        if is_mounted {
                            debug!(path = %globalmount.display(), "[FORCE_UNSTAGE] Found mounted staging path");
                            was_staged = true;

                            // Unmount with retry
                            debug!("[FORCE_UNSTAGE] Attempting to unmount");
                            let mut unmounted = false;

                            for attempt in 1..=3 {
                                let result = std::process::Command::new("umount")
                                    .arg(&globalmount)
                                    .status();

                                if result.map(|s| s.success()).unwrap_or(false) {
                                    debug!(attempt, "[FORCE_UNSTAGE] Unmounted");
                                    unmounted = true;
                                    operations_performed.push(format!("Unmounted {}", globalmount.display()));
                                    break;
                                }

                                if attempt < 3 {
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                }
                            }

                            // If normal unmount failed, try lazy unmount
                            if !unmounted {
                                warn!("[FORCE_UNSTAGE] Normal unmount failed, trying lazy unmount");
                                let result = std::process::Command::new("umount")
                                    .arg("-l")
                                    .arg(&globalmount)
                                    .status();

                                if result.map(|s| s.success()).unwrap_or(false) {
                                    debug!("[FORCE_UNSTAGE] Lazy unmount succeeded");
                                    operations_performed.push(format!("Lazy unmounted {}", globalmount.display()));
                                    unmounted = true;
                                }
                            }
                            
                            if !unmounted && !force {
                                let error_response = json!({
                                    "success": false,
                                    "error": format!("Failed to unmount {}", globalmount.display()),
                                    "was_staged": was_staged,
                                    "operations": operations_performed
                                });
                                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR));
                            }
                        }
                    }
                }
            }
        }

        // Step 2: Delete ublk device if it exists
        let device_path = format!("/dev/ublkb{}", ublk_id);
        if std::path::Path::new(&device_path).exists() {
            debug!(device_path, "[FORCE_UNSTAGE] Found ublk device");
            was_staged = true;

            // Stop the ublk device via SPDK
            let result = node_agent.disk_service.call_spdk_rpc(&json!({
                "method": "ublk_stop_disk",
                "params": {
                    "ublk_id": ublk_id
                }
            })).await;

            match result {
                Ok(_) => {
                    debug!("[FORCE_UNSTAGE] ublk device stopped");
                    operations_performed.push(format!("Stopped ublk device {}", ublk_id));
                }
                Err(e) => {
                    warn!(error = %e, "[FORCE_UNSTAGE] Failed to stop ublk device");
                    if !force {
                        let error_response = json!({
                            "success": false,
                            "error": format!("Failed to stop ublk device: {}", e),
                            "was_staged": was_staged,
                            "operations": operations_performed
                        });
                        return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR));
                    }
                }
            }
        }

        // Step 3: Disconnect from NVMe-oF if this was a remote volume
        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
        
        // Try to disconnect (best effort - may not be connected)
        let result = node_agent.disk_service.call_spdk_rpc(&json!({
            "method": "bdev_nvme_detach_controller",
            "params": {
                "name": nqn
            }
        })).await;
        
        match result {
            Ok(_) => {
                debug!(nqn, "[FORCE_UNSTAGE] Disconnected from NVMe-oF");
                operations_performed.push("Disconnected NVMe-oF".to_string());
            }
            Err(_) => {
                // Ignore - volume may not be remote
                debug!("[FORCE_UNSTAGE] No NVMe-oF connection to disconnect (volume may be local)");
            }
        }

        // Return success response
        let response = json!({
            "success": true,
            "was_staged": was_staged,
            "operations": operations_performed,
            "message": if was_staged {
                "Volume was staged and has been unstaged"
            } else {
                "Volume was not staged - no action needed"
            }
        });

        debug!(volume_id, was_staged, "[FORCE_UNSTAGE] Completed");
        Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
    }

    /// Handle POST /api/volumes/check_health - Check volume health status
    /// Returns detailed health information about a volume's backing storage
    async fn handle_check_volume_health(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let lvol_uuid = match request["lvol_uuid"].as_str() {
            Some(uuid) => uuid,
            None => {
                let error_response = json!({
                    "success": false,
                    "error": "Missing lvol_uuid in request"
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::BAD_REQUEST));
            }
        };

        debug!(lvol_uuid, "[HTTP_API] Checking health for lvol");

        match node_agent.disk_service.get_lvol_health(lvol_uuid).await {
            Ok(health_status) => {
                let response = json!({
                    "success": true,
                    "exists": health_status.exists,
                    "healthy": health_status.healthy,
                    "message": health_status.message,
                    "lvs_healthy": health_status.lvs_healthy,
                    "disk_healthy": health_status.disk_healthy
                });

                debug!(exists = health_status.exists, healthy = health_status.healthy, "[HTTP_API] Health check result");

                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                // SPDK query failed entirely - node agent unreachable or SPDK down
                error!(error = %e, "[HTTP_API] Health check failed");
                let error_response = json!({
                    "success": false,
                    "exists": false,
                    "healthy": false,
                    "message": format!("Health check failed: {}", e),
                    "error": e.to_string()
                });
                Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    /// Handle POST /api/volumes/check_exists - Lightweight check if lvol exists
    /// Used for graceful deletion when backing storage may be missing
    async fn handle_check_volume_exists(
        request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let lvol_uuid = match request["lvol_uuid"].as_str() {
            Some(uuid) => uuid,
            None => {
                let error_response = json!({
                    "success": false,
                    "error": "Missing lvol_uuid in request"
                });
                return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::BAD_REQUEST));
            }
        };

        debug!(lvol_uuid, "[HTTP_API] Checking existence for lvol");

        match node_agent.disk_service.check_lvol_exists(lvol_uuid).await {
            Ok(exists) => {
                let response = json!({
                    "success": true,
                    "exists": exists,
                    "lvol_uuid": lvol_uuid
                });

                debug!(lvol_uuid, exists, "[HTTP_API] Lvol existence check");

                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                // SPDK query failed - this is different from "lvol doesn't exist"
                // Return exists: false but also indicate the error
                warn!(error = %e, "[HTTP_API] Existence check failed (treating as not exists)");
                let response = json!({
                    "success": true,  // Operation succeeded (we got an answer)
                    "exists": false,  // Treat unreachable as "storage gone"
                    "lvol_uuid": lvol_uuid,
                    "warning": format!("Could not verify - assuming storage unavailable: {}", e)
                });
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
        }
    }

    // ==================== REPLICA RECONCILIATION ====================
    // These functions handle node recovery by setting up NVMe-oF targets
    // for local replicas so remote RAID members can reconnect.

    /// Reconcile replica targets on node startup
    /// Queries PVs with labels matching this node's UID and sets up NVMe-oF targets
    /// for any local replicas, enabling RAID bdevs on other nodes to reconnect.
    async fn reconcile_replica_targets(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let node_uid = self.node_uid.read().await.clone();
        debug!(node_name = %self.node_name, node_uid, "[RECONCILE] Starting replica target reconciliation");

        // Query PVs with label selector: flint.csi.storage.io/replica-{node_uid}=true
        let pvs: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
        let label_selector = format!("flint.csi.storage.io/replica-{}=true", node_uid);
        let lp = ListParams::default().labels(&label_selector);

        let pv_list = match pvs.list(&lp).await {
            Ok(list) => list,
            Err(e) => {
                error!(error = %e, "[RECONCILE] Failed to query PVs");
                return Err(format!("Failed to query PVs: {}", e).into());
            }
        };

        debug!(pv_count = pv_list.items.len(), "[RECONCILE] Found PVs with local replicas");

        let mut success_count = 0;
        let mut skip_count = 0;
        let mut error_count = 0;

        for pv in pv_list.items {
            let volume_id = match &pv.metadata.name {
                Some(name) => name.clone(),
                None => {
                    warn!("[RECONCILE] PV has no name, skipping");
                    skip_count += 1;
                    continue;
                }
            };

            debug!(volume_id, "[RECONCILE] Processing volume");

            // Extract replica info from PV volumeAttributes
            let replicas = match self.get_replicas_from_pv(&pv) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    debug!(volume_id, "[RECONCILE] Volume has no replica info (single replica?), skipping");
                    skip_count += 1;
                    continue;
                }
                Err(e) => {
                    warn!(volume_id, error = %e, "[RECONCILE] Failed to parse replicas");
                    error_count += 1;
                    continue;
                }
            };

            // Find the local replica for this node (match by node_uid or node_name)
            let (replica_index, local_replica) = match replicas.iter().enumerate()
                .find(|(_, r)| r.node_uid == node_uid || r.node_name == self.node_name) {
                Some((i, r)) => (i, r),
                None => {
                    warn!(volume_id, "[RECONCILE] No local replica found (label mismatch?)");
                    skip_count += 1;
                    continue;
                }
            };

            debug!(replica_index, lvol_uuid = %local_replica.lvol_uuid, "[RECONCILE] Found local replica");

            // Verify the lvol exists locally
            match self.verify_local_lvol(&local_replica.lvol_uuid).await {
                Ok(bdev_name) => {
                    debug!(bdev_name, "[RECONCILE] Local lvol verified");
                }
                Err(e) => {
                    warn!(lvol_uuid = %local_replica.lvol_uuid, error = %e, "[RECONCILE] Local lvol not found");
                    skip_count += 1;
                    continue;
                }
            }

            // Setup NVMe-oF target for this replica (idempotent)
            match self.setup_nvmeof_target_for_replica(&volume_id, replica_index, local_replica).await {
                Ok(_) => {
                    debug!(replica_index, "[RECONCILE] NVMe-oF target set up");
                    success_count += 1;
                }
                Err(e) => {
                    error!(error = %e, "[RECONCILE] Failed to setup NVMe-oF target");
                    error_count += 1;
                }
            }
        }

        info!(success_count, skip_count, error_count, "[RECONCILE] Reconciliation complete");

        Ok(())
    }

    /// Extract replica info from PV volumeAttributes
    fn get_replicas_from_pv(&self, pv: &PersistentVolume) -> Result<Option<Vec<ReplicaInfo>>, Box<dyn std::error::Error + Send + Sync>> {
        let spec = pv.spec.as_ref()
            .ok_or("PV has no spec")?;

        let csi = spec.csi.as_ref()
            .ok_or("PV has no CSI spec")?;

        let attrs = csi.volume_attributes.as_ref()
            .ok_or("PV has no volumeAttributes")?;

        // Check replica count first
        let replica_count = attrs.get("flint.csi.storage.io/replica-count")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);

        if replica_count <= 1 {
            return Ok(None);
        }

        // Multi-replica: Read replicas JSON
        let replicas_json = attrs.get("flint.csi.storage.io/replicas")
            .ok_or("Multi-replica volume missing replicas attribute")?;

        let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)?;
        Ok(Some(replicas))
    }

    /// Verify that a local lvol exists and return its bdev name
    async fn verify_local_lvol(&self, lvol_uuid: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let rpc = json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": lvol_uuid
            }
        });

        match self.disk_service.call_spdk_rpc(&rpc).await {
            Ok(response) => {
                if let Some(result) = response.get("result") {
                    if let Some(bdevs) = result.as_array() {
                        if !bdevs.is_empty() {
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

    /// Setup NVMe-oF target for a local replica
    /// Creates subsystem, adds namespace, and adds listener (all idempotent)
    async fn setup_nvmeof_target_for_replica(
        &self,
        volume_id: &str,
        replica_index: usize,
        replica: &ReplicaInfo,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Generate NQN using same format as driver
        let nqn = format!("nqn.2024-11.com.flint:volume:{}_{}", volume_id, replica_index);

        debug!(nqn, "[RECONCILE] Setting up NVMe-oF target");

        // Get node IP for listener
        let node_ip = self.driver.get_node_ip(&self.node_name).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?;

        let target_port = self.driver.nvmeof_target_port;
        let transport = &self.driver.nvmeof_transport;

        // Step 1: Check if subsystem already exists (idempotency)
        let get_subsystem_rpc = json!({
            "method": "nvmf_get_subsystems",
            "params": {
                "nqn": nqn
            }
        });

        let subsystem_exists = match self.disk_service.call_spdk_rpc(&get_subsystem_rpc).await {
            Ok(response) => {
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
            debug!(nqn, "[RECONCILE] Subsystem already exists (idempotent)");
            return Ok(());
        }

        // Step 2: Create subsystem
        let create_subsystem_rpc = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true,
                "serial_number": format!("SPDK{}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs()),
                "model_number": "SPDK CSI Volume"
            }
        });

        match self.disk_service.call_spdk_rpc(&create_subsystem_rpc).await {
            Ok(_) => debug!("[RECONCILE] Subsystem created"),
            Err(e) if e.to_string().contains("already exists") => {
                debug!("[RECONCILE] Subsystem already exists (race condition)");
            }
            Err(e) => return Err(format!("Failed to create subsystem: {}", e).into()),
        }

        // Step 3: Add namespace with lvol bdev
        let add_namespace_rpc = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "nsid": 1,
                    "bdev_name": replica.lvol_uuid
                }
            }
        });

        match self.disk_service.call_spdk_rpc(&add_namespace_rpc).await {
            Ok(_) => debug!("[RECONCILE] Namespace added"),
            Err(e) if e.to_string().contains("already exists") => {
                debug!("[RECONCILE] Namespace already exists");
            }
            Err(e) => return Err(format!("Failed to add namespace: {}", e).into()),
        }

        // Step 4: Add TCP listener
        let add_listener_rpc = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": transport.to_uppercase(),
                    "traddr": node_ip,
                    "trsvcid": target_port.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });

        match self.disk_service.call_spdk_rpc(&add_listener_rpc).await {
            Ok(_) => debug!(node_ip, target_port, "[RECONCILE] Listener added"),
            Err(e) if e.to_string().contains("already exists") => {
                debug!("[RECONCILE] Listener already exists");
            }
            Err(e) => return Err(format!("Failed to add listener: {}", e).into()),
        }

        debug!(nqn, "[RECONCILE] NVMe-oF target ready");
        Ok(())
    }
}

// Request/Response types for dashboard compatibility
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DiskSetupRequest {
    pub disks: Option<Vec<String>>,           // For /api/disks/setup
    pub pci_addresses: Option<Vec<String>>,   // For /api/disks/initialize  
    pub force_unmount: Option<bool>,
    pub backup_data: Option<bool>,
}

impl DiskSetupRequest {
    pub fn get_disks(&self) -> Vec<String> {
        self.disks.clone()
            .or_else(|| self.pci_addresses.clone())
            .unwrap_or_default()
    }
}

// Make MinimalNodeAgent cloneable for use in warp handlers
impl Clone for NodeAgent {
    fn clone(&self) -> Self {
        Self {
            node_name: self.node_name.clone(),
            node_uid: self.node_uid.clone(),
            spdk_socket_path: self.spdk_socket_path.clone(),
            disk_service: self.disk_service.clone(),
            driver: self.driver.clone(),
        }
    }
}

// Request/Response types for HTTP API
#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeBlobstoreRequest {
    pub pci_address: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateLvolRequest {
    pub lvs_name: String,
    pub volume_id: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub thin_provision: bool,
}

#[derive(Debug, Serialize, Deserialize)] 
pub struct DeleteLvolRequest {
    pub lvol_uuid: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResizeLvolRequest {
    pub lvol_uuid: String,
    pub new_size_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateMemoryDiskRequest {
    pub name: String,
    pub size_mb: u64,
    #[serde(default)]
    pub block_size: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteMemoryDiskRequest {
    pub name: String,
}
