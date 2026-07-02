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
    /// §10-14 orphan-sweep strike counts, persisted across sweep cycles
    /// (shared by clone so the monitor task and HTTP handlers see one).
    orphan_strikes: Arc<tokio::sync::Mutex<std::collections::HashMap<String, u32>>>,
    /// Data-path-lost strike counts (attached volume, raid missing),
    /// keyed by PV name — see `detect_lost_data_paths`.
    data_path_strikes: Arc<tokio::sync::Mutex<std::collections::HashMap<String, u32>>>,
    /// Dead-controller strike counts (reconnect-looping flint controllers),
    /// keyed by controller name — see `reap_dead_controllers`.
    controller_reap_strikes: Arc<tokio::sync::Mutex<std::collections::HashMap<String, u32>>>,
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
            orphan_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            data_path_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            controller_reap_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
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
            orphan_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            data_path_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            controller_reap_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
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
            // Storage-baseline reconcile (phase-6 yield): a lone spdk-tgt
            // container restart (liveness kill, OOM, crash) comes back as
            // an empty target — no controllers, no lvstore — because disk
            // attach previously ran only at DRIVER startup, and the driver
            // container didn't restart. Detect the collapse (disks were
            // seen, now none) and re-run full discovery with
            // auto-recovery, which re-attaches initialized disks and
            // reloads the lvstore; the 60s reconcile then re-exports.
            let mut last_disk_count: Option<usize> = None;
            loop {
                discovery_interval.tick().await;
                // Use FAST discovery (no auto-recovery) to reduce SPDK RPC spam
                // Auto-recovery is expensive (400+ RPC calls) and is triggered
                // below only when the baseline collapses.
                match disk_service.discover_local_disks_fast().await {
                    Ok(disks) => {
                        if disks.is_empty() && last_disk_count.map(|n| n > 0).unwrap_or(false) {
                            warn!(
                                "[DISK_DISCOVERY] Disk baseline collapsed (had {}, now 0) — \
                                 spdk-tgt likely restarted; re-running discovery with auto-recovery",
                                last_disk_count.unwrap_or(0)
                            );
                            match disk_service.discover_local_disks().await {
                                Ok(recovered) => {
                                    info!(
                                        disk_count = recovered.len(),
                                        "[DISK_DISCOVERY] Baseline recovery completed"
                                    );
                                    last_disk_count = Some(recovered.len());
                                }
                                Err(e) => {
                                    error!(error = %e, "[DISK_DISCOVERY] Baseline recovery failed");
                                }
                            }
                            continue;
                        }
                        last_disk_count = Some(disks.len());
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

        // Periodic reconcile + raid health monitor (phase 0 fix).
        // Reconcile previously ran only once at startup; an export lost later
        // (or a PV that appears after startup) was never repaired. The health
        // monitor surfaces degraded raids to the PV (annotation + event) —
        // previously a dead raid leg was invisible to the control plane.
        let monitor_agent = Arc::new(self.clone());
        let monitor_task = tokio::spawn(async move {
            let mut monitor_interval = interval(Duration::from_secs(60));
            // First tick fires immediately; startup already reconciled.
            monitor_interval.tick().await;
            loop {
                monitor_interval.tick().await;
                if let Err(e) = monitor_agent.reconcile_replica_targets().await {
                    warn!(error = %e, "[MONITOR] Replica reconciliation failed (non-fatal)");
                }
                if let Err(e) = monitor_agent.monitor_raid_health().await {
                    warn!(error = %e, "[MONITOR] Raid health check failed (non-fatal)");
                }
                if let Err(e) = monitor_agent.orphan_sweep().await {
                    warn!(error = %e, "[MONITOR] Orphan sweep failed (non-fatal)");
                }
                if let Err(e) = monitor_agent.detect_lost_data_paths().await {
                    warn!(error = %e, "[MONITOR] Data-path detection failed (non-fatal)");
                }
                if let Err(e) = monitor_agent.reap_dead_controllers().await {
                    warn!(error = %e, "[MONITOR] Dead-controller reap failed (non-fatal)");
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

        // Start the HTTP server, discovery loop, and reconcile/health monitor
        tokio::select! {
            _ = warp::serve(routes).run(([0, 0, 0, 0], node_agent_port)) => {
                info!("[NODE_AGENT] HTTP server stopped");
            }
            _ = discovery_task => {
                info!("[NODE_AGENT] Discovery task stopped");
            }
            _ = monitor_task => {
                info!("[NODE_AGENT] Monitor task stopped");
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
        if let Err(e) = Self::kernel_nvme_connect(nqn, target_ip, target_port, &node_agent.node_name).await {
            let error_response = json!({
                "success": false,
                "error": format!("Failed to connect NVMe device: {}", e)
            });
            return Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR));
        }

        // 4. Wait for device to appear and discover it.
        //
        // Phase 0 fix: the previous 3-second wait was too tight when the
        // kernel controller pre-existed (e.g. left over from an earlier
        // stage) — a namespace added to a live subsystem only surfaces after
        // an async AER-triggered rescan, which can exceed 3s. Wait up to 20s
        // and nudge the controller with an explicit ns-rescan every ~2s.
        let mut device_path = String::new();
        for attempt in 1..=100u32 {
            if let Ok(path) = Self::find_nvme_device_by_nqn(nqn).await {
                device_path = path;
                break;
            }
            if attempt % 10 == 0 {
                debug!(attempt, "[HTTP_API] Waiting for NVMe device; triggering ns rescan");
                Self::kernel_nvme_ns_rescan(nqn).await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }

        if device_path.is_empty() {
            let error_response = json!({
                "success": false,
                "error": "NVMe device did not appear after 20 seconds"
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
        // Convergent export (phase 0 fix): completes whatever subset of
        // {subsystem, namespace, listener} is missing instead of blindly
        // creating (duplicates fail -32602) or blindly reusing (a subsystem
        // can exist with namespaces but no listener after a partial attempt).
        // This also covers the NodeUnstageVolume-never-ran case the old
        // stale-subsystem cleanup handled: a leftover export for the same
        // bdev is simply converged upon and reused, and a namespace pointing
        // at a stale bdev is replaced.
        // Fencing: this export is consumed by the local kernel initiator.
        let allowed = vec![crate::nvmeof_export::flint_host_nqn(&node_agent.node_name)];
        // Kernel-facing namespace: pin a deterministic identity so an
        // in-place rebuild after spdk-tgt restart presents the SAME
        // namespace and the initiator reattaches (phase-6 layer 2).
        let volume_id = nqn.rsplit(':').next().unwrap_or(nqn);
        let (ns_uuid, ns_nguid) = crate::nvmeof_export::stable_ns_identity(volume_id);
        let spec = crate::nvmeof_export::ExportSpec {
            nqn,
            bdev_name,
            bdev_aliases: &[],
            trtype: "TCP",
            traddr: target_ip,
            trsvcid: target_port,
            allowed_hosts: crate::nvmeof_export::fencing_enabled().then_some(allowed.as_slice()),
            ns_identity: Some((&ns_uuid, &ns_nguid)),
        };
        crate::nvmeof_export::ensure_export(&node_agent.disk_service, &spec).await?;

        Ok(())
    }

    /// Helper: Execute kernel nvme connect
    async fn kernel_nvme_connect(
        nqn: &str,
        target_ip: &str,
        target_port: u16,
        node_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Stable per-node host NQN so the target's host fencing can admit
        // exactly this node (doc §3); matches the SPDK initiator identity.
        let hostnqn = crate::nvmeof_export::flint_host_nqn(node_name);
        let output = tokio::process::Command::new("nvme")
            .args(&[
                "connect",
                "-t", "tcp",
                "-a", target_ip,
                "-s", &target_port.to_string(),
                "-n", nqn,
                "-q", &hostnqn,
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

    /// Helper: Trigger a namespace rescan on the kernel controller connected
    /// to `nqn` (no-op if none). A namespace added to an already-connected
    /// subsystem only appears after a rescan; AER delivery can lag.
    async fn kernel_nvme_ns_rescan(nqn: &str) {
        let nvme_path = std::path::Path::new("/sys/class/nvme");
        let Ok(entries) = std::fs::read_dir(nvme_path) else { return };
        for entry in entries.flatten() {
            let subsysnqn_path = entry.path().join("subsysnqn");
            if let Ok(subsys_nqn) = std::fs::read_to_string(&subsysnqn_path) {
                if subsys_nqn.trim() == nqn {
                    let rescan = entry.path().join("rescan_controller");
                    let _ = std::fs::write(&rescan, "1");
                }
            }
        }
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

                            // Bounded unmount (mount_util): a plain umount
                            // on a mount whose backing device is gone blocks
                            // in D-state and would wedge the agent (same
                            // hazard as the NodeUnstage hang found by the
                            // 1.2.0 release gate, 2026-06-12).
                            debug!("[FORCE_UNSTAGE] Attempting bounded unmount");
                            let globalmount_str = globalmount.display().to_string();
                            let mut unmounted =
                                crate::mount_util::bounded_umount(&globalmount_str, false, 10)
                                    .await;
                            if unmounted {
                                debug!("[FORCE_UNSTAGE] Unmounted");
                                operations_performed.push(format!("Unmounted {}", globalmount.display()));
                            }

                            // If normal unmount failed, try lazy unmount
                            if !unmounted {
                                warn!("[FORCE_UNSTAGE] Normal unmount failed, trying lazy unmount");
                                let result =
                                    crate::mount_util::bounded_umount(&globalmount_str, true, 10)
                                        .await;

                                if result {
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

        // Fast path: PVs labeled flint.csi.storage.io/replica-{node_uid}=true
        let pvs: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
        let label_key = format!("flint.csi.storage.io/replica-{}", node_uid);
        let lp = ListParams::default().labels(&format!("{}=true", label_key));

        let mut pv_items = match pvs.list(&lp).await {
            Ok(list) => list.items,
            Err(e) => {
                error!(error = %e, "[RECONCILE] Failed to query PVs");
                return Err(format!("Failed to query PVs: {}", e).into());
            }
        };

        // Fallback (phase 0 fix, repro bug 4): nothing ever applies that
        // label — CreateVolume cannot, because the external-provisioner only
        // creates the PV object after CreateVolume returns. Without this
        // scan the reconcile matched zero PVs and post-reboot replica
        // re-export never ran. Scan this driver's PVs by volumeAttributes
        // and label the matches so the fast path works on later cycles.
        let labeled_names: std::collections::HashSet<String> = pv_items
            .iter()
            .filter_map(|pv| pv.metadata.name.clone())
            .collect();
        match pvs.list(&ListParams::default()).await {
            Ok(all) => {
                for pv in all.items {
                    let Some(name) = pv.metadata.name.clone() else { continue };
                    if labeled_names.contains(&name) {
                        continue;
                    }
                    let is_local_replica = self
                        .get_replicas_from_pv(&pv)
                        .ok()
                        .flatten()
                        .map(|replicas| {
                            replicas
                                .iter()
                                .any(|r| r.node_uid == node_uid || r.node_name == self.node_name)
                        })
                        .unwrap_or(false);
                    if !is_local_replica {
                        continue;
                    }
                    // Best-effort label so future cycles take the fast path.
                    let patch = serde_json::json!({
                        "metadata": { "labels": { &label_key: "true" } }
                    });
                    use kube::api::{Patch, PatchParams};
                    if let Err(e) = pvs.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await {
                        debug!(volume_id = %name, error = %e, "[RECONCILE] Could not label PV (continuing)");
                    }
                    pv_items.push(pv);
                }
            }
            Err(e) => {
                warn!(error = %e, "[RECONCILE] Fallback PV scan failed; only labeled PVs reconciled");
            }
        }

        debug!(pv_count = pv_items.len(), "[RECONCILE] Found PVs with local replicas");

        let mut success_count = 0;
        let mut skip_count = 0;
        let mut error_count = 0;

        for pv in pv_items {
            let volume_id = match &pv.metadata.name {
                Some(name) => name.clone(),
                None => {
                    warn!("[RECONCILE] PV has no name, skipping");
                    skip_count += 1;
                    continue;
                }
            };

            debug!(volume_id, "[RECONCILE] Processing volume");

            // RWX PVs are NFS-mounted; their replica exports belong to the
            // synthetic backing PV's entry (same replicas, handle-named).
            // Reconciling both creates a second, alias-named export that
            // claims the lvol and starves the real one (-32602 forever).
            if crate::replica_sync::is_rwx_pv(&pv) {
                debug!(volume_id, "[RECONCILE] RWX PV — exports owned by its nfs-server backing PV, skipping");
                skip_count += 1;
                continue;
            }

            // SPDK object names (export NQNs, raid names) derive from the
            // volumeHandle, which differs from the PV name for synthetic
            // NFS backing PVs (flint-nfs-pv-X / nfs-server-X). K8s lookups
            // (VolumeAttachments) stay keyed on the PV name.
            let spdk_id = pv
                .spec
                .as_ref()
                .and_then(|s| s.csi.as_ref())
                .map(|c| c.volume_handle.clone())
                .unwrap_or_else(|| volume_id.clone());

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

            // Sync-state awareness (incremental-rebuild phase 4): a stale or
            // standby replica's export belongs to the catch-up orchestrator
            // (fenced to the copy source) — re-exporting it here would
            // trample that fence every cycle. And after a catch-up revert
            // the live head has a new uuid (`active_lvol_uuid`); the
            // identity uuid addresses nothing.
            let sync_rec = pv
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(crate::replica_sync::SYNC_STATE_ANNOTATION))
                .and_then(|s| crate::replica_sync::VolumeSyncRecord::from_annotation(s).ok())
                .and_then(|r| r.get(&local_replica.lvol_uuid).cloned());
            if let Some(rec) = &sync_rec {
                if rec.sync_state != crate::replica_sync::SyncState::InSync {
                    debug!(
                        volume_id,
                        state = rec.sync_state.as_str(),
                        "[RECONCILE] Replica not in_sync — its export is owned by the catch-up orchestrator, skipping"
                    );
                    skip_count += 1;
                    continue;
                }
            }
            let live_uuid = sync_rec
                .as_ref()
                .map(|r| r.live_lvol_uuid().to_string())
                .unwrap_or_else(|| local_replica.lvol_uuid.clone());

            // Verify the lvol exists locally
            match self.verify_local_lvol(&live_uuid).await {
                Ok(bdev_name) => {
                    debug!(bdev_name, "[RECONCILE] Local lvol verified");
                }
                Err(e) => {
                    warn!(lvol_uuid = %live_uuid, error = %e, "[RECONCILE] Local lvol not found");
                    skip_count += 1;
                    continue;
                }
            }

            // §3 hygiene (phase 0 fix): after a reboot the replica lvol
            // re-registers carrying a raid superblock and examine
            // auto-assembles a phantom raid that claims it exclusive_write —
            // the export below would fail -32602 forever. A raid for this
            // volume on a node that is NOT the volume's current consumer is
            // by definition such a phantom; delete it (clearing superblocks
            // when SPDK supports it) before exporting.
            let attached_node = self.get_attached_node(&volume_id).await;
            if attached_node.as_deref() != Some(self.node_name.as_str()) {
                if let Err(e) = self.delete_phantom_raid_local(&spdk_id).await {
                    error!(volume_id, error = %e, "[RECONCILE] Failed to delete phantom raid");
                    error_count += 1;
                    continue;
                }
            }

            // Setup NVMe-oF target for this replica (idempotent)
            match self.setup_nvmeof_target_for_replica(&spdk_id, replica_index, local_replica, &live_uuid, attached_node.as_deref()).await {
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

    /// Surface the health of node-local raid bdevs to the control plane
    /// (phase 0 fix, repro bug 6). For every `raid_{volume_id}` on this node:
    /// degraded → patch the PV annotation
    /// `flint.csi.storage.io/replica-health` and emit a Warning event;
    /// healthy → clear the annotation. volumeAttributes are immutable, so
    /// the mutable annotation is the channel (same pattern as
    /// `filesystem-initialized`).
    async fn monitor_raid_health(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let payload = json!({
            "method": "bdev_raid_get_bdevs",
            "params": { "category": "all" }
        });
        let response = self.disk_service.call_spdk_rpc(&payload).await?;
        let raids = response
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        for raid in raids {
            let Some(raid_name) = raid.get("name").and_then(|n| n.as_str()) else { continue };
            let Some(volume_id) = raid_name.strip_prefix("raid_") else { continue };
            // RWX volumes stage under the synthetic "nfs-server-<vol>"
            // handle; PV reads/patches must target the user PV.
            let pv_name = crate::replica_sync::record_pv_name(volume_id);

            let state = raid.get("state").and_then(|s| s.as_str()).unwrap_or("unknown");
            let total = raid.get("num_base_bdevs").and_then(|n| n.as_u64()).unwrap_or(0);
            let bases: Vec<Value> = raid
                .get("base_bdevs_list")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default();
            let configured: Vec<&Value> = bases
                .iter()
                .filter(|b| b.get("is_configured").and_then(|c| c.as_bool()).unwrap_or(false))
                .collect();
            let degraded = state != "online" || (configured.len() as u64) < total;

            let pvs: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
            use kube::api::{Patch, PatchParams};

            if degraded {
                let health = json!({
                    "state": state,
                    "configured": configured.len(),
                    "total": total,
                    "observed_by": self.node_name,
                })
                .to_string();
                let patch = json!({
                    "metadata": {
                        "annotations": { "flint.csi.storage.io/replica-health": health }
                    }
                });
                match pvs.patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await {
                    Ok(_) => {
                        warn!(
                            volume_id, state,
                            configured = configured.len(),
                            total,
                            "[MONITOR] RAID degraded — PV annotated"
                        );
                    }
                    Err(e) => warn!(volume_id, error = %e, "[MONITOR] Failed to annotate PV"),
                }
                self.emit_pv_event(
                    volume_id,
                    "Warning",
                    "VolumeDegraded",
                    &format!(
                        "RAID for volume {} is {} with {}/{} base bdevs configured on node {}",
                        volume_id, state, configured.len(), total, self.node_name
                    ),
                )
                .await;
            } else {
                // Clear a previously-set annotation (merge-patch null deletes)
                let patch = json!({
                    "metadata": {
                        "annotations": { "flint.csi.storage.io/replica-health": null }
                    }
                });
                let _ = pvs.patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await;
            }

            // Phase 1 (incremental-rebuild §9-1): an online raid serves
            // writes, so a replica not backed by a configured base is
            // missing acknowledged data — record the in_sync → stale
            // transition on the PV. Non-online raids (phantoms, leftovers)
            // imply nothing about replica data and are left alone. Note
            // this also catches replicas the raid was *created without*
            // (degraded assembly), which num_base_bdevs above cannot see.
            if state == "online" {
                self.record_stale_replicas(volume_id, &raid).await;
            }
        }

        Ok(())
    }

    /// Mark replicas missing from an online raid as stale in the PV sync
    /// record (incremental-rebuild phase 1). Best effort; runs every monitor
    /// tick, so a lost write converges one minute later.
    async fn record_stale_replicas(&self, volume_id: &str, raid: &Value) {
        use crate::replica_sync;

        let now = replica_sync::now_rfc3339();
        let mut newly_stale: Vec<String> = Vec::new();
        let result =
            replica_sync::update_sync_record(&self.driver.kube_client, volume_id, |record| {
                // The closure may run more than once on write conflicts.
                newly_stale.clear();
                let Some(missing) =
                    replica_sync::replicas_missing_from_raid(raid, volume_id, record)
                else {
                    return;
                };
                for uuid in missing {
                    let why = format!(
                        "raid leg failed or missing while raid online on {}",
                        self.node_name
                    );
                    if record.mark_stale(&uuid, &why, &now) {
                        newly_stale.push(uuid);
                    }
                }
            })
            .await;

        match result {
            Ok(_) => {
                for uuid in &newly_stale {
                    warn!(
                        volume_id,
                        lvol_uuid = %uuid,
                        "[MONITOR] Replica marked stale (not backed by a configured base of the online raid)"
                    );
                    replica_sync::emit_pv_event(
                        &self.driver.kube_client,
                        &self.node_name,
                        volume_id,
                        "Warning",
                        "ReplicaStale",
                        &format!(
                            "Replica {} of volume {} is not backed by a configured base of the online raid on {} — it is no longer receiving writes",
                            uuid, volume_id, self.node_name
                        ),
                    )
                    .await;
                }
            }
            Err(e) => {
                warn!(volume_id, error = %e, "[MONITOR] Failed to update replica sync record");
            }
        }
    }

    /// Best-effort Kubernetes Warning/Normal event attached to a PV.
    async fn emit_pv_event(&self, pv_name: &str, event_type: &str, reason: &str, message: &str) {
        crate::replica_sync::emit_pv_event(
            &self.driver.kube_client,
            &self.node_name,
            pv_name,
            event_type,
            reason,
            message,
        )
        .await
    }

    /// §10-14 orphan sweep — reap flint lvols/exports whose PV is gone.
    /// See `crate::orphan_sweep` for the safety model (strict parsers,
    /// PV-absence authority, ordered candidacy, strike confirmation).
    /// Runs on the 60s monitor tick. `FLINT_ORPHAN_SWEEP=disabled` turns
    /// it off; `FLINT_ORPHAN_SWEEP_STRIKES` (default 3) is how many
    /// consecutive condemned cycles precede deletion.
    async fn orphan_sweep(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if std::env::var("FLINT_ORPHAN_SWEEP").is_ok_and(|v| v.eq_ignore_ascii_case("disabled")) {
            return Ok(());
        }
        let threshold: u32 = std::env::var("FLINT_ORPHAN_SWEEP_STRIKES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3)
            .max(1);

        // PV absence is the only condemnation authority, and only a
        // successful full list proves it — any error skips the cycle.
        let pvs_api: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
        let existing_pvs: std::collections::HashSet<String> = pvs_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter_map(|pv| pv.metadata.name)
            .collect();

        let mut lvols = Vec::new();
        let lvstores = self
            .disk_service
            .call_spdk_rpc(&json!({"method": "bdev_lvol_get_lvstores"}))
            .await?;
        for lvs in lvstores["result"].as_array().cloned().unwrap_or_default() {
            let Some(lvs_name) = lvs["name"].as_str() else { continue };
            let resp = self
                .disk_service
                .call_spdk_rpc(&json!({
                    "method": "bdev_lvol_get_lvols",
                    "params": {"lvs_name": lvs_name}
                }))
                .await?;
            for l in resp["result"].as_array().cloned().unwrap_or_default() {
                let Some(name) = l["name"].as_str() else { continue };
                lvols.push(crate::orphan_sweep::LvolEntry {
                    lvs: lvs_name.to_string(),
                    name: name.to_string(),
                    uuid: l["uuid"].as_str().unwrap_or_default().to_string(),
                });
            }
        }

        let subs_resp = self
            .disk_service
            .call_spdk_rpc(&json!({"method": "nvmf_get_subsystems", "params": {}}))
            .await?;
        let mut subsystems = Vec::new();
        for s in subs_resp["result"].as_array().cloned().unwrap_or_default() {
            let Some(nqn) = s["nqn"].as_str() else { continue };
            let ns_bdevs = s["namespaces"]
                .as_array()
                .map(|nss| {
                    nss.iter()
                        .filter_map(|n| n["bdev_name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            subsystems.push(crate::orphan_sweep::SubsystemEntry {
                nqn: nqn.to_string(),
                ns_bdevs,
            });
        }

        let bdevs_resp = self
            .disk_service
            .call_spdk_rpc(&json!({"method": "bdev_get_bdevs", "params": {}}))
            .await?;
        let mut all_bdevs = std::collections::HashSet::new();
        for b in bdevs_resp["result"].as_array().cloned().unwrap_or_default() {
            if let Some(n) = b["name"].as_str() {
                all_bdevs.insert(n.to_string());
            }
            if let Some(u) = b["uuid"].as_str() {
                all_bdevs.insert(u.to_string());
            }
            for a in b["aliases"].as_array().cloned().unwrap_or_default() {
                if let Some(a) = a.as_str() {
                    all_bdevs.insert(a.to_string());
                }
            }
        }

        // Ephemeral lvols can only be verified idle against the ublk
        // frontend list. In ublk mode (the BLOCK_DEVICE_BACKEND default)
        // a failed listing means unverifiable → eph skipped; in nvmf
        // mode a failure just means the ublk target was never created.
        let ublk_mode = std::env::var("BLOCK_DEVICE_BACKEND")
            .unwrap_or("ublk".to_string())
            .eq_ignore_ascii_case("ublk");
        let ublk_bdevs = match self
            .disk_service
            .call_spdk_rpc(&json!({"method": "ublk_get_disks"}))
            .await
        {
            Ok(resp) => resp["result"].as_array().map(|disks| {
                disks
                    .iter()
                    .filter_map(|d| d["bdev_name"].as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            }),
            Err(_) if !ublk_mode => Some(Vec::new()),
            Err(_) => None,
        };

        let input = crate::orphan_sweep::SweepInput {
            lvols,
            subsystems,
            ublk_bdevs,
            all_bdevs,
            existing_pvs,
        };
        let plan = {
            let mut strikes = self.orphan_strikes.lock().await;
            crate::orphan_sweep::plan_sweep(&input, &mut strikes, threshold)
        };
        if plan.eph_skipped_unverifiable > 0 {
            debug!(
                count = plan.eph_skipped_unverifiable,
                "[ORPHAN_SWEEP] ephemeral lvols skipped: ublk frontends unverifiable"
            );
        }
        if plan.delete_subsystem_nqns.is_empty() && plan.delete_lvol_aliases.is_empty() {
            return Ok(());
        }
        info!(
            subsystems = plan.delete_subsystem_nqns.len(),
            lvols = plan.delete_lvol_aliases.len(),
            "[ORPHAN_SWEEP] reaping orphans of absent PVs"
        );

        // Subsystems first: their write-opens block lvol deletion.
        for nqn in &plan.delete_subsystem_nqns {
            match self
                .disk_service
                .call_spdk_rpc(&json!({"method": "nvmf_delete_subsystem", "params": {"nqn": nqn}}))
                .await
            {
                Ok(_) => info!(nqn = %nqn, "[ORPHAN_SWEEP] deleted orphan subsystem"),
                Err(e) => {
                    debug!(nqn = %nqn, error = %e, "[ORPHAN_SWEEP] subsystem delete deferred")
                }
            }
        }

        // Lvols in retry passes: leaf-first order emerges because a
        // snapshot with clones refuses deletion until its clones are
        // gone (the campaign's manual sweep needed the same order).
        // Whatever a cycle can't delete (e.g. a copy pinned by a live
        // restore clone) stays condemned and is retried next cycle —
        // at debug level, so a long-lived pin doesn't spam warnings the
        // way the epoch GC did.
        let mut remaining = plan.delete_lvol_aliases;
        for _pass in 0..5 {
            if remaining.is_empty() {
                break;
            }
            let before = remaining.len();
            let mut next = Vec::new();
            for alias in std::mem::take(&mut remaining) {
                match self
                    .disk_service
                    .call_spdk_rpc(&json!({"method": "bdev_lvol_delete", "params": {"name": alias}}))
                    .await
                {
                    Ok(_) => info!(lvol = %alias, "[ORPHAN_SWEEP] deleted orphan lvol"),
                    Err(e) if e.to_string().contains("No such device") => {}
                    Err(e) => {
                        debug!(lvol = %alias, error = %e, "[ORPHAN_SWEEP] lvol delete deferred");
                        next.push(alias);
                    }
                }
            }
            remaining = next;
            if remaining.len() == before {
                break; // no progress; next cycle retries
            }
        }
        if !remaining.is_empty() {
            info!(
                deferred = remaining.len(),
                "[ORPHAN_SWEEP] orphan lvols deferred to next cycle (likely clone-pinned)"
            );
        }
        Ok(())
    }

    /// In-place data-path repair (phase-6 layer 2): rebuild the raid
    /// (same sync-state-aware assembly NodeStage uses) and re-export the
    /// loopback subsystem with its pinned namespace identity — the kernel
    /// initiator, still retrying inside its reconnect window, reattaches
    /// and queued I/O completes. Zero workload disruption. Refuses ublk
    /// frontends (the device node died with spdk-tgt; only a restage
    /// helps) and tears its raid back down if the attachment vanished
    /// mid-repair (a zombie consumer raid is the §3 hazard).
    /// Whether kubelet currently has `volume_handle` STAGED on this node
    /// (its `vol_data.json` exists under the CSI staging dir). The VA can
    /// linger mid-detach during a consumer handover; kubelet's staging
    /// record is the truth about whether a mount on this node still
    /// expects the data path. Observed live (layer-2 validation): the
    /// previous consumer's agent repaired against a detaching VA and
    /// kubelet's in-flight unstage immediately tore it down — harmless
    /// there, but the repair could steal replica fences for a tick.
    fn is_staged_here(volume_handle: &str) -> bool {
        let staging_base = "/var/lib/kubelet/plugins/kubernetes.io/csi/flint.csi.storage.io";
        let Ok(entries) = std::fs::read_dir(staging_base) else {
            return false;
        };
        for entry in entries.flatten() {
            let vol_data = entry.path().join("vol_data.json");
            let Ok(content) = std::fs::read_to_string(&vol_data) else {
                continue;
            };
            if serde_json::from_str::<serde_json::Value>(&content)
                .ok()
                .and_then(|v| v["volumeHandle"].as_str().map(|h| h == volume_handle))
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }

    async fn repair_data_path(
        &self,
        pv: &PersistentVolume,
        volume_handle: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let pv_name = pv.metadata.name.as_deref().unwrap_or(volume_handle);
        if pv
            .metadata
            .annotations
            .as_ref()
            .map(|a| a.contains_key("flint.io/ublk-id"))
            .unwrap_or(false)
        {
            return Err("ublk frontend — the device node died with spdk-tgt; restage required".into());
        }
        if !Self::is_staged_here(volume_handle) {
            return Err(
                "volume not staged on this node per kubelet (VA lingering mid-detach?) — \
                 repair refused"
                    .into(),
            );
        }
        let replicas = self
            .get_replicas_from_pv(pv)?
            .ok_or("no replica list on the PV")?;

        // Reassemble exactly as NodeStage would (sync-record-aware
        // admission, fenced replica attaches, §3 hygiene).
        let raid_bdev = self
            .driver
            .create_raid_from_replicas(volume_handle, &replicas)
            .await?;

        // Anti-zombie guard: if the attachment left while we rebuilt
        // (concurrent unstage), tear the raid back down and bail.
        if self.get_attached_node(pv_name).await.as_deref() != Some(self.node_name.as_str()) {
            let _ = self
                .disk_service
                .call_spdk_rpc(&json!({
                    "method": "bdev_raid_delete",
                    "params": { "name": format!("raid_{}", volume_handle) }
                }))
                .await;
            return Err("attachment left this node mid-repair — raid torn back down".into());
        }

        // Re-export the loopback subsystem: same NQN/listener/serial and
        // the PINNED namespace identity, listener-last via the convergent
        // module — the partial-rebuild hazard from the layer-2 experiment
        // (listener over a namespace-less subsystem makes the kernel
        // conclude the namespace was deleted) cannot recur.
        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_handle);
        let target_ip =
            std::env::var("NVMEOF_LOCAL_TARGET_IP").unwrap_or("127.0.0.1".to_string());
        let target_port = std::env::var("NVMEOF_TARGET_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(4420);
        let allowed = vec![crate::nvmeof_export::flint_host_nqn(&self.node_name)];
        let (ns_uuid, ns_nguid) = crate::nvmeof_export::stable_ns_identity(volume_handle);
        let spec = crate::nvmeof_export::ExportSpec {
            nqn: &nqn,
            bdev_name: &raid_bdev,
            bdev_aliases: &[],
            trtype: "TCP",
            traddr: &target_ip,
            trsvcid: target_port,
            allowed_hosts: crate::nvmeof_export::fencing_enabled().then_some(allowed.as_slice()),
            ns_identity: Some((&ns_uuid, &ns_nguid)),
        };
        crate::nvmeof_export::ensure_export(&self.disk_service, &spec).await?;
        Ok(())
    }

    /// Consumer-blindness detection (phase-6 yield, bug 1): a volume
    /// ATTACHED to this node whose raid bdev does not exist here has a
    /// dead data path the health monitor cannot see (its stale predicate
    /// requires an online raid). After 3 consecutive 60s observations
    /// (an in-flight NodeStage legitimately has VA-before-raid for up to
    /// the stage-delta budget), attempt the in-place repair (layer 2)
    /// first; only when it fails, flag the PV with the data-path-lost
    /// annotation + a Warning event. Clear our own flag (+ Normal event)
    /// when the raid reappears or the attachment leaves this node.
    /// Detach flint controllers that reconnect-loop against exports that
    /// now reject them (INVALID HOST flood after a replica node's spdk-tgt
    /// recreation + re-fence — the tier-2 spike's operational finding).
    /// Decision logic and safety model live in [`crate::controller_reap`];
    /// this gathers state and executes the plan.
    async fn reap_dead_controllers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if std::env::var("FLINT_CONTROLLER_REAP").is_ok_and(|v| v.eq_ignore_ascii_case("disabled")) {
            return Ok(());
        }
        let threshold: u32 = std::env::var("FLINT_CONTROLLER_REAP_STRIKES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3)
            .max(1);

        let ctrl_resp = self
            .disk_service
            .call_spdk_rpc(&json!({"method": "bdev_nvme_get_controllers"}))
            .await?;
        let controllers: Vec<crate::controller_reap::ControllerEntry> = ctrl_resp["result"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|c| {
                let name = c["name"].as_str()?.to_string();
                let states = c["ctrlrs"]
                    .as_array()
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(|p| p["state"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(crate::controller_reap::ControllerEntry { name, states })
            })
            .collect();

        let raids_resp = self
            .disk_service
            .call_spdk_rpc(&json!({"method": "bdev_raid_get_bdevs", "params": {"category": "all"}}))
            .await?;
        let raid_base_bdevs: std::collections::HashSet<String> = raids_resp["result"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .flat_map(|r| {
                r["base_bdevs_list"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|b| b["name"].as_str().map(str::to_string))
            .collect();

        let input = crate::controller_reap::ReapInput {
            controllers,
            raid_base_bdevs,
        };
        let plan = {
            let mut strikes = self.controller_reap_strikes.lock().await;
            crate::controller_reap::plan_reap(&input, &mut strikes, threshold)
        };
        if plan.is_empty() {
            return Ok(());
        }

        info!(
            count = plan.len(),
            "[CONTROLLER_REAP] detaching dead reconnect-looping controllers"
        );
        for name in &plan {
            match self
                .disk_service
                .call_spdk_rpc(
                    &json!({"method": "bdev_nvme_detach_controller", "params": {"name": name}}),
                )
                .await
            {
                Ok(_) => info!(controller = %name, "[CONTROLLER_REAP] detached dead controller"),
                Err(e) => {
                    debug!(controller = %name, error = %e, "[CONTROLLER_REAP] detach deferred")
                }
            }
        }
        Ok(())
    }

    async fn detect_lost_data_paths(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::cutover::{data_path_verdict, DataPathAction, DATA_PATH_LOST_ANNOTATION};
        use k8s_openapi::api::storage::v1::VolumeAttachment;

        // Raids present locally (one RPC).
        let raids_resp = self
            .disk_service
            .call_spdk_rpc(&json!({"method": "bdev_raid_get_bdevs", "params": {"category": "all"}}))
            .await?;
        let raids: std::collections::HashSet<String> = raids_resp["result"]
            .as_array()
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| r["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // Volumes attached to THIS node.
        let vas: Api<VolumeAttachment> = Api::all(self.driver.kube_client.clone());
        let attached_here: std::collections::HashSet<String> = vas
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|va| {
                va.spec.node_name == self.node_name
                    && va.status.as_ref().map(|s| s.attached).unwrap_or(false)
            })
            .filter_map(|va| va.spec.source.persistent_volume_name)
            .collect();

        let pvs: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
        let mut strikes = self.data_path_strikes.lock().await;
        let mut still_missing: std::collections::HashSet<String> = Default::default();
        for pv in pvs.list(&ListParams::default()).await?.items {
            let Some(pv_name) = pv.metadata.name.clone() else { continue };
            let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) else { continue };
            if csi.driver != "flint.csi.storage.io" {
                continue;
            }
            // Single-replica volumes have no raid by design.
            if !matches!(crate::replica_sync::replicas_from_pv(&pv), Ok(Some(_))) {
                continue;
            }
            // RWX consumers NFS-mount the volume: they hold an attachment
            // but never a raid, so "attached here + raid missing" is the
            // steady state on every workload node — a permanent false
            // positive that drives endless layer-3 bounces. The synthetic
            // backing PV (handle nfs-server-…) carries the real raid
            // coverage on the NFS server's node.
            if crate::replica_sync::is_rwx_pv(&pv) {
                continue;
            }
            let flagged_by_me = pv
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(DATA_PATH_LOST_ANNOTATION))
                .map(|v| v == &self.node_name)
                .unwrap_or(false);
            let attached = attached_here.contains(&pv_name);
            let raid_present = raids.contains(&format!("raid_{}", csi.volume_handle));

            let strikes_with_this = if attached && !raid_present {
                still_missing.insert(pv_name.clone());
                let n = strikes.entry(pv_name.clone()).or_insert(0);
                *n = n.saturating_add(1);
                *n
            } else {
                0
            };

            // Layer 2 first: once the loss is confirmed (3 strikes), try
            // the in-place repair every tick — including while flagged,
            // so a repair blocked transiently (replica node down) heals
            // later. Flag only when repair fails.
            // FLINT_DATA_PATH_REPAIR=disabled skips straight to flagging
            // (operational escape hatch; also used to exercise layer 3).
            let repair_enabled = !std::env::var("FLINT_DATA_PATH_REPAIR")
                .map(|v| v.eq_ignore_ascii_case("disabled"))
                .unwrap_or(false);
            if repair_enabled && attached && !raid_present && strikes_with_this >= 3 {
                match self.repair_data_path(&pv, &csi.volume_handle).await {
                    Ok(()) => {
                        info!(volume_id = %pv_name, "[DATA_PATH] In-place repair succeeded — raid rebuilt, export restored, kernel reattaches");
                        self.emit_pv_event(
                            &pv_name,
                            "Normal",
                            "VolumeDataPathRepaired",
                            &format!(
                                "Raid rebuilt and loopback export restored in place on {}; the \
                                 kernel initiator reattaches within its reconnect window — no \
                                 workload restart needed",
                                self.node_name
                            ),
                        )
                        .await;
                        strikes.remove(&pv_name);
                        still_missing.remove(&pv_name);
                        if flagged_by_me {
                            use kube::api::{Patch, PatchParams};
                            let patch = serde_json::json!({
                                "metadata": { "annotations": { DATA_PATH_LOST_ANNOTATION: null } }
                            });
                            let _ = pvs
                                .patch(&pv_name, &PatchParams::default(), &Patch::Merge(&patch))
                                .await;
                        }
                        continue;
                    }
                    Err(e) => {
                        warn!(volume_id = %pv_name, error = %e, "[DATA_PATH] In-place repair failed — falling back to flagging");
                    }
                }
            }
            match data_path_verdict(attached, raid_present, flagged_by_me, strikes_with_this, 3) {
                DataPathAction::Flag => {
                    use kube::api::{Patch, PatchParams};
                    let patch = serde_json::json!({
                        "metadata": { "annotations": { DATA_PATH_LOST_ANNOTATION: self.node_name } }
                    });
                    if let Err(e) =
                        pvs.patch(&pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await
                    {
                        warn!(volume_id = %pv_name, error = %e, "[DATA_PATH] Failed to annotate PV");
                        continue;
                    }
                    warn!(volume_id = %pv_name, "[DATA_PATH] Volume attached here but its raid is gone — data path lost");
                    self.emit_pv_event(
                        &pv_name,
                        "Warning",
                        "VolumeDataPathLost",
                        &format!(
                            "Volume is attached to {} but its raid bdev is missing there — the \
                             mounted filesystem is failing I/O. Likely an spdk-tgt restart; \
                             bounce the workload pod to restage (or enable rejoin-bounce).",
                            self.node_name
                        ),
                    )
                    .await;
                }
                DataPathAction::Clear => {
                    use kube::api::{Patch, PatchParams};
                    let patch = serde_json::json!({
                        "metadata": { "annotations": { DATA_PATH_LOST_ANNOTATION: null } }
                    });
                    if let Err(e) =
                        pvs.patch(&pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await
                    {
                        warn!(volume_id = %pv_name, error = %e, "[DATA_PATH] Failed to clear annotation");
                        continue;
                    }
                    info!(volume_id = %pv_name, "[DATA_PATH] Data path restored (or attachment left)");
                    self.emit_pv_event(
                        &pv_name,
                        "Normal",
                        "VolumeDataPathRestored",
                        &format!(
                            "Data-path-lost flag cleared by {}: the raid is back (or the \
                             attachment moved)",
                            self.node_name
                        ),
                    )
                    .await;
                }
                DataPathAction::Hold => {}
            }
        }
        strikes.retain(|k, _| still_missing.contains(k));
        Ok(())
    }

    /// Extract replica info from PV volumeAttributes
    fn get_replicas_from_pv(&self, pv: &PersistentVolume) -> Result<Option<Vec<ReplicaInfo>>, Box<dyn std::error::Error + Send + Sync>> {
        crate::replica_sync::replicas_from_pv(pv)
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
    /// Delete a phantom raid bdev for `volume_id` on this node, if present
    /// (§3 hygiene). Waits for in-flight examine first so the phantom can't
    /// re-create itself mid-delete; clears on-disk superblocks when the
    /// local SPDK supports it (v26.05+).
    async fn delete_phantom_raid_local(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let raid_name = format!("raid_{}", volume_id);
        let list = json!({ "method": "bdev_raid_get_bdevs", "params": { "category": "all" } });
        let response = self.disk_service.call_spdk_rpc(&list).await?;
        let exists = response
            .get("result")
            .and_then(|r| r.as_array())
            .map(|raids| {
                raids
                    .iter()
                    .any(|r| r.get("name").and_then(|n| n.as_str()) == Some(raid_name.as_str()))
            })
            .unwrap_or(false);
        if !exists {
            return Ok(());
        }

        // Settle async examine before deleting.
        let _ = self
            .disk_service
            .call_spdk_rpc(&json!({ "method": "bdev_wait_for_examine" }))
            .await;

        // clear_sb requires SPDK v26.05+
        let version = self
            .disk_service
            .call_spdk_rpc(&json!({ "method": "spdk_get_version" }))
            .await?;
        let major = version["result"]["fields"]["major"].as_i64().unwrap_or(0);
        let minor = version["result"]["fields"]["minor"].as_i64().unwrap_or(0);
        let clear_sb = major > 26 || (major == 26 && minor >= 5);

        let mut params = json!({ "name": raid_name });
        if clear_sb {
            params["clear_sb"] = json!(true);
        }
        self.disk_service
            .call_spdk_rpc(&json!({ "method": "bdev_raid_delete", "params": params }))
            .await?;
        warn!(
            volume_id, clear_sb,
            "[RECONCILE] Deleted phantom raid claiming local replica (§3 hygiene)"
        );
        Ok(())
    }

    /// `bdev_name` is the live head to export — the identity uuid unless a
    /// catch-up revert recorded an `active_lvol_uuid` override (phase 4).
    async fn setup_nvmeof_target_for_replica(
        &self,
        volume_id: &str,
        replica_index: usize,
        replica: &ReplicaInfo,
        bdev_name: &str,
        attached_node: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Generate NQN using same format as driver
        let nqn = format!("nqn.2024-11.com.flint:volume:{}_{}", volume_id, replica_index);

        debug!(nqn, "[RECONCILE] Setting up NVMe-oF target");

        // Get node IP for listener
        let node_ip = self.driver.get_node_ip(&self.node_name).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?;

        let target_port = self.driver.nvmeof_target_port;
        let transport = &self.driver.nvmeof_transport;

        // Convergent export: completes any partial state (subsystem without
        // ns, ns without listener, ...) instead of bailing out or failing on
        // duplicates. The old early-return on "subsystem exists" left empty
        // subsystems unexported forever (phase 0 fix).
        //
        // Fencing: the VolumeAttachment is the authority on which node may
        // consume this volume right now (resolved by the caller). Unattached
        // → default-closed (empty host list); the next NodeStage admits
        // itself.
        let allowed: Option<Vec<String>> = if crate::nvmeof_export::fencing_enabled() {
            let hosts = match attached_node {
                Some(consumer) => vec![crate::nvmeof_export::flint_host_nqn(consumer)],
                None => vec![],
            };
            Some(hosts)
        } else {
            None
        };
        let spec = crate::nvmeof_export::ExportSpec {
            nqn: &nqn,
            bdev_name,
            bdev_aliases: &[&replica.lvol_name],
            trtype: transport,
            traddr: &node_ip,
            trsvcid: target_port,
            allowed_hosts: allowed.as_deref(),
            ns_identity: None,
        };
        crate::nvmeof_export::ensure_export(&self.disk_service, &spec)
            .await
            .map_err(|e| format!("Failed to ensure NVMe-oF export for {}: {}", nqn, e))?;

        debug!(nqn, "[RECONCILE] NVMe-oF target ready");
        Ok(())
    }

    /// Node currently attached to this volume per its VolumeAttachment, if any.
    async fn get_attached_node(&self, volume_id: &str) -> Option<String> {
        use k8s_openapi::api::storage::v1::VolumeAttachment;
        let vas: Api<VolumeAttachment> = Api::all(self.driver.kube_client.clone());
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
            orphan_strikes: self.orphan_strikes.clone(),
            data_path_strikes: self.data_path_strikes.clone(),
            controller_reap_strikes: self.controller_reap_strikes.clone(),
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
    #[serde(default = "default_thin_provision")]
    pub thin_provision: bool,
}

fn default_thin_provision() -> bool {
    true
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
