use kube::{
    Client, Api, ResourceExt,
    api::{PatchParams, Patch, PostParams, ListParams},
};
use tokio::time::{Duration, interval};
use reqwest::Client as HttpClient;
use serde_json::json;
use chrono::Utc;
use std::env;
use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::path::Path;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use regex::Regex;
use warp::Filter;
use warp::{http::StatusCode, reply, Rejection, Reply};


use spdk_csi_driver::{SpdkDisk, SpdkDiskSpec, SpdkDiskStatus, IoStatistics};

mod spdk_csi_driver {
    use kube::CustomResource;
    use serde::{Deserialize, Serialize};

    #[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default)]
    #[kube(group = "flint.csi.storage.io", version = "v1", kind = "SpdkDisk", plural = "spdkdisks")]
    #[kube(namespaced)]
    #[kube(status = "SpdkDiskStatus")]
    pub struct SpdkDiskSpec {
        pub node: String,
        pub pcie_addr: String,
        pub capacity: i64,
        pub blobstore_uuid: Option<String>,
        pub nvme_controller_id: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct SpdkDiskStatus {
        pub total_capacity: i64,
        pub free_space: i64,
        pub used_space: i64,
        pub healthy: bool,
        pub last_checked: String,
        pub lvol_count: u32,
        pub blobstore_initialized: bool,
        pub io_stats: IoStatistics,
        pub lvs_name: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct IoStatistics {
        pub read_iops: u64,
        pub write_iops: u64,
        pub read_latency_us: u64,
        pub write_latency_us: u64,
        pub error_count: u64,
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

#[derive(Debug, Clone)]
struct NodeAgent {
    node_name: String,
    kube_client: Client,
    spdk_rpc_url: String,
    discovery_interval: u64,
    auto_initialize_blobstore: bool,
    backup_path: String,
    spdk_path: Option<String>,
    // New fields for metadata sync configuration
    metadata_sync_interval: u64,
    metadata_sync_enabled: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kube_client = Client::try_default().await?;
    let node_name = env::var("NODE_NAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-node".to_string());
    
    let agent = NodeAgent {
        node_name: node_name.clone(),
        kube_client,
        spdk_rpc_url: env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
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
        spdk_path: env::var("SPDK_PATH").ok(),
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
    
    // Wait for SPDK to be ready
    wait_for_spdk_ready(&agent).await?;
    
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

/// Calls spdk_blob_sync_md RPC to sync blob metadata to persistent storage
async fn sync_blob_metadata(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // First, get all lvol stores to sync their metadata
    let lvstores_response = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_get_lvstores"
        }))
        .send()
        .await?;

    if !lvstores_response.status().is_success() {
        return Ok(()); // Skip if lvol stores not available
    }

    let lvstores: serde_json::Value = lvstores_response.json().await?;
    
    if let Some(stores) = lvstores["result"].as_array() {
        for store in stores {
            if let Some(lvs_name) = store["name"].as_str() {
                println!("Syncing metadata for lvol store: {}", lvs_name);
                
                // Call spdk_blob_sync_md for this lvol store
                let sync_response = http_client
                    .post(&agent.spdk_rpc_url)
                    .json(&json!({
                        "method": "spdk_blob_sync_md",
                        "params": {
                            "lvs_name": lvs_name
                        }
                    }))
                    .send()
                    .await;

                match sync_response {
                    Ok(resp) if resp.status().is_success() => {
                        println!("Successfully synced metadata for lvol store: {}", lvs_name);
                    }
                    Ok(resp) => {
                        let error_text = resp.text().await.unwrap_or_default();
                        eprintln!("Failed to sync metadata for lvol store {}: {}", lvs_name, error_text);
                    }
                    Err(e) => {
                        eprintln!("Error calling spdk_blob_sync_md for lvol store {}: {}", lvs_name, e);
                    }
                }
                
                // Small delay between stores to avoid overwhelming SPDK
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    
    // Also try to sync metadata for all blobstores directly
    let bdevs_response = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_get_bdevs"
        }))
        .send()
        .await?;

    if bdevs_response.status().is_success() {
        let bdevs: serde_json::Value = bdevs_response.json().await?;
        
        if let Some(bdev_list) = bdevs["result"].as_array() {
            for bdev in bdev_list {
                // Look for blobstore bdevs (typically NVMe devices with blobstores)
                if let Some(driver_name) = bdev["driver_specific"]["blobfs"].as_object() {
                    if let Some(bdev_name) = bdev["name"].as_str() {
                        println!("Syncing blobstore metadata for bdev: {}", bdev_name);
                        
                        // Try alternative RPC method for blobstore sync
                        let _sync_response = http_client
                            .post(&agent.spdk_rpc_url)
                            .json(&json!({
                                "method": "blobfs_sync",
                                "params": {
                                    "bdev_name": bdev_name
                                }
                            }))
                            .send()
                            .await;
                        // Best effort - don't fail if this doesn't work
                    }
                }
            }
        }
    }
    
    Ok(())
}

/// Enhanced shutdown handler that performs final metadata sync before exit
async fn perform_graceful_shutdown_with_sync(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error>> {
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
    let http_client = HttpClient::new();
    
    println!("Initiating SPDK application shutdown...");
    let response = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "spdk_app_stop",
            "params": {}
        }))
        .send()
        .await;

