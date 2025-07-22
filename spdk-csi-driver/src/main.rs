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

mod csi_driver {
    pub mod csi {
        tonic::include_proto!("csi");
    }
}

use csi_driver::csi::csi::v1::{
    controller_server::ControllerServer,
    identity_server::IdentityServer,
    node_server::NodeServer,
};

/// Simple health check endpoint for Kubernetes liveness probes
async fn start_health_server(driver: Arc<SpdkCsiDriver>) {
    let health = warp::path("healthz")
        .and(warp::get())
        .and_then(move || {
            let driver_clone = driver.clone();
            async move {
                // Simple health check - verify we can connect to Kubernetes API
                match driver_clone.kube_client.apiserver_version().await {
                    Ok(_) => Ok(warp::reply::with_status("OK", warp::http::StatusCode::OK)),
                    Err(_) => Ok(warp::reply::with_status(
                        "Service Unavailable", 
                        warp::http::StatusCode::SERVICE_UNAVAILABLE
                    )),
                }
            }
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
    let health_driver = driver.clone();
    tokio::spawn(async move {
        start_health_server(health_driver).await;
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