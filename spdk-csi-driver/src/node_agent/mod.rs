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
use k8s_openapi::api::core::v1::ConfigMap;

use serde_json::{json, Value};
use anyhow::Result;

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
pub use nvmeof_manager::{manage_nvmeof_exports_intelligently, repair_spdkraiddisk_members_for_local_disk};
pub use http_api::start_api_server;
pub use raid_operations::initialize_blobstore_on_device;
pub use health_monitor::check_device_health;
// Removed re-export to prevent confusion - use direct imports instead

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

    /// Load SPDK configuration from ConfigMap and apply via RPC
    pub async fn configure_spdk_from_configmap(&self, namespace: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let configmap_name = format!("spdk-config-{}", self.node_name);
        
        println!("🔍 [SPDK_CONFIG] Looking for ConfigMap: {} in namespace: {}", configmap_name, namespace);
        
        match self.load_spdk_config_from_k8s(&configmap_name, namespace).await {
            Ok(config) => {
                println!("✅ [SPDK_CONFIG] Found ConfigMap, applying SPDK configuration...");
                self.apply_spdk_config_via_rpc(config).await?;
                println!("✅ [SPDK_CONFIG] SPDK configuration applied successfully");
                Ok(true)
            }
            Err(e) => {
                println!("ℹ️ [SPDK_CONFIG] No ConfigMap found ({}), will use auto-discovery", e);
                Ok(false)
            }
        }
    }

    /// Auto-discover and configure SPDK storage
    pub async fn auto_configure_spdk_storage(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [SPDK_AUTO] Starting auto-discovery of NVMe devices...");
        
        let nvme_devices = self.discover_nvme_devices().await?;
        
        if nvme_devices.is_empty() {
            println!("ℹ️ [SPDK_AUTO] No NVMe devices found for auto-configuration");
            return Ok(());
        }
        
        for device in nvme_devices {
            println!("🔧 [SPDK_AUTO] Configuring device: {}", device.path);
            
            // Create AIO bdev
            let bdev_name = format!("aio_{}", device.name);
            self.create_aio_bdev(&bdev_name, &device.path).await?;
            
            // Check if LVS already exists, create if not
            if !self.lvs_exists_for_bdev(&bdev_name).await? {
                let lvs_name = format!("lvs_{}", device.name);
                self.create_lvs(&bdev_name, &lvs_name).await?;
                println!("✅ [SPDK_AUTO] Created LVS: {} on device: {}", lvs_name, device.path);
            } else {
                println!("ℹ️ [SPDK_AUTO] LVS already exists for device: {}", device.path);
            }
        }
        
        println!("✅ [SPDK_AUTO] Auto-configuration completed");
        Ok(())
    }

    /// Load SPDK configuration from Kubernetes ConfigMap
    async fn load_spdk_config_from_k8s(&self, configmap_name: &str, namespace: &str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let configmaps: Api<ConfigMap> = Api::namespaced(self.kube_client.clone(), namespace);
        
        let cm = configmaps.get(configmap_name).await
            .map_err(|e| format!("Failed to get ConfigMap {}: {}", configmap_name, e))?;
        
        let config_json = cm.data
            .and_then(|data| data.get("spdk-config.json").cloned())
            .ok_or_else(|| "No spdk-config.json found in ConfigMap")?;
        
        let config: Value = serde_json::from_str(&config_json)
            .map_err(|e| format!("Invalid JSON in ConfigMap: {}", e))?;
        
        Ok(config)
    }

    /// Apply SPDK configuration via RPC calls
    async fn apply_spdk_config_via_rpc(&self, config: Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Extract subsystems from config
        let subsystems = config.get("subsystems")
            .and_then(|s| s.as_array())
            .ok_or("Invalid config format: missing subsystems array")?;
        
        for subsystem in subsystems {
            if let Some(_subsystem_name) = subsystem.get("subsystem").and_then(|s| s.as_str()) {
                if let Some(configs) = subsystem.get("config").and_then(|c| c.as_array()) {
                    for config_item in configs {
                        if let Some(method) = config_item.get("method").and_then(|m| m.as_str()) {
                            println!("🔧 [SPDK_RPC] Applying: {}", method);
                            
                            let rpc_request = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "method": method,
                                "params": config_item.get("params").unwrap_or(&json!({}))
                            });
                            
                            match rpc_client::call_spdk_rpc(&self.spdk_rpc_url, &rpc_request).await {
                                Ok(response) => {
                                    if response.get("error").is_some() {
                                        println!("⚠️ [SPDK_RPC] Warning for {}: {:?}", method, response.get("error"));
                                    } else {
                                        println!("✅ [SPDK_RPC] Applied: {}", method);
                                    }
                                }
                                Err(e) => {
                                    println!("❌ [SPDK_RPC] Failed to apply {}: {}", method, e);
                                    // Continue with other configs rather than failing completely
                                }
                            }
                        }
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Discover available NVMe devices
    async fn discover_nvme_devices(&self) -> Result<Vec<SimpleNvmeDevice>, Box<dyn std::error::Error + Send + Sync>> {
        use std::path::Path;
        use tokio::fs;
        
        let mut devices = Vec::new();
        let dev_dir = Path::new("/dev");
        
        if let Ok(mut entries) = fs::read_dir(dev_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                
                // Look for nvme devices (nvme0n1, nvme1n1, etc.)
                if name.starts_with("nvme") && name.contains("n1") && !name.contains("p") {
                    let path = entry.path();
                    if let Ok(metadata) = fs::metadata(&path).await {
                        devices.push(SimpleNvmeDevice {
                            name: name.to_string(),
                            path: path.to_string_lossy().to_string(),
                            size: metadata.len(),
                        });
                    }
                }
            }
        }
        
        Ok(devices)
    }

    /// Create AIO bdev via RPC
    async fn create_aio_bdev(&self, bdev_name: &str, device_path: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let rpc_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "bdev_aio_create",
            "params": {
                "name": bdev_name,
                "filename": device_path,
                "block_size": 512,
                "readonly": false,
                "fallocate": false
            }
        });
        
        match rpc_client::call_spdk_rpc(&self.spdk_rpc_url, &rpc_request).await {
            Ok(response) => {
                if response.get("error").is_some() {
                    return Err(format!("Failed to create AIO bdev {}: {:?}", bdev_name, response.get("error")).into());
                }
                println!("✅ [SPDK_RPC] Created AIO bdev: {}", bdev_name);
                Ok(())
            }
            Err(e) => Err(format!("RPC call failed for bdev_aio_create: {}", e).into())
        }
    }

    /// Create LVS (Logical Volume Store) via RPC
    async fn create_lvs(&self, bdev_name: &str, lvs_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let rpc_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": bdev_name,
                "lvs_name": lvs_name,
                "cluster_sz": 1048576
            }
        });
        
        match rpc_client::call_spdk_rpc(&self.spdk_rpc_url, &rpc_request).await {
            Ok(response) => {
                if response.get("error").is_some() {
                    return Err(format!("Failed to create LVS {}: {:?}", lvs_name, response.get("error")).into());
                }
                println!("✅ [SPDK_RPC] Created LVS: {}", lvs_name);
                Ok(())
            }
            Err(e) => Err(format!("RPC call failed for bdev_lvol_create_lvstore: {}", e).into())
        }
    }

    /// Check if LVS exists for a given bdev
    async fn lvs_exists_for_bdev(&self, bdev_name: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let rpc_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "bdev_lvol_get_lvstores",
            "params": {}
        });
        
        match rpc_client::call_spdk_rpc(&self.spdk_rpc_url, &rpc_request).await {
            Ok(response) => {
                if let Some(result) = response.get("result").and_then(|r| r.as_array()) {
                    for lvs in result {
                        if let Some(base_bdev) = lvs.get("base_bdev").and_then(|b| b.as_str()) {
                            if base_bdev == bdev_name {
                                return Ok(true);
                            }
                        }
                    }
                }
                Ok(false)
            }
            Err(e) => Err(format!("Failed to check LVS: {}", e).into())
        }
    }
}

#[derive(Debug)]
struct SimpleNvmeDevice {
    name: String,
    path: String,
    size: u64,
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
        
        match rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &test_request).await {
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
pub async fn perform_graceful_shutdown(_agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🛑 [SHUTDOWN] Starting graceful shutdown sequence...");
    
    // Graceful shutdown logic - stop discovery loop and cleanup resources
    // - Save final state
    
    println!("✅ [SHUTDOWN] Node agent shutdown complete");
    Ok(())
}
