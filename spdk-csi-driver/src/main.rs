// main.rs - Entry point for Minimal State SPDK CSI Driver
use std::sync::Arc;
use tonic::transport::Server;
use kube::Client;
use warp::Filter;

// Import minimal state components from library
use spdk_csi_driver::node_agent::NodeAgent;
use spdk_csi_driver::driver::SpdkCsiDriver;
use spdk_csi_driver::spdk_dashboard_backend_minimal::start_minimal_dashboard_backend;

// Use the CSI protobuf types from lib.rs instead of duplicating them
// This avoids the tonic::include_proto! macro issue

use spdk_csi_driver::csi::{
    controller_server::ControllerServer,
    identity_server::IdentityServer,
    node_server::NodeServer,
};

/// Simple health check endpoint for Kubernetes liveness probes
async fn start_health_server() {
    let health = warp::path("healthz")
        .and(warp::get())
        .map(move || {
            // Simple health check - always return OK for liveness probe
            // The fact that the container is running means it's healthy
            warp::reply::with_status("OK", warp::http::StatusCode::OK)
        });

    let health_port = std::env::var("HEALTH_PORT")
        .unwrap_or("9809".to_string())
        .parse()
        .unwrap_or(9809);
    
    println!("Starting health server on port {}", health_port);
    warp::serve(health)
        .run(([0, 0, 0, 0], health_port))
        .await;
}

/// Get the current pod's namespace from the service account token
async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error>> {
    // Try environment variable first (allows override)
    if let Ok(namespace) = std::env::var("FLINT_NAMESPACE") {
        return Ok(namespace);
    }
    
    // Read namespace from service account token file
    let namespace_path = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
    if std::path::Path::new(namespace_path).exists() {
        match tokio::fs::read_to_string(namespace_path).await {
            Ok(namespace) => {
                let namespace = namespace.trim().to_string();
                println!("📍 [NAMESPACE] Detected current namespace: {}", namespace);
                return Ok(namespace);
            }
            Err(e) => {
                println!("⚠️ [NAMESPACE] Failed to read namespace file: {}", e);
            }
        }
    }
    
    // Fallback to default if running outside cluster
    println!("⚠️ [NAMESPACE] Using fallback namespace: flint-system");
    Ok("flint-system".to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kube_client = Client::try_default().await?;
    let node_id = std::env::var("NODE_ID")
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or("unknown-node".to_string()));
    
    // Detect the namespace for custom resources
    let target_namespace = get_current_namespace().await?;
    
    let spdk_socket_path = std::env::var("SPDK_RPC_URL").unwrap_or("unix:///var/tmp/spdk.sock".to_string());
    
    // Create minimal state driver
    let driver = Arc::new(SpdkCsiDriver::new(
        kube_client.clone(),
        target_namespace.clone(),
        node_id.clone(),
        spdk_socket_path.clone(),
        "tcp".to_string(), // nvmeof_transport
        4420, // nvmeof_target_port
    ));
    
    println!("🎯 [CONFIG] Using namespace for custom resources: {}", driver.target_namespace);
    
    // Start health server for Kubernetes liveness probes
    tokio::spawn(async move {
        start_health_server().await;
    });

    // Start dashboard backend (if enabled)
    let enable_dashboard = std::env::var("ENABLE_DASHBOARD")
        .unwrap_or("false".to_string())
        .parse()
        .unwrap_or(false);
    
    if enable_dashboard {
        let dashboard_port = std::env::var("DASHBOARD_PORT")
            .unwrap_or("8080".to_string())
            .parse()
            .unwrap_or(8080);
        
        println!("📊 [DASHBOARD] Starting minimal dashboard backend on port {}", dashboard_port);
        tokio::spawn(async move {
            if let Err(e) = start_minimal_dashboard_backend(dashboard_port).await {
                eprintln!("❌ [DASHBOARD] Failed to start: {}", e);
            }
        });
    }
    
    let mode = std::env::var("CSI_MODE").unwrap_or("all".to_string());
    let endpoint = std::env::var("CSI_ENDPOINT")
        .unwrap_or("unix:///csi/csi.sock".to_string());
    
    // Start node agent (if in node mode)
    if mode == "node" || mode == "all" {
        let node_agent = Arc::new(NodeAgent::new(
            node_id.clone(),
            spdk_socket_path.clone(),
            driver.clone(),
        ));
        
        println!("🔧 [NODE_AGENT] Starting node agent on port 8081");
        let node_agent_clone = node_agent.clone();
        tokio::spawn(async move {
            if let Err(e) = node_agent_clone.start().await {
                eprintln!("❌ [NODE_AGENT] Failed to start: {}", e);
            }
        });
    }
    
    // Create minimal CSI services
    let identity_service = MinimalIdentityService::new(driver.clone());
    let controller_service = MinimalControllerService::new(driver.clone());
    let node_service = MinimalNodeService::new(driver.clone());
    
    // Build the router with services
    let mut router = Server::builder()
        .add_service(IdentityServer::new(identity_service));
    
    if mode == "controller" || mode == "all" {
        println!("Starting in Controller mode...");
        router = router.add_service(ControllerServer::new(controller_service));
    }
    
    if mode == "node" || mode == "all" {
        println!("Starting in Node mode...");
        router = router.add_service(NodeServer::new(node_service));
    }
    
    println!("✅ [CSI] Minimal State SPDK CSI Driver ('{}' mode) starting on {} for node {}", mode, endpoint, node_id);
    
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
        println!("Listening on TCP address: {}", addr);
        router.serve(addr).await?;
        
    } else {
        // Assume it's a direct address (e.g., "0.0.0.0:50051")
        let addr = endpoint.parse()?;
        println!("Listening on address: {}", addr);
        router.serve(addr).await?;
    }
    
    Ok(())
}

