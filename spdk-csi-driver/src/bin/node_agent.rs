// bin/node_agent.rs - Node Agent Binary Entry Point
//
// This is the main binary entry point for the SPDK node agent.
// The actual implementation is in the spdk_csi_driver::node_agent module.

use spdk_csi_driver::node_agent::{NodeAgent, start_api_server, run_discovery_loop};
use std::env;
use kube::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();
    
    println!("🚀 [NODE_AGENT] Starting SPDK Node Agent...");
    
    // Get configuration from environment
    let node_name = env::var("NODE_NAME").unwrap_or_else(|_| "unknown-node".to_string());
    let discovery_interval = env::var("DISCOVERY_INTERVAL")
        .unwrap_or_else(|_| "60".to_string())
        .parse()
        .unwrap_or(60);
    let auto_initialize_blobstore = env::var("AUTO_INITIALIZE_BLOBSTORE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let backup_path = env::var("BACKUP_PATH").unwrap_or_else(|_| "/tmp/spdk-backup".to_string());
    let spdk_rpc_url = env::var("SPDK_RPC_URL").unwrap_or_else(|_| "http://localhost:9998".to_string());
    let target_namespace = env::var("TARGET_NAMESPACE").unwrap_or_else(|_| "default".to_string());
    let cluster_id = env::var("CLUSTER_ID").unwrap_or_else(|_| "flint-cluster".to_string());
    
    println!("📋 [NODE_AGENT] Configuration:");
    println!("  Node Name: {}", node_name);
    println!("  SPDK RPC URL: {}", spdk_rpc_url);
    println!("  Discovery Interval: {}s", discovery_interval);
    println!("  Auto Initialize Blobstore: {}", auto_initialize_blobstore);
    println!("  Target Namespace: {}", target_namespace);
    
    // Initialize Kubernetes client
    let kube_client = Client::try_default().await?;
    
    // Create the node agent
    let agent = NodeAgent::new(
        node_name,
        kube_client,
        spdk_rpc_url,
        discovery_interval,
        auto_initialize_blobstore,
        backup_path,
        target_namespace,
        cluster_id,
    );
    
    println!("🎯 [NODE_AGENT] Starting API server and discovery loop...");
    
    // Start API server and discovery loop concurrently
    let api_task = tokio::spawn(start_api_server(agent.clone()));
    let discovery_task = tokio::spawn(run_discovery_loop(agent.clone()));
    
    // Wait for either task to complete (or both)
    tokio::select! {
        result = api_task => {
            match result {
                Ok(_) => println!("✅ [NODE_AGENT] API server completed"),
                Err(e) => eprintln!("❌ [NODE_AGENT] API server failed: {}", e),
            }
        }
        result = discovery_task => {
            match result {
                Ok(_) => println!("✅ [NODE_AGENT] Discovery loop completed"),
                Err(e) => eprintln!("❌ [NODE_AGENT] Discovery loop failed: {}", e),
            }
        }
    }
    
    println!("🛑 [NODE_AGENT] Node agent shutting down");
    Ok(())
}
