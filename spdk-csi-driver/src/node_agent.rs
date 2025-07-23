use kube::{
    Client, Api, 
    api::{PatchParams, Patch, PostParams},
};
use tokio::time::{Duration, interval};
use reqwest::Client as HttpClient;
use serde_json::json;
use chrono::Utc;
use std::env;
use std::fs;
use std::process::Command;
use std::path::Path;
use serde::{Deserialize, Serialize};
use regex::Regex;
use warp::Filter;
use warp::{http::StatusCode, reply, Rejection, Reply};

use spdk_csi_driver::{SpdkDisk, SpdkDiskSpec, SpdkDiskStatus, IoStatistics};
use spdk_csi_driver::spdk_native::get_spdk_instance;

/// Unified SPDK interface using embedded SPDK for common operations, RPC as fallback
async fn call_spdk_rpc(
    spdk_rpc_url: &str,
    rpc_request: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let method = rpc_request["method"].as_str().unwrap_or("");
    let default_params = json!({});
    let params = rpc_request.get("params").unwrap_or(&default_params);
    
    // Check if we're running in embedded mode
    let spdk_mode = env::var("SPDK_MODE").unwrap_or("rpc".to_string());
    
    if spdk_mode == "embedded" {
        println!("🚀 [SPDK_EMBEDDED] Method: {} (using embedded implementation)", method);
        
        // Use embedded SPDK for common operations
        match method {
        "bdev_aio_create" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded AIO bdev creation");
            let spdk = get_spdk_instance()?;
            let name = params["name"].as_str().unwrap_or("");
            let filename = params["filename"].as_str().unwrap_or("");
            
            match spdk.create_aio_bdev(filename, name).await {
                Ok(_) => Ok(json!({"result": true})),
                Err(e) => {
                    // Return error in SPDK RPC format
                    Ok(json!({
                        "error": {
                            "code": -1,
                            "message": e.to_string()
                        }
                    }))
                }
            }
        }
        
        "bdev_lvol_create_lvstore" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded LVS creation");
            let spdk = get_spdk_instance()?;
            let bdev_name = params["bdev_name"].as_str().unwrap_or("");
            let lvs_name = params["lvs_name"].as_str().unwrap_or("");
            
            match spdk.create_lvs(bdev_name, lvs_name).await {
                Ok(_) => Ok(json!({"result": true})),
                Err(e) => {
                    // Return error in SPDK RPC format  
                    Ok(json!({
                        "error": {
                            "code": -1,
                            "message": e.to_string()
                        }
                    }))
                }
            }
        }
        
        "bdev_lvol_get_lvstores" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded LVS list");
            let spdk = get_spdk_instance()?;
            
            match spdk.get_lvol_stores().await {
                Ok(stores) => Ok(json!({"result": stores})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] LVS list failed, falling back to RPC: {}", e);
                    call_spdk_rpc_fallback(spdk_rpc_url, rpc_request).await
                }
            }
        }
        
        "bdev_get_bdevs" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded bdev list");
            let spdk = get_spdk_instance()?;
            
            match spdk.get_bdevs().await {
                Ok(bdevs) => Ok(json!({"result": bdevs})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] Bdev list failed, falling back to RPC: {}", e);
                    call_spdk_rpc_fallback(spdk_rpc_url, rpc_request).await
                }
            }
        }
        
        "bdev_lvol_create" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded lvol creation");
            let spdk = get_spdk_instance()?;
            let lvs_name = params["lvs_name"].as_str().unwrap_or("");
            let lvol_name = params["lvol_name"].as_str().unwrap_or("");
            let size = params["size"].as_u64().unwrap_or(0);
            
            match spdk.create_lvol(lvs_name, lvol_name, size).await {
                Ok(bdev_name) => Ok(json!({"result": bdev_name})),
                Err(e) => {
                    Ok(json!({
                        "error": {
                            "code": -1,
                            "message": e.to_string()
                        }
                    }))
                }
            }
        }
        
        "bdev_get_blobstores" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded blobstore list");
            let spdk = get_spdk_instance()?;
            
            match spdk.get_blobstores().await {
                Ok(blobstores) => Ok(json!({"result": blobstores})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] Blobstore list failed: {}", e);
                    Ok(json!({"result": []}))  // Return empty array instead of falling back
                }
            }
        }
        
        "blobstore_sync_all" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded blobstore sync");
            let spdk = get_spdk_instance()?;
            
            match spdk.sync_all_blobstores().await {
                Ok(_) => Ok(json!({"result": true})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] Blobstore sync failed: {}", e);
                    Ok(json!({"result": false}))
                }
            }
        }
        
        "bdev_nvme_get_controllers" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded NVMe controller list");
            let spdk = get_spdk_instance()?;
            
            match spdk.get_nvme_controllers().await {
                Ok(controllers) => Ok(json!({"result": controllers})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] NVMe controller list failed: {}", e);
                    Ok(json!({"result": []}))
                }
            }
        }
        
        "bdev_raid_get_bdevs" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded RAID bdev list");
            let spdk = get_spdk_instance()?;
            
            match spdk.get_raid_bdevs().await {
                Ok(raids) => Ok(json!({"result": raids})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] RAID bdev list failed: {}", e);
                    Ok(json!({"result": []}))
                }
            }
        }
        
        "nvmf_get_subsystems" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded NVMe-oF subsystem list");
            let spdk = get_spdk_instance()?;
            
            match spdk.get_nvmeof_subsystems().await {
                Ok(subsystems) => Ok(json!({"result": subsystems})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] NVMe-oF subsystem list failed: {}", e);
                    Ok(json!({"result": []}))
                }
            }
        }
        
        "bdev_get_iostat" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded I/O statistics");
            let spdk = get_spdk_instance()?;
            
            match spdk.get_bdev_iostat().await {
                Ok(iostats) => Ok(json!({"result": iostats})),
                Err(e) => {
                    println!("⚠️ [SPDK_EMBEDDED] I/O statistics failed: {}", e);
                    Ok(json!({"result": []}))
                }
            }
        }
        
        "spdk_get_version" => {
            println!("🚀 [SPDK_EMBEDDED] Using embedded SPDK version");
            Ok(json!({"result": {"version": "24.01-embedded"}}))
        }
        
            _ => {
                // Fall back to RPC for other methods not implemented in embedded mode
                println!("🔄 [SPDK_FALLBACK] Method {} not implemented in embedded mode, using RPC", method);
                call_spdk_rpc_fallback(spdk_rpc_url, rpc_request).await
            }
        }
    } else {
        // RPC mode - use RPC for all methods
        println!("🔌 [SPDK_RPC] Method: {} (using RPC mode)", method);
        call_spdk_rpc_fallback(spdk_rpc_url, rpc_request).await
    }
}

/// Fallback RPC implementation for methods not yet implemented in embedded SPDK
async fn call_spdk_rpc_fallback(
    spdk_rpc_url: &str,
    rpc_request: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    if spdk_rpc_url.starts_with("unix://") {
        // Unix socket connection using tokio for async I/O
        use tokio::net::UnixStream;
        use tokio::io::{AsyncWriteExt, AsyncReadExt};
        
        let socket_path = spdk_rpc_url.trim_start_matches("unix://");
        let mut stream = UnixStream::connect(socket_path).await?;
        
        // Convert to proper JSON-RPC 2.0 format
        let jsonrpc_request = json!({
            "jsonrpc": "2.0",
            "method": rpc_request["method"],
            "params": rpc_request.get("params").unwrap_or(&json!({})),
            "id": 1
        });
        
        let message = format!("{}\n", jsonrpc_request.to_string());
        println!("🔌 [RPC_FALLBACK] Sending to SPDK: {}", message.trim());
        
        stream.write_all(message.as_bytes()).await?;
        
        // Use larger buffer and read until we get a complete response
        let mut buffer = vec![0; 32768]; // 32KB buffer
        let bytes_read = stream.read(&mut buffer).await?;
        
        if bytes_read == 0 {
            return Err("No response from SPDK".into());
        }
        
        let response_str = String::from_utf8_lossy(&buffer[..bytes_read]);
        println!("📨 [RPC_FALLBACK] Response from SPDK: {}", response_str.trim());
        
        let response: serde_json::Value = serde_json::from_str(response_str.trim())?;
        Ok(response)
    } else {
        // HTTP connection
        let http_client = HttpClient::new();
        let response = http_client
            .post(spdk_rpc_url)
            .json(rpc_request)
            .send()
            .await?;
        
        if !response.status().is_success() {
            return Err(format!("HTTP request failed with status: {}", response.status()).into());
        }
        
        let json_response: serde_json::Value = response.json().await?;
        Ok(json_response)
    }
}

// Disk setup functionality
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnimplementedDisk {
    pub pci_address: String,
    pub device_name: String,
    pub vendor_id: String,
    pub device_id: String,
    pub subsystem_vendor_id: String,
    pub subsystem_device_id: String,
    pub numa_node: Option<u32>,
    pub driver: String,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    pub firmware_version: String,
    pub namespace_id: Option<u32>,
    pub mounted_partitions: Vec<String>,
    pub filesystem_type: Option<String>,
    pub is_system_disk: bool,
    pub spdk_ready: bool,
    pub discovered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSetupRequest {
    pub pci_addresses: Vec<String>,
    pub force_unmount: bool,
    pub backup_data: bool,
    pub huge_pages_mb: Option<u32>,
    pub driver_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSetupResult {
    pub success: bool,
    pub setup_disks: Vec<String>,
    pub failed_disks: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub huge_pages_configured: Option<u32>,
    pub completed_at: String,
}

#[derive(Clone)]
struct NodeAgent {
    node_name: String,
    kube_client: Client,
    spdk_rpc_url: String,
    discovery_interval: u64,
    auto_initialize_blobstore: bool,
    backup_path: String,
    // New fields for metadata sync configuration
    metadata_sync_interval: u64,
    metadata_sync_enabled: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let kube_client = Client::try_default().await?;
    let node_name = env::var("NODE_NAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-node".to_string());
    
    let agent = NodeAgent {
        node_name: node_name.clone(),
        kube_client,
        spdk_rpc_url: env::var("SPDK_RPC_URL").unwrap_or("unix:///var/tmp/spdk.sock".to_string()),
        discovery_interval: env::var("DISK_DISCOVERY_INTERVAL")
            .unwrap_or("300".to_string())
            .parse()
            .unwrap_or(300),
        auto_initialize_blobstore: env::var("AUTO_INITIALIZE_BLOBSTORE")
            .unwrap_or("true".to_string())
            .parse()
            .unwrap_or(true),
        backup_path: env::var("DISK_BACKUP_PATH")
            .unwrap_or("/var/lib/spdk-csi/backups".to_string()),
        // Configure metadata sync - default to every 30 seconds
        metadata_sync_interval: env::var("METADATA_SYNC_INTERVAL")
            .unwrap_or("30".to_string())
            .parse()
            .unwrap_or(30),
        metadata_sync_enabled: env::var("METADATA_SYNC_ENABLED")
            .unwrap_or("true".to_string())
            .parse()
            .unwrap_or(true),
    };

    println!("Starting SPDK Node Agent on node: {}", node_name);
    
    // Check if we're running in embedded mode
    let spdk_mode = env::var("SPDK_MODE").unwrap_or("rpc".to_string());
    
    if spdk_mode == "embedded" {
        println!("🚀 [EMBEDDED] Initializing embedded SPDK mode");
        
        // Initialize embedded SPDK instance
        match get_spdk_instance() {
            Ok(_spdk) => {
                println!("✅ [EMBEDDED] SPDK instance initialized successfully");
            }
            Err(e) => {
                eprintln!("❌ [EMBEDDED] Failed to initialize SPDK: {}", e);
                println!("🔄 [EMBEDDED] Continuing with mock bindings for development");
            }
        }
    } else {
        println!("🔌 [RPC] Using RPC mode - waiting for SPDK to be ready");
        // Wait for SPDK to be ready via RPC
        wait_for_spdk_ready(&agent).await?;
    }
    
    // Start HTTP API server for disk setup operations
    let api_agent = agent.clone();
    tokio::spawn(async move {
        start_api_server(api_agent).await;
    });
    
    // Start periodic metadata sync task
    if agent.metadata_sync_enabled {
        let metadata_agent = agent.clone();
        tokio::spawn(async move {
            run_metadata_sync_loop(metadata_agent).await;
        });
        println!("Started periodic metadata sync task (interval: {}s)", agent.metadata_sync_interval);
    }
    
    // Start disk discovery loop
    run_discovery_loop(agent).await?;
    
    Ok(())
}

/// Background task that periodically calls spdk_blob_sync_md to sync metadata
/// This reduces the amount of work needed during SPDK shutdown
async fn run_metadata_sync_loop(agent: NodeAgent) {
    let mut interval = interval(Duration::from_secs(agent.metadata_sync_interval));
    
    loop {
        interval.tick().await;
        
        if let Err(e) = sync_blob_metadata(&agent).await {
            eprintln!("Metadata sync failed: {}", e);
        }
    }
}

/// Correctly sync blob metadata by directly accessing blobstores
async fn sync_blob_metadata_correct(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Starting direct blobstore metadata sync for node: {}", agent.node_name);
    
    // Step 1: Get all blobstores directly (not through bdevs)
    let blobstores = match call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_blobstores"
    })).await {
        Ok(result) => result,
        Err(_) => {
            println!("No blobstores found or RPC not available, trying alternative methods");
            return sync_blob_metadata_fallback(agent).await;
        }
    };
    let mut synced_count = 0;
    
    if let Some(blobstore_list) = blobstores["result"].as_array() {
        for blobstore in blobstore_list {
            if let Some(blobstore_name) = blobstore["name"].as_str() {
                println!("Syncing blobstore: {}", blobstore_name);
                
                // Direct blobstore sync - this is the correct approach
                let sync_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                    "method": "blobstore_sync",
                    "params": {
                        "name": blobstore_name
                    }
                })).await;
                    
                match sync_result {
                    Ok(_) => {
                        synced_count += 1;
                        println!("✓ Successfully synced blobstore: {}", blobstore_name);
                    }
                    Err(e) => {
                        eprintln!("✗ Error syncing blobstore {}: {}", blobstore_name, e);
                    }
                }
                
                // Small delay to avoid overwhelming SPDK
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    
    // Step 2: Also sync lvol stores (which are built on top of blobstores)
    let lvol_synced = sync_lvol_stores_metadata(&agent.spdk_rpc_url).await
        .unwrap_or(0);
    
    println!("Metadata sync completed: {} blobstores, {} lvol stores", 
             synced_count, lvol_synced);
    
    Ok(())
}