/// Minimal Identity Service Implementation
struct MinimalIdentityService {
    driver: Arc<SpdkCsiDriver>,
}

impl MinimalIdentityService {
    fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }
}

#[tonic::async_trait]
impl spdk_csi_driver::csi::identity_server::Identity for MinimalIdentityService {
    async fn get_plugin_info(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::GetPluginInfoRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::GetPluginInfoResponse>, tonic::Status> {
        Ok(tonic::Response::new(spdk_csi_driver::csi::GetPluginInfoResponse {
            name: "flint.csi.storage.io".to_string(),
            vendor_version: env!("CARGO_PKG_VERSION").to_string(),
            manifest: std::collections::HashMap::new(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::GetPluginCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::GetPluginCapabilitiesResponse>, tonic::Status> {
        use spdk_csi_driver::csi::{plugin_capability::service::Type as ServiceType, PluginCapability, plugin_capability::Service};
        
        let capabilities = vec![
            PluginCapability {
                r#type: Some(spdk_csi_driver::csi::plugin_capability::Type::Service(Service {
                    r#type: ServiceType::ControllerService as i32,
                })),
            },
        ];
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::GetPluginCapabilitiesResponse { capabilities }))
    }

    async fn probe(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ProbeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ProbeResponse>, tonic::Status> {
        Ok(tonic::Response::new(spdk_csi_driver::csi::ProbeResponse { ready: Some(true) }))
    }
}

/// Minimal Controller Service Implementation  
struct MinimalControllerService {
    driver: Arc<SpdkCsiDriver>,
}

impl MinimalControllerService {
    fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }
}

#[tonic::async_trait]
impl spdk_csi_driver::csi::controller_server::Controller for MinimalControllerService {
    async fn create_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::CreateVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::CreateVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        println!("🔧 [CONTROLLER] Creating volume: {}", req.name);
        
        // TODO: Implement minimal state volume creation via node agents
        Err(tonic::Status::unimplemented("Volume creation not yet implemented in minimal state"))
    }

    async fn delete_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::DeleteVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::DeleteVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        println!("🗑️ [CONTROLLER] Deleting volume: {}", req.volume_id);
        
        // TODO: Implement minimal state volume deletion via node agents
        Ok(tonic::Response::new(spdk_csi_driver::csi::DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerPublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerPublishVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Controller publish volume not implemented"))
    }

    async fn controller_unpublish_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerUnpublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerUnpublishVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Controller unpublish volume not implemented"))
    }

    async fn validate_volume_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ValidateVolumeCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ValidateVolumeCapabilitiesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Validate volume capabilities not implemented"))
    }

    async fn list_volumes(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ListVolumesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ListVolumesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("List volumes not implemented"))
    }

    async fn get_capacity(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::GetCapacityRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::GetCapacityResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Get capacity not implemented"))
    }

    async fn controller_get_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerGetCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerGetCapabilitiesResponse>, tonic::Status> {
        use spdk_csi_driver::csi::{controller_service_capability::rpc::Type as RpcType, ControllerServiceCapability, controller_service_capability::Rpc};
        
        let capabilities = vec![
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::CreateDeleteVolume as i32,
                })),
            },
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::CreateDeleteSnapshot as i32,
                })),
            },
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::ExpandVolume as i32,
                })),
            },
        ];
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerGetCapabilitiesResponse { capabilities }))
    }

    async fn create_snapshot(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::CreateSnapshotRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::CreateSnapshotResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Create snapshot not implemented"))
    }

    async fn delete_snapshot(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::DeleteSnapshotRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::DeleteSnapshotResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Delete snapshot not implemented"))
    }

    async fn list_snapshots(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ListSnapshotsRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ListSnapshotsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("List snapshots not implemented"))
    }

    async fn controller_expand_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerExpandVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerExpandVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Controller expand volume not implemented"))
    }

    async fn controller_get_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerGetVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerGetVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Controller get volume not implemented"))
    }

    async fn controller_modify_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerModifyVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerModifyVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Controller modify volume not implemented"))
    }
}

/// Minimal Node Service Implementation
struct MinimalNodeService {
    driver: Arc<SpdkCsiDriver>,
}

impl MinimalNodeService {
    fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }
}

#[tonic::async_trait]
impl spdk_csi_driver::csi::node_server::Node for MinimalNodeService {
    async fn node_stage_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeStageVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeStageVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Node stage volume not implemented"))
    }

    async fn node_unstage_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeUnstageVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeUnstageVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Node unstage volume not implemented"))
    }

    async fn node_publish_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodePublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodePublishVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Node publish volume not implemented"))
    }

    async fn node_unpublish_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeUnpublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeUnpublishVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Node unpublish volume not implemented"))
    }

    async fn node_get_volume_stats(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetVolumeStatsRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetVolumeStatsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Node get volume stats not implemented"))
    }

    async fn node_expand_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeExpandVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeExpandVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Node expand volume not implemented"))
    }

    async fn node_get_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetCapabilitiesResponse>, tonic::Status> {
        use spdk_csi_driver::csi::{node_service_capability::rpc::Type as RpcType, NodeServiceCapability, node_service_capability::Rpc};
        
        let capabilities = vec![
            NodeServiceCapability {
                r#type: Some(spdk_csi_driver::csi::node_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::StageUnstageVolume as i32,
                })),
            },
        ];
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::NodeGetCapabilitiesResponse { capabilities }))
    }

    async fn node_get_info(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetInfoRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetInfoResponse>, tonic::Status> {
        Ok(tonic::Response::new(spdk_csi_driver::csi::NodeGetInfoResponse {
            node_id: self.driver.node_id.clone(),
            max_volumes_per_node: 0, // 0 means unlimited
            accessible_topology: None,
        }))
    }
}