    match response {
        Ok(res) if res.status().is_success() => {
            println!("SPDK shutdown initiated successfully");
        }
        Ok(res) => {
            let error_text = res.text().await.unwrap_or_default();
            eprintln!("SPDK shutdown RPC failed: {}", error_text);
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
        // Get all uninitialized disks
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
    match agent.discover_uninitialized_disks().await {
        Ok(disks) => Ok(warp::reply::json(&json!({
            "success": true,
            "disks": disks,
            "count": disks.len(),
            "node": agent.node_name
        }))),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name
        })))
    }
}

async fn setup_disks_for_spdk(
    request: DiskSetupRequest,
    agent: NodeAgent
) -> Result<impl warp::Reply, warp::Rejection> {
    match agent.setup_disks_for_spdk(request).await {
        Ok(result) => Ok(warp::reply::json(&result)),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name,
            "completed_at": Utc::now().to_rfc3339()
        })))
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

async fn wait_for_spdk_ready(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let max_retries = 30; // 5 minutes
    
    for attempt in 1..=max_retries {
        match http_client
            .post(&agent.spdk_rpc_url)
            .json(&json!({"method": "spdk_get_version"}))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
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

async fn run_discovery_loop(agent: NodeAgent) -> Result<(), Box<dyn std::error::Error>> {
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

async fn discover_and_update_local_disks(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error>> {
    println!("Discovering NVMe devices on node {}", agent.node_name);
    
    // Discover local NVMe devices
    let discovered_devices = query_local_nvme_devices(agent).await?;
    
    if discovered_devices.is_empty() {
        println!("No NVMe devices found on node {}", agent.node_name);
        return Ok(());
    }
    
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
    
    for device in discovered_devices {
        let disk_name = format!("{}-{}", agent.node_name, device.controller_id);
        
        match spdk_disks.get(&disk_name).await {
            Ok(existing_disk) => {
                // Update existing disk
                update_existing_disk_resource(agent, &existing_disk, &device).await?;
            }
            Err(_) => {
                // Create new disk resource
                create_new_disk_resource(agent, &device).await?;
            }
        }
    }
    
    // Update I/O statistics for all disks on this node
    update_disk_io_statistics(agent).await?;
    
    println!("Disk discovery completed for node {}", agent.node_name);
    Ok(())
}

async fn query_local_nvme_devices(agent: &NodeAgent) -> Result<Vec<NvmeDevice>, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // Get all NVMe controllers from local SPDK
    let response = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_nvme_get_controllers"
        }))
        .send()
        .await?;

    let controllers: serde_json::Value = response.json().await?;
    let mut devices = Vec::new();
    
    if let Some(controller_list) = controllers["result"].as_array() {
        for controller in controller_list {
            if let Some(device) = parse_nvme_controller(controller) {
                devices.push(device);
            }
        }
    }
    
    // Also check for unbound NVMe devices that could be attached to SPDK
    let unbound_devices = discover_unbound_nvme_devices().await?;
    devices.extend(unbound_devices);
    
    Ok(devices)
}