// Fallback method if direct blobstore access isn't available
async fn sync_blob_metadata_fallback(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Using fallback blobstore sync methods");
    
    // Method 1: Global blobstore sync (if available)
    let global_sync_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "blobstore_sync_all"
    })).await;
        
    match global_sync_result {
        Ok(_) => {
            println!("✓ Global blobstore sync successful");
            return Ok(());
        }
        _ => println!("Global blobstore sync not available, trying individual methods"),
    }
    
    // Method 2: Sync through lvol stores (most common case)
    let lvol_count = sync_lvol_stores_metadata(&agent.spdk_rpc_url).await
        .unwrap_or(0);
    
    // Method 3: Find blobstores through their underlying bdevs
    let bdev_count = sync_blobstores_via_bdevs(&agent.spdk_rpc_url).await
        .unwrap_or(0);
    
    println!("Fallback sync completed: {} lvol stores, {} bdevs", lvol_count, bdev_count);
    Ok(())
}

// Sync lvol store metadata (which indirectly syncs underlying blobstores)
async fn sync_lvol_stores_metadata(rpc_url: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let lvstores = match call_spdk_rpc(rpc_url, &json!({"method": "bdev_lvol_get_lvstores"})).await {
        Ok(result) => result,
        Err(_) => return Ok(0),
    };
    let mut synced_count = 0;
    
    if let Some(stores) = lvstores["result"].as_array() {
        for store in stores {
            if let Some(lvs_name) = store["name"].as_str() {
                println!("Syncing lvol store metadata: {}", lvs_name);
                
                // This syncs the lvol store's metadata, which includes blob metadata
                let sync_result = call_spdk_rpc(rpc_url, &json!({
                    "method": "bdev_lvol_sync_metadata",
                    "params": {
                        "lvs_name": lvs_name
                    }
                })).await;
                    
                match sync_result {
                    Ok(_) => {
                        synced_count += 1;
                        println!("✓ Synced lvol store: {}", lvs_name);
                    }
                    Err(_) => {
                        // Try alternative RPC method
                        let alt_result = call_spdk_rpc(rpc_url, &json!({
                            "method": "spdk_blob_sync_md",
                            "params": {
                                "lvs_name": lvs_name
                            }
                        })).await;
                            
                        if alt_result.is_ok() {
                            synced_count += 1;
                            println!("✓ Synced lvol store (alt method): {}", lvs_name);
                        } else {
                            println!("✗ Failed to sync lvol store: {}", lvs_name);
                        }
                    }
                }
                
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    
    Ok(synced_count)
}

// Last resort: Find blobstores through their underlying bdevs
async fn sync_blobstores_via_bdevs(rpc_url: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let bdevs = match call_spdk_rpc(rpc_url, &json!({"method": "bdev_get_bdevs"})).await {
        Ok(result) => result,
        Err(_) => return Ok(0),
    };
    let mut synced_count = 0;
    
    if let Some(bdev_list) = bdevs["result"].as_array() {
        for bdev in bdev_list {
            if let Some(bdev_name) = bdev["name"].as_str() {
                // Look for bdevs that have blobstores on them
                if has_blobstore_on_bdev(bdev) {
                    println!("Found bdev with blobstore: {}", bdev_name);
                    
                    // Try to get the blobstore name from the bdev
                    if let Some(blobstore_name) = extract_blobstore_name(bdev) {
                        let sync_result = call_spdk_rpc(rpc_url, &json!({
                            "method": "blobstore_sync",
                            "params": {
                                "name": blobstore_name
                            }
                        })).await;
                            
                        if sync_result.is_ok() {
                            synced_count += 1;
                            println!("✓ Synced blobstore via bdev: {} -> {}", bdev_name, blobstore_name);
                        }
                    }
                }
            }
        }
    }
    
    Ok(synced_count)
}

fn has_blobstore_on_bdev(bdev: &serde_json::Value) -> bool {
    // Check if this bdev has a blobstore
    bdev.get("has_blobstore").and_then(|v| v.as_bool()).unwrap_or(false) ||
    bdev["driver_specific"].get("blobstore").is_some() ||
    bdev["driver_specific"]["nvme"]
        .as_object()
        .and_then(|nvme| nvme.get("blobstore_initialized"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn extract_blobstore_name(bdev: &serde_json::Value) -> Option<String> {
    // Try to extract the blobstore name from bdev metadata
    bdev["driver_specific"]["blobstore"]["name"].as_str().map(String::from)
        .or_else(|| {
            // For NVMe bdevs, blobstore name might be derived from bdev name
            bdev["name"].as_str().map(|name| format!("blobstore_{}", name))
        })
}

// Most robust approach: Try multiple methods in order of preference
async fn sync_blob_metadata(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Starting robust blobstore metadata sync for node: {}", agent.node_name);
    
    // Try methods in order of preference:
    
    // 1. Direct blobstore access (best)
    if let Ok(()) = sync_blob_metadata_correct(agent).await {
        return Ok(());
    }
    
    // 2. Global sync (second best)
    if let Ok(()) = try_global_blobstore_sync(agent).await {
        return Ok(());
    }
    
    // 3. Lvol store sync (common case)
    if let Ok(count) = sync_lvol_stores_metadata(&agent.spdk_rpc_url).await {
        if count > 0 {
            println!("Successfully synced {} lvol stores", count);
            return Ok(());
        }
    }
    
    // 4. Last resort: bdev-based approach
    sync_blob_metadata_fallback(agent).await
}

async fn try_global_blobstore_sync(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({"method": "blobstore_sync_all"})).await;
        
    match result {
        Ok(_) => {
            println!("✓ Global blobstore sync successful");
            Ok(())
        }
        Err(_e) => {
            Err("Global blobstore sync failed".into())
        }
    }
}


/// Enhanced shutdown handler that performs final metadata sync before exit
async fn perform_graceful_shutdown_with_sync(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Performing graceful shutdown with final metadata sync...");
    
    // Perform final metadata sync before shutdown
    if agent.metadata_sync_enabled {
        println!("Performing final metadata sync before shutdown...");
        if let Err(e) = sync_blob_metadata(agent).await {
            eprintln!("Warning: Final metadata sync failed: {}", e);
        } else {
            println!("Final metadata sync completed successfully");
        }
    }
    
    // Then proceed with normal SPDK shutdown
    println!("Initiating SPDK application shutdown...");
    let result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "spdk_app_stop",
        "params": {}
    })).await;

    match result {
        Ok(_) => {
            println!("SPDK shutdown initiated successfully");
        }
        Err(e) => {
            eprintln!("Failed to send SPDK shutdown RPC: {}", e);
        }
    }
    
    Ok(())
}

async fn start_api_server(agent: NodeAgent) {
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);

    let agent_filter = warp::any().map(move || agent.clone());

    let api = warp::path("api").and(
        // Get all available disks for disk setup management
        warp::path("disks")
            .and(warp::path("uninitialized"))
            .and(warp::get())
            .and(agent_filter.clone())
            .and_then(get_uninitialized_disks)
        .or(
            // Setup disks for SPDK
            warp::path("disks")
                .and(warp::path("setup"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(setup_disks_for_spdk)
        )
        .or(
            // Reset disks back to kernel
            warp::path("disks")
                .and(warp::path("reset"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(reset_disks_to_kernel)
        )
        .or(
            // Get setup status
            warp::path("disks")
                .and(warp::path("status"))
                .and(warp::get())
                .and(agent_filter.clone())
                .and_then(get_disk_setup_status)
        )
        .or(
            // Refresh disk discovery
            warp::path("disks")
                .and(warp::path("refresh"))
                .and(warp::post())
                .and(agent_filter.clone())
                .and_then(refresh_disk_discovery)
        )
        .or(
            // Enhanced shutdown with metadata sync
            warp::path("spdk")
                .and(warp::path("shutdown"))
                .and(warp::post())
                .and(agent_filter.clone())
                .and_then(shutdown_spdk_process_with_sync)
        )
        .or(
            // Manual metadata sync trigger
            warp::path("spdk")
                .and(warp::path("sync-metadata"))
                .and(warp::post())
                .and(agent_filter.clone())
                .and_then(trigger_metadata_sync)
        )
        .or(
            // Get metadata sync status
            warp::path("spdk")
                .and(warp::path("sync-status"))
                .and(warp::get())
                .and(agent_filter.clone())
                .and_then(get_metadata_sync_status)
        )
        .or(
            // Generic SPDK RPC proxy for cross-node communication
            warp::path("spdk")
                .and(warp::path("rpc"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(proxy_spdk_rpc)
        )
    );

    let routes = api.with(cors);
    let port = env::var("API_PORT").unwrap_or("8081".to_string()).parse().unwrap_or(8081);
    
    println!("SPDK Node Agent API server starting on port {}", port);
    warp::serve(routes)
        .run(([0, 0, 0, 0], port))
        .await;
}

// HTTP API handlers for disk setup operations
async fn get_uninitialized_disks(agent: NodeAgent) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [API] Received request for uninitialized disks on node: {}", agent.node_name);
    
    match agent.discover_all_disks().await {
        Ok(disks) => {
            println!("🌐 [API] Discovery successful: {} disks found", disks.len());
            for (i, disk) in disks.iter().enumerate() {
                println!("🌐 [API]   Disk {}: PCI={}, Name={}, Driver={}, System={}, SPDK Ready={}, Size={}GB", 
                         i+1, disk.pci_address, disk.device_name, disk.driver, 
                         disk.is_system_disk, disk.spdk_ready, disk.size_bytes / (1024*1024*1024));
            }
            
            let response = json!({
                "success": true,
                "disks": disks,
                "count": disks.len(),
                "node": agent.node_name
            });
            println!("🌐 [API] Returning successful response with {} disks", disks.len());
            Ok(warp::reply::json(&response))
        }
        Err(e) => {
            println!("❌ [API] Discovery failed with error: {}", e);
            let response = json!({
                "success": false,
                "error": e.to_string(),
                "node": agent.node_name
            });
            println!("🌐 [API] Returning error response: {:?}", response);
            Ok(warp::reply::json(&response))
        }
    }
}

async fn setup_disks_for_spdk(
    request: DiskSetupRequest,
    agent: NodeAgent
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [API] Received disk setup request for node: {}", agent.node_name);
    println!("🌐 [API] Request: {:?}", request);
    
    match agent.setup_disks_for_spdk(request).await {
        Ok(result) => {
            println!("🌐 [API] Setup completed successfully: {:?}", result);
            Ok(warp::reply::json(&result))
        }
        Err(e) => {
            println!("🌐 [API] Setup failed with error: {}", e);
            let error_response = json!({
                "success": false,
                "error": e.to_string(),
                "node": agent.node_name,
                "completed_at": Utc::now().to_rfc3339()
            });
            println!("🌐 [API] Returning error response: {:?}", error_response);
            Ok(warp::reply::json(&error_response))
        }
    }
}

async fn reset_disks_to_kernel(
    request: serde_json::Value,
    agent: NodeAgent
) -> Result<impl warp::Reply, warp::Rejection> {
    let pci_addresses: Vec<String> = request["pci_addresses"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    match agent.reset_disks_to_kernel(pci_addresses).await {
        Ok(result) => Ok(warp::reply::json(&result)),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name,
            "completed_at": Utc::now().to_rfc3339()
        })))
    }
}

async fn get_disk_setup_status(agent: NodeAgent) -> Result<impl warp::Reply, warp::Rejection> {
    match agent.get_all_disk_status().await {
        Ok(status) => Ok(warp::reply::json(&status)),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name
        })))
    }
}

async fn refresh_disk_discovery(agent: NodeAgent) -> Result<impl warp::Reply, warp::Rejection> {
    match discover_and_update_local_disks(&agent).await {
        Ok(_) => Ok(warp::reply::json(&json!({
            "success": true,
            "message": "Disk discovery refreshed",
            "node": agent.node_name,
            "refreshed_at": Utc::now().to_rfc3339()
        }))),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name
        })))
    }
}

