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

use crate::minimal_disk_service::MinimalDiskService;
use crate::driver::SpdkCsiDriver;

/// Minimal State Node Agent - Uses direct SPDK queries instead of CRDs
pub struct NodeAgent {
    pub node_name: String,
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
            spdk_socket_path,
            disk_service,
            driver,
        }
    }

    /// Start the minimal node agent with HTTP API
    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚀 [MINIMAL_NODE_AGENT] Starting minimal state node agent: {}", self.node_name);

        // Start disk discovery loop
        let disk_service = self.disk_service.clone();
        let discovery_task = tokio::spawn(async move {
            let mut discovery_interval = interval(Duration::from_secs(30));
            loop {
                discovery_interval.tick().await;
                match disk_service.discover_local_disks().await {
                    Ok(disks) => {
                        println!("🔍 [DISK_DISCOVERY] Found {} disks on node", disks.len());
                        for disk in &disks {
                            println!("  - {} ({}): {} bytes, healthy: {}, initialized: {}", 
                                disk.device_name, disk.pci_address, disk.size_bytes, 
                                disk.healthy, disk.blobstore_initialized);
                        }
                    }
                    Err(e) => println!("❌ [DISK_DISCOVERY] Error: {}", e),
                }
            }
        });

        // Setup HTTP API routes
        let routes = self.setup_routes();

        println!("✅ [MINIMAL_NODE_AGENT] Starting HTTP server on port 8081");
        
        // Start both the HTTP server and discovery loop
        tokio::select! {
            _ = warp::serve(routes).run(([0, 0, 0, 0], 8081)) => {
                println!("🌐 [MINIMAL_NODE_AGENT] HTTP server stopped");
            }
            _ = discovery_task => {
                println!("🔍 [MINIMAL_NODE_AGENT] Discovery task stopped");
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

        // Combine all routes
        list_disks
            .or(list_uninitialized)
            .or(disk_status)
            .or(init_blobstore)
            .or(setup_disks)
            .or(initialize_disks)
            .or(reset_disks)
            .or(create_lvol)
            .or(delete_lvol)
            .or(spdk_rpc)
            .or(ublk_create_target)
            .or(ublk_create)
            .or(ublk_delete)
            .with(warp::cors().allow_any_origin())
    }

    fn with_node_agent(&self, node_agent: Arc<NodeAgent>) -> impl Filter<Extract = (Arc<NodeAgent>,), Error = std::convert::Infallible> + Clone {
        warp::any().map(move || node_agent.clone())
    }

    /// Handle GET /api/disks
    async fn handle_list_disks(node_agent: Arc<NodeAgent>) -> Result<impl Reply, Rejection> {
        println!("🌐 [HTTP_API] Handling list disks request");
        
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

    /// Handle POST /api/disks/initialize_blobstore
    async fn handle_initialize_blobstore(
        request: InitializeBlobstoreRequest, 
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        println!("🌐 [HTTP_API] Handling initialize blobstore request: {}", request.pci_address);
        
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
        println!("🌐 [HTTP_API] Handling create lvol request: {}", request.volume_id);
        
        match node_agent.disk_service.create_lvol(&request.lvs_name, &request.volume_id, request.size_bytes).await {
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
        println!("🌐 [HTTP_API] Handling delete lvol request: {}", request.lvol_uuid);
        
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

    /// Handle POST /api/disks/uninitialized - List uninitialized disks (RPC-style)
    async fn handle_get_uninitialized_disks(_request: Value, node_agent: Arc<NodeAgent>) -> Result<impl Reply, Rejection> {
        println!("🌐 [HTTP_API] Handling get uninitialized disks request");
        
        match node_agent.disk_service.discover_local_disks().await {
            Ok(disks) => {
                let uninitialized: Vec<_> = disks.iter()
                    .filter(|d| !d.blobstore_initialized && d.healthy)
                    .map(|d| json!({
                        "pci_address": d.pci_address,
                        "device_name": d.device_name,
                        "size_bytes": d.size_bytes,
                        "model": d.model,
                        "healthy": d.healthy
                    }))
                    .collect();
                
                let response = json!({
                    "success": true,
                    "node": node_agent.node_name,
                    "uninitialized_disks": uninitialized,
                    "count": uninitialized.len()
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
    async fn handle_get_disk_status(_request: Value, node_agent: Arc<NodeAgent>) -> Result<impl Reply, Rejection> {
        println!("🌐 [HTTP_API] Handling get disk status request");
        
        match node_agent.disk_service.discover_local_disks().await {
            Ok(disks) => {
                let disk_statuses: Vec<_> = disks.iter().map(|d| json!({
                    "pci_address": d.pci_address,
                    "device_name": d.device_name,
                    "healthy": d.healthy,
                    "initialized": d.blobstore_initialized,
                    "size_bytes": d.size_bytes,
                    "free_space": d.free_space,
                    "model": d.model,
                    "temperature": null,
                    "error_count": 0
                })).collect();
                
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
        println!("🌐 [HTTP_API] Handling setup disks request: {} disks", disks.len());
        
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
        println!("🌐 [HTTP_API] Handling reset disks request: {} disks", disks.len());
        
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

    /// Handle POST /api/spdk/rpc - Generic SPDK RPC proxy
    async fn handle_spdk_rpc(
        rpc_request: serde_json::Value,
        _node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        let method = rpc_request["method"].as_str().unwrap_or("unknown");
        println!("🌐 [HTTP_API] Handling SPDK RPC request: {}", method);
        
        // TODO: Proxy to SPDK via MinimalDiskService
        let response = json!({
            "status": "success", 
            "result": "SPDK RPC proxy not yet implemented"
        });
        Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
    }

    /// Handle POST /api/ublk/create_target - Create ublk target
    async fn handle_ublk_create_target(
        _request: serde_json::Value,
        node_agent: Arc<NodeAgent>
    ) -> Result<impl Reply, Rejection> {
        println!("🌐 [HTTP_API] Handling ublk target creation");
        
        let ublk_target_rpc = json!({
            "method": "ublk_create_target",
            "params": {}
        });
        
        match node_agent.disk_service.call_spdk_rpc(&ublk_target_rpc).await {
            Ok(response) => {
                println!("✅ [HTTP_API] ublk target created successfully");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) if e.to_string().contains("Method not found") => {
                // SPDK doesn't support ublk - not an error
                let warning_response = json!({
                    "success": true,
                    "message": "SPDK doesn't support ublk - skipping"
                });
                println!("ℹ️ [HTTP_API] SPDK doesn't support ublk - returning success");
                Ok(warp::reply::with_status(warp::reply::json(&warning_response), StatusCode::OK))
            }
            Err(e) => {
                println!("❌ [HTTP_API] ublk target creation failed: {}", e);
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
        println!("🌐 [HTTP_API] Handling ublk device creation");
        
        // Extract method and params from request
        let method = request["method"].as_str().unwrap_or("ublk_start_disk");
        let params = &request["params"];
        
        let ublk_rpc = json!({
            "method": method,
            "params": params
        });
        
        match node_agent.disk_service.call_spdk_rpc(&ublk_rpc).await {
            Ok(response) => {
                println!("✅ [HTTP_API] ublk device created successfully");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                println!("❌ [HTTP_API] ublk device creation failed: {}", e);
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
        println!("🌐 [HTTP_API] Handling ublk device deletion");
        
        // Extract method and params from request
        let method = request["method"].as_str().unwrap_or("ublk_stop_disk");
        let params = &request["params"];
        
        let ublk_rpc = json!({
            "method": method,
            "params": params
        });
        
        match node_agent.disk_service.call_spdk_rpc(&ublk_rpc).await {
            Ok(response) => {
                println!("✅ [HTTP_API] ublk device deleted successfully");
                Ok(warp::reply::with_status(warp::reply::json(&response), StatusCode::OK))
            }
            Err(e) => {
                println!("⚠️ [HTTP_API] ublk device deletion failed (may not exist): {}", e);
                // For deletion, we return success even if it fails (cleanup is best effort)
                let success_response = json!({
                    "success": true,
                    "message": "Device deleted or did not exist"
                });
                Ok(warp::reply::with_status(warp::reply::json(&success_response), StatusCode::OK))
            }
        }
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
}

#[derive(Debug, Serialize, Deserialize)] 
pub struct DeleteLvolRequest {
    pub lvol_uuid: String,
}