#[derive(Debug, Clone)]
struct NvmeDevice {
    controller_id: String,
    pcie_addr: String,
    capacity: i64,
    model: String,
    serial: String,
    firmware_version: String,
    numa_node: Option<u32>,
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
        serial: controller["serial"].as_str().unwrap_or("Unknown").to_string(),
        firmware_version: controller["fw_rev"].as_str().unwrap_or("Unknown").to_string(),
        numa_node: controller["numa_node"].as_u64().map(|n| n as u32),
    })
}

async fn discover_unbound_nvme_devices() -> Result<Vec<NvmeDevice>, Box<dyn std::error::Error>> {
    use std::process::Command;
    
    // Use lspci to find NVMe devices
    let output = Command::new("lspci")
        .args(["-D", "-d", "::0108"]) // NVMe class code
        .output()?;
    
    let lspci_output = String::from_utf8(output.stdout)?;
    let mut devices = Vec::new();
    
    for line in lspci_output.lines() {
        if let Some(pcie_addr) = line.split_whitespace().next() {
            // Check if device is bound to a driver
            let sys_path = format!("/sys/bus/pci/devices/{}/driver", pcie_addr);
            if !std::path::Path::new(&sys_path).exists() {
                // Unbound device - get more info
                if let Ok(device) = get_nvme_device_info(pcie_addr).await {
                    devices.push(device);
                }
            }
        }
    }
    
    Ok(devices)
}

async fn get_nvme_device_info(pcie_addr: &str) -> Result<NvmeDevice, Box<dyn std::error::Error>> {
    use std::fs;
    
    // Read device info from sysfs
    let vendor_path = format!("/sys/bus/pci/devices/{}/vendor", pcie_addr);
    let device_path = format!("/sys/bus/pci/devices/{}/device", pcie_addr);
    
    let vendor = fs::read_to_string(vendor_path).unwrap_or_default().trim().to_string();
    let device = fs::read_to_string(device_path).unwrap_or_default().trim().to_string();
    
    // Estimate capacity (this would need more sophisticated detection in production)
    let capacity = 1_000_000_000_000; // 1TB default
    
    Ok(NvmeDevice {
        controller_id: format!("unbound_{}", pcie_addr.replace(":", "_")),
        pcie_addr: pcie_addr.to_string(),
        capacity,
        model: format!("Unbound NVMe Device {}", device),
        serial: "Unknown".to_string(),
        firmware_version: "Unknown".to_string(),
        numa_node: None,
    })
}

async fn create_new_disk_resource(agent: &NodeAgent, device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error>> {
    let disk_name = format!("{}-{}", agent.node_name, device.controller_id);
    
    let spdk_disk = SpdkDisk::new(&disk_name, SpdkDiskSpec {
        node: agent.node_name.clone(),
        pcie_addr: device.pcie_addr.clone(),
        capacity: device.capacity,
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
    
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
    spdk_disks.create(&PostParams::default(), &spdk_disk_with_status).await?;
    
    println!("Created SpdkDisk resource: {} for device {} ({})", 
             disk_name, device.pcie_addr, device.model);
    
    // Initialize blobstore if auto-initialization is enabled
    if agent.auto_initialize_blobstore {
        initialize_blobstore_on_device(agent, &spdk_disk_with_status).await?;
    }
    
    Ok(())
}

async fn initialize_blobstore_on_device(agent: &NodeAgent, disk: &SpdkDisk) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let lvs_name = format!("lvs_{}", disk.metadata.name.as_ref().unwrap());
    
    // First, try to attach the NVMe device to SPDK if it's not already attached
    let controller_id = disk.spec.nvme_controller_id.as_ref().unwrap_or(&"nvme0".to_string());
    let attach_result = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": controller_id,
                "trtype": "PCIe",
                "traddr": disk.spec.pcie_addr
            }
        }))
        .send()
        .await;
    
    // Wait a moment for the device to be ready
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Create lvol store (which serves as our blobstore)
    let bdev_name = format!("{}n1", controller_id);
    let lvol_store_result = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": bdev_name,
                "lvs_name": lvs_name,
                "cluster_sz": 65536 // 64KB clusters for good performance
            }
        }))
        .send()
        .await;
    
    match lvol_store_result {
        Ok(resp) if resp.status().is_success() => {
            update_disk_blobstore_status(agent, disk, true, Some(lvs_name)).await?;
            println!("Initialized lvol store on disk: {}", disk.metadata.name.as_ref().unwrap());
        }
        Ok(resp) => {
            let error_text = resp.text().await.unwrap_or_default();
            eprintln!("Failed to create lvol store on {}: {}", disk.spec.pcie_addr, error_text);
        }
        Err(e) => {
            eprintln!("Failed to create lvol store on {}: {}", disk.spec.pcie_addr, e);
        }
    }
    
    Ok(())
}