/// Enhanced shutdown handler that performs metadata sync before shutdown
async fn shutdown_spdk_process_with_sync(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    println!("Received request to gracefully shut down SPDK process with metadata sync.");
    
    match perform_graceful_shutdown_with_sync(&agent).await {
        Ok(_) => {
            let reply = reply::json(&json!({
                "success": true,
                "message": "SPDK shutdown with metadata sync initiated successfully."
            }));
            Ok(reply::with_status(reply, StatusCode::OK))
        }
        Err(e) => {
            eprintln!("Failed to perform graceful shutdown: {}", e);
            let reply = reply::json(&json!({
                "success": false,
                "error": format!("Graceful shutdown failed: {}", e)
            }));
            Ok(reply::with_status(reply, StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

/// API handler to manually trigger metadata sync
async fn trigger_metadata_sync(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    println!("Received request to manually trigger metadata sync.");
    
    match sync_blob_metadata(&agent).await {
        Ok(_) => {
            let reply = reply::json(&json!({
                "success": true,
                "message": "Metadata sync completed successfully.",
                "synced_at": Utc::now().to_rfc3339()
            }));
            Ok(reply::with_status(reply, StatusCode::OK))
        }
        Err(e) => {
            eprintln!("Manual metadata sync failed: {}", e);
            let reply = reply::json(&json!({
                "success": false,
                "error": format!("Metadata sync failed: {}", e)
            }));
            Ok(reply::with_status(reply, StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

/// API handler to get metadata sync configuration and status
async fn get_metadata_sync_status(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    let reply = reply::json(&json!({
        "success": true,
        "metadata_sync_enabled": agent.metadata_sync_enabled,
        "metadata_sync_interval": agent.metadata_sync_interval,
        "node": agent.node_name,
        "checked_at": Utc::now().to_rfc3339()
    }));
    Ok(reply::with_status(reply, StatusCode::OK))
}

/// Generic SPDK RPC proxy for cross-node communication
/// Forwards JSON-RPC calls to the local SPDK instance via Unix socket
async fn proxy_spdk_rpc(
    rpc_request: serde_json::Value,
    agent: NodeAgent
) -> Result<impl Reply, Rejection> {
    // Forward the RPC call to local SPDK
    match call_spdk_rpc(&agent.spdk_rpc_url, &rpc_request).await {
        Ok(json_result) => {
            let reply = reply::json(&json_result);
            Ok(reply::with_status(reply, StatusCode::OK))
        }
        Err(e) => {
            let reply = reply::json(&json!({
                "error": format!("Failed to connect to SPDK: {}", e),
                "node": agent.node_name,
                "spdk_url": agent.spdk_rpc_url
            }));
            Ok(reply::with_status(reply, StatusCode::SERVICE_UNAVAILABLE))
        }
    }
}

async fn wait_for_spdk_ready(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let max_retries = 30; // 5 minutes
    
    for attempt in 1..=max_retries {
        // Check if we're using Unix socket or HTTP
        let result = if agent.spdk_rpc_url.starts_with("unix://") {
            // Unix socket connection
            use std::os::unix::net::UnixStream;
            use std::io::{Write, Read};
            
            let socket_path = agent.spdk_rpc_url.trim_start_matches("unix://");
            match UnixStream::connect(socket_path) {
                Ok(mut stream) => {
                    let rpc_call = json!({"jsonrpc": "2.0", "method": "spdk_get_version", "id": 1});
                    let message = format!("{}\n", rpc_call.to_string());
                    
                    match stream.write_all(message.as_bytes()) {
                        Ok(_) => {
                            let mut buffer = [0; 4096];
                            match stream.read(&mut buffer) {
                                Ok(_) => Ok(()),
                                Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                            }
                        }
                        Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    }
                }
                Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
            }
        } else {
            // HTTP connection
            let http_client = HttpClient::new();
            match http_client
                .post(&agent.spdk_rpc_url)
                .json(&json!({"method": "spdk_get_version"}))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => Ok(()),
                Ok(_) => Err("HTTP request failed".into()),
                Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
            }
        };
        
        match result {
            Ok(_) => {
                println!("SPDK is ready on node {}", agent.node_name);
                return Ok(());
            }
            _ => {
                if attempt == max_retries {
                    return Err("SPDK failed to become ready within timeout".into());
                }
                println!("Waiting for SPDK to be ready... (attempt {}/{})", attempt, max_retries);
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    
    Ok(())
}

async fn run_discovery_loop(agent: NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

async fn discover_and_update_local_disks(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔍 [DISCOVERY] Starting NVMe device discovery on node: {}", agent.node_name);
    println!("🔧 [DISCOVERY] Config - auto_init_blobstore: {}, discovery_interval: {}s", 
             agent.auto_initialize_blobstore, agent.discovery_interval);
    
    // Discover local NVMe devices
    let discovered_devices = query_local_nvme_devices(agent).await?;
    
    if discovered_devices.is_empty() {
        println!("❌ [DISCOVERY] No NVMe devices found on node: {}", agent.node_name);
        return Ok(());
    }
    
    println!("✅ [DISCOVERY] Found {} NVMe device(s) on node: {}", discovered_devices.len(), agent.node_name);
    for device in &discovered_devices {
        println!("📀 [DISCOVERY] Device: {} ({}) - PCIe: {}, Size: {}GB", 
                 device.controller_id, device.model, device.pcie_addr, 
                 device.capacity / (1024 * 1024 * 1024));
    }
    
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
    
    for device in &discovered_devices {
        let disk_name = format!("{}-{}", agent.node_name, device.controller_id);
        
        // Implement robust get-or-create with retry logic
        match get_or_create_disk_resource(agent, &spdk_disks, &disk_name, &device).await {
            Ok(disk) => {
                println!("✅ [DISCOVERY] Successfully got/created disk resource: {}", disk_name);
                
                // Check if blobstore initialization is needed
                if let Some(status) = &disk.status {
                    println!("🔍 [BLOBSTORE] Checking initialization status for {}: blobstore_initialized={}, auto_init={}", 
                             disk_name, status.blobstore_initialized, agent.auto_initialize_blobstore);
                    
                    if !status.blobstore_initialized && agent.auto_initialize_blobstore {
                        println!("🚀 [BLOBSTORE] Starting blobstore initialization for: {}", disk_name);
                        
                        match initialize_blobstore_on_device(agent, &disk).await {
                            Ok(_) => {
                                println!("✅ [BLOBSTORE] Successfully initialized blobstore for: {}", disk_name);
                                // Status update is now handled inside initialize_blobstore_on_device
                            }
                            Err(e) => {
                                println!("❌ [BLOBSTORE] Failed to initialize blobstore for {}: {}", disk_name, e);
                                eprintln!("Failed to initialize blobstore for {}: {}", disk_name, e);
                            }
                        }
                    } else if status.blobstore_initialized {
                        println!("✅ [BLOBSTORE] Already initialized for: {}", disk_name);
                    } else {
                        println!("⏭️ [BLOBSTORE] Auto-initialization disabled for: {}", disk_name);
                    }
                } else {
                    println!("⚠️ [BLOBSTORE] No status found for disk: {}", disk_name);
                }
            }
            Err(e) => {
                println!("❌ [DISCOVERY] Failed to get or create disk resource {}: {}", disk_name, e);
                eprintln!("Failed to get or create disk resource {}: {}", disk_name, e);
            }
        }
    }
    
    // Update I/O statistics for all disks on this node
    update_disk_io_statistics(agent).await?;
    
    println!("✅ [DISCOVERY] Disk discovery completed successfully for node: {}", agent.node_name);
    println!("📊 [DISCOVERY] Summary - Processed {} device(s)", discovered_devices.len());
    Ok(())
}

async fn get_or_create_disk_resource(
    agent: &NodeAgent,
    spdk_disks: &Api<SpdkDisk>,
    disk_name: &str,
    device: &NvmeDevice,
) -> Result<SpdkDisk, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [DISK_RESOURCE] Attempting to get or create resource: {}", disk_name);
    
    // Try to get existing resource first
    match spdk_disks.get(disk_name).await {
        Ok(existing_disk) => {
            println!("✅ [DISK_RESOURCE] Found existing resource: {}", disk_name);
            println!("🔍 [DISK_RESOURCE] Checking if update needed for: {}", disk_name);
            
            // Resource exists - update it if needed
            update_existing_disk_resource(agent, &existing_disk, device).await?;
            
            // Fetch the updated resource
            println!("📥 [DISK_RESOURCE] Fetching updated resource: {}", disk_name);
            Ok(spdk_disks.get(disk_name).await?)
        }
        Err(kube::Error::Api(api_err)) if api_err.code == 404 => {
            println!("❌ [DISK_RESOURCE] Resource not found, creating new: {}", disk_name);
            
            // Resource doesn't exist - try to create it
            match create_new_disk_resource_internal(agent, spdk_disks, disk_name, device).await {
                Ok(disk) => {
                    println!("✅ [DISK_RESOURCE] Successfully created new resource: {}", disk_name);
                    Ok(disk)
                }
                Err(e) => {
                    println!("⚠️ [DISK_RESOURCE] Creation failed ({}), checking for race condition: {}", e, disk_name);
                    
                    // If creation failed, it might be due to race condition - retry get
                    eprintln!("Creation failed ({}), retrying get for {}", e, disk_name);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    
                    match spdk_disks.get(disk_name).await {
                        Ok(disk) => {
                            println!("🔄 [DISK_RESOURCE] Found existing resource {} after creation conflict (race condition resolved)", disk_name);
                            Ok(disk)
                        }
                        Err(get_err) => {
                            let error_msg = format!("Failed to create and retrieve {}: create_err={}, get_err={}", 
                                                   disk_name, e, get_err);
                            println!("❌ [DISK_RESOURCE] {}", error_msg);
                            Err(error_msg.into())
                        }
                    }
                }
            }
        }
        Err(e) => {
            let error_msg = format!("Unexpected error getting {}: {}", disk_name, e);
            println!("❌ [DISK_RESOURCE] {}", error_msg);
            Err(error_msg.into())
        }
    }
}

async fn create_new_disk_resource_internal(
    agent: &NodeAgent,
    spdk_disks: &Api<SpdkDisk>,
    disk_name: &str,
    device: &NvmeDevice,
) -> Result<SpdkDisk, Box<dyn std::error::Error + Send + Sync>> {
    let device_path = format!("/dev/{}", device.controller_id);
    
    let spdk_disk = SpdkDisk::new_with_metadata(disk_name, SpdkDiskSpec {
        node_id: agent.node_name.clone(),
        device_path,
        size: format!("{}GB", device.capacity / (1024 * 1024 * 1024)),
        pcie_addr: device.pcie_addr.clone(),
        blobstore_uuid: None,
        nvme_controller_id: Some(device.controller_id.clone()),
    });
    
    // Set initial status
    let mut spdk_disk_with_status = spdk_disk;
    spdk_disk_with_status.status = Some(SpdkDiskStatus {
        total_capacity: device.capacity,
        free_space: device.capacity,
        used_space: 0,
        healthy: true,
        last_checked: Utc::now().to_rfc3339(),
        lvol_count: 0,
        blobstore_initialized: false,
        io_stats: IoStatistics::default(),
        lvs_name: None,
    });
    
    let created_disk = spdk_disks.create(&PostParams::default(), &spdk_disk_with_status).await?;
    
    println!("Created SpdkDisk resource: {} for device {} ({})", 
             disk_name, device.pcie_addr, device.model);
    
    Ok(created_disk)
}

async fn query_local_nvme_devices(agent: &NodeAgent) -> Result<Vec<NvmeDevice>, Box<dyn std::error::Error + Send + Sync>> {
    let mut devices = Vec::new();
    
    // Get all NVMe controllers that are already attached to SPDK
    let controllers = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_nvme_get_controllers"
    })).await;
    
    if let Ok(controllers_result) = controllers {
        if let Some(controller_list) = controllers_result["result"].as_array() {
            for controller in controller_list {
                if let Some(device) = parse_nvme_controller(controller) {
                    devices.push(device);
                }
            }
        }
    }
    
    // ALSO get kernel-bound SPDK-ready devices that should be included in discovery
    // This fixes the issue where SPDK-ready kernel-bound disks were ignored in periodic discovery
    let pci_devices = agent.get_nvme_pci_devices().await.unwrap_or_default();
    
    for pci_addr in pci_devices {
        // Skip if we already have this device from SPDK controllers
        if devices.iter().any(|d| d.pcie_addr == pci_addr) {
            continue;
        }
        
        // Check if this is a SPDK-ready device that should be discovered
        if let Ok(disk_info) = agent.get_disk_info(&pci_addr).await {
            if disk_info.spdk_ready && !disk_info.is_system_disk {
                // Convert to NvmeDevice format for consistency
                let device = NvmeDevice {
                    controller_id: disk_info.device_name.clone(),
                    pcie_addr: disk_info.pci_address.clone(),
                    capacity: disk_info.size_bytes as i64,
                    model: disk_info.model.clone(),
                };
                println!("Included SPDK-ready kernel-bound device in discovery: {} ({})", 
                         device.controller_id, device.pcie_addr);
                devices.push(device);
            }
        }
    }
    
    Ok(devices)
}

#[derive(Debug, Clone)]
struct NvmeDevice {
    controller_id: String,
    pcie_addr: String,
    capacity: i64,
    model: String,
}

fn parse_nvme_controller(controller: &serde_json::Value) -> Option<NvmeDevice> {
    let name = controller["name"].as_str()?;
    let pcie_addr = controller["trid"]["traddr"].as_str()?;
    
    // Get capacity from namespaces
    let namespaces = controller["namespaces"].as_array()?;
    let capacity = namespaces.iter()
        .map(|ns| ns["size"].as_u64().unwrap_or(0) as i64)
        .sum();
    
    Some(NvmeDevice {
        controller_id: name.to_string(),
        pcie_addr: pcie_addr.to_string(),
        capacity,
        model: controller["model"].as_str().unwrap_or("Unknown").to_string(),
    })
}




async fn initialize_blobstore_on_device(agent: &NodeAgent, disk: &SpdkDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let disk_name = disk.metadata.name.as_ref().unwrap();
    let lvs_name = format!("lvs_{}", disk_name);
    
    println!("🚀 [SPDK_INIT] Starting blobstore initialization for disk: {}", disk_name);
    println!("🔧 [SPDK_INIT] LVS name: {}, PCIe: {}", lvs_name, disk.spec.pcie_addr);
    
    // First, try to attach the NVMe device to SPDK if it's not already attached
    let controller_id = disk.spec.nvme_controller_id.as_deref().unwrap_or("nvme0");
    println!("🔌 [SPDK_INIT] Attempting to attach controller: {} at PCIe: {}", controller_id, disk.spec.pcie_addr);
    
    let attach_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_nvme_attach_controller",
        "params": {
            "name": controller_id,
            "trtype": "PCIe",
            "traddr": disk.spec.pcie_addr
        }
    })).await;
    
    match attach_result {
        Ok(_) => println!("✅ [SPDK_INIT] Successfully attached controller: {}", controller_id),
        Err(e) => println!("⚠️ [SPDK_INIT] Controller attach failed (may already be attached): {}", e),
    }
    
    // Wait a moment for the device to be ready
    println!("⏳ [SPDK_INIT] Waiting for device to be ready...");
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Determine bdev name and create AIO bdev if needed
    let bdev_name = if disk.spec.device_path.starts_with("/dev/") {
        // For kernel-bound devices, create an AIO bdev first
        println!("🔗 [SPDK_INIT] Creating AIO bdev for kernel device: {}", disk.spec.device_path);
        
        // Extract device name from path (e.g., "/dev/nvme1n1" -> "nvme1n1")
        let device_name = disk.spec.device_path.trim_start_matches("/dev/");
        
        let aio_bdev_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
            "method": "bdev_aio_create",
            "params": {
                "name": format!("aio_{}", device_name),
                "filename": format!("/dev/{}", device_name),
                "block_size": 4096
            }
        })).await;

        match aio_bdev_result {
            Ok(result) => {
                if let Some(error) = result.get("error") {
                    // AIO bdev might already exist, which is fine
                    if error["code"].as_i64() == Some(-17) {
                        println!("✅ [SPDK_INIT] AIO bdev already exists: aio_{}", device_name);
                    } else {
                        println!("❌ [SPDK_INIT] Failed to create AIO bdev: {}", error);
                        return Err(format!("Failed to create AIO bdev: {}", error).into());
                    }
                } else {
                    println!("✅ [SPDK_INIT] Successfully created AIO bdev: aio_{}", device_name);
                }
            }
            Err(e) => {
                println!("❌ [SPDK_INIT] Failed to create AIO bdev: {}", e);
                return Err(e);
            }
        }

        format!("aio_{}", device_name)
    } else {
        // Use SPDK controller name for SPDK-attached devices
        let name = format!("{}n1", controller_id);
        println!("🔗 [SPDK_INIT] Using SPDK bdev name: {}", name);
        name
    };

    // Now handle LVS creation with discovery-first approach
    println!("🔍 [SPDK_INIT] Checking if LVS already exists: {}", lvs_name);
    
    match call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_get_lvstores"
    })).await {
        Ok(lvstores_result) => {
            let mut our_lvs_exists = false;
            
            if let Some(lvstore_list) = lvstores_result["result"].as_array() {
                for lvstore in lvstore_list {
                    if let Some(name) = lvstore["name"].as_str() {
                        if name == lvs_name {
                            our_lvs_exists = true;
                            println!("✅ [SPDK_INIT] Found existing LVS: {}", lvs_name);
                            println!("📊 [SPDK_INIT] LVS details: {}", serde_json::to_string_pretty(lvstore).unwrap_or_default());
                            break;
                        }
                    }
                }
            }
            
            if our_lvs_exists {
                println!("✅ [SPDK_INIT] LVS already exists, updating Kubernetes status to match SPDK reality");
                
                // Update status to reflect that LVS exists
                let patch = json!({
                    "status": {
                        "blobstore_initialized": true,
                        "lvs_name": lvs_name,
                        "last_checked": Utc::now().to_rfc3339()
                    }
                });
                
                let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
                match spdk_disks.patch_status(disk_name, &PatchParams::default(), &Patch::Merge(patch)).await {
                    Ok(_) => println!("✅ [SPDK_INIT] Updated disk status to reflect existing LVS: {}", disk_name),
                    Err(e) => println!("⚠️ [SPDK_INIT] Failed to update disk status: {}", e),
                }
                
                println!("🎉 [SPDK_INIT] Blobstore initialization completed successfully (discovered existing LVS): {}", disk_name);
                return Ok(());
            } else {
                println!("🔍 [SPDK_INIT] No existing LVS found, will create new one: {}", lvs_name);
            }
        }
        Err(e) => {
            println!("⚠️ [SPDK_INIT] Failed to query existing LVS stores: {}", e);
            println!("🔄 [SPDK_INIT] Proceeding with LVS creation attempt");
        }
    }

    // Only create LVS if it doesn't already exist
    println!("🏗️ [SPDK_INIT] Creating LVS on bdev: {} with name: {}", bdev_name, lvs_name);
    let lvol_store_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_create_lvstore",
        "params": {
            "bdev_name": bdev_name,
            "lvs_name": lvs_name,
            "cluster_sz": 1048576
        }
    })).await;

    match lvol_store_result {
        Ok(result) => {
            // Check if the result contains an error
            if let Some(error) = result.get("error") {
                let error_code = error["code"].as_i64().unwrap_or(0);
                let error_msg = error["message"].as_str().unwrap_or("Unknown error");
                
                // Handle "File exists" error specially - check if our LVS actually exists
                if error_code == -17 && error_msg == "File exists" {
                    println!("⚠️ [SPDK_INIT] LVS creation reported 'File exists', checking if our LVS exists: {}", lvs_name);
                    
                    // Query existing LVS stores to see if ours exists
                    match call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                        "method": "bdev_lvol_get_lvstores"
                    })).await {
                        Ok(lvstores_result) => {
                            let mut our_lvs_exists = false;
                            
                            if let Some(lvstore_list) = lvstores_result["result"].as_array() {
                                for lvstore in lvstore_list {
                                    if let Some(name) = lvstore["name"].as_str() {
                                        if name == lvs_name {
                                            our_lvs_exists = true;
                                            println!("✅ [SPDK_INIT] Found existing LVS: {}", lvs_name);
                                            println!("📊 [SPDK_INIT] LVS details: {}", serde_json::to_string_pretty(lvstore).unwrap_or_default());
                                            break;
                                        }
                                    }
                                }
                            }
                            
                            if our_lvs_exists {
                                println!("✅ [SPDK_INIT] LVS already exists with correct name, treating as success: {}", lvs_name);
                                
                                // Update status to reflect successful initialization
                                let patch = json!({
                                    "status": {
                                        "blobstore_initialized": true,
                                        "lvs_name": lvs_name,
                                        "last_checked": Utc::now().to_rfc3339()
                                    }
                                });
                                
                                let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
                                match spdk_disks.patch_status(disk_name, &PatchParams::default(), &Patch::Merge(patch)).await {
                                    Ok(_) => println!("✅ [SPDK_INIT] Updated disk status for existing LVS: {}", disk_name),
                                    Err(e) => println!("⚠️ [SPDK_INIT] Failed to update disk status: {}", e),
                                }
                                
                                println!("🎉 [SPDK_INIT] Blobstore initialization completed successfully (LVS pre-existed): {}", disk_name);
                            } else {
                                let error_msg = format!("File exists error but our LVS '{}' not found", lvs_name);
                                println!("❌ [SPDK_INIT] {}", error_msg);
                                return Err(error_msg.into());
                            }
                        }
                        Err(e) => {
                            let error_msg = format!("Failed to query existing LVS stores: {}", e);
                            println!("❌ [SPDK_INIT] {}", error_msg);
                            return Err(error_msg.into());
                        }
                    }
                } else if error_code == -1 && error_msg == "Operation not permitted" {
                    println!("⚠️ [SPDK_INIT] LVS creation reported 'Operation not permitted', likely bdev already claimed. Checking existing LVS: {}", lvs_name);
                    
                    // Query existing LVS stores to see if ours exists (bdev might be claimed by existing LVS)
                    match call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                        "method": "bdev_lvol_get_lvstores"
                    })).await {
                        Ok(lvstores_result) => {
                            let mut our_lvs_exists = false;
                            
                            if let Some(lvstore_list) = lvstores_result["result"].as_array() {
                                for lvstore in lvstore_list {
                                    if let Some(name) = lvstore["name"].as_str() {
                                        if name == lvs_name {
                                            our_lvs_exists = true;
                                            println!("✅ [SPDK_INIT] Found existing LVS claiming the bdev: {}", lvs_name);
                                            break;
                                        }
                                    }
                                }
                            }
                            
                            if our_lvs_exists {
                                println!("✅ [SPDK_INIT] LVS already exists and claims the bdev, treating as success: {}", lvs_name);
                                
                                // Update status to reflect successful initialization
                                let patch = json!({
                                    "status": {
                                        "blobstore_initialized": true,
                                        "lvs_name": lvs_name,
                                        "last_checked": Utc::now().to_rfc3339()
                                    }
                                });
                                
                                let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
                                match spdk_disks.patch_status(disk_name, &PatchParams::default(), &Patch::Merge(patch)).await {
                                    Ok(_) => println!("✅ [SPDK_INIT] Updated disk status for existing LVS: {}", disk_name),
                                    Err(e) => println!("⚠️ [SPDK_INIT] Failed to update disk status: {}", e),
                                }
                                
                                println!("🎉 [SPDK_INIT] Blobstore initialization completed successfully (LVS already claimed bdev): {}", disk_name);
                            } else {
                                let error_msg = format!("Operation not permitted but our LVS '{}' not found - bdev might be claimed by different LVS", lvs_name);
                                println!("❌ [SPDK_INIT] {}", error_msg);
                                return Err(error_msg.into());
                            }
                        }
                        Err(e) => {
                            let error_msg = format!("Failed to query existing LVS stores after 'Operation not permitted': {}", e);
                            println!("❌ [SPDK_INIT] {}", error_msg);
                            return Err(error_msg.into());
                        }
                    }
                } else {
                    // Other SPDK errors
                    let error_msg = format!("SPDK RPC error: {}", error);
                    println!("❌ [SPDK_INIT] LVS creation failed: {}", error_msg);
                    return Err(error_msg.into());
                }
            } else {
                // Successful creation
                println!("✅ [SPDK_INIT] Successfully created LVS: {}", lvs_name);
                println!("📊 [SPDK_INIT] LVS result: {}", serde_json::to_string_pretty(&result).unwrap_or_default());
                
                // Update status to reflect successful initialization
                let patch = json!({
                    "status": {
                        "blobstore_initialized": true,
                        "lvs_name": lvs_name,
                        "last_checked": Utc::now().to_rfc3339()
                    }
                });
                
                let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
                match spdk_disks.patch_status(disk_name, &PatchParams::default(), &Patch::Merge(patch)).await {
                    Ok(_) => println!("✅ [SPDK_INIT] Updated disk status for: {}", disk_name),
                    Err(e) => println!("⚠️ [SPDK_INIT] Failed to update disk status: {}", e),
                }
                
                println!("🎉 [SPDK_INIT] Blobstore initialization completed successfully for: {}", disk_name);
            }
        }
        Err(e) => {
            println!("❌ [SPDK_INIT] Failed to create LVS on {}: {}", disk.spec.pcie_addr, e);
            eprintln!("Failed to create lvol store on {}: {}", disk.spec.pcie_addr, e);
            return Err(e);
        }
    }
    
    Ok(())
}

