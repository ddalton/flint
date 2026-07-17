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
    /// PVs whose raid this agent has observed PRESENT (so a later absence
    /// is a collapse, not an in-flight NodeStage) and PVs already warned
    /// this episode — see `raid_collapse_verdict` (7b-3 P1).
    data_path_raid_seen: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    data_path_warned: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// Dead-controller strike counts (reconnect-looping flint controllers),
    /// keyed by controller name — see `reap_dead_controllers`.
    controller_reap_strikes: Arc<tokio::sync::Mutex<std::collections::HashMap<String, u32>>>,
    /// #1: NVMe-oF targets this node currently exports, keyed by NQN, with
    /// the params needed to RE-create each (recorded on create, dropped on
    /// delete, seeded from live SPDK at startup). The fast loss-detector
    /// diffs the keys against SPDK's live subsystems and re-exports any
    /// missing one directly — covering single-replica volumes (DS/MDS
    /// state.db, plain RWO) that the replica-only periodic reconcile skips.
    exported_targets: Arc<tokio::sync::Mutex<std::collections::HashMap<String, TargetExport>>>,
    /// ublk-backend analog of `exported_targets`: ublk disks this node
    /// should be serving, keyed by ublk id → backing bdev name (recorded on
    /// create, dropped on stop, backfilled by ground-truth rehydration).
    /// The fast loss-detector diffs the keys against `ublk_get_disks` and
    /// recovers/restarts any missing disk — recovery first, because with
    /// UBLK_F_USER_RECOVERY the kernel device (and the mount on top of it)
    /// survives an spdk-tgt death waiting for a new server.
    expected_ublk: Arc<tokio::sync::Mutex<std::collections::HashMap<u32, String>>>,
    /// Remote-chain companions to `expected_ublk`: bdev name → (volume
    /// NQN, storage node) for consumer-side hybrid chains, so the fast
    /// detector can re-drive the SPDK-initiator attach too — not just the
    /// ublk disk — when a roll takes both sides down (1u/1.15: waiting on
    /// 60s monitor ticks lost the race against the liveness bounce).
    expected_remote: Arc<tokio::sync::Mutex<std::collections::HashMap<String, (String, String)>>>,
}

