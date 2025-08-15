// node_agent/disk_discovery.rs - NVMe Disk Discovery and Management
//
// This module handles discovery of local NVMe devices, reading hardware information,
// and managing disk identification using persistent paths (Portworx-style approach).

use crate::node_agent::{NodeAgent, rpc_client::call_spdk_rpc};
use crate::FlintDiskMetadata;
use serde_json::json;
use std::process::Command;
use std::fs;
use regex::Regex;

/// NVMe device information structure
#[derive(Debug, Clone)]
pub struct NvmeDevice {
    // Location-dependent fields
    pub controller_id: String,
    pub pcie_addr: String,
    pub device_path: String,
    
    // Immutable identification fields (Portworx-style)
    pub disk_id: String,           // /dev/disk/by-id/ path
    pub serial_number: String,     // Primary hardware identifier
    pub wwn: Option<String>,       // World Wide Name if available
    pub model: String,
    pub vendor: String,
    
    // Hardware characteristics
    pub capacity: i64,
    
    // Cluster metadata (if disk is already part of a cluster)
    pub cluster_metadata: Option<FlintDiskMetadata>,
}



/// Main entry point for disk discovery and updating local disk status
pub async fn discover_and_update_local_disks(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔍 [DISCOVERY] Starting NVMe device discovery on node: {}", agent.node_name);
    println!("🔧 [DISCOVERY] Config - auto_init_blobstore: {}, discovery_interval: {}s", 
             agent.auto_initialize_blobstore, agent.discovery_interval);
    
    // Discover local NVMe devices using Portworx-style persistent paths
    let discovered_devices = agent.discover_devices_by_persistent_paths().await?;
    
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

    // Automatically attach discovered disks to SPDK (unified with manual setup path)
    for device in &discovered_devices {
        if let Err(e) = agent.attach_discovered_disk_to_spdk(device).await {
            println!("⚠️ [DISCOVERY] Failed to attach disk {} to SPDK: {}", device.device_path, e);
        }
    }
    
    // Perform health monitoring and create alerts for operator review
    println!("🏥 [DISCOVERY] Running health checks and alert generation...");
    for device in &discovered_devices {
        if let Err(e) = crate::node_agent::health_monitor::check_device_health(agent, device).await {
            println!("⚠️ [DISCOVERY] Health check failed for device {}: {}", device.controller_id, e);
        }
    }
    
    println!("✅ [DISCOVERY] Disk discovery and health monitoring completed successfully for node: {}", agent.node_name);
    Ok(())
}

impl NodeAgent {