async fn update_existing_disk_resource(agent: &NodeAgent, disk: &SpdkDisk, device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
    let disk_name = disk.metadata.name.as_ref().unwrap();
    
    let mut needs_update = false;
    let mut updated_status = disk.status.clone().unwrap_or_default();
    
    // Update capacity if changed
    let current_capacity_gb = disk.spec.size.trim_end_matches("GB").parse::<i64>().unwrap_or(0) * (1024 * 1024 * 1024);
    if current_capacity_gb != device.capacity {
        let new_size = format!("{}GB", device.capacity / (1024 * 1024 * 1024));
        let patch = json!({
            "spec": {
                "size": new_size
            }
        });
        spdk_disks.patch(disk_name, &PatchParams::default(), &Patch::Merge(patch)).await?;
        
        // Update total capacity in status
        updated_status.total_capacity = device.capacity;
        // Adjust free space proportionally
        let usage_ratio = if updated_status.total_capacity > 0 {
            updated_status.used_space as f64 / updated_status.total_capacity as f64
        } else {
            0.0
        };
        updated_status.free_space = device.capacity - (device.capacity as f64 * usage_ratio) as i64;
        needs_update = true;
    }
    
    // Update health status
    let is_healthy = check_device_health(agent, device).await.unwrap_or(false);
    if updated_status.healthy != is_healthy {
        updated_status.healthy = is_healthy;
        needs_update = true;
    }
    
    // Initialize blobstore if needed
    if !updated_status.blobstore_initialized && agent.auto_initialize_blobstore {
        initialize_blobstore_on_device(agent, disk).await?;
        updated_status.blobstore_initialized = true;
        updated_status.lvs_name = Some(format!("lvs_{}", disk_name));
        needs_update = true;
    }
    
    if needs_update {
        updated_status.last_checked = Utc::now().to_rfc3339();
        spdk_disks
            .patch_status(disk_name, &PatchParams::default(), &Patch::Merge(json!({
                "status": updated_status
            })))
            .await?;
    }
    
    Ok(())
}

