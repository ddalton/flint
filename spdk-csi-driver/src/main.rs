// main.rs - Entry point for SPDK CSI Driver with NVMe-oF Support
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Server;
use kube::Client;
use warp::Filter;

mod controller;
mod node;
mod identity;
mod driver;
mod csi_snapshotter;

use controller::ControllerService;
use node::NodeService;
use identity::IdentityService;
use driver::SpdkCsiDriver;

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

    println!("Starting health server on port 9809");
    warp::serve(health)
        .run(([0, 0, 0, 0], 9809))
        .await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kube_client = Client::try_default().await?;
    let node_id = std::env::var("NODE_ID")
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or("unknown-node".to_string()));
    
    // Configure NVMe-oF transport settings
    let nvmeof_transport = std::env::var("NVMEOF_TRANSPORT").unwrap_or("tcp".to_string());
    let nvmeof_target_port = std::env::var("NVMEOF_TARGET_PORT")
        .unwrap_or("4420".to_string())
        .parse()
        .unwrap_or(4420);
    
    // Validate transport type
    if !["tcp", "rdma", "fc"].contains(&nvmeof_transport.to_lowercase().as_str()) {
        eprintln!("Warning: Unknown NVMe-oF transport '{}', using 'tcp'", nvmeof_transport);
    }
    
    let driver = Arc::new(SpdkCsiDriver {
        node_id: node_id.clone(),
        kube_client,
        spdk_rpc_url: std::env::var("SPDK_RPC_URL").unwrap_or("unix:///var/tmp/spdk.sock".to_string()),
        spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
        nvmeof_target_port,
        nvmeof_transport: nvmeof_transport.clone(),
    });
    
    // Start health server for Kubernetes liveness probes
    tokio::spawn(async move {
        start_health_server().await;
    });
    
    let mode = std::env::var("CSI_MODE").unwrap_or("all".to_string());
    let endpoint = std::env::var("CSI_ENDPOINT")
        .unwrap_or("unix:///csi/csi.sock".to_string());
    
    // Create service instances
    let identity_service = IdentityService::new(driver.clone());
    let controller_service = ControllerService::new(driver.clone());
    let node_service = NodeService::new(driver.clone());
    
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
    
    println!(
        "SPDK CSI Driver ('{}' mode) starting on {} for node {} with NVMe-oF transport {}:{}",
        mode, endpoint, node_id, nvmeof_transport, nvmeof_target_port
    );
    
    // Handle different endpoint types
    if endpoint.starts_with("unix://") {
        let socket_path = endpoint.trim_start_matches("unix://");
        
        println!("🔧 [SOCKET] Preparing Unix socket at: {}", socket_path);
        
        // Remove existing socket file if it exists
        if std::path::Path::new(socket_path).exists() {
            println!("🔧 [SOCKET] Socket file exists, attempting to remove: {}", socket_path);
            match std::fs::remove_file(socket_path) {
                Ok(_) => println!("✅ [SOCKET] Successfully removed existing socket file"),
                Err(e) => {
                    println!("❌ [SOCKET] Failed to remove existing socket: {} (error: {})", socket_path, e);
                    return Err(e.into());
                }
            }
        } else {
            println!("🔧 [SOCKET] No existing socket file found at: {}", socket_path);
        }
        
        // Create parent directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            println!("🔧 [SOCKET] Ensuring parent directory exists: {}", parent.display());
            match std::fs::create_dir_all(parent) {
                Ok(_) => println!("✅ [SOCKET] Parent directory ready: {}", parent.display()),
                Err(e) => {
                    println!("❌ [SOCKET] Failed to create parent directory: {} (error: {})", parent.display(), e);
                    return Err(e.into());
                }
            }
        }
        
        // Use UnixListener for Unix domain socket
        use tokio::net::UnixListener;
        use tokio_stream::wrappers::UnixListenerStream;
        
        println!("🔧 [SOCKET] Attempting to bind Unix socket: {}", socket_path);
        let listener = match UnixListener::bind(socket_path) {
            Ok(listener) => {
                println!("✅ [SOCKET] Successfully bound Unix socket: {}", socket_path);
                listener
            }
            Err(e) => {
                println!("❌ [SOCKET] Failed to bind Unix socket: {} (error: {})", socket_path, e);
                return Err(e.into());
            }
        };
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
