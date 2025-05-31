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
use serde::{Deserialize, Serialize};

use spdk_csi_driver::{SpdkDisk, SpdkDiskSpec, SpdkDiskStatus, IoStatistics};

mod spdk_csi_driver {
    use kube::CustomResource;
    use serde::{Deserialize, Serialize};

    #[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default)]
    #[kube(group = "csi.spdk.io", version = "v1", kind = "SpdkDisk", plural = "spdkdisks")]
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

#[derive(Debug, Clone)]
struct NodeAgent {
    node_name: String,
    kube_client: Client,
    spdk_rpc_url: String,
    discovery_interval: u64,
    auto_initialize_blobstore: bool,
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
    };

    println!("Starting SPDK Node Agent on node: {}", node_name);
    
    // Wait for SPDK to be ready
    wait_for_spdk_ready(&agent).await?;
    
    // Start disk discovery loop
    run_discovery_loop(agent).await?;
    
    Ok(())
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