async fn check_device_health(agent: &NodeAgent, device: &NvmeDevice) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Check if device is accessible via SPDK
    let bdev_name = format!("{}n1", device.controller_id);
    let result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_bdevs",
        "params": {
            "name": bdev_name
        }
    })).await;
    
    if result.is_err() {
        return Ok(false);
    }
    
    // Additional health checks could be added here
    // - SMART data analysis
    // - Temperature monitoring
    // - Error rate checking
    
    Ok(true)
}

async fn update_disk_blobstore_status(
    agent: &NodeAgent,
    disk_name: &str,
    blobstore_initialized: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [STATUS_UPDATE] Updating blobstore status for {}: initialized={}", disk_name, blobstore_initialized);
    
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
    
    let patch = json!({
        "status": {
            "blobstore_initialized": blobstore_initialized,
            "last_checked": Utc::now().to_rfc3339()
        }
    });
    
    spdk_disks.patch_status(disk_name, &PatchParams::default(), &Patch::Merge(patch)).await?;
    
    println!("✅ [STATUS_UPDATE] Successfully updated blobstore status for: {}", disk_name);
    Ok(())
}

async fn update_disk_io_statistics(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get I/O statistics from SPDK
    let iostat = match call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_iostat"
    })).await {
        Ok(result) => result,
        Err(_) => return Ok(()), // Skip if iostat not available
    };
    
    if let Some(bdevs) = iostat["result"]["bdevs"].as_array() {
        let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
        
        for bdev in bdevs {
            if let Some(bdev_name) = bdev["name"].as_str() {
                // Find corresponding SpdkDisk by matching the bdev name pattern
                // For NVMe devices, the pattern is usually nvme0n1, nvme1n1, etc.
                if let Some(controller_part) = bdev_name.strip_suffix("n1") {
                    let disk_name = format!("{}-{}", agent.node_name, controller_part);
                    
                    if let Ok(disk) = spdk_disks.get(&disk_name).await {
                        let mut status = disk.status.unwrap_or_default();
                        
                        // Update I/O statistics
                        status.io_stats.read_iops = bdev["read_ios"].as_u64().unwrap_or(0);
                        status.io_stats.write_iops = bdev["write_ios"].as_u64().unwrap_or(0);
                        status.io_stats.read_latency_us = bdev["read_latency_ticks"].as_u64().unwrap_or(0) / 1000;
                        status.io_stats.write_latency_us = bdev["write_latency_ticks"].as_u64().unwrap_or(0) / 1000;
                        status.io_stats.error_count = bdev["io_error"].as_u64().unwrap_or(0);
                        status.last_checked = Utc::now().to_rfc3339();
                        
                        spdk_disks
                            .patch_status(&disk_name, &PatchParams::default(), &Patch::Merge(json!({
                                "status": status
                            })))
                            .await
                            .ok(); // Ignore errors for statistics updates
                    }
                }
            }
        }
        
        // Also update lvol store statistics
        update_lvol_store_statistics(agent, &spdk_disks).await?;
    }
    
    Ok(())
}

async fn update_lvol_store_statistics(
    agent: &NodeAgent,
    spdk_disks: &Api<SpdkDisk>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get lvol store information
    let lvstores = match call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_get_lvstores"
    })).await {
        Ok(result) => result,
        Err(_) => return Ok(()),
    };
    
    if let Some(stores) = lvstores["result"].as_array() {
        for store in stores {
            if let Some(lvs_name) = store["name"].as_str() {
                // Extract disk name from lvs name (format: lvs_node-controller)
                if let Some(disk_name) = lvs_name.strip_prefix("lvs_") {
                    if let Ok(disk) = spdk_disks.get(disk_name).await {
                        let mut status = disk.status.unwrap_or_default();
                        
                        // Update capacity information from lvol store
                        let total_data_clusters = store["total_data_clusters"].as_u64().unwrap_or(0);
                        let free_clusters = store["free_clusters"].as_u64().unwrap_or(0);
                        let cluster_size = store["cluster_size"].as_u64().unwrap_or(65536);
                        
                        let total_capacity = (total_data_clusters * cluster_size) as i64;
                        let free_space = (free_clusters * cluster_size) as i64;
                        let used_space = total_capacity - free_space;
                        
                        // Count logical volumes in this store
                        let lvol_count = store["lvols"].as_array().map(|v| v.len()).unwrap_or(0) as u32;
                        
                        status.total_capacity = total_capacity;
                        status.free_space = free_space;
                        status.used_space = used_space;
                        status.lvol_count = lvol_count;
                        status.last_checked = Utc::now().to_rfc3339();
                        
                        spdk_disks
                            .patch_status(disk_name, &PatchParams::default(), &Patch::Merge(json!({
                                "status": status
                            })))
                            .await
                            .ok();
                    }
                }
            }
        }
    }
    
    Ok(())
}

// Disk setup implementation methods for NodeAgent
impl NodeAgent {
    async fn discover_all_disks(&self) -> Result<Vec<UnimplementedDisk>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Starting discover_all_disks for node: {}", self.node_name);
        let mut all_disks = Vec::new();
        
        // Get all NVMe PCI devices
        let pci_devices = self.get_nvme_pci_devices().await?;
        println!("🔍 [DISCOVERY] Processing {} PCI devices...", pci_devices.len());
        
        for pci_addr in pci_devices {
            println!("🔍 [DISCOVERY] Processing PCI device: {}", pci_addr);
            match self.get_disk_info(&pci_addr).await {
                Ok(disk_info) => {
                    println!("✅ [DISCOVERY] Successfully got disk info for {}: name='{}', driver='{}', spdk_ready={}, is_system={}", 
                             pci_addr, disk_info.device_name, disk_info.driver, disk_info.spdk_ready, disk_info.is_system_disk);
                    all_disks.push(disk_info);
                }
                Err(e) => {
                    println!("❌ [DISCOVERY] Failed to get disk info for {}: {}", pci_addr, e);
                }
            }
        }
        
        println!("🔍 [DISCOVERY] Discovery completed: {} total disks found", all_disks.len());
        for (i, disk) in all_disks.iter().enumerate() {
            println!("🔍 [DISCOVERY]   Disk {}: {} (PCI: {}, Driver: {}, System: {}, SPDK Ready: {})", 
                     i+1, disk.device_name, disk.pci_address, disk.driver, disk.is_system_disk, disk.spdk_ready);
        }
        
