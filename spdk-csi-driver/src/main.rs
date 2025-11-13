// main.rs - Entry point for Minimal State SPDK CSI Driver
use std::sync::Arc;
use tonic::transport::Server;
use kube::Client;
use warp::Filter;

// Import minimal state components from library
use spdk_csi_driver::node_agent::NodeAgent;
use spdk_csi_driver::driver::{SpdkCsiDriver, NvmeofConnectionInfo};
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
        println!("🔵 [GRPC] Identity.GetPluginInfo called");
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
        println!("🔵 [GRPC] Identity.GetPluginCapabilities called");
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
        println!("🔵 [GRPC] Identity.Probe called");
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
        let volume_id = req.name.clone();
        println!("🎯 [CONTROLLER] Creating volume: {}", volume_id);

        // Extract parameters
        let size_bytes = req.capacity_range
            .and_then(|cr| if cr.required_bytes > 0 { Some(cr.required_bytes) } else { Some(cr.limit_bytes) })
            .unwrap_or(1024 * 1024 * 1024) as u64; // Default 1GB

        let replica_count = req.parameters.get("numReplicas")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);

        println!("📊 [CONTROLLER] Volume {} - Size: {} bytes, Replicas: {}", volume_id, size_bytes, replica_count);

        // Call the driver's create volume method 
        match self.driver.create_volume(&volume_id, size_bytes, replica_count).await {
            Ok(_volume_info) => {
                println!("✅ [CONTROLLER] Volume {} created successfully", volume_id);
                
                let response = spdk_csi_driver::csi::CreateVolumeResponse {
                    volume: Some(spdk_csi_driver::csi::Volume {
                        volume_id: volume_id.clone(),
                        capacity_bytes: size_bytes as i64,
                        volume_context: std::collections::HashMap::new(),
                        content_source: None,
                        accessible_topology: vec![],
                    }),
                };
                Ok(tonic::Response::new(response))
            }
            Err(e) => {
                println!("❌ [CONTROLLER] Volume creation failed: {}", e);
                Err(tonic::Status::internal(format!("Volume creation failed: {}", e)))
            }
        }
    }

    async fn delete_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::DeleteVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::DeleteVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        
        println!("🗑️ [CONTROLLER] Deleting volume: {}", volume_id);
        
        // Get volume information to know which node it's on
        let volume_info = match self.driver.get_volume_info(&volume_id).await {
            Ok(info) => info,
            Err(e) => {
                println!("⚠️ [CONTROLLER] Volume not found (may already be deleted): {}", e);
                // Not an error - idempotent delete
                return Ok(tonic::Response::new(spdk_csi_driver::csi::DeleteVolumeResponse {}));
            }
        };

        println!("📊 [CONTROLLER] Deleting volume on node: {}", volume_info.node_name);

        // Delete the logical volume on the storage node
        match self.driver.delete_lvol(&volume_info.node_name, &volume_info.lvol_uuid).await {
            Ok(_) => {
                println!("✅ [CONTROLLER] Logical volume deleted successfully");
            }
            Err(e) => {
                println!("❌ [CONTROLLER] Failed to delete logical volume: {}", e);
                return Err(tonic::Status::internal(format!("Failed to delete volume: {}", e)));
            }
        }

        // Clean up any NVMe-oF targets that might still exist
        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
        if let Err(e) = self.driver.remove_nvmeof_target(&volume_info.node_name, &nqn).await {
            println!("⚠️ [CONTROLLER] Failed to remove NVMe-oF target (may not exist): {}", e);
            // Continue anyway - best effort cleanup
        }

        println!("✅ [CONTROLLER] Volume {} deleted successfully", volume_id);
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::ControllerPublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerPublishVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let node_id = req.node_id.clone();
        
        println!("📤 [CONTROLLER] Publishing volume {} to node {}", volume_id, node_id);

        // Get volume information (which node it's on)
        let volume_info = match self.driver.get_volume_info(&volume_id).await {
            Ok(info) => info,
            Err(e) => {
                println!("❌ [CONTROLLER] Failed to get volume info: {}", e);
                return Err(tonic::Status::not_found(format!("Volume not found: {}", e)));
            }
        };

        println!("📊 [CONTROLLER] Volume {} is on node {}", volume_id, volume_info.node_name);

        let mut publish_context = std::collections::HashMap::new();
        
        // Check if pod is on the same node as the logical volume
        if volume_info.node_name == node_id {
            println!("✅ [CONTROLLER] Volume is local to node - no NVMe-oF needed");
            
            // Store volume info in publish context for NodeStage
            publish_context.insert("volumeType".to_string(), "local".to_string());
            publish_context.insert("bdevName".to_string(), volume_info.lvol_uuid.clone());
            publish_context.insert("lvsName".to_string(), volume_info.lvs_name.clone());
        } else {
            println!("🌐 [CONTROLLER] Volume is remote - setting up NVMe-oF");
            
            // Construct bdev name for lvol
            let bdev_name = volume_info.lvol_uuid.clone();
            
            // Setup NVMe-oF target on the node hosting the logical volume
            let conn_info = match self.driver.setup_nvmeof_target_on_node(
                &volume_info.node_name,
                &bdev_name,
                &volume_id
            ).await {
                Ok(info) => info,
                Err(e) => {
                    println!("❌ [CONTROLLER] Failed to setup NVMe-oF target: {}", e);
                    return Err(tonic::Status::internal(format!("Failed to setup NVMe-oF: {}", e)));
                }
            };

            println!("✅ [CONTROLLER] NVMe-oF target ready: {}", conn_info.nqn);

            // Store connection info in publish context for NodeStage
            publish_context.insert("volumeType".to_string(), "remote".to_string());
            publish_context.insert("nqn".to_string(), conn_info.nqn.clone());
            publish_context.insert("targetIp".to_string(), conn_info.target_ip.clone());
            publish_context.insert("targetPort".to_string(), conn_info.target_port.to_string());
            publish_context.insert("transport".to_string(), conn_info.transport.clone());
            publish_context.insert("storageNode".to_string(), volume_info.node_name.clone());
        }

        publish_context.insert("volumeId".to_string(), volume_id.clone());

        println!("✅ [CONTROLLER] Volume {} published successfully", volume_id);
        
        let response = spdk_csi_driver::csi::ControllerPublishVolumeResponse {
            publish_context,
        };
        
        Ok(tonic::Response::new(response))
    }

    async fn controller_unpublish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::ControllerUnpublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerUnpublishVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let node_id = req.node_id.clone();
        
        println!("📥 [CONTROLLER] Unpublishing volume {} from node {:?}", volume_id, node_id);

        // Get volume information
        let volume_info = match self.driver.get_volume_info(&volume_id).await {
            Ok(info) => info,
            Err(e) => {
                println!("⚠️ [CONTROLLER] Volume not found (may already be deleted): {}", e);
                // Not an error - volume might already be deleted
                return Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerUnpublishVolumeResponse {}));
            }
        };

        // If node_id is specified and volume is remote, we need to cleanup
        if !node_id.is_empty() {
            if volume_info.node_name != node_id {
                println!("🧹 [CONTROLLER] Volume is remote - cleaning up NVMe-oF connections");
                
                let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
                
                // Disconnect from NVMe-oF target on the node where pod was running
                // Note: We need to create a temporary driver instance for the target node
                // For now, we'll use the controller's node_id since this is a cleanup operation
                println!("🔌 [CONTROLLER] Note: NVMe disconnection handled by NodeUnpublish on node {}", node_id);
                
                // Remove the NVMe-oF target from the storage node
                if let Err(e) = self.driver.remove_nvmeof_target(&volume_info.node_name, &nqn).await {
                    println!("⚠️ [CONTROLLER] Failed to remove NVMe-oF target (continuing): {}", e);
                }
            } else {
                println!("ℹ️ [CONTROLLER] Volume is local - no NVMe-oF cleanup needed");
            }
        }

        println!("✅ [CONTROLLER] Volume {} unpublished successfully", volume_id);
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerUnpublishVolumeResponse {}))
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
        println!("🔵 [GRPC] Controller.ControllerGetCapabilities called");
        use spdk_csi_driver::csi::{controller_service_capability::rpc::Type as RpcType, ControllerServiceCapability, controller_service_capability::Rpc};
        
        let capabilities = vec![
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::CreateDeleteVolume as i32,
                })),
            },
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::PublishUnpublishVolume as i32,
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
        request: tonic::Request<spdk_csi_driver::csi::NodeStageVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeStageVolumeResponse>, tonic::Status> {
        println!("🔵 [GRPC] *** Node.NodeStageVolume CALLED ***");
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let staging_target_path = req.staging_target_path.clone();
        let publish_context = req.publish_context.clone();
        
        println!("📦 [NODE] Staging volume {} at {}", volume_id, staging_target_path);

        // Get volume type from publish context
        let volume_type = publish_context.get("volumeType")
            .ok_or_else(|| tonic::Status::invalid_argument("No volumeType in publish context"))?;

        let bdev_name = if volume_type == "local" {
            // Local volume - bdev is the lvol UUID
            let bdev = publish_context.get("bdevName")
                .ok_or_else(|| tonic::Status::invalid_argument("No bdevName in publish context"))?;
            println!("✅ [NODE] Local volume - using bdev: {}", bdev);
            bdev.clone()
        } else if volume_type == "remote" {
            // Remote volume - need to connect to NVMe-oF target first
            println!("🌐 [NODE] Remote volume - connecting to NVMe-oF target");
            
            let nqn = publish_context.get("nqn")
                .ok_or_else(|| tonic::Status::invalid_argument("No nqn in publish context"))?;
            let target_ip = publish_context.get("targetIp")
                .ok_or_else(|| tonic::Status::invalid_argument("No targetIp in publish context"))?;
            let target_port = publish_context.get("targetPort")
                .ok_or_else(|| tonic::Status::invalid_argument("No targetPort in publish context"))?
                .parse::<u16>()
                .map_err(|e| tonic::Status::invalid_argument(format!("Invalid targetPort: {}", e)))?;
            let transport = publish_context.get("transport")
                .ok_or_else(|| tonic::Status::invalid_argument("No transport in publish context"))?;

            let conn_info = NvmeofConnectionInfo {
                nqn: nqn.clone(),
                target_ip: target_ip.clone(),
                target_port,
                transport: transport.clone(),
            };

            // Connect to NVMe-oF target
            match self.driver.connect_to_nvmeof_target(&conn_info).await {
                Ok(bdev) => {
                    println!("✅ [NODE] Connected to NVMe-oF target, bdev: {}", bdev);
                    bdev
                }
                Err(e) => {
                    println!("❌ [NODE] Failed to connect to NVMe-oF target: {}", e);
                    return Err(tonic::Status::internal(format!("Failed to connect to NVMe-oF: {}", e)));
                }
            }
        } else {
            return Err(tonic::Status::invalid_argument(format!("Unknown volume type: {}", volume_type)));
        };

        // Now create ublk device from the bdev
        println!("🔧 [NODE] Creating ublk device for bdev: {}", bdev_name);
        
        let ublk_id = self.driver.generate_ublk_id(&volume_id);
        
        match self.driver.create_ublk_device(&bdev_name, ublk_id).await {
            Ok(device_path) => {
                println!("✅ [NODE] ublk device created: {}", device_path);
                
                // Create staging directory if it doesn't exist
                if let Err(e) = std::fs::create_dir_all(&staging_target_path) {
                    println!("⚠️ [NODE] Failed to create staging directory (may exist): {}", e);
                }

                // For filesystem volumes, format and mount the device
                // Check if this is a filesystem volume by looking at volume_capability
                if let Some(volume_capability) = req.volume_capability {
                    if let Some(access_type) = volume_capability.access_type {
                        match access_type {
                            spdk_csi_driver::csi::volume_capability::AccessType::Mount(mount_config) => {
                                let fs_type = if mount_config.fs_type.is_empty() {
                                    "ext4".to_string()
                                } else {
                                    mount_config.fs_type
                                };
                                
                                println!("📁 [NODE] Formatting device {} with {} filesystem", device_path, fs_type);
                                
                                // Check if device is already formatted
                                let blkid_output = std::process::Command::new("blkid")
                                    .arg(&device_path)
                                    .output()
                                    .map_err(|e| tonic::Status::internal(format!("Failed to check filesystem: {}", e)))?;
                                
                                if !blkid_output.status.success() {
                                    // Device not formatted, format it
                                    println!("🔧 [NODE] Formatting device with mkfs.{}", fs_type);
                                    let mkfs_output = if fs_type == "ext4" {
                                        std::process::Command::new("mkfs.ext4")
                                            .arg("-F") // Force format even if already formatted
                                            .arg(&device_path)
                                            .output()
                                            .map_err(|e| tonic::Status::internal(format!("Failed to format device: {}", e)))?
                                    } else if fs_type == "xfs" {
                                        std::process::Command::new("mkfs.xfs")
                                            .arg("-f") // Force format
                                            .arg(&device_path)
                                            .output()
                                            .map_err(|e| tonic::Status::internal(format!("Failed to format device: {}", e)))?
                                    } else {
                                        std::process::Command::new(format!("mkfs.{}", fs_type))
                                            .arg(&device_path)
                                            .output()
                                            .map_err(|e| tonic::Status::internal(format!("Failed to format device: {}", e)))?
                                    };
                                    
                                    if !mkfs_output.status.success() {
                                        let error = String::from_utf8_lossy(&mkfs_output.stderr);
                                        println!("❌ [NODE] Format failed: {}", error);
                                        return Err(tonic::Status::internal(format!("Failed to format device: {}", error)));
                                    }
                                    println!("✅ [NODE] Device formatted successfully");
                                } else {
                                    println!("ℹ️ [NODE] Device already formatted, skipping format");
                                }
                                
                                // Mount the device to staging path
                                println!("🔧 [NODE] Mounting {} to {}", device_path, staging_target_path);
                                let mount_output = std::process::Command::new("mount")
                                    .arg(&device_path)
                                    .arg(&staging_target_path)
                                    .output()
                                    .map_err(|e| tonic::Status::internal(format!("Failed to mount device: {}", e)))?;
                                
                                if !mount_output.status.success() {
                                    let error = String::from_utf8_lossy(&mount_output.stderr);
                                    println!("❌ [NODE] Mount failed: {}", error);
                                    return Err(tonic::Status::internal(format!("Failed to mount device: {}", error)));
                                }
                                
                                println!("✅ [NODE] Device mounted to staging path");
                            }
                            spdk_csi_driver::csi::volume_capability::AccessType::Block(_) => {
                                println!("ℹ️ [NODE] Block volume - no filesystem mounting needed");
                            }
                        }
                    }
                }

        println!("✅ [NODE] Volume {} staged successfully", volume_id);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodeStageVolumeResponse {});
        println!("🔵 [GRPC] NodeStageVolume returning success response");
        Ok(response)
            }
            Err(e) => {
                println!("❌ [NODE] Failed to create ublk device: {}", e);
                Err(tonic::Status::internal(format!("Failed to create ublk device: {}", e)))
            }
        }
    }

    async fn node_unstage_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodeUnstageVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeUnstageVolumeResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeUnstageVolume called");
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let staging_target_path = req.staging_target_path.clone();
        
        println!("📤 [NODE] Unstaging volume {} from {}", volume_id, staging_target_path);

        // Unmount the filesystem from staging path (if mounted)
        if std::path::Path::new(&staging_target_path).exists() {
            println!("🔧 [NODE] Unmounting staging path: {}", staging_target_path);
            let umount_output = std::process::Command::new("umount")
                .arg(&staging_target_path)
                .output()
                .map_err(|e| tonic::Status::internal(format!("Failed to execute umount: {}", e)))?;
            
            if !umount_output.status.success() {
                let error = String::from_utf8_lossy(&umount_output.stderr);
                // Only log warning - unmount might fail if already unmounted
                println!("⚠️ [NODE] Unmount failed (may not be mounted): {}", error);
            } else {
                println!("✅ [NODE] Staging path unmounted successfully");
            }
        }

        // Delete the ublk device
        let ublk_id = self.driver.generate_ublk_id(&volume_id);
        
        match self.driver.delete_ublk_device(ublk_id).await {
            Ok(_) => {
                println!("✅ [NODE] ublk device stopped successfully");
            }
            Err(e) => {
                println!("⚠️ [NODE] Failed to stop ublk device (may not exist): {}", e);
                // Continue anyway - best effort cleanup
            }
        }

        // Disconnect from NVMe-oF if this was a remote volume
        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
        if let Err(e) = self.driver.disconnect_from_nvmeof_target(&nqn).await {
            println!("⚠️ [NODE] Failed to disconnect from NVMe-oF (may not be connected): {}", e);
            // Continue anyway - best effort cleanup
        }

        println!("✅ [NODE] Volume {} unstaged successfully", volume_id);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodeUnstageVolumeResponse {});
        println!("🔵 [GRPC] NodeUnstageVolume returning success response");
        Ok(response)
    }

    async fn node_publish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodePublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodePublishVolumeResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodePublishVolume called");
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let target_path = req.target_path.clone();
        let staging_target_path = req.staging_target_path.clone();
        
        println!("📋 [NODE] Publishing volume {} to {}", volume_id, target_path);
        println!("📋 [NODE] Staging path: {}", staging_target_path);

        // Create target directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&target_path) {
            println!("⚠️ [NODE] Failed to create target directory (may exist): {}", e);
        }

        // Determine if this is a filesystem or block volume
        let is_block_volume = if let Some(volume_capability) = req.volume_capability {
            matches!(volume_capability.access_type, 
                Some(spdk_csi_driver::csi::volume_capability::AccessType::Block(_)))
        } else {
            false // Default to filesystem
        };

        if is_block_volume {
            // Block volume - bind mount the device directly
            let ublk_id = self.driver.generate_ublk_id(&volume_id);
            let device_path = format!("/dev/ublkb{}", ublk_id);
            
            println!("📋 [NODE] Block volume - bind mounting device {} to {}", device_path, target_path);
            
            if !std::path::Path::new(&device_path).exists() {
                println!("❌ [NODE] ublk device {} does not exist", device_path);
                return Err(tonic::Status::internal(format!("ublk device {} not found", device_path)));
            }
            
            let mount_output = std::process::Command::new("mount")
                .args(["--bind", &device_path, &target_path])
                .output()
                .map_err(|e| tonic::Status::internal(format!("Failed to execute mount: {}", e)))?;

            if !mount_output.status.success() {
                let error = String::from_utf8_lossy(&mount_output.stderr);
                println!("❌ [NODE] Mount failed: {}", error);
                return Err(tonic::Status::internal(format!("Failed to mount: {}", error)));
            }
        } else {
            // Filesystem volume - bind mount from staging path
            println!("📋 [NODE] Filesystem volume - bind mounting staging path to target");
            
            // Verify staging path exists and is mounted
            if !std::path::Path::new(&staging_target_path).exists() {
                println!("❌ [NODE] Staging path {} does not exist", staging_target_path);
                return Err(tonic::Status::internal(format!("Staging path {} not found", staging_target_path)));
            }
            
            let mount_output = std::process::Command::new("mount")
                .args(["--bind", &staging_target_path, &target_path])
                .output()
                .map_err(|e| tonic::Status::internal(format!("Failed to execute mount: {}", e)))?;

            if !mount_output.status.success() {
                let error = String::from_utf8_lossy(&mount_output.stderr);
                println!("❌ [NODE] Mount failed: {}", error);
                return Err(tonic::Status::internal(format!("Failed to mount: {}", error)));
            }
        }

        println!("✅ [NODE] Volume {} published successfully at {}", volume_id, target_path);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodePublishVolumeResponse {});
        println!("🔵 [GRPC] NodePublishVolume returning success response");
        Ok(response)
    }

    async fn node_unpublish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodeUnpublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeUnpublishVolumeResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeUnpublishVolume called");
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let target_path = req.target_path.clone();
        
        println!("📤 [NODE] Unpublishing volume {} from {}", volume_id, target_path);

        // Unmount the target path (bind mount from staging path)
        if std::path::Path::new(&target_path).exists() {
            println!("🔧 [NODE] Unmounting target path: {}", target_path);
            let umount_output = std::process::Command::new("umount")
                .arg(&target_path)
                .output()
                .map_err(|e| tonic::Status::internal(format!("Failed to execute umount: {}", e)))?;
            
            if !umount_output.status.success() {
                let error = String::from_utf8_lossy(&umount_output.stderr);
                println!("⚠️ [NODE] Unmount failed (may not be mounted): {}", error);
                // Continue anyway - best effort cleanup
            } else {
                println!("✅ [NODE] Target path unmounted successfully");
            }
            
            // Remove the target directory
            if let Err(e) = std::fs::remove_dir(&target_path) {
                println!("⚠️ [NODE] Failed to remove target directory: {}", e);
            }
        }

        println!("✅ [NODE] Volume {} unpublished successfully", volume_id);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodeUnpublishVolumeResponse {});
        println!("🔵 [GRPC] NodeUnpublishVolume returning success response");
        Ok(response)
    }

    async fn node_get_volume_stats(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetVolumeStatsRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetVolumeStatsResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeGetVolumeStats called");
        Err(tonic::Status::unimplemented("Node get volume stats not implemented"))
    }

    async fn node_expand_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeExpandVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeExpandVolumeResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeExpandVolume called");
        Err(tonic::Status::unimplemented("Node expand volume not implemented"))
    }

    async fn node_get_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetCapabilitiesResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeGetCapabilities called");
        use spdk_csi_driver::csi::{node_service_capability::rpc::Type as RpcType, NodeServiceCapability, node_service_capability::Rpc};
        
        let capabilities = vec![
            NodeServiceCapability {
                r#type: Some(spdk_csi_driver::csi::node_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::StageUnstageVolume as i32,
                })),
            },
        ];
        
        println!("✅ [GRPC] Node.NodeGetCapabilities returning: StageUnstageVolume capability");
        Ok(tonic::Response::new(spdk_csi_driver::csi::NodeGetCapabilitiesResponse { capabilities }))
    }

    async fn node_get_info(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetInfoRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetInfoResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeGetInfo called");
        Ok(tonic::Response::new(spdk_csi_driver::csi::NodeGetInfoResponse {
            node_id: self.driver.node_id.clone(),
            max_volumes_per_node: 0, // 0 means unlimited
            accessible_topology: None,
        }))
    }
}
