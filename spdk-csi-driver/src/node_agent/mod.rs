// node_agent/mod.rs - Node Agent Module Coordination
//
// This module coordinates the various components of the SPDK node agent:
// - Disk discovery and management
// - NVMe-oF export management  
// - HTTP API server for management operations
// - RAID operations and blobstore initialization
// - Health monitoring and status tracking

use kube::{Client, Api};
use tokio::time::{Duration, interval};
use serde::{Deserialize, Serialize};
use serde_json::json;
use anyhow::Result;
use chrono::Utc;
use std::env;

// Re-export submodules
pub mod disk_discovery;
pub mod nvmeof_manager;
pub mod http_api;
pub mod raid_operations;
pub mod health_monitor;
pub mod rpc_client;

// Re-export key types and functions
pub use disk_discovery::{discover_and_update_local_disks, NvmeDevice};
pub use nvmeof_manager::{manage_nvmeof_exports_intelligently, discover_and_publish_nvmeof_disks_legacy, repair_spdkraiddisk_members_for_local_disk};
pub use http_api::start_api_server;
pub use raid_operations::initialize_blobstore_on_device;
pub use health_monitor::check_device_health;
pub use rpc_client::call_spdk_rpc;

// Core dependencies - FlintDiskMetadata and models only used in specific modules

/// Core NodeAgent struct - coordinates all node-level operations
#[derive(Clone)]
pub struct NodeAgent {
    pub node_name: String,
    pub kube_client: Client,
    pub spdk_rpc_url: String,
    pub discovery_interval: u64,
    pub auto_initialize_blobstore: bool,
    pub backup_path: String,
    pub target_namespace: String,
    pub cluster_id: String,
}

impl NodeAgent {
    pub fn new(
        node_name: String,
        kube_client: Client,
        spdk_rpc_url: String,
        discovery_interval: u64,
        auto_initialize_blobstore: bool,
        backup_path: String,
        target_namespace: String,
        cluster_id: String,
    ) -> Self {
        Self {
            node_name,
            kube_client,
            spdk_rpc_url,
            discovery_interval,
            auto_initialize_blobstore,
            backup_path,
            target_namespace,
            cluster_id,
        }
    }

    /// Get current node IP address from Kubernetes
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::Node as k8sNode;
        
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        
        let node = nodes_api.get(&self.node_name).await
            .map_err(|e| format!("Failed to get node {}: {}", self.node_name, e))?;
        
        // Get internal IP address
        if let Some(status) = node.status {
            if let Some(addresses) = status.addresses {
                for addr in addresses {
                    if addr.type_ == "InternalIP" {
                        return Ok(addr.address);
                    }
                }
            }
        }
        
        Err("No internal IP found for node".into())
    }
}

/// Get Kubernetes cluster ID for cluster identification
pub async fn get_kubernetes_cluster_id() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Try to get cluster ID from kube-system namespace UID
    let client = Client::try_default().await?;
    let ns_api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client);
    
    match ns_api.get("kube-system").await {
        Ok(ns) => {
            if let Some(uid) = ns.metadata.uid {
                // Use first 8 characters of namespace UID as cluster ID
                return Ok(uid.chars().take(8).collect());
            }
        }
        Err(_) => {
            // Fallback to hostname-based cluster ID
            if let Ok(hostname) = env::var("HOSTNAME") {
                return Ok(hostname.chars().take(8).collect());
            }
        }
    }
    
    Ok("unknown".to_string())
}

/// Get current namespace from service account token
pub async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Try environment variable first (allows override)
    if let Ok(ns) = env::var("NAMESPACE") {
        return Ok(ns);
    }
    
    // Try reading from service account
    if let Ok(namespace) = tokio::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").await {
        return Ok(namespace.trim().to_string());
    }
    
    // Default fallback
    Ok("default".to_string())
}

/// Wait for SPDK to become ready by testing RPC connectivity
pub async fn wait_for_spdk_ready(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let max_retries = 36; // 6 minutes total (36 * 10 seconds)
    let mut last_error = String::new();
    
    println!("⏳ [SPDK_READY] Waiting for SPDK to be ready...");
    
    let spdk_socket = if agent.spdk_rpc_url.starts_with("unix://") {
        &agent.spdk_rpc_url[7..] // Remove "unix://" prefix
    } else {
        "N/A (HTTP endpoint)"
    };
    
    for attempt in 1..=max_retries {
        let test_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "spdk_get_version"
        });
        
        match call_spdk_rpc(&agent.spdk_rpc_url, &test_request).await {
            Ok(response) => {
                if response.get("result").is_some() {
                    println!("✅ [SPDK_READY] SPDK is ready! Version check successful.");
                    return Ok(());
                } else if let Some(error) = response.get("error") {
                    last_error = format!("SPDK RPC error: {}", error);
                } else {
                    last_error = "Invalid SPDK RPC response format".to_string();
                }
            }
            Err(e) => {
                last_error = e.to_string();
                if attempt == max_retries {
                    println!("❌ [SPDK_READY] SPDK failed to become ready after {} attempts", max_retries);
                    println!("❌ [SPDK_READY] Final error: {}", last_error);
                    println!("❌ [SPDK_READY] Socket path: {}", spdk_socket);
                    println!("❌ [SPDK_READY] Troubleshooting:");
                    println!("   - Check if SPDK target daemon is running");
                    println!("   - Verify socket file exists and has correct permissions");
                    println!("   - Check SPDK logs for startup errors");
                    println!("   - Ensure proper configuration file is loaded");
                    return Err(format!("SPDK failed to become ready within {} minutes. Last error: {}", max_retries / 6, last_error).into());
                }
                
                // Show progress every 5 attempts (50 seconds)
                if attempt % 5 == 0 {
                    println!("⏳ [SPDK_READY] Still waiting for SPDK (attempt {}/{})... Latest error: {}", attempt, max_retries, last_error);
                } else {
                    println!("⏳ [SPDK_READY] Waiting for SPDK to be ready... (attempt {}/{})", attempt, max_retries);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    
    Ok(())
}

/// Main discovery loop - runs periodically to discover and update disk status
pub async fn run_discovery_loop(agent: NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut interval = interval(Duration::from_secs(agent.discovery_interval));
    
    // Run initial discovery immediately
    if let Err(e) = discover_and_update_local_disks(&agent).await {
        eprintln!("Initial disk discovery failed: {}", e);
    }
    
    loop {
        interval.tick().await;
        
        if let Err(e) = discover_and_update_local_disks(&agent).await {
            eprintln!("Disk discovery failed: {}", e);
        }
    }
}

/// Perform graceful shutdown of node agent
pub async fn perform_graceful_shutdown(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🛑 [SHUTDOWN] Starting graceful shutdown sequence...");
    
    // TODO: Implement graceful shutdown logic
    // - Stop discovery loop
    // - Cleanup temporary resources
    // - Save final state
    
    println!("✅ [SHUTDOWN] Node agent shutdown complete");
    Ok(())
}