        Ok(all_disks)
    }

    async fn get_nvme_pci_devices(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Scanning for NVMe PCI devices using lspci...");
        
        let output = Command::new("lspci")
            .args(["-D", "-d", "::0108"]) // NVMe class code
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        let mut devices = Vec::new();

        println!("🔍 [DISCOVERY] lspci output:");
        for line in stdout.lines() {
            println!("🔍 [DISCOVERY]   {}", line);
            if let Some(pci_addr) = line.split_whitespace().next() {
                devices.push(pci_addr.to_string());
                println!("🔍 [DISCOVERY] Found PCI device: {}", pci_addr);
            }
        }

        println!("🔍 [DISCOVERY] Total NVMe PCI devices found: {}", devices.len());
        Ok(devices)
    }

    async fn get_disk_info(&self, pci_addr: &str) -> Result<UnimplementedDisk, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISK_INFO] Getting disk info for PCI address: {}", pci_addr);
        let sysfs_path = format!("/sys/bus/pci/devices/{}", pci_addr);
        
        // Read PCI device information
        println!("🔍 [DISK_INFO] Reading PCI device information from: {}", sysfs_path);
        let vendor_id = self.read_sysfs_file(&format!("{}/vendor", sysfs_path)).await?;
        let device_id = self.read_sysfs_file(&format!("{}/device", sysfs_path)).await?;
        let subsystem_vendor = self.read_sysfs_file(&format!("{}/subsystem_vendor", sysfs_path)).await
            .unwrap_or_else(|_| vendor_id.clone());
        let subsystem_device = self.read_sysfs_file(&format!("{}/subsystem_device", sysfs_path)).await
            .unwrap_or_else(|_| device_id.clone());
        
        println!("🔍 [DISK_INFO] PCI Info - Vendor: {}, Device: {}", vendor_id.trim(), device_id.trim());
        
        // Get NUMA node
        let numa_node = self.read_sysfs_file(&format!("{}/numa_node", sysfs_path)).await
            .ok()
            .and_then(|s| s.trim().parse().ok());
        println!("🔍 [DISK_INFO] NUMA node: {:?}", numa_node);

        // Get current driver
        let driver = self.get_current_driver(pci_addr).await?;
        println!("🔍 [DISK_INFO] Current driver: '{}'", driver);
        
        // Get device information - for unbound devices, use PCI info and reasonable defaults
        let (device_name, size_bytes, model, serial, firmware_version, mounted_partitions, is_system_disk) = 
            if driver == "unbound" {
                println!("🔍 [DISK_INFO] Device is unbound, using PCI-based detection");
                // For unbound devices, use PCI address as device identifier
                let device_name = format!("nvme-{}", pci_addr.replace(":", "-"));
                
                // Get estimated size from PCI config or use reasonable default for NVMe
                let estimated_size = self.estimate_nvme_size_from_pci(pci_addr).await.unwrap_or(1_000_000_000_000); // 1TB default
                
                // Get model name from vendor/device ID lookup
                let model = self.get_model_from_pci_ids(&vendor_id, &device_id).await;
                
                println!("🔍 [DISK_INFO] Unbound device info - Name: {}, Size: {} bytes, Model: {}", 
                         device_name, estimated_size, model);
                
                (
                    device_name,
                    estimated_size,
                    model,
                    "Unknown".to_string(), // Serial not available without binding
                    "Unknown".to_string(), // Firmware not available without binding
                    Vec::new(), // No mounted partitions for unbound devices
                    false, // Unbound devices are never system disks
                )
            } else {
                println!("🔍 [DISK_INFO] Device is bound to '{}', getting block device information", driver);
                // For bound devices, get the actual block device information
                let device_name = self.find_nvme_device_name(pci_addr).await?;
                println!("🔍 [DISK_INFO] Found block device name: {}", device_name);
                let (size_bytes, model, serial, firmware_version) = self.get_nvme_details(&device_name).await?;
                let mounted_partitions = self.get_mounted_partitions(&device_name).await?;
                let is_system_disk = self.is_system_disk(&device_name, &mounted_partitions).await?;
                println!("🔍 [DISK_INFO] Bound device info - Name: {}, Size: {} bytes, Model: {}, Mounted: {:?}, System: {}", 
                         device_name, size_bytes, model, mounted_partitions, is_system_disk);
                (device_name, size_bytes, model, serial, firmware_version, mounted_partitions, is_system_disk)
            };
        
        // Determine if SPDK ready (supports both userspace and kernel-bound modes)
        let spdk_ready = self.is_spdk_compatible_driver(&driver) || 
                        (self.is_virtualized_environment().await.unwrap_or(false) && 
                         driver == "nvme" && !is_system_disk);
        
        println!("🔍 [DISK_INFO] SPDK compatibility - Driver compatible: {}, Is system: {}, SPDK ready: {}", 
                 self.is_spdk_compatible_driver(&driver), is_system_disk, spdk_ready);

        let disk_info = UnimplementedDisk {
            pci_address: pci_addr.to_string(),
            device_name,
            vendor_id: vendor_id.trim().to_string(),
            device_id: device_id.trim().to_string(),
            subsystem_vendor_id: subsystem_vendor.trim().to_string(),
            subsystem_device_id: subsystem_device.trim().to_string(),
            numa_node,
            driver,
            size_bytes,
            model,
            serial,
            firmware_version,
            namespace_id: Some(1),
            mounted_partitions,
            filesystem_type: None,
            is_system_disk,
            spdk_ready,
            discovered_at: Utc::now().to_rfc3339(),
        };
        
        println!("✅ [DISK_INFO] Completed disk info for {}: {}", pci_addr, disk_info.device_name);
        Ok(disk_info)
    }

    async fn read_sysfs_file(&self, path: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(fs::read_to_string(path)?)
    }

    async fn get_current_driver(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let driver_path = format!("/sys/bus/pci/devices/{}/driver", pci_addr);
        
        match fs::read_link(&driver_path) {
            Ok(link) => {
                if let Some(driver_name) = link.file_name() {
                    Ok(driver_name.to_string_lossy().to_string())
                } else {
                    Ok("unknown".to_string())
                }
            }
            Err(_) => Ok("unbound".to_string()),
        }
    }

    async fn find_nvme_device_name(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let nvme_path = format!("/sys/bus/pci/devices/{}/nvme", pci_addr);
        
        if let Ok(entries) = fs::read_dir(&nvme_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("nvme") {
                        return Ok(format!("{}n1", name));
                    }
                }
            }
        }
        
        // Fallback: try to find by PCI address in /sys/block
        for entry in fs::read_dir("/sys/block")? {
            let entry = entry?;
            let device_name = entry.file_name().to_string_lossy().to_string();
            
            if device_name.starts_with("nvme") {
                let device_path = format!("/sys/block/{}/device", device_name);
                if let Ok(real_path) = fs::read_link(&device_path) {
                    if real_path.to_string_lossy().contains(pci_addr) {
                        return Ok(device_name);
                    }
                }
            }
        }
        
        Err("NVMe device not found".into())
    }

    async fn get_nvme_details(&self, device_name: &str) -> Result<(u64, String, String, String), Box<dyn std::error::Error + Send + Sync>> {
        // Use nvme-cli to get device information
        let output = Command::new("nvme")
            .args(["id-ctrl", &format!("/dev/{}", device_name)])
            .output();

        let (model, serial, firmware_version) = if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let model = self.extract_nvme_field(&stdout, "mn").unwrap_or("Unknown".to_string());
            let serial = self.extract_nvme_field(&stdout, "sn").unwrap_or("Unknown".to_string());
            let firmware = self.extract_nvme_field(&stdout, "fr").unwrap_or("Unknown".to_string());
            (model, serial, firmware)
        } else {
            // Fallback to sysfs
            let model = self.read_sysfs_file(&format!("/sys/block/{}/device/model", device_name)).await
                .unwrap_or("Unknown".to_string());
            (model.trim().to_string(), "Unknown".to_string(), "Unknown".to_string())
        };

        // Get size from blockdev
        let size_bytes = self.get_device_size(device_name).await?;

        Ok((size_bytes, model, serial, firmware_version))
    }

    fn extract_nvme_field(&self, nvme_output: &str, field: &str) -> Option<String> {
        let pattern = format!(r"{}\s*:\s*(.+)", field);
        let re = Regex::new(&pattern).ok()?;
        
        if let Some(captures) = re.captures(nvme_output) {
            Some(captures[1].trim().to_string())
        } else {
            None
        }
    }

    async fn get_device_size(&self, device_name: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("blockdev")
            .args(["--getsize64", &format!("/dev/{}", device_name)])
            .output()?;

        let size_str = String::from_utf8(output.stdout)?;
        Ok(size_str.trim().parse()?)
    }

    async fn estimate_nvme_size_from_pci(&self, pci_addr: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        // Try to get size information from PCI configuration or use lspci
        let output = Command::new("lspci")
            .args(["-v", "-s", pci_addr])
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        
        // Look for memory regions that might indicate device size
        // This is a rough estimation since NVMe size isn't directly in PCI config
        for line in stdout.lines() {
            if line.contains("Memory at") && line.contains("size=") {
                // Extract size if available, but for NVMe this is typically not the storage size
                // Fall back to common NVMe sizes
                return Ok(1_000_000_000_000); // 1TB default
            }
        }
        
        // For AWS EBS NVMe devices, try to use common sizes
        if stdout.contains("Amazon") {
            // This could be enhanced to detect EBS volume sizes
            return Ok(1_000_000_000_000); // 1TB default for unbound EBS volumes
        }
        
        Ok(1_000_000_000_000) // 1TB default
    }

    async fn get_model_from_pci_ids(&self, vendor_id: &str, device_id: &str) -> String {
        // Convert hex IDs to model names for common vendors
        match (vendor_id.trim(), device_id.trim()) {
            ("0x1d0f", "0x8061") => "Amazon Elastic Block Store".to_string(),
            ("0x144d", _) => "Samsung NVMe SSD".to_string(),
            ("0x15b7", _) => "SanDisk NVMe SSD".to_string(),
            ("0x1344", _) => "Micron NVMe SSD".to_string(),
            ("0x1179", _) => "Toshiba NVMe SSD".to_string(),
            ("0x1c5c", _) => "SK Hynix NVMe SSD".to_string(),
            ("0x1987", _) => "Phison NVMe SSD".to_string(),
            ("0x1bb1", _) => "Seagate NVMe SSD".to_string(),
            ("0x1f40", _) => "NETAC NVMe SSD".to_string(),
            ("0x10ec", _) => "Realtek NVMe SSD".to_string(),
            ("0x8086", _) => "Intel NVMe SSD".to_string(),
            ("0x1cc1", _) => "ADATA NVMe SSD".to_string(),
            _ => format!("NVMe SSD ({}:{})", vendor_id.trim(), device_id.trim()),
        }
    }

    async fn get_mounted_partitions(&self, device_name: &str) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("lsblk")
            .args(["-ln", "-o", "NAME,MOUNTPOINT", &format!("/dev/{}", device_name)])
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        let mut mounted = Vec::new();

        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && !parts[1].is_empty() {
                mounted.push(parts[1].to_string());
            }
        }

        Ok(mounted)
    }

    async fn is_system_disk(&self, device_name: &str, mounted_partitions: &[String]) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Check if any partition is mounted on critical system paths
        let critical_mounts = ["/", "/boot", "/boot/efi", "/var", "/usr", "/home"];
        
        for mount in mounted_partitions {
            if critical_mounts.contains(&mount.as_str()) {
                return Ok(true);
            }
        }

        // Check for containerized system file bind mounts (common in Kubernetes)
        let container_system_mounts = [
            "/etc/resolv.conf", "/etc/hosts", "/etc/hostname", 
            "/etc/passwd", "/etc/group", "/etc/shadow",
            "/dev/termination-log"
        ];
        
        for mount in mounted_partitions {
            if container_system_mounts.contains(&mount.as_str()) {
                println!("Detected system disk {} due to container system mount: {}", device_name, mount);
                return Ok(true);
            }
        }

        // Check if device contains the root filesystem
        let output = Command::new("findmnt")
            .args(["-n", "-o", "SOURCE", "/"])
            .output()?;

        let root_device = String::from_utf8(output.stdout)?;
        if root_device.contains(device_name) {
            return Ok(true);
        }

        // Additional check: see if this device is mounted to critical system paths
        // by examining all mounts of this device
        let mount_output = Command::new("mount")
            .output()?;
        
        if mount_output.status.success() {
            let mount_info = String::from_utf8(mount_output.stdout)?;
            for line in mount_info.lines() {
                if line.contains(device_name) {
                    // Check if this device is mounted to any critical system location
                    for critical_mount in &critical_mounts {
                        if line.contains(&format!(" on {} ", critical_mount)) {
                            println!("Detected system disk {} mounted on critical path: {}", device_name, critical_mount);
                            return Ok(true);
                        }
                    }
                }
            }
        }

        Ok(false)
    }

    fn is_spdk_compatible_driver(&self, driver: &str) -> bool {
        matches!(driver, "vfio-pci" | "uio_pci_generic" | "igb_uio")
    }

    async fn setup_disks_for_spdk(&self, request: DiskSetupRequest) -> Result<DiskSetupResult, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SETUP_HANDLER] Starting setup_disks_for_spdk on node: {}", self.node_name);
        println!("🔧 [SETUP_HANDLER] Request details:");
        println!("   - PCI addresses: {:?}", request.pci_addresses);
        println!("   - Force unmount: {}", request.force_unmount);
        println!("   - Backup data: {}", request.backup_data);
        println!("   - Huge pages MB: {:?}", request.huge_pages_mb);
        println!("   - Driver override: {:?}", request.driver_override);
        
        let mut result = DiskSetupResult {
            success: true,
            setup_disks: Vec::new(),
            failed_disks: Vec::new(),
            warnings: Vec::new(),
            huge_pages_configured: None,
            completed_at: Utc::now().to_rfc3339(),
        };

        // Validate all disks first
        println!("🔧 [SETUP_HANDLER] Step 1: Validating {} disks...", request.pci_addresses.len());
        for pci_addr in &request.pci_addresses {
            println!("🔧 [SETUP_HANDLER] Validating disk: {}", pci_addr);
            if let Err(e) = self.validate_disk_for_setup(pci_addr, request.force_unmount).await {
                println!("❌ [SETUP_HANDLER] Validation failed for {}: {}", pci_addr, e);
                result.failed_disks.push((pci_addr.clone(), e.to_string()));
                result.success = false;
                continue;
            } else {
                println!("✅ [SETUP_HANDLER] Validation passed for: {}", pci_addr);
            }
        }

        if !result.success && !request.force_unmount {
            println!("❌ [SETUP_HANDLER] Validation failed and force_unmount=false, aborting");
            return Ok(result);
        }

        // Setup huge pages (always configure for SPDK optimization)
        let hugepage_mb = request.huge_pages_mb.unwrap_or(0); // 0 will trigger auto-calculation
        match self.setup_huge_pages(hugepage_mb).await {
            Ok(configured) => {
                result.huge_pages_configured = Some(configured);
                if hugepage_mb == 0 {
                    result.warnings.push(format!("Auto-configured {}MB hugepages for optimal SPDK performance", configured));
                }
            }
            Err(e) => {
                result.warnings.push(format!("Huge pages setup warning: {}", e));
            }
        }

        // Process each disk
        println!("🔧 [SETUP_HANDLER] Step 3: Processing {} disks for setup...", request.pci_addresses.len());
        for pci_addr in &request.pci_addresses {
            if result.failed_disks.iter().any(|(addr, _)| addr == pci_addr) {
                println!("⏭️  [SETUP_HANDLER] Skipping already failed disk: {}", pci_addr);
                continue;
            }

            println!("🔧 [SETUP_HANDLER] Setting up disk: {}", pci_addr);
            match self.setup_single_disk(pci_addr, &request).await {
                Ok(_) => {
                    println!("✅ [SETUP_HANDLER] Successfully set up disk: {}", pci_addr);
                    result.setup_disks.push(pci_addr.clone());
                }
                Err(e) => {
                    println!("❌ [SETUP_HANDLER] Failed to set up disk {}: {}", pci_addr, e);
                    result.failed_disks.push((pci_addr.clone(), e.to_string()));
                    result.success = false;
                }
            }
        }

        result.completed_at = Utc::now().to_rfc3339();
        
        println!("🔧 [SETUP_HANDLER] Setup completed. Final result:");
        println!("   - Success: {}", result.success);
        println!("   - Setup disks: {:?}", result.setup_disks);
        println!("   - Failed disks: {:?}", result.failed_disks);
        println!("   - Warnings: {:?}", result.warnings);
        println!("   - Huge pages configured: {:?}", result.huge_pages_configured);
        
        Ok(result)
    }

    async fn validate_disk_for_setup(&self, pci_addr: &str, force_unmount: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [VALIDATION] Validating disk for setup: {}", pci_addr);
        println!("🔍 [VALIDATION] Force unmount: {}", force_unmount);
        
        // Check if PCI device exists
        let sysfs_path = format!("/sys/bus/pci/devices/{}", pci_addr);
        println!("🔍 [VALIDATION] Checking PCI device path: {}", sysfs_path);
        if !std::path::Path::new(&sysfs_path).exists() {
            let error_msg = format!("PCI device {} does not exist", pci_addr);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        println!("✅ [VALIDATION] PCI device path exists");
        
        // Get current driver
        let current_driver = self.get_current_driver(pci_addr).await?;
        println!("🔍 [VALIDATION] Current driver: '{}'", current_driver);
        
        // Get disk information
        let disk_info = self.get_disk_info(pci_addr).await?;
        println!("🔍 [VALIDATION] Disk info - Name: {}, Driver: {}, System: {}, SPDK Ready: {}", 
                 disk_info.device_name, disk_info.driver, disk_info.is_system_disk, disk_info.spdk_ready);
        
        // Check if it's a system disk
        if disk_info.is_system_disk {
            let error_msg = format!("Cannot setup system disk: {} ({})", pci_addr, disk_info.device_name);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        println!("✅ [VALIDATION] Not a system disk");
        
        // Check mounted partitions
        if !disk_info.mounted_partitions.is_empty() && !force_unmount {
            let error_msg = format!("Disk has mounted partitions: {:?}. Use force_unmount=true to proceed", disk_info.mounted_partitions);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        
        if !disk_info.mounted_partitions.is_empty() && force_unmount {
            println!("⚠️ [VALIDATION] Disk has mounted partitions but force_unmount=true: {:?}", disk_info.mounted_partitions);
        } else {
            println!("✅ [VALIDATION] No mounted partitions to worry about");
        }
        
        // For unbound devices, we can't check block device files since they don't exist yet
        if current_driver == "unbound" {
            println!("✅ [VALIDATION] Device is unbound - validation passed (no block device to check)");
            return Ok(());
        }
        
        // For bound devices, validate the block device exists and is accessible
        let device_path = format!("/dev/{}", disk_info.device_name);
        println!("🔍 [VALIDATION] Checking block device path: {}", device_path);
        if !std::path::Path::new(&device_path).exists() {
            let error_msg = format!("Block device {} does not exist", device_path);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        println!("✅ [VALIDATION] Block device path exists");
        
        println!("✅ [VALIDATION] All validation checks passed for: {}", pci_addr);
        Ok(())
    }

    async fn setup_single_disk(&self, pci_addr: &str, request: &DiskSetupRequest) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SETUP] Starting disk setup for PCI address: {}", pci_addr);
        println!("🔧 [SETUP] Request parameters: force_unmount={}, backup_data={}, huge_pages_mb={:?}, driver_override={:?}", 
                 request.force_unmount, request.backup_data, request.huge_pages_mb, request.driver_override);
        
        let disk_info = self.get_disk_info(pci_addr).await?;
        println!("🔧 [SETUP] Disk info retrieved:");
        println!("   - Device name: {}", disk_info.device_name);
        println!("   - Driver: '{}'", disk_info.driver);
        println!("   - Size: {} bytes", disk_info.size_bytes);
        println!("   - Model: {}", disk_info.model);
        println!("   - Is system disk: {}", disk_info.is_system_disk);
        println!("   - SPDK ready: {}", disk_info.spdk_ready);
        println!("   - Mounted partitions: {:?}", disk_info.mounted_partitions);

        // Validate the disk can be set up
        if disk_info.is_system_disk {
            println!("❌ [SETUP] Cannot setup system disk for SPDK");
            return Err("Cannot setup system disk for SPDK".into());
        }

        if disk_info.spdk_ready {
            println!("❌ [SETUP] Disk is already setup for SPDK");
            return Err("Disk is already setup for SPDK".into());
        }

        // Step 1: If device is bound to nvme and has mounted partitions, handle them
        println!("🔧 [SETUP] Step 1: Checking mounted partitions...");
        if disk_info.driver == "nvme" && !disk_info.mounted_partitions.is_empty() {
            println!("⚠️  [SETUP] Device has mounted partitions: {:?}", disk_info.mounted_partitions);
            if !request.force_unmount {
                println!("❌ [SETUP] force_unmount=false, cannot proceed with mounted partitions");
                return Err(format!("Disk has mounted partitions: {:?}. Use force_unmount=true to proceed", disk_info.mounted_partitions).into());
            }

            println!("🔧 [SETUP] force_unmount=true, proceeding with unmounting...");
            // Backup data if requested
            if request.backup_data {
                println!("🔧 [SETUP] Backing up disk data...");
                self.backup_disk_data(&disk_info).await?;
                println!("✅ [SETUP] Disk data backup completed");
            }

            // Unmount all partitions
            println!("🔧 [SETUP] Unmounting disk partitions...");
            self.unmount_disk_partitions(&disk_info).await?;
            println!("✅ [SETUP] Disk partitions unmounted");
        } else {
            println!("✅ [SETUP] No mounted partitions to handle");
        }

        // AWS/Virtualized Environment: Use kernel-bound mode instead of userspace drivers
        println!("🔧 [SETUP] Step 2: Checking environment type...");
        let is_virtualized = self.is_virtualized_environment().await?;
        println!("🔧 [SETUP] Virtualized environment: {}", is_virtualized);
        
        if is_virtualized {
            let should_use_kernel = self.should_use_kernel_mode(&disk_info).await?;
            println!("🔧 [SETUP] Should use kernel mode: {}", should_use_kernel);
            
            if should_use_kernel {
                println!("🔧 [SETUP] Using kernel-bound mode for AWS/virtualized compatibility");
                return self.setup_kernel_bound_disk(pci_addr, &disk_info).await;
            } else {
                println!("🔧 [SETUP] Kernel mode not suitable, falling back to userspace drivers");
            }
        } else {
            println!("🔧 [SETUP] Bare metal environment detected, using userspace drivers");
        }

        // Traditional bare metal path: Use userspace drivers
        println!("🔧 [SETUP] Step 3: Traditional bare metal userspace driver setup");
        
        // Step 3a: Unbind from current driver (if bound)
        println!("🔧 [SETUP] Step 3a: Checking current driver binding...");
        if disk_info.driver != "unbound" {
            println!("🔧 [SETUP] Unbinding from current driver: {}", disk_info.driver);
            self.unbind_from_driver(pci_addr, &disk_info.driver).await?;
            println!("✅ [SETUP] Successfully unbound from driver: {}", disk_info.driver);
        } else {
            println!("✅ [SETUP] Device already unbound, no unbinding needed");
        }

        // Step 3b: Choose optimal driver for environment
        println!("🔧 [SETUP] Step 3b: Selecting optimal SPDK driver...");
        let target_driver = if let Some(override_driver) = &request.driver_override {
            println!("🔧 [SETUP] Using driver override: {}", override_driver);
            override_driver.clone()
        } else {
            let selected_driver = self.select_optimal_spdk_driver().await?;
            println!("🔧 [SETUP] Auto-selected driver: {}", selected_driver);
            selected_driver
        };

        // Step 3c: Load target driver module
        println!("🔧 [SETUP] Step 3c: Loading driver module: {}", target_driver);
        self.load_driver_module(&target_driver).await?;
        println!("✅ [SETUP] Driver module loaded successfully");

        // Step 3d: Bind to SPDK-compatible driver
        println!("🔧 [SETUP] Step 3d: Binding to SPDK driver: {}", target_driver);
        self.bind_to_driver(pci_addr, &target_driver).await?;
        println!("✅ [SETUP] Successfully bound to driver: {}", target_driver);

        // Step 3e: Verify setup
        println!("🔧 [SETUP] Step 3e: Verifying SPDK setup...");
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.verify_spdk_setup(pci_addr).await?;
        println!("✅ [SETUP] SPDK setup verification completed successfully");

        println!("🎉 [SETUP] Disk setup completed successfully for PCI address: {}", pci_addr);
        Ok(())
    }

    /// Select the optimal SPDK userspace driver for the current environment
    async fn select_optimal_spdk_driver(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Check if we're in a virtualized environment
        let is_virtualized = self.is_virtualized_environment().await?;
        
        if is_virtualized {
            // In VMs, prefer vfio-pci for security isolation
            if self.is_driver_available("vfio-pci").await? {
                return Ok("vfio-pci".to_string());
            }
        } else {
            // On bare metal, prioritize non-IOMMU drivers for performance
            
            // 1st choice: uio_pci_generic (most common, no IOMMU needed)
            if self.is_driver_available("uio_pci_generic").await? {
                println!("Using uio_pci_generic for bare metal SPDK (no IOMMU required)");
                return Ok("uio_pci_generic".to_string());
            }
            
            // 2nd choice: igb_uio (better compatibility than uio_pci_generic)
            if self.is_driver_available("igb_uio").await? {
                println!("Using igb_uio for bare metal SPDK (no IOMMU required)");
                return Ok("igb_uio".to_string());
            }
            
            // 3rd choice: vfio-pci with no-IOMMU mode
            if self.is_vfio_noiommu_available().await? {
                println!("Using vfio-pci in no-IOMMU mode for bare metal");
                return Ok("vfio-pci".to_string());
            }
        }

        // Fallback to vfio-pci (will likely fail if no IOMMU)
        Ok("vfio-pci".to_string())
    }

    /// Check if we're running in a virtualized environment
    async fn is_virtualized_environment(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Check common virtualization indicators
        
        // 1. Check DMI system information
        if let Ok(product_name) = fs::read_to_string("/sys/class/dmi/id/product_name") {
            let product = product_name.trim().to_lowercase();
            if product.contains("virtualbox") || 
               product.contains("vmware") || 
               product.contains("qemu") ||
               product.contains("kvm") ||
               product.contains("xen") ||
               product.contains("amazon ec2") {
                return Ok(true);
            }
        }

        // 2. Check for hypervisor flag in CPU
        if let Ok(output) = Command::new("grep")
            .args(["-q", "hypervisor", "/proc/cpuinfo"])
            .output() {
            if output.status.success() {
                return Ok(true);
            }
        }

        // 3. Check for virtualization in systemd-detect-virt
        if let Ok(output) = Command::new("systemd-detect-virt")
            .output() {
            if output.status.success() {
                let virt_type = String::from_utf8_lossy(&output.stdout).trim().to_string();
                return Ok(virt_type != "none");
            }
        }

        // Default to bare metal if detection fails
        Ok(false)
    }

    /// Check if a specific kernel driver is available
    async fn is_driver_available(&self, driver: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Try to load the module (won't load if already loaded)
        let output = Command::new("modprobe")
            .arg("--dry-run")
            .arg(driver)
            .output()?;

        Ok(output.status.success())
    }

    /// Check if VFIO no-IOMMU mode is available
    async fn is_vfio_noiommu_available(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Check if vfio-pci is available
        if !self.is_driver_available("vfio-pci").await? {
            return Ok(false);
        }

        // Check if no-IOMMU mode can be enabled
        let noiommu_path = "/sys/module/vfio/parameters/enable_unsafe_noiommu_mode";
        if Path::new(noiommu_path).exists() {
            // Try to enable no-IOMMU mode
            if fs::write(noiommu_path, "1").is_ok() {
                return Ok(true);
            }
        }

        // Try loading vfio with no-IOMMU parameter
        let output = Command::new("modprobe")
            .args(["vfio", "enable_unsafe_noiommu_mode=1"])
            .output()?;

        Ok(output.status.success())
    }

    async fn backup_disk_data(&self, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let backup_dir = format!("{}/{}", self.backup_path, disk_info.pci_address.replace(":", "_"));
        fs::create_dir_all(&backup_dir)?;

        let backup_file = format!("{}/disk_backup_{}.img", backup_dir, 
            chrono::Utc::now().format("%Y%m%d_%H%M%S"));

        let output = Command::new("dd")
            .args([
                &format!("if=/dev/{}", disk_info.device_name),
                &format!("of={}", backup_file),
                "bs=1M",
                "count=1024", // Backup first 1GB
                "status=progress"
            ])
            .output()?;

        if !output.status.success() {
            return Err(format!("Backup failed: {}", String::from_utf8_lossy(&output.stderr)).into());
        }

        println!("Backed up disk data to: {}", backup_file);
        Ok(())
    }

    async fn unmount_disk_partitions(&self, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("lsblk")
            .args(["-ln", "-o", "NAME", &format!("/dev/{}", disk_info.device_name)])
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        
        for line in stdout.lines() {
            let partition = line.trim();
            if partition != disk_info.device_name && partition.starts_with(&disk_info.device_name) {
                let unmount_result = Command::new("umount")
                    .args(["-f", &format!("/dev/{}", partition)])
                    .output();

                if let Ok(output) = unmount_result {
                    if !output.status.success() {
                        let error = String::from_utf8_lossy(&output.stderr);
                        if !error.contains("not mounted") {
                            eprintln!("Warning: Failed to unmount /dev/{}: {}", partition, error);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn unbind_from_driver(&self, pci_addr: &str, driver: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let unbind_path = format!("/sys/bus/pci/drivers/{}/unbind", driver);
        
        if Path::new(&unbind_path).exists() {
            fs::write(&unbind_path, pci_addr)?;
            
            // Wait for unbind to complete
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if self.get_current_driver(pci_addr).await? == "unbound" {
                    break;
                }
            }
        }

        Ok(())
    }

    async fn load_driver_module(&self, driver: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if module is already loaded
        let output = Command::new("lsmod")
            .output()?;

        let modules = String::from_utf8(output.stdout)?;
        if modules.contains(driver) {
            return Ok(());
        }

        // Load the module
        let output = Command::new("modprobe")
            .arg(driver)
            .output()?;

        if !output.status.success() {
            return Err(format!("Failed to load driver module {}: {}", 
                driver, String::from_utf8_lossy(&output.stderr)).into());
        }

        Ok(())
    }

    async fn bind_to_driver(&self, pci_addr: &str, driver: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Enable VFIO if using vfio-pci
        if driver == "vfio-pci" {
            self.enable_vfio(pci_addr).await?;
        }

        let bind_path = format!("/sys/bus/pci/drivers/{}/bind", driver);
        
        if !Path::new(&bind_path).exists() {
            return Err(format!("Driver {} bind path not found", driver).into());
        }

        // Write PCI address to bind file
        fs::write(&bind_path, pci_addr)?;

        // Wait for bind to complete
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.get_current_driver(pci_addr).await? == driver {
                return Ok(());
            }
        }

        Err(format!("Failed to bind {} to {}", pci_addr, driver).into())
    }

    async fn enable_vfio(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Read vendor and device IDs
        let vendor_id = self.read_sysfs_file(&format!("/sys/bus/pci/devices/{}/vendor", pci_addr)).await?;
        let device_id = self.read_sysfs_file(&format!("/sys/bus/pci/devices/{}/device", pci_addr)).await?;

        // Create VFIO device ID
        let device_id_str = format!("{} {}", vendor_id.trim(), device_id.trim());

        // Add to VFIO new_id
        let new_id_path = "/sys/bus/pci/drivers/vfio-pci/new_id";
        if Path::new(new_id_path).exists() {
            let _ = fs::write(new_id_path, &device_id_str);
        }

        Ok(())
    }

    async fn verify_spdk_setup(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let current_driver = self.get_current_driver(pci_addr).await?;
        
        if !self.is_spdk_compatible_driver(&current_driver) {
            return Err(format!("Disk setup verification failed. Current driver: {}", current_driver).into());
        }

        Ok(())
    }

    /// Check if disk should use kernel-bound mode (AWS/virtualized environments)
    async fn should_use_kernel_mode(&self, disk_info: &UnimplementedDisk) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [KERNEL_MODE] Evaluating kernel mode criteria:");
        println!("   - Is system disk: {}", disk_info.is_system_disk);
        println!("   - Current driver: '{}'", disk_info.driver);
        println!("   - Driver is nvme: {}", disk_info.driver == "nvme");
        println!("   - Driver is empty: {}", disk_info.driver.is_empty());
        println!("   - Driver is unbound: {}", disk_info.driver == "unbound");
        
        let not_system_disk = !disk_info.is_system_disk;
        let driver_compatible = disk_info.driver == "nvme" || disk_info.driver.is_empty() || disk_info.driver == "unbound";
        let result = not_system_disk && driver_compatible;
        
        println!("🔍 [KERNEL_MODE] Decision logic:");
        println!("   - Not system disk: {}", not_system_disk);
        println!("   - Driver compatible: {}", driver_compatible);
        println!("   - Final result: {}", result);
        
        Ok(result)
    }

    /// Setup disk for SPDK using kernel-bound mode (no driver binding)
    async fn setup_kernel_bound_disk(&self, pci_addr: &str, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [KERNEL_SETUP] Starting kernel-bound disk setup for PCI address: {}", pci_addr);
        println!("🔧 [KERNEL_SETUP] Device info: name={}, driver='{}', size={}", 
                 disk_info.device_name, disk_info.driver, disk_info.size_bytes);
        
        // Step 1: Ensure disk is bound to nvme driver (if unbound)
        println!("🔧 [KERNEL_SETUP] Step 1: Checking driver binding...");
        if disk_info.driver.is_empty() || disk_info.driver == "unbound" {
            println!("🔧 [KERNEL_SETUP] Device is unbound, binding {} to nvme driver", pci_addr);
            match self.bind_to_driver(pci_addr, "nvme").await {
                Ok(_) => {
                    println!("✅ [KERNEL_SETUP] Successfully bound to nvme driver");
                    // Wait for block device to appear
                    println!("🔧 [KERNEL_SETUP] Waiting 2 seconds for block device to appear...");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    println!("❌ [KERNEL_SETUP] Failed to bind to nvme driver: {}", e);
                    return Err(format!("Failed to bind {} to nvme driver: {}", pci_addr, e).into());
                }
            }
        } else {
            println!("✅ [KERNEL_SETUP] Device already bound to driver: {}", disk_info.driver);
        }
        
        // Step 2: Ensure the device is accessible via block device
        println!("🔧 [KERNEL_SETUP] Step 2: Verifying block device access...");
        let block_device = format!("/dev/{}", disk_info.device_name);
        println!("🔧 [KERNEL_SETUP] Checking if block device exists: {}", block_device);
        
        if !std::path::Path::new(&block_device).exists() {
            println!("❌ [KERNEL_SETUP] Block device {} not found", block_device);
            // Try to find the actual device name
            println!("🔧 [KERNEL_SETUP] Searching for actual device name...");
            match self.find_nvme_device_name(pci_addr).await {
                Ok(actual_name) => {
                    println!("🔧 [KERNEL_SETUP] Found actual device name: {}", actual_name);
                    let actual_block_device = format!("/dev/{}", actual_name);
                    if std::path::Path::new(&actual_block_device).exists() {
                        println!("✅ [KERNEL_SETUP] Using actual block device: {}", actual_block_device);
                    } else {
                        println!("❌ [KERNEL_SETUP] Actual block device also not found: {}", actual_block_device);
                        return Err(format!("Block device not found: neither {} nor {}", block_device, actual_block_device).into());
                    }
                }
                Err(e) => {
                    println!("❌ [KERNEL_SETUP] Could not find device name: {}", e);
                    return Err(format!("Block device {} not found after nvme binding: {}", block_device, e).into());
                }
            }
        } else {
            println!("✅ [KERNEL_SETUP] Block device exists: {}", block_device);
        }

        // Step 3: Try to attach the NVMe device to SPDK using kernel access
        println!("🔧 [KERNEL_SETUP] Step 3: Attaching to SPDK...");
        let attach_result = self.attach_kernel_nvme_to_spdk(pci_addr, &disk_info.device_name).await;
        match attach_result {
            Ok(_) => {
                println!("✅ [KERNEL_SETUP] Successfully attached {} to SPDK via kernel access", disk_info.device_name);
            }
            Err(e) => {
                // Don't fail setup if SPDK attachment fails - the disk can still be used
                println!("⚠️  [KERNEL_SETUP] Could not attach {} to SPDK (will use direct kernel access): {}", disk_info.device_name, e);
                println!("🔧 [KERNEL_SETUP] This is not necessarily a failure - disk can still be used directly");
            }
        }

        // Step 4: Mark as ready for SPDK (kernel mode)
        println!("🎉 [KERNEL_SETUP] Disk {} configured for SPDK in kernel-bound mode", pci_addr);
        Ok(())
    }

    /// Attach kernel-bound NVMe device to SPDK for bdev access
    async fn attach_kernel_nvme_to_spdk(&self, pci_addr: &str, device_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SPDK_ATTACH] Starting SPDK attachment for device: {}", device_name);
        println!("🔧 [SPDK_ATTACH] PCI address: {}", pci_addr);
        println!("🔧 [SPDK_ATTACH] SPDK RPC URL: {}", self.spdk_rpc_url);
        
        // Try to create a kernel bdev in SPDK for this device
        let bdev_name = format!("kernel_{}", device_name);
        let device_path = format!("/dev/{}", device_name);
        println!("🔧 [SPDK_ATTACH] Target bdev name: {}", bdev_name);
        println!("🔧 [SPDK_ATTACH] Target device path: {}", device_path);

        // Verify device path exists before trying to create bdev
        if !std::path::Path::new(&device_path).exists() {
            println!("❌ [SPDK_ATTACH] Device path does not exist: {}", device_path);
            return Err(format!("Device path {} does not exist", device_path).into());
        }
        println!("✅ [SPDK_ATTACH] Device path exists: {}", device_path);

        // Use SPDK's aio bdev to access the kernel device
        println!("🔧 [SPDK_ATTACH] Attempting to create AIO bdev...");
        let rpc_request = json!({
            "method": "bdev_aio_create",
            "params": {
                "name": bdev_name,
                "filename": device_path,
                "block_size": 512
            }
        });
        println!("🔧 [SPDK_ATTACH] AIO RPC request: {}", serde_json::to_string_pretty(&rpc_request).unwrap());

        match call_spdk_rpc(&self.spdk_rpc_url, &rpc_request).await {
            Ok(response) => {
                println!("✅ [SPDK_ATTACH] Successfully created SPDK aio bdev '{}' for kernel device {}", bdev_name, device_path);
                println!("🔧 [SPDK_ATTACH] AIO response: {:?}", response);
                Ok(())
            }
            Err(e) => {
                println!("⚠️  [SPDK_ATTACH] AIO bdev creation failed: {}", e);
                println!("🔧 [SPDK_ATTACH] Trying uring bdev as fallback...");
                
                // If aio fails, try uring bdev (newer, better performance)
                let uring_request = json!({
                    "method": "bdev_uring_create", 
                    "params": {
                        "name": bdev_name,
                        "filename": device_path,
                        "block_size": 512
                    }
                });
                println!("🔧 [SPDK_ATTACH] URING RPC request: {}", serde_json::to_string_pretty(&uring_request).unwrap());

                match call_spdk_rpc(&self.spdk_rpc_url, &uring_request).await {
                    Ok(response) => {
                        println!("✅ [SPDK_ATTACH] Successfully created SPDK uring bdev '{}' for kernel device {}", bdev_name, device_path);
                        println!("🔧 [SPDK_ATTACH] URING response: {:?}", response);
                        Ok(())
                    }
                    Err(uring_err) => {
                        println!("❌ [SPDK_ATTACH] Both AIO and URING bdev creation failed");
                        println!("❌ [SPDK_ATTACH] AIO error: {}", e);
                        println!("❌ [SPDK_ATTACH] URING error: {}", uring_err);
                        Err(format!("Failed to create SPDK bdev for {}: aio error: {}, uring error: {}", 
                                  device_path, e, uring_err).into())
                    }
                }
            }
        }
    }

    async fn setup_huge_pages(&self, huge_pages_mb: u32) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        // Calculate optimal hugepage allocation for SPDK if not specified or too small
        let optimal_mb = if huge_pages_mb == 0 || huge_pages_mb < 2048 {
            self.calculate_optimal_hugepages().await?
        } else {
            huge_pages_mb
        };
        
        let huge_pages_2m = optimal_mb / 2; // 2MB pages
        
        println!("Setting up {}MB ({}x2MB) hugepages for SPDK", optimal_mb, huge_pages_2m);
        
        // Set number of huge pages
        fs::write("/proc/sys/vm/nr_hugepages", huge_pages_2m.to_string())?;

        // Mount hugepages if not already mounted
        let _output = Command::new("mount")
            .args(["-t", "hugetlbfs", "hugetlbfs", "/dev/hugepages"])
            .output();

        // Read back actual configured huge pages
        let configured = fs::read_to_string("/proc/sys/vm/nr_hugepages")?
            .trim()
            .parse::<u32>()
            .unwrap_or(0) * 2;

        Ok(configured)
    }

    /// Calculate optimal hugepage allocation based on system memory for SPDK workloads
    async fn calculate_optimal_hugepages(&self) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        // Read total system memory
        let meminfo = fs::read_to_string("/proc/meminfo")?;
        let total_mem_kb = meminfo
            .lines()
            .find(|line| line.starts_with("MemTotal:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(4 * 1024 * 1024); // Default to 4GB if parsing fails

        let total_mem_gb = total_mem_kb / (1024 * 1024);
        
        let hugepage_mb = if total_mem_gb >= 128 {
            // Large production systems (≥128GB): allocate 4GB for optimal SPDK performance
            4096
        } else if total_mem_gb >= 64 {
            // Medium-large systems: allocate 3GB
            3072
        } else if total_mem_gb >= 32 {
            // Medium systems: allocate 2GB (SPDK minimum recommended)
            2048
        } else {
            // Smaller systems: allocate 1GB (may impact performance)
            println!("⚠️  Warning: Only {}GB RAM detected. 2GB hugepages recommended for SPDK.", total_mem_gb);
            1024
        };

        println!("Auto-calculated hugepages: {}MB for system with {}GB RAM", hugepage_mb, total_mem_gb);
        Ok(hugepage_mb)
    }

    async fn reset_disks_to_kernel(&self, pci_addresses: Vec<String>) -> Result<DiskSetupResult, Box<dyn std::error::Error + Send + Sync>> {
        let mut result = DiskSetupResult {
            success: true,
            setup_disks: Vec::new(),
            failed_disks: Vec::new(),
            warnings: Vec::new(),
            huge_pages_configured: None,
            completed_at: Utc::now().to_rfc3339(),
        };

        for pci_addr in pci_addresses {
            match self.reset_single_disk(&pci_addr).await {
                Ok(_) => {
                    result.setup_disks.push(pci_addr);
                }
                Err(e) => {
                    result.failed_disks.push((pci_addr, e.to_string()));
                    result.success = false;
                }
            }
        }

        result.completed_at = Utc::now().to_rfc3339();
        Ok(result)
    }

    async fn reset_single_disk(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let current_driver = self.get_current_driver(pci_addr).await?;
        
        // Unbind from current driver if bound
        if current_driver != "unbound" {
            self.unbind_from_driver(pci_addr, &current_driver).await?;
        }

        // Bind back to nvme driver
        self.load_driver_module("nvme").await?;
        
        // Use PCI rescan to rebind to nvme
        fs::write("/sys/bus/pci/rescan", "1")?;
        
        // Wait for device to reappear
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify reset
        let new_driver = self.get_current_driver(pci_addr).await?;
        if new_driver != "nvme" {
            return Err(format!("Reset verification failed. Current driver: {}", new_driver).into());
        }

        Ok(())
    }

    async fn get_all_disk_status(&self) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let all_pci_devices = self.get_nvme_pci_devices().await?;
        let mut disk_statuses = Vec::new();

        for pci_addr in all_pci_devices {
            if let Ok(disk_info) = self.get_disk_info(&pci_addr).await {
                disk_statuses.push(json!({
                    "pci_address": disk_info.pci_address,
                    "device_name": disk_info.device_name,
                    "driver": disk_info.driver,
                    "spdk_ready": disk_info.spdk_ready,
                    "is_system_disk": disk_info.is_system_disk,
                    "size_gb": disk_info.size_bytes / (1024 * 1024 * 1024),
                    "model": disk_info.model,
                    "mounted_partitions": disk_info.mounted_partitions,
                    "discovered_at": disk_info.discovered_at
                }));
            }
        }

        Ok(json!({
            "success": true,
            "node": self.node_name,
            "total_disks": disk_statuses.len(),
            "spdk_ready_disks": disk_statuses.iter().filter(|d| d["spdk_ready"].as_bool().unwrap_or(false)).count(),
            "uninitialized_disks": disk_statuses.iter().filter(|d| !d["spdk_ready"].as_bool().unwrap_or(true) && !d["is_system_disk"].as_bool().unwrap_or(true)).count(),
            "disks": disk_statuses,
            "checked_at": Utc::now().to_rfc3339(),
            "metadata_sync_enabled": self.metadata_sync_enabled,
            "metadata_sync_interval": self.metadata_sync_interval
        }))
    }
}
            