async fn update_existing_disk_resource(agent: &NodeAgent, disk: &SpdkDisk, device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
    let disk_name = disk.metadata.name.as_ref().unwrap();
    
    let mut needs_update = false;
    let mut updated_status = disk.status.clone().unwrap_or_default();
    
    // Update capacity if changed
    if disk.spec.capacity != device.capacity {
        let patch = json!({
            "spec": {
                "capacity": device.capacity
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

async fn check_device_health(agent: &NodeAgent, device: &NvmeDevice) -> Result<bool, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // Check if device is accessible via SPDK
    let bdev_name = format!("{}n1", device.controller_id);
    let response = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": bdev_name
            }
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
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
    disk: &SpdkDisk, 
    initialized: bool,
    lvs_name: Option<String>
) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(agent.kube_client.clone(), "default");
    let disk_name = disk.metadata.name.as_ref().unwrap();
    
    let mut status = disk.status.clone().unwrap_or_default();
    status.blobstore_initialized = initialized;
    status.lvs_name = lvs_name;
    status.last_checked = Utc::now().to_rfc3339();
    
    spdk_disks
        .patch_status(disk_name, &PatchParams::default(), &Patch::Merge(json!({
            "status": status
        })))
        .await?;
    
    Ok(())
}

async fn update_disk_io_statistics(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // Get I/O statistics from SPDK
    let response = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_get_iostat"
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Ok(()); // Skip if iostat not available
    }
    
    let iostat: serde_json::Value = response.json().await?;
    
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
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // Get lvol store information
    let response = http_client
        .post(&agent.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_get_lvstores"
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Ok(());
    }
    
    let lvstores: serde_json::Value = response.json().await?;
    
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
    async fn discover_uninitialized_disks(&self) -> Result<Vec<UnimplementedDisk>, Box<dyn std::error::Error>> {
        let mut uninitialized_disks = Vec::new();
        
        // Get all NVMe PCI devices
        let pci_devices = self.get_nvme_pci_devices().await?;
        
        for pci_addr in pci_devices {
            if let Ok(disk_info) = self.get_uninitialized_disk_info(&pci_addr).await {
                // Only include disks that are not already setup for SPDK and not system disks
                if !disk_info.spdk_ready && !disk_info.is_system_disk {
                    uninitialized_disks.push(disk_info);
                }
            }
        }
        
        Ok(uninitialized_disks)
    }

    async fn get_nvme_pci_devices(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let output = Command::new("lspci")
            .args(["-D", "-d", "::0108"]) // NVMe class code
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        let mut devices = Vec::new();

        for line in stdout.lines() {
            if let Some(pci_addr) = line.split_whitespace().next() {
                devices.push(pci_addr.to_string());
            }
        }

        Ok(devices)
    }

    async fn get_uninitialized_disk_info(&self, pci_addr: &str) -> Result<UnimplementedDisk, Box<dyn std::error::Error>> {
        let sysfs_path = format!("/sys/bus/pci/devices/{}", pci_addr);
        
        // Read PCI device information
        let vendor_id = self.read_sysfs_file(&format!("{}/vendor", sysfs_path)).await?;
        let device_id = self.read_sysfs_file(&format!("{}/device", sysfs_path)).await?;
        let subsystem_vendor = self.read_sysfs_file(&format!("{}/subsystem_vendor", sysfs_path)).await
            .unwrap_or_else(|_| vendor_id.clone());
        let subsystem_device = self.read_sysfs_file(&format!("{}/subsystem_device", sysfs_path)).await
            .unwrap_or_else(|_| device_id.clone());
        
        // Get NUMA node
        let numa_node = self.read_sysfs_file(&format!("{}/numa_node", sysfs_path)).await
            .ok()
            .and_then(|s| s.trim().parse().ok());

        // Get current driver
        let driver = self.get_current_driver(pci_addr).await?;
        
        // Find associated block device
        let device_name = self.find_nvme_device_name(pci_addr).await?;
        
        // Get device details
        let (size_bytes, model, serial, firmware_version) = self.get_nvme_details(&device_name).await?;
        
        // Check for mounted partitions
        let mounted_partitions = self.get_mounted_partitions(&device_name).await?;
        
        // Check if it's a system disk
        let is_system_disk = self.is_system_disk(&device_name, &mounted_partitions).await?;
        
        // Determine if SPDK ready
        let spdk_ready = self.is_spdk_compatible_driver(&driver);
        
        Ok(UnimplementedDisk {
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
        })
    }

    async fn read_sysfs_file(&self, path: &str) -> Result<String, Box<dyn std::error::Error>> {
        Ok(fs::read_to_string(path)?)
    }

    async fn get_current_driver(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error>> {
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

    async fn find_nvme_device_name(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error>> {
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

    async fn get_nvme_details(&self, device_name: &str) -> Result<(u64, String, String, String), Box<dyn std::error::Error>> {
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

    async fn get_device_size(&self, device_name: &str) -> Result<u64, Box<dyn std::error::Error>> {
        let output = Command::new("blockdev")
            .args(["--getsize64", &format!("/dev/{}", device_name)])
            .output()?;

        let size_str = String::from_utf8(output.stdout)?;
        Ok(size_str.trim().parse()?)
    }

    async fn get_mounted_partitions(&self, device_name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
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

    async fn is_system_disk(&self, device_name: &str, mounted_partitions: &[String]) -> Result<bool, Box<dyn std::error::Error>> {
        // Check if any partition is mounted on critical system paths
        let critical_mounts = ["/", "/boot", "/boot/efi", "/var", "/usr", "/home"];
        
        for mount in mounted_partitions {
            if critical_mounts.contains(&mount.as_str()) {
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

        Ok(false)
    }

    fn is_spdk_compatible_driver(&self, driver: &str) -> bool {
        matches!(driver, "vfio-pci" | "uio_pci_generic" | "igb_uio")
    }

    async fn setup_disks_for_spdk(&self, request: DiskSetupRequest) -> Result<DiskSetupResult, Box<dyn std::error::Error>> {
        let mut result = DiskSetupResult {
            success: true,
            setup_disks: Vec::new(),
            failed_disks: Vec::new(),
            warnings: Vec::new(),
            huge_pages_configured: None,
            completed_at: Utc::now().to_rfc3339(),
        };

        // Validate all disks first
        for pci_addr in &request.pci_addresses {
            if let Err(e) = self.validate_disk_for_setup(pci_addr, request.force_unmount).await {
                result.failed_disks.push((pci_addr.clone(), e.to_string()));
                result.success = false;
                continue;
            }
        }

        if !result.success && !request.force_unmount {
            return Ok(result);
        }

        // Setup huge pages if requested
        if let Some(huge_pages_mb) = request.huge_pages_mb {
            match self.setup_huge_pages(huge_pages_mb).await {
                Ok(configured) => {
                    result.huge_pages_configured = Some(configured);
                }
                Err(e) => {
                    result.warnings.push(format!("Huge pages setup warning: {}", e));
                }
            }
        }

        // Process each disk
        for pci_addr in &request.pci_addresses {
            if result.failed_disks.iter().any(|(addr, _)| addr == pci_addr) {
                continue;
            }

            match self.setup_single_disk(pci_addr, &request).await {
                Ok(_) => {
                    result.setup_disks.push(pci_addr.clone());
                }
                Err(e) => {
                    result.failed_disks.push((pci_addr.clone(), e.to_string()));
                    result.success = false;
                }
            }
        }

        result.completed_at = Utc::now().to_rfc3339();
        Ok(result)
    }

    async fn validate_disk_for_setup(&self, pci_addr: &str, force_unmount: bool) -> Result<(), Box<dyn std::error::Error>> {
        let disk_info = self.get_uninitialized_disk_info(pci_addr).await?;

        if disk_info.is_system_disk {
            return Err("Cannot setup system disk for SPDK".into());
        }

        if !disk_info.mounted_partitions.is_empty() && !force_unmount {
            return Err(format!("Disk has mounted partitions: {:?}. Use force_unmount=true to proceed", disk_info.mounted_partitions).into());
        }

        if disk_info.spdk_ready {
            return Err("Disk is already setup for SPDK".into());
        }

        Ok(())
    }

    async fn setup_single_disk(&self, pci_addr: &str, request: &DiskSetupRequest) -> Result<(), Box<dyn std::error::Error>> {
        let disk_info = self.get_uninitialized_disk_info(pci_addr).await?;

        // Step 1: Backup data if requested
        if request.backup_data && !disk_info.mounted_partitions.is_empty() {
            self.backup_disk_data(&disk_info).await?;
        }

        // Step 2: Unmount all partitions
        if !disk_info.mounted_partitions.is_empty() {
            self.unmount_disk_partitions(&disk_info).await?;
        }

        // Step 3: Unbind from current driver
        if disk_info.driver != "unbound" {
            self.unbind_from_driver(pci_addr, &disk_info.driver).await?;
        }

        // Step 4: Load target driver module
        let target_driver = request.driver_override.as_ref()
            .unwrap_or(&"vfio-pci".to_string());
        self.load_driver_module(target_driver).await?;

        // Step 5: Bind to new driver
        self.bind_to_driver(pci_addr, target_driver).await?;

        // Step 6: Verify setup
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.verify_spdk_setup(pci_addr).await?;

        Ok(())
    }

    async fn backup_disk_data(&self, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error>> {
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

    async fn unmount_disk_partitions(&self, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error>> {
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

    async fn unbind_from_driver(&self, pci_addr: &str, driver: &str) -> Result<(), Box<dyn std::error::Error>> {
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

    async fn load_driver_module(&self, driver: &str) -> Result<(), Box<dyn std::error::Error>> {
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

    async fn bind_to_driver(&self, pci_addr: &str, driver: &str) -> Result<(), Box<dyn std::error::Error>> {
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

    async fn enable_vfio(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
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

    async fn verify_spdk_setup(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let current_driver = self.get_current_driver(pci_addr).await?;
        
        if !self.is_spdk_compatible_driver(&current_driver) {
            return Err(format!("Disk setup verification failed. Current driver: {}", current_driver).into());
        }

        Ok(())
    }

    async fn setup_huge_pages(&self, huge_pages_mb: u32) -> Result<u32, Box<dyn std::error::Error>> {
        let huge_pages_2m = huge_pages_mb / 2; // 2MB pages
        
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

    async fn reset_disks_to_kernel(&self, pci_addresses: Vec<String>) -> Result<DiskSetupResult, Box<dyn std::error::Error>> {
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

    async fn reset_single_disk(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
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

    async fn get_all_disk_status(&self) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let all_pci_devices = self.get_nvme_pci_devices().await?;
        let mut disk_statuses = Vec::new();

        for pci_addr in all_pci_devices {
            if let Ok(disk_info) = self.get_uninitialized_disk_info(&pci_addr).await {
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
            