/// #1: the params to reconstruct one NVMe-oF export after spdk-tgt drops it.
#[derive(Debug, Clone)]
struct TargetExport {
    bdev_name: String,
    target_ip: String,
    target_port: u16,
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
            data_path_raid_seen: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            data_path_warned: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            controller_reap_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            exported_targets: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            expected_ublk: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            expected_remote: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
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
            data_path_raid_seen: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            data_path_warned: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            controller_reap_strikes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            exported_targets: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            expected_ublk: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            expected_remote: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
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
                if let Err(e) = monitor_agent.rehydrate_exports_from_ground_truth().await {
                    warn!(error = %e, "[MONITOR] Export rehydration failed (non-fatal)");
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

        // #1 seed: adopt exports this node is already serving (created by a
        // prior node-agent process) so the loss-detector protects them too.
        self.seed_exported_nqns_from_spdk().await;

        // F8: ground-truth rehydration. A pod-level restart (DaemonSet
        // roll) leaves BOTH the registry and the target empty — seeding
        // from live subsystems finds nothing, and without this pass the
        // node's staged volumes hang in reconnect forever.
        if let Err(e) = self.rehydrate_exports_from_ground_truth().await {
            warn!(error = %e, "[NODE_AGENT] Export rehydration failed (non-fatal; monitor loop retries)");
        }

        // #1 fast export loss-detector: a tight (10s) loop that re-exports
        // immediately when spdk-tgt drops a subsystem it should be serving
        // (hard stop/restart), so recovery is seconds not up to a monitor
        // tick. Separate from the 60s monitor so its cadence is independent.
        let loss_agent = Arc::new(self.clone());
        let loss_is_ublk = Self::ublk_backend();
        let loss_task = tokio::spawn(async move {
            let mut loss_interval = interval(Duration::from_secs(10));
            loss_interval.tick().await; // first tick immediate; skip it
            loop {
                loss_interval.tick().await;
                if let Err(e) = loss_agent.reconcile_exports_if_lost().await {
                    debug!(error = %e, "[MONITOR] Export loss-detector failed (non-fatal)");
                }
                if loss_is_ublk {
                    if let Err(e) = loss_agent.reconcile_ublk_if_lost().await {
                        debug!(error = %e, "[MONITOR] ublk loss-detector failed (non-fatal)");
                    }
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
            _ = loss_task => {
                info!("[NODE_AGENT] Export loss-detector task stopped");
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

        // POST /api/disks/delete - Delete the LVS from a disk (inverse of
        // initialize; refuses while lvols exist). The dashboard's
        // "Delete SPDK Disk" action proxies here.
        let delete_disk = warp::path!("api" / "disks" / "delete")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_node_agent(node_agent.clone()))
            .and_then(Self::handle_delete_disk);

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
            .or(delete_disk)
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
                let all_disks: Vec<NodeDiskListing> = disks.iter()
                    .filter(|d| d.healthy)
                    .map(|d| NodeDiskListing {
                        pci_address: d.pci_address.clone(),
                        device_name: d.device_name.clone(),
                        size_bytes: d.size_bytes,
                        model: d.model.clone(),
                        healthy: d.healthy,
                        vendor_id: "0x0000".to_string(),
                        device_id: "0x0000".to_string(),
                        subsystem_vendor_id: "0x0000".to_string(),
                        subsystem_device_id: "0x0000".to_string(),
                        numa_node: 0,
                        driver: if d.blobstore_initialized { "vfio-pci" } else { "kernel" }.to_string(),
                        serial: String::new(),
                        firmware_version: String::new(),
                        namespace_id: 1,
                        mounted_partitions: d.mounted_partitions.clone(),
                        filesystem_type: None,
                        is_system_disk: d.is_system_disk,
                        spdk_ready: d.blobstore_initialized, // LVS initialized = ready
                        // Memory disks (malloc) are always driver_ready, physical disks need LVS
                        driver_ready: d.pci_address.starts_with("memory:") || d.blobstore_initialized,
                        blobstore_initialized: d.blobstore_initialized,
                        discovered_at: chrono::Utc::now().to_rfc3339(),
                    })
                    .collect();

                let response = UninitializedDisksResponse {
                    success: true,
                    node: node_agent.node_name.clone(),
                    count: all_disks.len(),
                    // Field name kept for compatibility: it carries ALL disks
                    uninitialized_disks: all_disks,
                };
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = NodeAgentError { success: false, error: e.to_string() };
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
                let disk_statuses: Vec<NodeDiskStatus> = disks.iter()
                    .filter(|d| d.healthy)
                    .map(|d| NodeDiskStatus {
                        pci_address: d.pci_address.clone(),
                        device_name: d.device_name.clone(),
                        size_bytes: d.size_bytes,
                        model: d.model.clone(),
                        healthy: d.healthy,
                        vendor_id: "0x0000".to_string(),
                        device_id: "0x0000".to_string(),
                        subsystem_vendor_id: "0x0000".to_string(),
                        subsystem_device_id: "0x0000".to_string(),
                        numa_node: 0,
                        driver: d.driver.clone(),
                        serial: String::new(),
                        firmware_version: String::new(),
                        namespace_id: 1,
                        mounted_partitions: d.mounted_partitions.clone(),
                        filesystem_type: None,
                        is_system_disk: d.is_system_disk,
                        spdk_ready: d.blobstore_initialized,
                        // Memory disks (malloc) are always driver_ready, physical disks need LVS
                        driver_ready: d.pci_address.starts_with("memory:") || d.blobstore_initialized,
                        blobstore_initialized: d.blobstore_initialized,
                        discovered_at: chrono::Utc::now().to_rfc3339(),
                        free_space: d.free_space,
                        temperature: None,
                        error_count: 0,
                    })
                    .collect();

                let response = NodeDisksStatusResponse {
                    node: node_agent.node_name.clone(),
                    disks: disk_statuses,
                    last_updated: chrono::Utc::now().to_rfc3339(),
                };

                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let error_response = NodeAgentError { success: false, error: e.to_string() };
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

        let response = DiskSetupResponse {
            success: failed_disks.is_empty(),
            setup_disks,
            failed_disks,
            warnings,
            completed_at: chrono::Utc::now().to_rfc3339(),
        };

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
        let response = DiskSetupResponse {
            success: false,
            setup_disks: Vec::new(),
            failed_disks: disks,
            warnings: vec!["Disk reset not yet implemented in minimal state".to_string()],
            completed_at: chrono::Utc::now().to_rfc3339(),
        };

        Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::NOT_IMPLEMENTED))
    }

    /// Handle POST /api/disks/delete - remove the LVS from a disk, returning
    /// it to the uninitialized pool. Refusals (lvols still present) come back
    /// as 409 with success:false so the UI shows the real reason.
    async fn handle_delete_disk(
        request: DeleteDiskRequest,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        debug!(pci_address = %request.pci_address, "[HTTP_API] Handling delete disk request");

        match node_agent.disk_service.delete_blobstore(&request.pci_address).await {
            Ok(lvs_name) => {
                let message = if lvs_name.is_empty() {
                    "No LVS present on the disk (nothing to delete)".to_string()
                } else {
                    format!("Deleted LVS {}", lvs_name)
                };
                let response = DiskDeleteResponse {
                    success: true,
                    message: Some(message),
                    error: None,
                    completed_at: chrono::Utc::now().to_rfc3339(),
                };
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                let msg = e.to_string();
                let status = if msg.contains("Refusing to delete") {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                let response = DiskDeleteResponse {
                    success: false,
                    message: None,
                    error: Some(msg),
                    completed_at: chrono::Utc::now().to_rfc3339(),
                };
                Ok(warp::reply::with_status(warp::reply::json(&response), status))
            }
        }
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

        // SPDK-aware idempotency (was: device-node existence alone — which
        // reported success on a quiesced orphan whose spdk-tgt had died).
        // ensure_ublk_disk consults live SPDK state, recovers a surviving
        // kernel device, or starts fresh — and registers the disk with the
        // fast loss-detector.
        if let (Some(_requested_id), Some(bdev_name)) =
            (params["ublk_id"].as_u64(), params["bdev_name"].as_str())
        {
            let live = node_agent.snapshot_ublk_disks().await;
            // The requested id is the legacy 20-bit hash — unusable (the
            // kernel bounds ids to ublks_max, default 64). Reuse the id
            // already serving this bdev (idempotent restage), else
            // allocate the smallest free one. The ACTUAL id rides back in
            // the response; the driver stores it in the PV annotation,
            // which unstage and rehydration treat as the authority.
            let ublk_id = match live
                .as_ref()
                .and_then(|l| l.iter().find(|(_, b)| b.as_str() == bdev_name))
                .map(|(id, _)| *id)
            {
                Some(id) => id,
                None => match node_agent.alloc_ublk_id(live.as_ref()).await {
                    Some(id) => id,
                    None => {
                        let error_response = json!({
                            "success": false,
                            "error": "no free ublk id (ublks_max exhausted)"
                        });
                        return Ok(warp::reply::with_status(
                            warp::reply::json(&error_response),
                            StatusCode::INTERNAL_SERVER_ERROR,
                        ));
                    }
                },
            };
            return match node_agent
                .ensure_ublk_disk(ublk_id, bdev_name, live.as_ref())
                .await
            {
                Ok(_) => {
                    let success_response = json!({
                        "result": format!("/dev/ublkb{}", ublk_id),
                        "ublk_id": ublk_id
                    });
                    Ok(warp::reply::with_status(warp::reply::json(&success_response), StatusCode::OK))
                }
                Err(e) => {
                    error!(error = %e, "[HTTP_API] ublk device creation failed");
                    let error_response = json!({
                        "success": false,
                        "error": e.to_string()
                    });
                    Ok(warp::reply::with_status(warp::reply::json(&error_response), StatusCode::INTERNAL_SERVER_ERROR))
                }
            };
        }

        // Params incomplete — legacy passthrough.
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

        // Deliberate stop: the loss-detector must not resurrect it.
        if let Some(ublk_id) = params["ublk_id"].as_u64() {
            node_agent.expected_ublk.lock().await.remove(&(ublk_id as u32));
        }

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

        // 1. Check if already connected (idempotency) — but ONLY reuse a
        // controller that is actually `live`. #3: after an spdk-tgt hard
        // stop the consumer's controller is wedged (dead/connecting) yet its
        // /dev node lingers; returning it here made NodeStage remount the
        // dead device and CrashLoop the consumer. A non-live controller is
        // disconnected so we fall through to re-create the target and
        // reconnect fresh.
        if let Ok(existing_device) = Self::find_nvme_device_by_nqn(nqn).await {
            let state = Self::nvme_controller_state_for_nqn(nqn).await.unwrap_or_default();
            if crate::nvme_recovery::controller_state_is_live(&state) {
                debug!(device = %existing_device, "[HTTP_API] NVMe device already exists and is live (idempotent)");
                let response = json!({
                    "device_path": existing_device,
                    "nvme_device": Self::extract_nvme_controller(&existing_device),
                    "nqn": nqn
                });
                return Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK));
            }
            warn!(
                device = %existing_device, state = %state,
                "[HTTP_API] Stale/wedged NVMe controller for NQN — disconnecting before reconnect (#3)"
            );
            if let Err(e) = Self::kernel_nvme_disconnect(nqn).await {
                warn!(error = %e, "[HTTP_API] Disconnect of stale controller failed (continuing to reconnect)");
            }
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

        // F9 guard (attach/detach campaign): NodeUnstage cleanup is
        // initiator-scoped — deleting the subsystem is only safe while this
        // node is the volume's sole consumer. After a force-detach +
        // cross-node re-attach, a revived node's deferred unstage would
        // otherwise nvmf_delete_subsystem the export actively serving the
        // new consumer (host fencing does not protect the subsystem object
        // itself). Fail closed: on evidence of a foreign consumer, skip the
        // delete — a leaked subsystem is reconciled later (orphan sweep /
        // F8 rehydration); a deleted live one is a cross-node data-plane
        // kill — and just fence this node out.
        let own_host = crate::nvmeof_export::flint_host_nqn(&node_agent.node_name);
        let mut foreign_reason: Option<String> = None;
        // (a) live controllers on the subsystem from another host?
        if let Ok(resp) = node_agent
            .disk_service
            .call_spdk_rpc(&json!({
                "method": "nvmf_subsystem_get_controllers",
                "params": { "nqn": nqn }
            }))
            .await
        {
            let foreign: Vec<String> = resp
                .get("result")
                .and_then(|r| r.as_array())
                .map(|cs| {
                    cs.iter()
                        .filter_map(|c| c.get("hostnqn").and_then(|h| h.as_str()))
                        .filter(|h| *h != own_host)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !foreign.is_empty() {
                foreign_reason = Some(format!("live foreign controller(s): {}", foreign.join(", ")));
            }
        } // Err ⇒ subsystem likely gone; the delete below is a no-op either way.
        // (b) VolumeAttachment ground truth: volume attached to another node?
        if foreign_reason.is_none() {
            if let Some(owner) = crate::identity::classify_subsystem_nqn(nqn) {
                if let Some(attached) = node_agent.get_attached_node(&owner).await {
                    if attached != node_agent.node_name {
                        foreign_reason = Some(format!("VolumeAttachment owned by {}", attached));
                    }
                }
            }
        }
        if let Some(reason) = foreign_reason {
            warn!(nqn = %nqn, reason = %reason,
                "[HTTP_API] F9 guard: subsystem is serving another consumer — skipping delete, fencing this node out");
            let _ = node_agent
                .disk_service
                .call_spdk_rpc(&json!({
                    "method": "nvmf_subsystem_remove_host",
                    "params": { "nqn": nqn, "host": own_host }
                }))
                .await;
            node_agent.exported_targets.lock().await.remove(nqn);
            let response = json!({
                "success": true,
                "message": "initiator cleanup done; subsystem retained (in use by another consumer)"
            });
            return Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK));
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

        // #1: stop tracking this export so the loss-detector doesn't try to
        // re-create a deliberately-deleted subsystem.
        node_agent.exported_targets.lock().await.remove(nqn);

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
        node_agent
            .ensure_export_for(nqn, bdev_name, target_ip, target_port)
            .await
    }

    /// Convergent local NVMe-oF export for one volume + registry tracking
    /// (#1). Completes whatever subset of {subsystem, namespace, listener}
    /// is missing (phase 0 fix) — so it is equally the CREATE path and the
    /// RE-CREATE path the loss-detector calls after spdk-tgt drops the
    /// subsystem. Records the export params so the detector can rebuild it.
    async fn ensure_export_for(
        &self,
        nqn: &str,
        bdev_name: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Fencing: this export is consumed by the local kernel initiator.
        let allowed = vec![crate::nvmeof_export::flint_host_nqn(&self.node_name)];
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
        crate::nvmeof_export::ensure_export(&self.disk_service, &spec).await?;

        // #1: remember this export so the fast loss-detector can notice if
        // spdk-tgt later drops it (restart) and re-create it directly.
        self.exported_targets.lock().await.insert(
            nqn.to_string(),
            TargetExport {
                bdev_name: bdev_name.to_string(),
                target_ip: target_ip.to_string(),
                target_port,
            },
        );

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
        // #2 survivable reconnect: hold the controller reconnecting across an
        // spdk-tgt bounce (with #1 re-exporting the subsystem) so I/O
        // auto-restores instead of the kernel default giving up. Tunable via
        // FLINT_NVME_CTRL_LOSS_TMO / FLINT_NVME_RECONNECT_DELAY.
        let policy = crate::nvme_recovery::ReconnectPolicy::from_env();
        let mut args: Vec<String> = vec![
            "connect".into(),
            "-t".into(), "tcp".into(),
            "-a".into(), target_ip.to_string(),
            "-s".into(), target_port.to_string(),
            "-n".into(), nqn.to_string(),
            "-q".into(), hostnqn,
        ];
        args.extend(policy.connect_args());
        let output = tokio::process::Command::new("nvme")
            .args(&args)
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

    /// Helper (#3): the kernel controller state for `nqn`, from
    /// `/sys/class/nvme/nvmeX/state` (e.g. `live`, `connecting`, `resetting`,
    /// `deleting`). `None` if no controller for this NQN exists. Used to tell
    /// a REUSABLE (`live`) controller from a stale/wedged one that must be
    /// disconnected before reconnecting.
    async fn nvme_controller_state_for_nqn(nqn: &str) -> Option<String> {
        let nvme_path = std::path::Path::new("/sys/class/nvme");
        let entries = std::fs::read_dir(nvme_path).ok()?;
        for entry in entries.flatten() {
            let subsysnqn_path = entry.path().join("subsysnqn");
            if let Ok(subsys_nqn) = std::fs::read_to_string(&subsysnqn_path) {
                if subsys_nqn.trim() == nqn {
                    let state = std::fs::read_to_string(entry.path().join("state"))
                        .ok()
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    return Some(state);
                }
            }
        }
        None
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
                        if lvol_name == crate::identity::lvol_name(volume_id) {
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
                    node_agent.expected_ublk.lock().await.remove(&ublk_id);
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
        let nqn = crate::identity::volume_nqn(volume_id);
        
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
    /// #1: seed the exported-NQN registry from SPDK's current flint volume
    /// subsystems at startup, so the loss-detector protects exports this
    /// node is already serving (created by a PREVIOUS node-agent process —
    /// e.g. after a driver update / node-DS roll), not only ones this
    /// process staged. Best-effort: on any RPC failure the registry stays as
    /// is and the 60s PV-based reconcile remains the backstop.
    async fn seed_exported_nqns_from_spdk(&self) {
        const FLINT_VOLUME_NQN_PREFIX: &str = "nqn.2024-11.com.flint:volume:";
        let resp = match self
            .disk_service
            .call_spdk_rpc(&serde_json::json!({ "method": "nvmf_get_subsystems" }))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                debug!(error = %e, "[NVME-RECOVERY #1] seed: nvmf_get_subsystems failed (backstop reconcile still runs)");
                return;
            }
        };
        let subs = resp.get("result").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        let mut seeded = 0usize;
        let mut reg = self.exported_targets.lock().await;
        for s in &subs {
            let Some(nqn) = s.get("nqn").and_then(|n| n.as_str()) else { continue };
            if !nqn.starts_with(FLINT_VOLUME_NQN_PREFIX) {
                continue;
            }
            // Reconstruct the re-export params from the live subsystem.
            let bdev_name = s
                .get("namespaces")
                .and_then(|n| n.as_array())
                .and_then(|nss| nss.first())
                .and_then(|ns| ns.get("bdev_name"))
                .and_then(|b| b.as_str());
            let listener = s
                .get("listen_addresses")
                .and_then(|l| l.as_array())
                .and_then(|ls| ls.first());
            // Listener may be flat or nested under "address" (SPDK version).
            let addr = listener.map(|l| l.get("address").unwrap_or(l));
            let traddr = addr.and_then(|a| a.get("traddr")).and_then(|t| t.as_str());
            let trsvcid = addr
                .and_then(|a| a.get("trsvcid"))
                .and_then(|t| t.as_str())
                .and_then(|t| t.parse::<u16>().ok());
            if let (Some(bdev_name), Some(traddr), Some(trsvcid)) = (bdev_name, traddr, trsvcid) {
                reg.insert(
                    nqn.to_string(),
                    TargetExport {
                        bdev_name: bdev_name.to_string(),
                        target_ip: traddr.to_string(),
                        target_port: trsvcid,
                    },
                );
                seeded += 1;
            }
        }
        if seeded > 0 {
            info!(count = seeded, "[NVME-RECOVERY #1] seeded export registry from live SPDK subsystems");
        }
    }

    /// Stage-time kernel-facing endpoint (mirrors the driver's
    /// create_nvmeof_block_device): loopback unless NVMEOF_LOCAL_TARGET_IP
    /// overrides, port from NVMEOF_TARGET_PORT.
    fn local_export_endpoint() -> (String, u16) {
        let ip = std::env::var("NVMEOF_LOCAL_TARGET_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = std::env::var("NVMEOF_TARGET_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(4420);
        (ip, port)
    }

    /// Kernel-facing block exposure backend. MUST mirror
    /// `create_block_device`'s dispatch rule: anything except "nvmeof"
    /// selects ublk.
    fn backend_is_ublk_value(val: Option<&str>) -> bool {
        val != Some("nvmeof")
    }

    fn ublk_backend() -> bool {
        Self::backend_is_ublk_value(std::env::var("BLOCK_DEVICE_BACKEND").ok().as_deref())
    }

    /// Parse an `ublk_get_disks` result array into ublk id → bdev name.
    /// Accepts both the RPC's `id` field and the config-dump `ublk_id`
    /// spelling.
    fn parse_ublk_disks(result: Option<&serde_json::Value>) -> std::collections::HashMap<u32, String> {
        result
            .and_then(|r| r.as_array())
            .map(|disks| {
                disks
                    .iter()
                    .filter_map(|d| {
                        let id = d
                            .get("id")
                            .or_else(|| d.get("ublk_id"))
                            .and_then(|i| i.as_u64())? as u32;
                        let bdev = d.get("bdev_name").and_then(|b| b.as_str())?;
                        Some((id, bdev.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One live snapshot of SPDK's ublk disks. `None` when the RPC itself
    /// fails (no ublk support, or no ublk target yet) — callers treat that
    /// as "state unknown", not "no disks".
    async fn snapshot_ublk_disks(&self) -> Option<std::collections::HashMap<u32, String>> {
        self.disk_service
            .call_spdk_rpc(&json!({ "method": "ublk_get_disks" }))
            .await
            .ok()
            .map(|resp| Self::parse_ublk_disks(resp.get("result")))
    }

    /// Idempotent `ublk_create_target` — required once per spdk-tgt
    /// process before any disk can be started or recovered. Startup does
    /// this too, but a tgt that restarted UNDER a live agent needs it
    /// re-issued from the reconcile paths.
    async fn ensure_ublk_target(&self) {
        match self
            .disk_service
            .call_spdk_rpc(&json!({ "method": "ublk_create_target", "params": {} }))
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("File exists") {
                    debug!(error = %msg, "[UBLK] ublk_create_target failed (will retry on next pass)");
                }
            }
        }
    }

    /// ublk id for a staged volume: the stage-time PV annotation is the
    /// authority (`flint.io/ublk-id`, written by store_block_device_info);
    /// the stable volume-id hash is the fallback for PVs staged before the
    /// annotation existed or whose patch was lost.
    fn resolve_ublk_id(&self, pv: &PersistentVolume, volume_handle: &str) -> u32 {
        pv.metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("flint.io/ublk-id"))
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or_else(|| {
                self.driver
                    .generate_ublk_id(crate::identity::storage_id_of_handle(volume_handle))
            })
    }

    /// Converge one ublk disk toward "served by this spdk-tgt". Recovery
    /// first: with UBLK_F_USER_RECOVERY (kernel 6.18+, SPDK v26.05) the
    /// kernel device survives an spdk-tgt death in quiesced state — the
    /// filesystem mounted on it lives — and `ublk_recover_disk` re-binds
    /// it without a consumer bounce. A fresh `ublk_start_disk` is the
    /// fallback when no kernel device exists. Registers the disk in
    /// `expected_ublk` on success so the fast loss-detector owns it.
    /// Returns true when a mutation (recover or start) happened.
    async fn ensure_ublk_disk(
        &self,
        ublk_id: u32,
        bdev_name: &str,
        live: Option<&std::collections::HashMap<u32, String>>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(live_bdev) = live.and_then(|l| l.get(&ublk_id)) {
            if live_bdev == bdev_name {
                // Already served — just make sure the detector owns it.
                self.expected_ublk
                    .lock()
                    .await
                    .insert(ublk_id, bdev_name.to_string());
                return Ok(false);
            }
            // Same id serving a different bdev: a 20-bit hash collision or
            // foreign disk. Never stomp it.
            return Err(format!(
                "ublk id {} already serves bdev {} (wanted {}) — refusing to replace",
                ublk_id, live_bdev, bdev_name
            )
            .into());
        }

        self.ensure_ublk_target().await;

        let device_exists = std::path::Path::new(&format!("/dev/ublkb{}", ublk_id)).exists();
        if device_exists {
            match self
                .disk_service
                .call_spdk_rpc(&json!({
                    "method": "ublk_recover_disk",
                    "params": { "bdev_name": bdev_name, "ublk_id": ublk_id }
                }))
                .await
            {
                Ok(_) => {
                    warn!(ublk_id, bdev = %bdev_name,
                        "[UBLK] recovered quiesced kernel device (mount preserved)");
                    self.expected_ublk
                        .lock()
                        .await
                        .insert(ublk_id, bdev_name.to_string());
                    return Ok(true);
                }
                Err(e) => {
                    // Not recoverable (old kernel without USER_RECOVERY, or
                    // a stale node) — fall through to a fresh start, which
                    // the kernel rejects while the old device lingers; the
                    // error surfaces to the caller for the next tick.
                    debug!(ublk_id, error = %e, "[UBLK] recover failed — trying fresh start");
                }
            }
        }

        // The kernel EINVALs ADD_DEV when nr_hw_queues exceeds the CPU
        // count (found live on runv: chart numQueues=8 vs 4 vCPUs), so
        // clamp the tuning knob to the host.
        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        let num_queues = std::env::var("UBLK_NUM_QUEUES")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(4)
            .clamp(1, host_cpus);
        let queue_depth = std::env::var("UBLK_QUEUE_DEPTH")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(256);
        self.disk_service
            .call_spdk_rpc(&json!({
                "method": "ublk_start_disk",
                "params": {
                    "bdev_name": bdev_name,
                    "ublk_id": ublk_id,
                    "num_queues": num_queues,
                    "queue_depth": queue_depth
                }
            }))
            .await
            .map_err(|e| format!("ublk_start_disk {} ({}): {}", ublk_id, bdev_name, e))?;
        warn!(ublk_id, bdev = %bdev_name, "[UBLK] started ublk disk");
        self.expected_ublk
            .lock()
            .await
            .insert(ublk_id, bdev_name.to_string());
        Ok(true)
    }

    /// Smallest usable ublk id. The kernel bounds ADD_DEV ids to
    /// `ublks_max` (default 64; SPDK sizes its control ring from the same
    /// sysfs knob), so the legacy 20-bit volume-id hash is unusable as an
    /// id — the kernel EINVALs it. Skips ids SPDK serves, ids this agent
    /// has promised (registry), and ids with a lingering kernel device
    /// node (a quiesced stranger from a previous life must not be
    /// adopted by an unrelated volume).
    async fn alloc_ublk_id(
        &self,
        live: Option<&std::collections::HashMap<u32, String>>,
    ) -> Option<u32> {
        let max = std::fs::read_to_string("/sys/module/ublk_drv/parameters/ublks_max")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(64);
        let reserved = self.expected_ublk.lock().await;
        (0..max).find(|id| {
            live.map_or(true, |l| !l.contains_key(id))
                && !reserved.contains_key(id)
                && !std::path::Path::new(&format!("/dev/ublkb{}", id)).exists()
        })
    }

    /// Whether `sub` (an `nvmf_get_subsystems` entry) already serves the
    /// desired export: a namespace (optionally backed by `want_bdev`), a
    /// listener on `want_traddr`, and — when a host is required — that
    /// host admitted. Used by rehydration to make the common
    /// nothing-to-do pass free of mutation RPCs.
    fn export_satisfied(
        sub: Option<&serde_json::Value>,
        want_bdev: Option<&str>,
        want_traddr: &str,
        want_host: Option<&str>,
    ) -> bool {
        let Some(sub) = sub else { return false };
        let ns_ok = sub
            .get("namespaces")
            .and_then(|n| n.as_array())
            .map(|nss| {
                !nss.is_empty()
                    && want_bdev
                        .map(|b| {
                            nss.iter().any(|ns| {
                                ns.get("bdev_name").and_then(|v| v.as_str()) == Some(b)
                                    || ns.get("uuid").and_then(|v| v.as_str()) == Some(b)
                            })
                        })
                        .unwrap_or(true)
            })
            .unwrap_or(false);
        let listener_ok = sub
            .get("listen_addresses")
            .and_then(|l| l.as_array())
            .map(|ls| {
                ls.iter().any(|l| {
                    let addr = l.get("address").unwrap_or(l);
                    addr.get("traddr").and_then(|t| t.as_str()) == Some(want_traddr)
                })
            })
            .unwrap_or(false);
        let host_ok = match want_host {
            None => true,
            Some(h) => {
                sub.get("hosts")
                    .and_then(|hs| hs.as_array())
                    .map(|hs| hs.iter().any(|e| e.get("nqn").and_then(|n| n.as_str()) == Some(h)))
                    .unwrap_or(false)
                    || sub
                        .get("allow_any_host")
                        .and_then(|a| a.as_bool())
                        .unwrap_or(false)
            }
        };
        ns_ok && listener_ok && host_ok
    }

    /// F8 (attach/detach campaign): rebuild this node's exports from
    /// PERSISTENT ground truth — PVs + VolumeAttachments — not just live
    /// SPDK state. `seed_exported_nqns_from_spdk` can only adopt exports
    /// that still exist in the target; after a csi-node POD restart
    /// (agent + spdk-tgt die together, e.g. a DaemonSet roll) the target
    /// comes back BARE and the registry starts empty, so the loss-detector
    /// protects nothing and every staged volume hangs in reconnect forever
    /// (drills 1.9b/1.15). The durable truth outlives the pod: an attached
    /// VolumeAttachment plus the PV's volumeAttributes say exactly which
    /// exports this node must serve. Convergent and idempotent; runs at
    /// startup and from the 60s monitor loop. Multi-replica exports are
    /// owned by reconcile_replica_targets; RWX PVs by the NFS liveness
    /// reconciler (their block export belongs to the synthetic backing PV,
    /// which this pass handles like any other single-replica PV).
    async fn rehydrate_exports_from_ground_truth(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::storage::v1::VolumeAttachment;

        // VA ground truth: PV name → consumer node, attached only.
        let vas: Api<VolumeAttachment> = Api::all(self.driver.kube_client.clone());
        let va_map: std::collections::HashMap<String, String> = vas
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter_map(|va| {
                let attached = va.status.as_ref().map(|s| s.attached).unwrap_or(false);
                let pv = va.spec.source.persistent_volume_name.clone()?;
                attached.then(|| (pv, va.spec.node_name))
            })
            .collect();
        if va_map.is_empty() {
            return Ok(());
        }

        // One snapshot of live subsystems for all satisfied-checks.
        let subsystems: std::collections::HashMap<String, serde_json::Value> = self
            .disk_service
            .call_spdk_rpc(&serde_json::json!({ "method": "nvmf_get_subsystems" }))
            .await?
            .get("result")
            .and_then(|r| r.as_array())
            .map(|subs| {
                subs.iter()
                    .filter_map(|s| {
                        s.get("nqn")
                            .and_then(|n| n.as_str())
                            .map(|n| (n.to_string(), s.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let (local_ip, local_port) = Self::local_export_endpoint();
        let own_host = crate::nvmeof_export::flint_host_nqn(&self.node_name);
        let fencing = crate::nvmeof_export::fencing_enabled();
        let mut node_ip: Option<String> = None; // lazy; only cross-node exports need it
        // ublk backend: one live-disk snapshot for the satisfied-checks
        // (analog of the subsystems snapshot above). None ⇒ the RPC failed
        // (fresh tgt without a ublk target object) ⇒ state unknown, every
        // disk goes through ensure_ublk_disk.
        let is_ublk = Self::ublk_backend();
        let ublk_disks = if is_ublk { self.snapshot_ublk_disks().await } else { None };

        let pvs: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
        let mut rebuilt = 0usize;
        // ublk reaper bookkeeping: which live disks SHOULD exist on this
        // node (desired), and which bdev names are attributable to flint
        // single-replica volumes at all (lvol uuid → pv). A disk that is
        // attributable but not desired is a leak — e.g. the local disk a
        // stale VA made us rebuild after the consumer moved away — and
        // the fast detector would otherwise resurrect it forever.
        let mut desired_ublk: std::collections::HashSet<String> = Default::default();
        let mut attributable_lvols: std::collections::HashSet<String> = Default::default();
        for pv in pvs.list(&ListParams::default()).await?.items {
            let Some(pv_name) = pv.metadata.name.clone() else { continue };
            let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) else { continue };
            if csi.driver != "flint.csi.storage.io" {
                continue;
            }
            // RWX PVs are NFS-mounted; their block export belongs to the
            // synthetic backing PV's own PV/VA pair.
            if crate::replica_sync::is_rwx_pv(&pv) {
                continue;
            }
            let Some(attrs) = csi.volume_attributes.as_ref() else { continue };
            // Replica volumes: export ownership lives with
            // reconcile_replica_targets (alias NQNs, raid semantics).
            let replica_count = attrs
                .get("flint.csi.storage.io/replica-count")
                .and_then(|c| c.parse::<u32>().ok())
                .unwrap_or(1);
            if replica_count > 1 {
                continue;
            }
            let Some(storage_node) = attrs.get("flint.csi.storage.io/node-name") else { continue };
            let Some(lvol_uuid) = attrs.get("flint.csi.storage.io/lvol-uuid") else { continue };
            if is_ublk {
                attributable_lvols.insert(lvol_uuid.clone());
            }
            let Some(consumer) = va_map.get(&pv_name) else { continue };
            // SPDK object names derive from the volumeHandle (differs from
            // the PV name for synthetic NFS backing PVs).
            let nqn = crate::identity::volume_nqn(&csi.volume_handle);
            let sub = subsystems.get(&nqn);

            if storage_node == &self.node_name && consumer == &self.node_name {
                if is_ublk {
                    // ublk backend: the kernel consumes /dev/ublkb<id> on
                    // the lvol bdev directly — no loopback export exists.
                    // Ids are agent-allocated (kernel bounds them to
                    // ublks_max), so match live disks by BACKING BDEV; the
                    // PV annotation is the id's persistent record.
                    desired_ublk.insert(lvol_uuid.clone());
                    if let Some((id, bdev)) = ublk_disks
                        .as_ref()
                        .and_then(|l| l.iter().find(|(_, b)| b.as_str() == lvol_uuid.as_str()))
                    {
                        // Serving — backfill detector ownership (covers
                        // agent-only restarts, like the seed pass does for
                        // loopback exports).
                        self.expected_ublk.lock().await.insert(*id, bdev.clone());
                        continue;
                    }
                    let ublk_id = self.resolve_ublk_id(&pv, &csi.volume_handle);
                    let bdev = match self.verify_local_lvol(lvol_uuid).await {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(volume_id = %pv_name, error = %e,
                                  "[REHYDRATE] lvol not loaded yet — seeding fast detector");
                            // Ride the 10s loop, not the 60s monitor:
                            // ensure_ublk_disk fails benignly until the
                            // lvol loads, then recovers/starts.
                            self.expected_ublk
                                .lock()
                                .await
                                .insert(ublk_id, lvol_uuid.clone());
                            continue;
                        }
                    };
                    match self.ensure_ublk_disk(ublk_id, &bdev, ublk_disks.as_ref()).await {
                        Ok(true) => {
                            warn!(ublk_id, volume_id = %pv_name,
                                  "[REHYDRATE] rebuilt local ublk disk from ground truth");
                            rebuilt += 1;
                        }
                        Ok(false) => {}
                        Err(e) => warn!(ublk_id, error = %e, "[REHYDRATE] ublk rebuild failed"),
                    }
                    continue;
                }
                // Loopback export consumed by the local kernel initiator.
                if Self::export_satisfied(sub, None, &local_ip, fencing.then_some(&own_host)) {
                    // Healthy — just make sure the loss-detector owns it
                    // (covers agent-only restarts where seed already ran,
                    // and backfills registry entries lost mid-flight).
                    let mut reg = self.exported_targets.lock().await;
                    if !reg.contains_key(&nqn) {
                        if let Some(bdev) = sub
                            .and_then(|s| s.get("namespaces"))
                            .and_then(|n| n.as_array())
                            .and_then(|nss| nss.first())
                            .and_then(|ns| ns.get("bdev_name"))
                            .and_then(|b| b.as_str())
                        {
                            reg.insert(
                                nqn.clone(),
                                TargetExport {
                                    bdev_name: bdev.to_string(),
                                    target_ip: local_ip.clone(),
                                    target_port: local_port,
                                },
                            );
                        }
                    }
                    continue;
                }
                // The lvol must be back before we can export it — disk
                // auto-recovery reloads the lvstore at startup; converge on
                // a later tick if it hasn't landed yet.
                let bdev = match self.verify_local_lvol(lvol_uuid).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(volume_id = %pv_name, error = %e,
                              "[REHYDRATE] lvol not loaded yet — retrying next tick");
                        continue;
                    }
                };
                match self.ensure_export_for(&nqn, &bdev, &local_ip, local_port).await {
                    Ok(()) => {
                        warn!(nqn = %nqn, "[REHYDRATE] rebuilt loopback export from ground truth");
                        rebuilt += 1;
                    }
                    Err(e) => warn!(nqn = %nqn, error = %e, "[REHYDRATE] loopback re-export failed"),
                }
            } else if storage_node == &self.node_name {
                // Storage-side export for a cross-node consumer — mirror
                // setup_nvmeof_target_on_node: listener on the node IP,
                // fenced to the consumer. NOT registry-tracked (the
                // loss-detector re-creates with kernel-facing loopback
                // semantics, wrong here); this pass owns it, like replica
                // exports.
                if node_ip.is_none() {
                    match self.driver.get_node_ip(&self.node_name).await {
                        Ok(ip) => node_ip = Some(ip),
                        Err(e) => {
                            warn!(error = %e, "[REHYDRATE] node IP lookup failed");
                            continue;
                        }
                    }
                }
                let ip = node_ip.as_deref().unwrap();
                let consumer_host = crate::nvmeof_export::flint_host_nqn(consumer);
                if Self::export_satisfied(sub, None, ip, fencing.then_some(&consumer_host)) {
                    continue;
                }
                let bdev = match self.verify_local_lvol(lvol_uuid).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(volume_id = %pv_name, error = %e,
                              "[REHYDRATE] lvol not loaded yet — retrying next tick");
                        continue;
                    }
                };
                let allowed = vec![consumer_host];
                let spec = crate::nvmeof_export::ExportSpec {
                    nqn: &nqn,
                    bdev_name: &bdev,
                    bdev_aliases: &[],
                    trtype: &self.driver.nvmeof_transport,
                    traddr: ip,
                    trsvcid: self.driver.nvmeof_target_port,
                    allowed_hosts: fencing.then_some(allowed.as_slice()),
                    ns_identity: None,
                };
                match crate::nvmeof_export::ensure_export(&self.disk_service, &spec).await {
                    Ok(()) => {
                        warn!(nqn = %nqn, consumer = %consumer,
                              "[REHYDRATE] rebuilt storage-side export for cross-node consumer");
                        rebuilt += 1;
                    }
                    Err(e) => {
                        warn!(nqn = %nqn, error = %e, "[REHYDRATE] storage-side re-export failed")
                    }
                }
            } else if consumer == &self.node_name {
                if is_ublk {
                    // Hybrid path: NVMe-oF between nodes (SPDK initiator),
                    // ublk for the kernel-facing exposure. Chain-liveness =
                    // the nvme bdev exists (a dead remote attach drops it)
                    // AND the ublk disk is served.
                    let controller_name = crate::identity::initiator_controller_name(&nqn);
                    let expected_bdev = format!("{}n1", controller_name);
                    desired_ublk.insert(expected_bdev.clone());
                    let served = ublk_disks
                        .as_ref()
                        .and_then(|l| l.iter().find(|(_, b)| b.as_str() == expected_bdev))
                        .map(|(id, _)| *id);
                    if let Some(id) = served {
                        if self.bdev_exists(&expected_bdev).await {
                            self.expected_ublk.lock().await.insert(id, expected_bdev);
                            continue;
                        }
                    }
                    let ublk_id = self.resolve_ublk_id(&pv, &csi.volume_handle);
                    let outcome = match self.ensure_remote_attach(&nqn, storage_node).await {
                        Ok(bdev) => self.ensure_ublk_disk(ublk_id, &bdev, ublk_disks.as_ref()).await,
                        Err(e) => Err(e),
                    };
                    match outcome {
                        Ok(true) => {
                            warn!(nqn = %nqn, storage_node = %storage_node, ublk_id,
                                  "[REHYDRATE] rebuilt remote-consumer ublk chain from ground truth");
                            rebuilt += 1;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            warn!(nqn = %nqn, error = %e,
                              "[REHYDRATE] remote ublk chain rebuild failed — seeding fast detector");
                            self.expected_remote.lock().await.insert(
                                expected_bdev.clone(),
                                (nqn.clone(), storage_node.clone()),
                            );
                            self.expected_ublk
                                .lock()
                                .await
                                .insert(ublk_id, expected_bdev.clone());
                        }
                    }
                    continue;
                }
                // Staged here, stored remotely: rebuild the SPDK initiator
                // chain (remote attach) + the loopback re-export the
                // surviving kernel session is reconnect-looping toward.
                // (A dead chain drops the nvme bdev, which drops the
                // namespace, so satisfied-ness tracks chain liveness.)
                if Self::export_satisfied(sub, None, &local_ip, fencing.then_some(&own_host)) {
                    continue;
                }
                match self.rehydrate_remote_consumer_chain(&nqn, storage_node).await {
                    Ok(()) => {
                        warn!(nqn = %nqn, storage_node = %storage_node,
                              "[REHYDRATE] rebuilt remote-consumer chain from ground truth");
                        rebuilt += 1;
                    }
                    Err(e) => warn!(nqn = %nqn, storage_node = %storage_node, error = %e,
                          "[REHYDRATE] remote consumer chain rebuild failed (will retry)"),
                }
            }
        }
        if is_ublk {
            if let Some(live) = &ublk_disks {
                for (id, bdev) in live {
                    if desired_ublk.contains(bdev) {
                        continue;
                    }
                    // Only reap what we can attribute to a flint
                    // single-replica volume: a local disk on a known lvol,
                    // or a remote-chain disk (nvme_…_volume_…n1). Raid,
                    // malloc, and unknown bdevs are not ours to stop.
                    let attributable = attributable_lvols.contains(bdev)
                        || (bdev.starts_with("nvme_nqn") && bdev.contains("_volume_"));
                    if !attributable {
                        continue;
                    }
                    warn!(ublk_id = id, bdev = %bdev,
                        "[REHYDRATE] reaping stale ublk disk (volume no longer consumed on this node)");
                    let _ = self
                        .disk_service
                        .call_spdk_rpc(&json!({
                            "method": "ublk_stop_disk",
                            "params": { "ublk_id": id }
                        }))
                        .await;
                    self.expected_ublk.lock().await.remove(id);
                }
            }
        }
        if rebuilt > 0 {
            info!(rebuilt, "[REHYDRATE] exports rebuilt from ground truth (PVs + VolumeAttachments)");
        }
        Ok(())
    }

    /// Rebuild the consumer-side data chain for a volume staged on this
    /// node but stored on `storage_node`: SPDK initiator re-attach to the
    /// remote export, then the loopback re-export. The storage side is
    /// rebuilt by that node's own rehydration; until it lands the attach
    /// fails and we converge on a later tick.
    async fn rehydrate_remote_consumer_chain(
        &self,
        nqn: &str,
        storage_node: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let expected_bdev = self.ensure_remote_attach(nqn, storage_node).await?;
        let (ip, port) = Self::local_export_endpoint();
        self.ensure_export_for(nqn, &expected_bdev, &ip, port).await
    }

    async fn bdev_exists(&self, name: &str) -> bool {
        self.disk_service
            .call_spdk_rpc(&json!({
                "method": "bdev_get_bdevs",
                "params": { "name": name }
            }))
            .await
            .ok()
            .and_then(|r| r.get("result").and_then(|v| v.as_array()).map(|a| !a.is_empty()))
            .unwrap_or(false)
    }

    /// Ensure the SPDK-initiator attach to `storage_node`'s export for
    /// `nqn` is alive; returns the namespace bdev name (`nvme_…n1`) both
    /// backends layer their kernel exposure on.
    async fn ensure_remote_attach(
        &self,
        nqn: &str,
        storage_node: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let controller_name = crate::identity::initiator_controller_name(nqn);
        let expected_bdev = format!("{}n1", controller_name);

        let have_bdev = self.bdev_exists(&expected_bdev).await;
        if !have_bdev {
            // A controller without its bdev is dead weight from a previous
            // life — replace it (mirrors connect_to_nvmeof_target).
            let ctrlr_exists = self
                .disk_service
                .call_spdk_rpc(&json!({
                    "method": "bdev_nvme_get_controllers",
                    "params": { "name": controller_name }
                }))
                .await
                .ok()
                .and_then(|r| r.get("result").and_then(|v| v.as_array()).map(|a| !a.is_empty()))
                .unwrap_or(false);
            if ctrlr_exists {
                let _ = self
                    .disk_service
                    .call_spdk_rpc(&json!({
                        "method": "bdev_nvme_detach_controller",
                        "params": { "name": controller_name }
                    }))
                    .await;
            }
            let remote_ip = self
                .driver
                .get_node_ip(storage_node)
                .await
                .map_err(|e| format!("node IP for {}: {}", storage_node, e))?;
            self.disk_service
                .call_spdk_rpc(&json!({
                    "method": "bdev_nvme_attach_controller",
                    "params": {
                        "name": controller_name,
                        "trtype": self.driver.nvmeof_transport.to_uppercase(),
                        "traddr": remote_ip,
                        "trsvcid": self.driver.nvmeof_target_port.to_string(),
                        "subnqn": nqn,
                        "adrfam": "IPv4",
                        "hostnqn": crate::nvmeof_export::flint_host_nqn(&self.node_name),
                        // Survivable reconnect — see connect_to_nvmeof_target
                        // (chaos drill 1u/B3: default attach drops the bdev
                        // during a storage outage, destroying the ublk chain).
                        "ctrlr_loss_timeout_sec": -1,
                        "reconnect_delay_sec": 2
                    }
                }))
                .await
                .map_err(|e| format!("remote attach {}: {}", nqn, e))?;
        }
        Ok(expected_bdev)
    }

    /// #1 fast loss-detector: if SPDK is missing any NQN this node believes
    /// it exports (spdk-tgt restarted or dropped a subsystem), re-export
    /// everything IMMEDIATELY instead of waiting out the 60s periodic
    /// reconcile — so the client's held-open controller (#2) reconnects
    /// within seconds. Cheap: one `nvmf_get_subsystems` + a set diff; a noop
    /// (no reconcile) whenever every tracked export is present.
    async fn reconcile_exports_if_lost(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let targets = self.exported_targets.lock().await.clone();
        if targets.is_empty() {
            return Ok(());
        }
        let resp = self
            .disk_service
            .call_spdk_rpc(&serde_json::json!({ "method": "nvmf_get_subsystems" }))
            .await?;
        // A subsystem counts as present only when COMPLETE (has a namespace
        // AND a listener). A post-restart re-export that created the
        // subsystem but couldn't add_ns yet (lvol bdev not reloaded) is
        // NOT satisfied → it stays in `missing` and the convergent
        // ensure_export runs again next tick until the namespace lands.
        let satisfied: std::collections::HashSet<String> = resp
            .get("result")
            .and_then(|r| r.as_array())
            .map(|subs| {
                subs.iter()
                    .filter_map(|s| {
                        let nqn = s.get("nqn").and_then(|n| n.as_str())?;
                        let has_ns = s
                            .get("namespaces")
                            .and_then(|n| n.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false);
                        let has_listener = s
                            .get("listen_addresses")
                            .and_then(|l| l.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false);
                        crate::nvme_recovery::subsystem_is_satisfied(has_ns, has_listener)
                            .then(|| nqn.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let registered: std::collections::HashSet<String> = targets.keys().cloned().collect();
        let missing = crate::nvme_recovery::missing_exports(&registered, &satisfied);
        if missing.is_empty() {
            return Ok(());
        }
        warn!(
            missing_count = missing.len(),
            missing = format!("{:?}", missing),
            "[NVME-RECOVERY #1] SPDK missing exported subsystem(s) — spdk-tgt likely restarted; re-creating directly"
        );
        // Re-create each missing subsystem DIRECTLY from the recorded params
        // (covers single-replica volumes the replica-only periodic reconcile
        // skips). Idempotent + stable ns identity, so the client's held-open
        // controller (#2) reattaches to the same namespace.
        for nqn in &missing {
            if let Some(rec) = targets.get(nqn) {
                if let Err(e) = self
                    .ensure_export_for(nqn, &rec.bdev_name, &rec.target_ip, rec.target_port)
                    .await
                {
                    warn!(nqn = %nqn, error = %e, "[NVME-RECOVERY #1] re-export failed (retry next tick)");
                }
            }
        }
        Ok(())
    }

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
    /// ublk analog of `reconcile_exports_if_lost`: if SPDK is missing a
    /// ublk disk this node believes it serves (spdk-tgt restarted), recover
    /// or restart it IMMEDIATELY — the quiesced kernel device is holding
    /// the mounted filesystem open waiting for a new server, and every
    /// second in that state is application I/O stall.
    async fn reconcile_ublk_if_lost(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let expected = self.expected_ublk.lock().await.clone();
        if expected.is_empty() {
            return Ok(());
        }
        let live = match self.snapshot_ublk_disks().await {
            Some(l) => l,
            None => {
                // RPC failed — a fresh tgt has no ublk target object yet.
                self.ensure_ublk_target().await;
                std::collections::HashMap::new()
            }
        };
        for (id, bdev) in &expected {
            if live.contains_key(id) {
                continue;
            }
            // Hybrid chains: the ublk disk sits on an SPDK-initiator bdev
            // that may itself be gone — re-drive the remote attach first.
            let remote = self.expected_remote.lock().await.get(bdev).cloned();
            if let Some((nqn, storage_node)) = remote {
                if !self.bdev_exists(bdev).await {
                    if let Err(e) = self.ensure_remote_attach(&nqn, &storage_node).await {
                        debug!(nqn = %nqn, error = %e,
                            "[UBLK-DETECTOR] remote attach not ready (will retry)");
                        continue;
                    }
                }
            }
            // r2 chains: a REGISTERED raid bdev that's missing is always a
            // post-stage loss (entries only exist after a successful
            // serve), so the 10s tick can drive the full in-place repair
            // without the 3-strike monitor's in-flight-NodeStage guard —
            // dropping RAID-host tgt-kill recovery from ~4min to seconds.
            // The 60s monitor stays as the backstop for unregistered
            // chains (fresh agent restarts rebuild the registry lazily).
            if !self.bdev_exists(bdev).await {
                if let Some(volume_id) = crate::identity::parse_raid_name(bdev) {
                    let pv_name = crate::identity::storage_id_of_handle(volume_id);
                    let pvs: Api<PersistentVolume> =
                        Api::all(self.driver.kube_client.clone());
                    match pvs.get(&pv_name).await {
                        Ok(pv) => {
                            let handle = pv
                                .spec
                                .as_ref()
                                .and_then(|s| s.csi.as_ref())
                                .map(|c| c.volume_handle.clone())
                                .unwrap_or_else(|| volume_id.to_string());
                            match self.repair_data_path(&pv, &handle).await {
                                Ok(()) => {
                                    info!(ublk_id = id, bdev = %bdev,
                                        "[UBLK-DETECTOR] r2 raid chain rebuilt in place");
                                    continue;
                                }
                                Err(e) => {
                                    debug!(ublk_id = id, bdev = %bdev, error = %e,
                                        "[UBLK-DETECTOR] r2 raid rebuild not ready (will retry)");
                                    continue;
                                }
                            }
                        }
                        Err(_) => { /* PV gone/API blip — reaper or next tick */ }
                    }
                }
            }
            match self.ensure_ublk_disk(*id, bdev, Some(&live)).await {
                Ok(_) => {}
                Err(e) => {
                    // A registry entry can outlive its volume (teardown
                    // racing a DS roll leaves the in-memory map stale) —
                    // then the rebuild ENODEVs forever. Reap the entry
                    // when its bdev is gone AND no live PV claims the id.
                    if !self.bdev_exists(bdev).await
                        && !self.ublk_id_claimed_by_any_pv(*id).await
                    {
                        self.expected_ublk.lock().await.remove(id);
                        self.expected_remote.lock().await.remove(bdev);
                        info!(ublk_id = id, bdev = %bdev,
                            "[UBLK-DETECTOR] reaped stale registry entry (no live PV claims this id)");
                        continue;
                    }
                    warn!(ublk_id = id, bdev = %bdev, error = %e,
                        "[UBLK-DETECTOR] disk rebuild failed (will retry)")
                }
            }
        }
        Ok(())
    }

    /// Does any live flint PV claim this ublk id? Checks the stage-time
    /// annotation first, then the stable-hash fallback (mirrors
    /// resolve_ublk_id). Errs on the side of "claimed" when the API is
    /// unreachable — never reap on a blind tick.
    async fn ublk_id_claimed_by_any_pv(&self, id: u32) -> bool {
        let pvs: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
        let list = match pvs.list(&Default::default()).await {
            Ok(l) => l,
            Err(_) => return true,
        };
        for pv in &list.items {
            let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) else {
                continue;
            };
            if csi.driver != "flint.csi.storage.io" {
                continue;
            }
            if self.resolve_ublk_id(pv, &csi.volume_handle) == id {
                return true;
            }
        }
        false
    }

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
            let Some(volume_id) = crate::identity::parse_raid_name(raid_name) else { continue };
            // RWX volumes stage under the synthetic "nfs-server-<vol>"
            // handle; PV reads/patches must target the user PV.
            let pv_name = crate::identity::storage_id_of_handle(volume_id);

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
                // Tier-2 7b: a hot-rejoin-marked replica may be serving in
                // this raid while its chain is still remote (localization
                // backfill in flight) — the volume is NOT fully redundant
                // yet. Report that instead of clearing (the same read idiom
                // KubeStore::load uses).
                let localizing = crate::replica_sync::update_sync_record(
                    &self.driver.kube_client,
                    volume_id,
                    |_| {},
                )
                .await
                .ok()
                .flatten()
                .map(|r| r.replicas.iter().filter(|x| x.hot_rejoin.is_some()).count())
                .unwrap_or(0);
                if localizing > 0 {
                    let health = json!({
                        "state": "localizing",
                        "configured": configured.len(),
                        "total": total,
                        "localizing_replicas": localizing,
                        "observed_by": self.node_name,
                    })
                    .to_string();
                    let patch = json!({
                        "metadata": {
                            "annotations": { "flint.csi.storage.io/replica-health": health }
                        }
                    });
                    let _ =
                        pvs.patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await;
                } else {
                    // Clear a previously-set annotation (merge-patch null
                    // deletes)
                    let patch = json!({
                        "metadata": {
                            "annotations": { "flint.csi.storage.io/replica-health": null }
                        }
                    });
                    let _ =
                        pvs.patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await;
                }
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
        // ublk frontend: with UBLK_F_USER_RECOVERY (kernel 6.18+, SPDK
        // v26.05) the kernel device SURVIVES spdk-tgt death quiesced and
        // the mount lives — the raid chain beneath it is what needs
        // rebuilding, then ublk_recover_disk re-binds in place. (The old
        // blanket "restage required" refusal predates USER_RECOVERY and
        // left r2 ublk volumes dead forever while the 10s detector
        // retried against a raid bdev nobody was rebuilding — 2u/2.2a.)
        let ublk_id = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("flint.io/ublk-id"))
            .and_then(|v| v.parse::<u32>().ok());
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
                    "params": { "name": crate::identity::raid_name(volume_handle) }
                }))
                .await;
            return Err("attachment left this node mid-repair — raid torn back down".into());
        }

        if let Some(id) = ublk_id {
            // ublk frontend: no export to rebuild — recover the quiesced
            // kernel device onto the reassembled raid (start is the
            // fallback inside ensure_ublk_disk for a truly fresh node).
            let live = self.snapshot_ublk_disks().await;
            self.ensure_ublk_disk(id, &raid_bdev, live.as_ref()).await?;
            return Ok(());
        }

        // Re-export the loopback subsystem: same NQN/listener/serial and
        // the PINNED namespace identity, listener-last via the convergent
        // module — the partial-rebuild hazard from the layer-2 experiment
        // (listener over a namespace-less subsystem makes the kernel
        // conclude the namespace was deleted) cannot recur.
        let nqn = crate::identity::volume_nqn(volume_handle);
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
        use crate::cutover::{
            data_path_verdict, raid_collapse_verdict, CollapseEvent, DataPathAction,
            DATA_PATH_LOST_ANNOTATION,
        };
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
            let raid_present = raids.contains(&crate::identity::raid_name(&csi.volume_handle));

            // Collapse visibility (7b-3 P1): a raid previously observed
            // present that vanishes under a live attachment is announced
            // on the FIRST strike — the 3-strike cadence below only gates
            // repair and layer-3 flagging, and layer-2 repair winning that
            // race used to make a total outage silent.
            {
                let mut seen = self.data_path_raid_seen.lock().await;
                let mut warned = self.data_path_warned.lock().await;
                match raid_collapse_verdict(
                    attached,
                    raid_present,
                    seen.contains(&pv_name),
                    warned.contains(&pv_name),
                ) {
                    CollapseEvent::Lost => {
                        warned.insert(pv_name.clone());
                        warn!(volume_id = %pv_name, "[DATA_PATH] Raid bdev vanished under a live attachment — data path lost, in-place repair pending confirmation");
                        self.emit_pv_event(
                            &pv_name,
                            "Warning",
                            "VolumeDataPathLost",
                            &format!(
                                "The raid bdev for this attached volume vanished on {} — the \
                                 mounted filesystem is failing I/O. In-place repair runs after \
                                 confirmation strikes; if the workload keeps erroring after \
                                 VolumeDataPathRepaired, bounce it to remount.",
                                self.node_name
                            ),
                        )
                        .await;
                    }
                    CollapseEvent::Restored => {
                        warned.remove(&pv_name);
                        if !flagged_by_me {
                            self.emit_pv_event(
                                &pv_name,
                                "Normal",
                                "VolumeDataPathRestored",
                                &format!(
                                    "The raid bdev for this volume is back on {} (repaired or \
                                     reassembled before escalation)",
                                    self.node_name
                                ),
                            )
                            .await;
                        }
                    }
                    CollapseEvent::None => {}
                }
                if raid_present {
                    seen.insert(pv_name.clone());
                } else if !attached {
                    // Unstaged: a later re-stage legitimately precedes its
                    // raid again — forget, or we would false-positive.
                    seen.remove(&pv_name);
                    warned.remove(&pv_name);
                }
            }

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
                        // Repaired closes the warned episode too — without
                        // this the next tick tacks on a redundant
                        // VolumeDataPathRestored (observed in the live
                        // validation).
                        self.data_path_warned.lock().await.remove(&pv_name);
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
        let raid_name = crate::identity::raid_name(volume_id);
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
        let nqn = crate::identity::replica_export_nqn(volume_id, replica_index);

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

        // NOTE: this is the MULTI-replica export path (ns_identity: None,
        // consumed by a remote SPDK raid bdev). It is NOT tracked in the #1
        // registry — the loss-detector re-exports with kernel-facing
        // stable-ns semantics, wrong here; the 60s reconcile_replica_targets
        // owns re-export for replica volumes.

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
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct DiskSetupRequest {
    pub disks: Option<Vec<String>>,           // For /api/disks/setup
    pub pci_addresses: Option<Vec<String>>,   // For /api/disks/initialize
    pub force_unmount: Option<bool>,
    pub backup_data: Option<bool>,
}

/// Disk row served by POST /api/disks/status — the Disk Setup tab's source.
/// Several fields are fixed compatibility values the agent fabricates for the
/// frontend (vendor ids, empty serial, numa 0); typed here so the generated
/// OpenAPI spec documents what is actually sent.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct NodeDiskStatus {
    pub pci_address: String,
    pub device_name: String,
    pub size_bytes: u64,
    pub model: String,
    pub healthy: bool,
    pub vendor_id: String,
    pub device_id: String,
    pub subsystem_vendor_id: String,
    pub subsystem_device_id: String,
    pub numa_node: u32,
    pub driver: String,
    pub serial: String,
    pub firmware_version: String,
    pub namespace_id: u32,
    pub mounted_partitions: Vec<String>,
    pub filesystem_type: Option<String>,
    pub is_system_disk: bool,
    pub spdk_ready: bool,
    pub driver_ready: bool,
    pub blobstore_initialized: bool,
    pub discovered_at: String,
    pub free_space: u64,
    pub temperature: Option<f64>,
    pub error_count: u64,
}

/// Disk row served by POST /api/disks/uninitialized — same compatibility
/// shape minus the status-only telemetry fields.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct NodeDiskListing {
    pub pci_address: String,
    pub device_name: String,
    pub size_bytes: u64,
    pub model: String,
    pub healthy: bool,
    pub vendor_id: String,
    pub device_id: String,
    pub subsystem_vendor_id: String,
    pub subsystem_device_id: String,
    pub numa_node: u32,
    pub driver: String,
    pub serial: String,
    pub firmware_version: String,
    pub namespace_id: u32,
    pub mounted_partitions: Vec<String>,
    pub filesystem_type: Option<String>,
    pub is_system_disk: bool,
    pub spdk_ready: bool,
    pub driver_ready: bool,
    pub blobstore_initialized: bool,
    pub discovered_at: String,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct NodeDisksStatusResponse {
    pub node: String,
    pub disks: Vec<NodeDiskStatus>,
    pub last_updated: String,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct UninitializedDisksResponse {
    pub success: bool,
    pub node: String,
    pub uninitialized_disks: Vec<NodeDiskListing>,
    pub count: usize,
}

/// Per-disk outcome shape for setup/initialize/reset. `failed_disks` holds
/// PCI addresses only — human-readable causes ride in `warnings`.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct DiskSetupResponse {
    pub success: bool,
    pub setup_disks: Vec<String>,
    pub failed_disks: Vec<String>,
    pub warnings: Vec<String>,
    pub completed_at: String,
}

/// Agent-level failure envelope (also produced by the dashboard proxy when
/// an agent is unreachable).
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct NodeAgentError {
    pub success: bool,
    pub error: String,
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
            data_path_raid_seen: self.data_path_raid_seen.clone(),
            data_path_warned: self.data_path_warned.clone(),
            controller_reap_strikes: self.controller_reap_strikes.clone(),
            exported_targets: self.exported_targets.clone(),
            expected_ublk: self.expected_ublk.clone(),
            expected_remote: self.expected_remote.clone(),
        }
    }
}

// Request/Response types for HTTP API
#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeBlobstoreRequest {
    pub pci_address: String,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DeleteDiskRequest {
    pub pci_address: String,
}

/// Outcome of deleting a disk's LVS (the inverse of setup/initialize).
/// A refusal while lvols still exist comes back as 409 with the reason in
/// `error`; deleting an uninitialized disk is a successful no-op.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DiskDeleteResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub completed_at: String,
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

#[cfg(test)]
mod rehydrate_tests {
    use super::NodeAgent;
    use serde_json::json;

    fn sub(ns_bdev: Option<&str>, traddr: Option<&str>, hosts: &[&str], any_host: bool) -> serde_json::Value {
        json!({
            "nqn": "nqn.2024-11.com.flint:volume:pvc-x",
            "namespaces": ns_bdev.map(|b| vec![json!({"nsid": 1, "bdev_name": b, "uuid": "u"})]).unwrap_or_default(),
            "listen_addresses": traddr.map(|a| vec![json!({"trtype": "TCP", "traddr": a, "trsvcid": "4420"})]).unwrap_or_default(),
            "hosts": hosts.iter().map(|h| json!({"nqn": h})).collect::<Vec<_>>(),
            "allow_any_host": any_host,
        })
    }

    #[test]
    fn absent_subsystem_is_not_satisfied() {
        assert!(!NodeAgent::export_satisfied(None, None, "127.0.0.1", None));
    }

    #[test]
    fn complete_export_is_satisfied() {
        let s = sub(Some("lvol1"), Some("127.0.0.1"), &["hostA"], false);
        assert!(NodeAgent::export_satisfied(Some(&s), None, "127.0.0.1", Some("hostA")));
        assert!(NodeAgent::export_satisfied(Some(&s), Some("lvol1"), "127.0.0.1", Some("hostA")));
    }

    #[test]
    fn missing_namespace_or_listener_fails() {
        let no_ns = sub(None, Some("127.0.0.1"), &["hostA"], false);
        assert!(!NodeAgent::export_satisfied(Some(&no_ns), None, "127.0.0.1", Some("hostA")));
        let no_listener = sub(Some("lvol1"), None, &["hostA"], false);
        assert!(!NodeAgent::export_satisfied(Some(&no_listener), None, "127.0.0.1", Some("hostA")));
    }

    #[test]
    fn wrong_listener_addr_fails() {
        // A loopback-only export does NOT satisfy a cross-node consumer
        // that needs the node-IP listener (and vice versa).
        let s = sub(Some("lvol1"), Some("127.0.0.1"), &["hostA"], false);
        assert!(!NodeAgent::export_satisfied(Some(&s), None, "172.31.3.30", Some("hostA")));
    }

    #[test]
    fn wrong_fence_fails_but_any_host_passes() {
        // Fence still pointing at the previous consumer must trigger a
        // converge (the F8 cross-node move case).
        let s = sub(Some("lvol1"), Some("127.0.0.1"), &["hostOld"], false);
        assert!(!NodeAgent::export_satisfied(Some(&s), None, "127.0.0.1", Some("hostNew")));
        let open = sub(Some("lvol1"), Some("127.0.0.1"), &[], true);
        assert!(NodeAgent::export_satisfied(Some(&open), None, "127.0.0.1", Some("hostNew")));
    }

    #[test]
    fn wrong_bdev_fails() {
        // The namespace exists but points at a stale bdev (lvol was
        // re-created) — not satisfied, ensure_export must converge it.
        let s = sub(Some("stale"), Some("127.0.0.1"), &["hostA"], false);
        assert!(!NodeAgent::export_satisfied(Some(&s), Some("lvol1"), "127.0.0.1", Some("hostA")));
    }

    #[test]
    fn backend_selector_mirrors_create_block_device() {
        // create_block_device: "nvmeof" is the ONLY value selecting the
        // loopback backend; unset/anything-else means ublk.
        assert!(NodeAgent::backend_is_ublk_value(None));
        assert!(NodeAgent::backend_is_ublk_value(Some("ublk")));
        assert!(NodeAgent::backend_is_ublk_value(Some("garbage")));
        assert!(!NodeAgent::backend_is_ublk_value(Some("nvmeof")));
    }

    #[test]
    fn parse_ublk_disks_reads_rpc_shape() {
        // Real `ublk_get_disks` items carry `id` + `bdev_name` (+ extras).
        let r = json!([
            {"ublk_device": "/dev/ublkb7", "id": 7, "queue_depth": 512, "num_queues": 4, "bdev_name": "lvolA"},
            {"ublk_device": "/dev/ublkb9", "id": 9, "bdev_name": "nvme_x_n1"}
        ]);
        let m = NodeAgent::parse_ublk_disks(Some(&r));
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&7).map(String::as_str), Some("lvolA"));
        assert_eq!(m.get(&9).map(String::as_str), Some("nvme_x_n1"));
    }

    #[test]
    fn parse_ublk_disks_accepts_config_dump_spelling_and_skips_partials() {
        // Config-dump entries say `ublk_id`; entries missing an id or bdev
        // must be skipped, not panic or map to junk.
        let r = json!([
            {"ublk_id": 3, "bdev_name": "b3"},
            {"id": 4},
            {"bdev_name": "orphan"}
        ]);
        let m = NodeAgent::parse_ublk_disks(Some(&r));
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&3).map(String::as_str), Some("b3"));
    }

    #[test]
    fn parse_ublk_disks_tolerates_absent_or_non_array() {
        assert!(NodeAgent::parse_ublk_disks(None).is_empty());
        let not_array = json!({"unexpected": true});
        assert!(NodeAgent::parse_ublk_disks(Some(&not_array)).is_empty());
    }
}