    /// Enhanced system disk detection using PCI address
    pub async fn system_disk_check_by_pci(&self, pci_addr: &str) -> bool {
        println!("🔍 [SYSTEM_CHECK_PCI] Checking if PCI device {} contains system disk", pci_addr);
        
        // Method 1: Find any block device that belongs to this PCI and check if it's mounted on root
        if let Ok(entries) = fs::read_dir("/sys/block") {
            for entry in entries {
                if let Ok(entry) = entry {
                    let device_name = entry.file_name();
                    let device_str = device_name.to_string_lossy();
                    
                    // Check if this block device belongs to our PCI address
                    if let Ok(pci_path) = fs::read_link(format!("/sys/block/{}/device", device_str)) {
                        if let Some(pci_str) = pci_path.to_string_lossy().split('/').last() {
                            if pci_str == pci_addr {
                                // This device belongs to our PCI address, check if it's system disk
                                if self.quick_system_disk_check(&device_str).await {
                                    println!("⚠️ [SYSTEM_CHECK_PCI] PCI device {} contains system disk via {}", pci_addr, device_str);
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        
        false
    }

    /// Quick system disk check for individual devices
    pub async fn quick_system_disk_check(&self, device_name: &str) -> bool {
        println!("🔍 [SYSTEM_CHECK] Checking if {} is a system disk", device_name);
        
        // Method 1: Check if it's mounted on root filesystem
        if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", "/"]).output() {
            let root_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!("🔍 [SYSTEM_CHECK] Root filesystem source: {}", root_source);
            
            if root_source.contains(device_name) {
                println!("⚠️ [SYSTEM_CHECK] {} is mounted as root filesystem", device_name);
                return true;
            }
        }
        
        // Method 2: Check boot partitions
        if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", "/boot"]).output() {
            let boot_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if boot_source.contains(device_name) {
                println!("⚠️ [SYSTEM_CHECK] {} contains boot partition", device_name);
                return true;
            }
        }
        
        // Method 3: Check if any partition is mounted on critical system paths
        let critical_paths = ["/", "/boot", "/var", "/usr", "/opt"];
        for path in &critical_paths {
            if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", path]).output() {
                let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if source.contains(device_name) {
                    println!("⚠️ [SYSTEM_CHECK] {} is mounted on critical path {}", device_name, path);
                    return true;
                }
            }
        }
        
        false
    }

    /// Get NVMe PCI devices using lspci
    pub async fn get_nvme_pci_devices(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Scanning for NVMe PCI devices using lspci...");
        
        let output = Command::new("lspci")
            .args(["-D", "-d", "::0108"]) // NVMe class code
            .output()?;
        
        if !output.status.success() {
            return Err("lspci command failed".into());
        }
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut devices = Vec::new();
        
        for line in stdout.lines() {
            // Parse PCI address from lspci output: "0000:00:04.0 ..."
            if let Some(pci_addr) = line.split_whitespace().next() {
                devices.push(pci_addr.to_string());
                println!("🔍 [DISCOVERY] Found NVMe PCI device: {}", pci_addr);
            }
        }
        
        println!("🔍 [DISCOVERY] Total NVMe devices found: {}", devices.len());
        Ok(devices)
    }



    /// Read a sysfs file and return its contents
    pub async fn read_sysfs_file(&self, path: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        tokio::fs::read_to_string(path).await
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("Failed to read {}: {}", path, e).into())
    }

    /// Get current driver for a PCI device
    pub async fn get_current_driver(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let driver_path = format!("/sys/bus/pci/devices/{}/driver", pci_addr);
        
        match tokio::fs::read_link(&driver_path).await {
            Ok(driver_link) => {
                if let Some(driver_name) = driver_link.file_name() {
                    Ok(driver_name.to_string_lossy().to_string())
                } else {
                    Ok("unknown".to_string())
                }
            }
            Err(_) => Ok("none".to_string()), // No driver bound
        }
    }

    /// Find NVMe device name from PCI address
    pub async fn find_nvme_device_name(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DEVICE_NAME] Finding device name for PCI: {}", pci_addr);
        
        // Method 1: Look in /sys/bus/pci/devices/{pci}/nvme/
        let nvme_path = format!("/sys/bus/pci/devices/{}/nvme", pci_addr);
        if let Ok(entries) = fs::read_dir(&nvme_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let nvme_controller = entry.file_name();
                    let controller_str = nvme_controller.to_string_lossy();
                    
                    // Look for namespaces under this controller
                    let namespace_path = format!("{}/{}", nvme_path, controller_str);
                    if let Ok(ns_entries) = fs::read_dir(&namespace_path) {
                        for ns_entry in ns_entries {
                            if let Ok(ns_entry) = ns_entry {
                                let ns_name = ns_entry.file_name();
                                let ns_str = ns_name.to_string_lossy();
                                
                                // Look for nvmeXnY pattern
                                if ns_str.starts_with("nvme") && ns_str.contains('n') {
                                    println!("✅ [DEVICE_NAME] Found device: {}", ns_str);
                                    return Ok(ns_str.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        
        // Method 2: Search /sys/block for devices with matching PCI address
        if let Ok(entries) = fs::read_dir("/sys/block") {
            for entry in entries {
                if let Ok(entry) = entry {
                    let device_name = entry.file_name();
                    let device_str = device_name.to_string_lossy();
                    
                    // Only check nvme devices
                    if device_str.starts_with("nvme") {
                        let device_link_path = format!("/sys/block/{}/device", device_str);
                        if let Ok(device_link) = fs::read_link(&device_link_path) {
                            if device_link.to_string_lossy().contains(pci_addr) {
                                println!("✅ [DEVICE_NAME] Found device via /sys/block: {}", device_str);
                                return Ok(device_str.to_string());
                            }
                        }
                    }
                }
            }
        }
        
        Err(format!("No NVMe device found for PCI address {}", pci_addr).into())
    }

    /// Get detailed NVMe information from device name
    pub async fn get_nvme_details(&self, device_name: &str) -> Result<(u64, String, String, String), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [NVME_DETAILS] Getting details for device: {}", device_name);
        
        let device_path = format!("/dev/{}", device_name);
        
        // Get size using blockdev
        let size = self.get_device_size(device_name).await.unwrap_or(0);
        
        // Try to get more details using nvme-cli if available
        let (mut model, mut serial, vendor) = ("Unknown".to_string(), "Unknown".to_string(), "Unknown".to_string());
        
        // Try nvme id-ctrl command
        if let Ok(output) = Command::new("nvme").args(["id-ctrl", &device_path]).output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                
                // Parse model number
                if let Some(model_line) = stdout.lines().find(|line| line.contains("mn ")) {
                    if let Some(model_part) = model_line.split(':').nth(1) {
                        model = model_part.trim().to_string();
                    }
                }
                
                // Parse serial number
                if let Some(serial_line) = stdout.lines().find(|line| line.contains("sn ")) {
                    if let Some(serial_part) = serial_line.split(':').nth(1) {
                        serial = serial_part.trim().to_string();
                    }
                }
            }
        }
        
        Ok((size, model, serial, vendor))
    }

    /// Discover devices using persistent paths (Portworx-style)
    pub async fn discover_devices_by_persistent_paths(&self) -> Result<Vec<NvmeDevice>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [PERSISTENT_PATHS] Discovering devices using persistent paths");
        
        let disk_by_id_path = "/dev/disk/by-id";
        let mut devices = Vec::new();
        
        if !std::path::Path::new(disk_by_id_path).exists() {
            println!("⚠️ [PERSISTENT_PATHS] /dev/disk/by-id does not exist");
            return Ok(devices);
        }
        
        let entries = fs::read_dir(disk_by_id_path)?;
        
        for entry in entries {
            let entry = entry?;
            let disk_id_name = entry.file_name();
            let disk_id_str = disk_id_name.to_string_lossy();
            
            // Look for NVMe devices (nvme-* pattern)
            if disk_id_str.starts_with("nvme-") && !disk_id_str.contains("-part") {
                let disk_id_path = format!("{}/{}", disk_by_id_path, disk_id_str);
                
                if let Ok(device_path) = fs::read_link(&disk_id_path) {
                    let resolved_path = if device_path.is_absolute() {
                        device_path.to_string_lossy().to_string()
                    } else {
                        format!("/dev/{}", device_path.file_name().unwrap().to_string_lossy())
                    };
                    
                    println!("🔍 [PERSISTENT_PATHS] Processing: {} -> {}", disk_id_str, resolved_path);
                    
                    match self.create_device_from_persistent_path(&disk_id_path, &resolved_path).await {
                        Ok(device) => {
                            println!("✅ [PERSISTENT_PATHS] Created device: {}", device.controller_id);
                            devices.push(device);
                        }
                        Err(e) => {
                            println!("⚠️ [PERSISTENT_PATHS] Failed to create device for {}: {}", disk_id_str, e);
                        }
                    }
                }
            }
        }
        
        println!("✅ [PERSISTENT_PATHS] Discovered {} devices", devices.len());
        Ok(devices)
    }

    /// Create device information from persistent path
    pub async fn create_device_from_persistent_path(&self, disk_id_path: &str, device_path: &str) -> Result<NvmeDevice, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [CREATE_DEVICE] Creating device from: {}", disk_id_path);
        
        // Extract device name (e.g., "nvme0n1" from "/dev/nvme0n1")
        let device_name = device_path.strip_prefix("/dev/").unwrap_or(device_path);
        
        // Extract controller name (e.g., "nvme0" from "nvme0n1")
        let controller_id = extract_nvme_controller_name(device_name);
        println!("🔍 [CREATE_DEVICE] Extracted controller_id: '{}' from device_name: '{}'", controller_id, device_name);
        
        // Get PCI address for this device
        let pcie_addr = self.find_pci_address_for_device(device_name).await?;
        
        // Get device details
        let (capacity, model, serial, vendor) = self.get_nvme_details(device_name).await?;
        
        // Get WWN if available
        let wwn = self.get_device_wwn(device_name).await.ok();
        
        // Read cluster metadata if present
        let cluster_metadata = self.read_disk_cluster_metadata(device_name).await.ok();
        
        Ok(NvmeDevice {
            controller_id,
            pcie_addr,
            device_path: device_path.to_string(),
            disk_id: disk_id_path.to_string(),
            serial_number: serial,
            wwn,
            model,
            vendor,
            capacity: capacity as i64,
            cluster_metadata,
        })
    }

    /// Get device WWN (World Wide Name)
    pub async fn get_device_wwn(&self, device_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let wwn_path = format!("/sys/block/{}/wwid", device_name);
        
        match tokio::fs::read_to_string(&wwn_path).await {
            Ok(wwn) => Ok(wwn.trim().to_string()),
            Err(_) => {
                // Try alternative method using nvme id-ns
                let device_path = format!("/dev/{}", device_name);
                if let Ok(output) = Command::new("nvme").args(["id-ns", &device_path]).output() {
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if let Some(wwn_line) = stdout.lines().find(|line| line.contains("wwn")) {
                            if let Some(wwn_part) = wwn_line.split(':').nth(1) {
                                return Ok(wwn_part.trim().to_string());
                            }
                        }
                    }
                }
                Err("WWN not found".into())
            }
        }
    }

    /// Find PCI address for a given device name
    pub async fn find_pci_address_for_device(&self, device_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let pci_regex = Regex::new(r"([0-9a-fA-F]{4}:[0-9a-fA-F]{2}:[0-9a-fA-F]{2}\.[0-9a-fA-F])")?;
        
        // Method 1: Check /sys/block/{device}/device (traditional path)
        let device_link_path = format!("/sys/block/{}/device", device_name);
        if let Ok(device_link) = fs::read_link(&device_link_path) {
            let link_str = device_link.to_string_lossy();
            if let Some(captures) = pci_regex.captures(&link_str) {
                if let Some(pci_addr) = captures.get(1) {
                    return Ok(pci_addr.as_str().to_string());
                }
            }
        }
        
        // Method 2: Check /sys/class/nvme/{controller}/device (AWS EBS NVMe path)
        let controller_name = device_name.chars()
            .take_while(|c| c.is_alphabetic() || c.is_numeric())
            .collect::<String>();
        
        let nvme_device_path = format!("/sys/class/nvme/{}/device", controller_name);
        if let Ok(nvme_device_link) = fs::read_link(&nvme_device_path) {
            let link_str = nvme_device_link.to_string_lossy();
            if let Some(captures) = pci_regex.captures(&link_str) {
                if let Some(pci_addr) = captures.get(1) {
                    println!("✅ [PCI_DISCOVERY] Found PCI address {} for device {} via /sys/class/nvme/", pci_addr.as_str(), device_name);
                    return Ok(pci_addr.as_str().to_string());
                }
            }
        }
        
        // Method 3: Try to resolve the full path and extract PCI address
        if let Ok(full_path) = fs::canonicalize(format!("/sys/block/{}/device", device_name)) {
            let path_str = full_path.to_string_lossy();
            if let Some(captures) = pci_regex.captures(&path_str) {
                if let Some(pci_addr) = captures.get(1) {
                    return Ok(pci_addr.as_str().to_string());
                }
            }
        }
        
        Err(format!("PCI address not found for device {}", device_name).into())
    }

    /// Read LVS stores from SPDK
    pub async fn read_lvs_stores(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let response = super::call_spdk_rpc(&self.spdk_rpc_url, &json!({
            "method": "bdev_lvol_get_lvstores"
        })).await?;
        
        let mut lvs_names = Vec::new();
        
        if let Some(lvstores) = response["result"].as_array() {
            for lvstore in lvstores {
                if let Some(name) = lvstore["name"].as_str() {
                    lvs_names.push(name.to_string());
                }
            }
        }
        
        Ok(lvs_names)
    }

    /// Attach a discovered disk to SPDK (unified code path for manual and automatic setup)
    pub async fn attach_discovered_disk_to_spdk(&self, device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔗 [ATTACH_SPDK] Attaching discovered disk to SPDK: {}", device.device_path);
        
        // Skip system disks - use the full device name (e.g., "nvme1n1" not "nvme1")
        let device_name = device.device_path.strip_prefix("/dev/").unwrap_or(&device.device_path);
        if self.quick_system_disk_check(device_name).await {
            println!("⚠️ [ATTACH_SPDK] Skipping system disk: {}", device.device_path);
            return Ok(());
        }

        // Use the same logic as initialize_disk_blobstore but without LVS creation  
        let bdev_name = format!("nvme-{}", device_name);

        // Check if bdev already exists in SPDK  
        let bdevs = super::call_spdk_rpc(&self.spdk_rpc_url, &json!({ 
            "method": "bdev_get_bdevs" 
        })).await?;
        
        let Some(bdev_list) = bdevs["result"].as_array() else {
            return Err("Failed to get bdev list".into());
        };

        let bdev_exists = bdev_list.iter().any(|b| b["name"].as_str() == Some(&bdev_name));
        if !bdev_exists {
            // Create AIO bdev for the device (unified with manual setup path)
            println!("🔧 [ATTACH_SPDK] Creating AIO bdev: {}", bdev_name);
            let create_bdev = super::call_spdk_rpc(&self.spdk_rpc_url, &json!({
                "method": "bdev_aio_create",
                "params": {
                    "name": bdev_name,
                    "filename": device.device_path
                }
            })).await;

            match create_bdev {
                Ok(_) => {
                    println!("✅ [ATTACH_SPDK] Successfully attached disk to SPDK: {} -> {}", device.device_path, bdev_name);
                }
                Err(e) => {
                    println!("❌ [ATTACH_SPDK] Failed to create AIO bdev for {}: {}", device.device_path, e);
                    return Err(e);
                }
            }
        } else {
            println!("ℹ️ [ATTACH_SPDK] Bdev already exists: {}", bdev_name);
        }

        Ok(())
    }

    /// Read disk cluster metadata (returns default metadata)
    pub async fn read_disk_cluster_metadata(&self, device_name: &str) -> Result<FlintDiskMetadata, Box<dyn std::error::Error + Send + Sync>> {
        // Return default metadata - actual implementation would read from disk
        Ok(FlintDiskMetadata {
            version: 1,
            cluster_id: "default".to_string(),
            cluster_name: Some("default-cluster".to_string()),
            disk_uuid: format!("disk-{}", device_name),
            pool_uuid: "default-pool".to_string(),
            pool_name: "default".to_string(),
            hardware_id: device_name.to_string(),
            serial_number: "unknown".to_string(),
            model: "unknown".to_string(),
            vendor: "unknown".to_string(),
            wwn: None,
            initialized_at: chrono::Utc::now().to_rfc3339(),
            initialized_by_node: "unknown".to_string(),
            last_attached_node: "unknown".to_string(),
            attachment_history: Vec::new(),
            total_size: 1000000000000, // 1TB default
            usable_size: 950000000000,  // 950GB usable
            sector_size: 512,
            optimal_io_size: 4096,
        })
    }

    /// Get device size in bytes
    pub async fn get_device_size(&self, device_name: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let size_path = format!("/sys/block/{}/size", device_name);
        
        if let Ok(size_str) = tokio::fs::read_to_string(&size_path).await {
            if let Ok(sectors) = size_str.trim().parse::<u64>() {
                // Convert sectors to bytes (assuming 512-byte sectors)
                return Ok(sectors * 512);
            }
        }
        
        Err(format!("Could not read size for device {}", device_name).into())
    }

    /// Estimate NVMe size from PCI information (rough estimation)
    pub async fn estimate_nvme_size_from_pci(&self, pci_addr: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        // Try to find associated block device
        if let Ok(device_name) = self.find_nvme_device_name(pci_addr).await {
            return self.get_device_size(&device_name).await;
        }
        
        // Fallback: return 0 if we can't determine size
        Ok(0)
    }

    /// Get model name from PCI vendor/device IDs
    pub async fn get_model_from_pci_ids(&self, vendor_id: &str, device_id: &str) -> String {
        // Simple mapping of common NVMe vendor/device IDs
        match (vendor_id, device_id) {
            ("0x8086", "0x2522") => "Intel NVMe SSD".to_string(),
            ("0x8086", "0x2700") => "Intel Optane SSD".to_string(),
            ("0x144d", "0xa802") => "Samsung NVMe SSD".to_string(),
            ("0x144d", "0xa804") => "Samsung PM9A1 NVMe SSD".to_string(),
            ("0x15b7", "0x5006") => "SanDisk NVMe SSD".to_string(),
            ("0x1344", "0x5407") => "Micron NVMe SSD".to_string(),
            _ => format!("NVMe SSD ({}:{})", vendor_id, device_id),
        }
    }
}

/// Extract NVMe controller name from device name
/// e.g., "nvme0n1" -> "nvme0", "nvme1n1" -> "nvme1"
pub fn extract_nvme_controller_name(device_name: &str) -> String {
    // Find the 'n' that separates controller from namespace (followed by a digit)
    let mut chars = device_name.char_indices().peekable();
    
    while let Some((i, ch)) = chars.next() {
        if ch == 'n' {
            // Check if this 'n' is followed by a digit (namespace number)
            if let Some((_, next_ch)) = chars.peek() {
                if next_ch.is_ascii_digit() {
                    return device_name[..i].to_string();
                }
            }
        }
    }
    
    // Fallback: if no 'n' followed by digit found, return the whole name
    device_name.to_string()
}

/// Check device health using basic filesystem checks
pub async fn check_device_health(_agent: &NodeAgent, device: &NvmeDevice) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Basic health check - verify device is accessible
    let path = std::path::Path::new(&device.device_path);
    
    if !path.exists() {
        return Ok(false);
    }
    
    // Additional health checks could include:
    // - SMART data via nvme-cli
    // - I/O latency tests
    // - Error log analysis
    Ok(true)
}
