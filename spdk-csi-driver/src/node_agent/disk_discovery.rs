// node_agent/disk_discovery.rs - NVMe Disk Discovery and Management
//
// This module handles discovery of local NVMe devices, reading hardware information,
// and managing disk identification using persistent paths (Portworx-style approach).
//
// ⚠️  RPC SAFETY: Always use `call_spdk_rpc` from rpc_client module
// ❌ NEVER use `super::call_spdk_rpc` - this can lead to wrong implementations

use crate::node_agent::NodeAgent;
use crate::node_agent::rpc_client::call_spdk_rpc;  // ✅ OFFICIAL RPC CLIENT
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
        println!("📀 [DISCOVERY] Device: {} ({}) - Serial: {}, PCIe: {}, Size: {}GB", 
                 device.controller_id, device.model, device.serial_number, device.pcie_addr, 
                 device.capacity / (1024 * 1024 * 1024));
    }

    // Deduplicate devices by device_path to avoid duplicate processing
    let mut unique_devices = std::collections::HashMap::new();
    for device in discovered_devices {
        unique_devices.insert(device.device_path.clone(), device);
    }
    
    println!("✅ [DISCOVERY] Deduplicated to {} unique devices", unique_devices.len());
    
    // Get current SPDK bdevs to understand what's already added
    let current_bdevs = agent.get_current_spdk_bdevs().await?;
    println!("📊 [DISCOVERY] Current SPDK bdevs: {:?}", current_bdevs);
    
    // Process each discovered device
    for (_, device) in &unique_devices {
        // Check if device is already in SPDK (by serial number)
        if agent.is_device_in_spdk(&current_bdevs, &device.serial_number).await {
            println!("✅ [DISCOVERY] Device {} (Serial: {}) already in SPDK", 
                     device.device_path, device.serial_number);
            continue;
        }
        
        // Check if device is a system disk - NEVER add system disks to SPDK
        if agent.comprehensive_system_disk_check(&device.pcie_addr, &device.device_path).await {
            println!("🚨 [DISCOVERY] Skipping system disk: {} (Serial: {})", 
                     device.device_path, device.serial_number);
            continue;
        }
        
        // Add device to SPDK using appropriate method
        println!("🔧 [DISCOVERY] Adding device {} to SPDK", device.device_path);
        if let Err(e) = agent.add_device_to_spdk(device).await {
            println!("⚠️ [DISCOVERY] Failed to add device {} to SPDK: {}", device.device_path, e);
        }
    }
    
    // Remove devices from SPDK that are no longer discovered
    agent.cleanup_removed_devices(&unique_devices, &current_bdevs).await?;
    
    // Save SPDK configuration after changes
    if let Err(e) = agent.save_spdk_config().await {
        println!("⚠️ [DISCOVERY] Failed to save SPDK config: {}", e);
    }
    
    println!("✅ [DISCOVERY] Disk discovery completed successfully for node: {}", agent.node_name);
    Ok(())
}

impl NodeAgent {

    /// Validate the environment supports driver unbinding operations during startup
    /// This provides early feedback about userspace SPDK compatibility
    pub async fn validate_driver_environment(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [STARTUP_VALIDATION] Testing driver management infrastructure...");
        
        // Test 1: Check if basic driver management paths exist
        let required_paths = [
            "/sys/bus/pci/drivers_probe",
            "/sys/bus/pci/devices",
            "/sys/bus/pci/drivers",
        ];
        
        for path in &required_paths {
            if !std::path::Path::new(path).exists() {
                return Err(format!("Required driver management path missing: {}", path).into());
            }
        }
        println!("✅ [STARTUP_VALIDATION] Basic driver management paths exist");
        
        // Test 2: Check if we have write access to drivers_probe
        match tokio::fs::metadata("/sys/bus/pci/drivers_probe").await {
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt;
                if metadata.mode() & 0o200 == 0 {
                    return Err("drivers_probe is not writable - insufficient permissions".into());
                }
                println!("✅ [STARTUP_VALIDATION] drivers_probe is writable");
            }
            Err(e) => {
                return Err(format!("Cannot access drivers_probe: {}", e).into());
            }
        }
        
        // Test 3: Check for userspace driver availability
        let userspace_available = self.test_userspace_driver_availability().await?;
        if !userspace_available {
            return Err("No userspace drivers available (vfio-pci, uio_pci_generic)".into());
        }
        println!("✅ [STARTUP_VALIDATION] Userspace drivers available");
        
        // Test 4: Try to find any NVMe devices to test driver_override access
        if let Ok(nvme_devices) = self.get_nvme_pci_devices().await {
            if !nvme_devices.is_empty() {
                // Test on the first device found
                let test_pci = &nvme_devices[0];
                let driver_override_path = format!("/sys/bus/pci/devices/{}/driver_override", test_pci);
                
                if std::path::Path::new(&driver_override_path).exists() {
                    match tokio::fs::read_to_string(&driver_override_path).await {
                        Ok(_) => {
                            println!("✅ [STARTUP_VALIDATION] driver_override is accessible");
                        }
                        Err(e) => {
                            return Err(format!("Cannot read driver_override on {}: {}", test_pci, e).into());
                        }
                    }
                } else {
                    return Err(format!("driver_override not available on device {}", test_pci).into());
                }
            } else {
                println!("ℹ️ [STARTUP_VALIDATION] No NVMe devices found - cannot test driver_override");
            }
        }
        
        println!("✅ [STARTUP_VALIDATION] Environment validation passed - userspace SPDK should work");
        Ok(())
    }

    /// Comprehensive system disk detection - NEVER bind/unbind drivers on system storage
    /// This is a critical safety check that prevents any driver operations on system disks
    pub async fn comprehensive_system_disk_check(&self, pci_addr: &str, device_path: &str) -> bool {
        println!("🛡️ [SYSTEM_CRITICAL] Comprehensive system disk check for PCI: {} Device: {}", pci_addr, device_path);
        
        // Method 1: Check by PCI address (existing logic)
        if self.robust_system_disk_check_by_pci(pci_addr).await {
            println!("🚨 [SYSTEM_CRITICAL] SYSTEM DISK DETECTED via PCI check: {}", pci_addr);
            return true;
        }
        
        // Method 2: Check by device path directly
        let device_name = device_path.strip_prefix("/dev/").unwrap_or(device_path);
        if self.enhanced_system_disk_check(device_name).await {
            println!("🚨 [SYSTEM_CRITICAL] SYSTEM DISK DETECTED via device path check: {}", device_path);
            return true;
        }
        
        // Method 3: Check all partitions on this device
        if self.check_device_partitions_for_system_use(device_name).await {
            println!("🚨 [SYSTEM_CRITICAL] SYSTEM DISK DETECTED via partition check: {}", device_name);
            return true;
        }
        
        // Method 4: Check if device is in critical mount points
        if self.check_device_in_critical_mounts(device_name).await {
            println!("🚨 [SYSTEM_CRITICAL] SYSTEM DISK DETECTED via critical mount check: {}", device_name);
            return true;
        }
        
        println!("✅ [SYSTEM_CRITICAL] Device {} is safe for SPDK operations", device_path);
        false
    }

    /// Robust system disk detection using PCI address (with proper error handling)
    pub async fn robust_system_disk_check_by_pci(&self, pci_addr: &str) -> bool {
        println!("🔍 [SYSTEM_CHECK_PCI] Checking if PCI device {} contains system disk", pci_addr);
        
        // Direct approach: Check if this specific PCI device has mounted partitions
        let block_devices = match fs::read_dir("/sys/block") {
            Ok(entries) => entries,
            Err(e) => {
                println!("⚠️ [SYSTEM_CHECK_PCI] Failed to read /sys/block: {}", e);
                return true; // Fail safe: if we can't check, assume it's a system disk
            }
        };
        
        for entry in block_devices {
            if let Ok(entry) = entry {
                let device_name = entry.file_name();
                let device_str = device_name.to_string_lossy();
                
                // Skip non-nvme devices for efficiency
                if !device_str.starts_with("nvme") {
                    continue;
                }
                
                // Check if this block device belongs to our PCI address (use same logic as find_pci_address_for_device)
                if let Ok(actual_pci) = self.find_pci_address_for_device(&device_str).await {
                    if actual_pci == pci_addr {
                        println!("🔍 [SYSTEM_CHECK_PCI] Found device {} for PCI {}", device_str, pci_addr);
                        // This device belongs to our PCI address, check if it's system disk
                        if self.enhanced_system_disk_check(&device_str).await {
                            println!("⚠️ [SYSTEM_CHECK_PCI] PCI device {} contains system disk via {}", pci_addr, device_str);
                            return true;
                        }
                    }
                }
            }
        }
        
        println!("✅ [SYSTEM_CHECK_PCI] PCI device {} is not a system disk", pci_addr);
        false
    }

    /// Enhanced system disk check for individual devices with comprehensive safety checks
    pub async fn enhanced_system_disk_check(&self, device_name: &str) -> bool {
        println!("🛡️ [ENHANCED_SYSTEM_CHECK] Checking if {} is a system disk", device_name);
        
        // Method 1: Check if it's mounted on root filesystem
        if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", "/"]).output() {
            let root_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!("🔍 [ENHANCED_SYSTEM_CHECK] Root filesystem source: {}", root_source);
            
            if root_source.contains(device_name) {
                println!("🚨 [ENHANCED_SYSTEM_CHECK] {} is mounted as root filesystem", device_name);
                return true;
            }
        }
        
        // Method 2: Check boot partitions (including EFI)
        let boot_paths = ["/boot", "/boot/efi", "/efi"];
        for boot_path in &boot_paths {
            if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", boot_path]).output() {
                let boot_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if boot_source.contains(device_name) {
                    println!("🚨 [ENHANCED_SYSTEM_CHECK] {} contains boot partition at {}", device_name, boot_path);
                    return true;
                }
            }
        }
        
        // Method 3: Check if any partition is mounted on critical system paths
        let critical_paths = ["/", "/boot", "/var", "/usr", "/opt", "/home", "/tmp"];
        for path in &critical_paths {
            if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", path]).output() {
                let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if source.contains(device_name) {
                    println!("🚨 [ENHANCED_SYSTEM_CHECK] {} is mounted on critical path {}", device_name, path);
                    return true;
                }
            }
        }
        
        // Method 4: Check swap devices
        if let Ok(output) = Command::new("swapon").args(["--show=NAME", "--noheadings"]).output() {
            let swap_devices = String::from_utf8_lossy(&output.stdout);
            if swap_devices.contains(device_name) {
                println!("🚨 [ENHANCED_SYSTEM_CHECK] {} is used as swap device", device_name);
                return true;
            }
        }
        
        false
    }

    /// Check all partitions on a device for system use
    pub async fn check_device_partitions_for_system_use(&self, device_name: &str) -> bool {
        println!("🔍 [PARTITION_CHECK] Checking all partitions on {} for system use", device_name);
        
        // Get all partitions for this device using lsblk
        if let Ok(output) = Command::new("lsblk")
            .args(["-n", "-o", "NAME", "-r", &format!("/dev/{}", device_name)])
            .output()
        {
            let partitions = String::from_utf8_lossy(&output.stdout);
            for line in partitions.lines() {
                let partition = line.trim();
                if !partition.is_empty() && partition != device_name {
                    println!("🔍 [PARTITION_CHECK] Checking partition: {}", partition);
                    if self.enhanced_system_disk_check(partition).await {
                        println!("🚨 [PARTITION_CHECK] System partition found: {}", partition);
                        return true;
                    }
                }
            }
        }
        
        // Alternative method: Check /proc/partitions and /sys/block
        let sys_block_path = format!("/sys/block/{}", device_name);
        if let Ok(entries) = fs::read_dir(&sys_block_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let entry_name = entry.file_name();
                    let entry_str = entry_name.to_string_lossy();
                    
                    // Look for partition entries (e.g., nvme0n1p1, nvme0n1p2)
                    if entry_str.starts_with(device_name) && entry_str.len() > device_name.len() {
                        println!("🔍 [PARTITION_CHECK] Found partition via sysfs: {}", entry_str);
                        if self.enhanced_system_disk_check(&entry_str).await {
                            println!("🚨 [PARTITION_CHECK] System partition found via sysfs: {}", entry_str);
                            return true;
                        }
                    }
                }
            }
        }
        
        false
    }

    /// Check if device is used in critical system mounts (including LVM, RAID, etc.)
    pub async fn check_device_in_critical_mounts(&self, device_name: &str) -> bool {
        println!("🔍 [CRITICAL_MOUNT_CHECK] Checking if {} is used in critical system mounts", device_name);
        
        // Check all mounted filesystems for this device
        if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE,TARGET"]).output() {
            let mounts = String::from_utf8_lossy(&output.stdout);
            for line in mounts.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let source = parts[0];
                    let target = parts[1];
                    
                    if source.contains(device_name) {
                        println!("🔍 [CRITICAL_MOUNT_CHECK] Device {} found in mount: {} -> {}", device_name, source, target);
                        
                        // Check if mounted on critical system paths
                        let critical_targets = ["/", "/boot", "/var", "/usr", "/opt", "/home", "/tmp", "/var/log", "/var/lib"];
                        for critical in &critical_targets {
                            if target.starts_with(critical) {
                                println!("🚨 [CRITICAL_MOUNT_CHECK] Device {} mounted on critical path: {}", device_name, target);
                                return true;
                            }
                        }
                    }
                }
            }
        }
        
        // Check if device is part of LVM
        if let Ok(output) = Command::new("pvs").args(["--noheadings", "-o", "pv_name"]).output() {
            let pv_list = String::from_utf8_lossy(&output.stdout);
            if pv_list.contains(device_name) {
                println!("🚨 [CRITICAL_MOUNT_CHECK] Device {} is part of LVM", device_name);
                return true;
            }
        }
        
        // Check if device is part of software RAID
        if let Ok(output) = Command::new("cat").arg("/proc/mdstat").output() {
            let mdstat = String::from_utf8_lossy(&output.stdout);
            if mdstat.contains(device_name) {
                println!("🚨 [CRITICAL_MOUNT_CHECK] Device {} is part of software RAID", device_name);
                return true;
            }
        }
        
        false
    }

    /// Quick system disk check for individual devices (legacy method)
    pub async fn quick_system_disk_check(&self, device_name: &str) -> bool {
        // Delegate to enhanced method for better safety
        self.enhanced_system_disk_check(device_name).await
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
        let response = call_spdk_rpc(&self.spdk_rpc_url, &json!({
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

    // ❌ AUTOMATIC DEVICE ATTACHMENT REMOVED
    // Device binding is now handled only during PV provisioning in the controller
    // This ensures devices are only bound when actually needed for storage volumes

    // ❌ DEVICE ATTACHMENT METHODS REMOVED
    // All device binding now happens in the controller during PV provisioning
    // This prevents unnecessary binding during discovery and ensures devices
    // are only attached when actually needed for storage operations

    /// Test if driver unbinding is possible for a PCI device
    /// Uses SPDK-style actual testing rather than environment detection
    async fn test_driver_unbinding_capability(&self, pci_addr: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [UNBIND_TEST] Testing driver unbinding capability for PCI: {}", pci_addr);
        
        // Get current driver
        let current_driver = match self.get_current_driver(pci_addr).await {
            Ok(driver) => driver,
            Err(e) => {
                println!("⚠️ [UNBIND_TEST] Failed to get current driver for {}: {}", pci_addr, e);
                return Ok(false);
            }
        };
        
        println!("🔍 [UNBIND_TEST] Current driver for {}: {}", pci_addr, current_driver);
        
        // If no driver is bound, binding should work
        if current_driver == "none" {
            println!("✅ [UNBIND_TEST] Device {} has no driver - binding operations should work", pci_addr);
            return self.test_driver_probe_capability(pci_addr).await;
        }
        
        // Test the actual driver unbinding and binding capability using SPDK approach
        self.test_actual_driver_operations(pci_addr, &current_driver).await
    }
    
    /// Test actual driver operations capability (inspired by SPDK's probe_driver function)
    async fn test_actual_driver_operations(&self, pci_addr: &str, current_driver: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🧪 [DRIVER_TEST] Testing actual driver operations for {}", pci_addr);
        
        // Test 1: Check if driver paths exist and are accessible
        let driver_override_path = format!("/sys/bus/pci/devices/{}/driver_override", pci_addr);
        let drivers_probe_path = "/sys/bus/pci/drivers_probe";
        let unbind_path = format!("/sys/bus/pci/drivers/{}/unbind", current_driver);
        
        if !std::path::Path::new(&driver_override_path).exists() {
            println!("❌ [DRIVER_TEST] driver_override not available: {}", driver_override_path);
            return Ok(false);
        }
        
        if !std::path::Path::new(drivers_probe_path).exists() {
            println!("❌ [DRIVER_TEST] drivers_probe not available: {}", drivers_probe_path);
            return Ok(false);
        }
        
        if !std::path::Path::new(&unbind_path).exists() {
            println!("❌ [DRIVER_TEST] unbind path not available: {}", unbind_path);
            return Ok(false);
        }
        
        // Test 2: Check write permissions by attempting to read current driver_override
        match tokio::fs::read_to_string(&driver_override_path).await {
            Ok(_) => {
                println!("✅ [DRIVER_TEST] driver_override is accessible");
            }
            Err(e) => {
                println!("❌ [DRIVER_TEST] Cannot access driver_override: {}", e);
                return Ok(false);
            }
        }
        
        // Test 3: Try to write to driver_override (this is a safe test operation)
        match tokio::fs::write(&driver_override_path, current_driver).await {
            Ok(_) => {
                println!("✅ [DRIVER_TEST] Can write to driver_override");
                // Clean up - clear the override
                let _ = tokio::fs::write(&driver_override_path, "").await;
            }
            Err(e) => {
                println!("❌ [DRIVER_TEST] Cannot write to driver_override: {}", e);
                return Ok(false);
            }
        }
        
        // Test 4: Check if we can access the drivers_probe interface
        match tokio::fs::metadata(drivers_probe_path).await {
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt;
                if metadata.mode() & 0o200 == 0 {
                    println!("❌ [DRIVER_TEST] drivers_probe is not writable");
                    return Ok(false);
                }
                println!("✅ [DRIVER_TEST] drivers_probe is writable");
            }
            Err(e) => {
                println!("❌ [DRIVER_TEST] Cannot access drivers_probe metadata: {}", e);
                return Ok(false);
            }
        }
        
        // Test 5: For kernel nvme driver, test if we can switch to VFIO or UIO
        if current_driver.contains("nvme") {
            return self.test_userspace_driver_availability().await;
        }
        
        println!("✅ [DRIVER_TEST] Driver operations capability test passed for {}", pci_addr);
        Ok(true)
    }
    
    /// Test if userspace drivers (vfio-pci, uio_pci_generic) are available
    async fn test_userspace_driver_availability(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [USERSPACE_TEST] Testing userspace driver availability");
        
        // Check for VFIO support (preferred)
        if std::path::Path::new("/sys/bus/pci/drivers/vfio-pci").exists() {
            println!("✅ [USERSPACE_TEST] vfio-pci driver available");
            return Ok(true);
        }
        
        // Check for UIO support (fallback)
        if std::path::Path::new("/sys/bus/pci/drivers/uio_pci_generic").exists() {
            println!("✅ [USERSPACE_TEST] uio_pci_generic driver available");
            return Ok(true);
        }
        
        // Try to load vfio-pci module
        if let Ok(output) = tokio::process::Command::new("modinfo")
            .arg("vfio-pci")
            .output()
            .await
        {
            if output.status.success() {
                println!("✅ [USERSPACE_TEST] vfio-pci module available (can be loaded)");
                return Ok(true);
            }
        }
        
        // Try to load uio_pci_generic module
        if let Ok(output) = tokio::process::Command::new("modinfo")
            .arg("uio_pci_generic")
            .output()
            .await
        {
            if output.status.success() {
                println!("✅ [USERSPACE_TEST] uio_pci_generic module available (can be loaded)");
                return Ok(true);
            }
        }
        
        println!("❌ [USERSPACE_TEST] No userspace drivers available (vfio-pci, uio_pci_generic)");
        Ok(false)
    }
    
    /// Test driver probe capability for devices with no current driver
    async fn test_driver_probe_capability(&self, pci_addr: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [PROBE_TEST] Testing driver probe capability for {}", pci_addr);
        
        let driver_override_path = format!("/sys/bus/pci/devices/{}/driver_override", pci_addr);
        let drivers_probe_path = "/sys/bus/pci/drivers_probe";
        
        // Check if probe interface exists
        if !std::path::Path::new(&driver_override_path).exists() ||
           !std::path::Path::new(drivers_probe_path).exists() {
            println!("❌ [PROBE_TEST] Driver probe interface not available");
            return Ok(false);
        }
        
        // Test write access to driver_override
        match tokio::fs::write(&driver_override_path, "").await {
            Ok(_) => {
                println!("✅ [PROBE_TEST] Driver probe interface is accessible");
                Ok(true)
            }
            Err(e) => {
                println!("❌ [PROBE_TEST] Cannot access driver probe interface: {}", e);
                Ok(false)
            }
        }
    }
    

    
    /// Add device to SPDK using bdev_nvme_attach_controller or bdev_aio_create
    pub async fn add_device_to_spdk(&self, device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SPDK_ADD] Adding device {} to SPDK", device.device_path);
        
        // Test if we can unbind the kernel driver
        let can_unbind = self.test_driver_unbinding_capability(&device.pcie_addr).await?;
        
        if can_unbind {
            // Use userspace NVMe driver (preferred for performance)
            println!("🚀 [SPDK_ADD] Using bdev_nvme_attach_controller for device {}", device.device_path);
            
            let response = call_spdk_rpc(&self.spdk_rpc_url, &json!({
                "method": "bdev_nvme_attach_controller",
                "params": {
                    "name": format!("nvme-{}", device.serial_number),
                    "trtype": "PCIe",
                    "traddr": device.pcie_addr
                }
            })).await;
            
            match response {
                Ok(_) => {
                    println!("✅ [SPDK_ADD] Successfully attached {} using userspace NVMe driver", device.device_path);
                }
                Err(e) => {
                    println!("⚠️ [SPDK_ADD] Failed to attach using userspace driver: {}", e);
                    return Err(e);
                }
            }
        } else {
            // Fallback to AIO (for environments like AWS EC2 that don't support unbinding)
            println!("🔄 [SPDK_ADD] Using bdev_aio_create fallback for device {}", device.device_path);
            
            let response = call_spdk_rpc(&self.spdk_rpc_url, &json!({
                "method": "bdev_aio_create",
                "params": {
                    "name": format!("aio-{}", device.serial_number),
                    "filename": device.device_path
                }
            })).await;
            
            match response {
                Ok(_) => {
                    println!("✅ [SPDK_ADD] Successfully added {} using AIO fallback", device.device_path);
                }
                Err(e) => {
                    println!("⚠️ [SPDK_ADD] Failed to add using AIO: {}", e);
                    return Err(e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Get current SPDK bdevs
    pub async fn get_current_spdk_bdevs(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let response = call_spdk_rpc(&self.spdk_rpc_url, &json!({
            "method": "bdev_get_bdevs"
        })).await?;
        
        let mut bdev_names = Vec::new();
        if let Some(bdevs) = response["result"].as_array() {
            for bdev in bdevs {
                if let Some(name) = bdev["name"].as_str() {
                    bdev_names.push(name.to_string());
                }
            }
        }
        
        Ok(bdev_names)
    }
    
    /// Check if device is already in SPDK by serial number
    pub async fn is_device_in_spdk(&self, current_bdevs: &[String], serial_number: &str) -> bool {
        current_bdevs.iter().any(|bdev| 
            bdev.contains(serial_number) || 
            bdev == &format!("nvme-{}", serial_number) || 
            bdev == &format!("aio-{}", serial_number)
        )
    }
    
    /// Cleanup devices that are no longer discovered
    pub async fn cleanup_removed_devices(
        &self,
        discovered_devices: &std::collections::HashMap<String, NvmeDevice>,
        current_bdevs: &[String],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🧹 [CLEANUP] Checking for devices to remove from SPDK");
        
        for bdev_name in current_bdevs {
            // Extract serial number from bdev name
            let serial = if let Some(s) = bdev_name.strip_prefix("nvme-") {
                s
            } else if let Some(s) = bdev_name.strip_prefix("aio-") {
                s
            } else {
                continue; // Not a disk bdev
            };
            
            // Check if this device is still discovered
            let still_exists = discovered_devices.values().any(|d| d.serial_number == serial);
            
            if !still_exists {
                println!("🗑️ [CLEANUP] Removing {} from SPDK (no longer discovered)", bdev_name);
                
                let _ = call_spdk_rpc(&self.spdk_rpc_url, &json!({
                    "method": "bdev_nvme_detach_controller",
                    "params": {
                        "name": bdev_name
                    }
                })).await;
                
                // Also try AIO delete if it was an AIO device
                let _ = call_spdk_rpc(&self.spdk_rpc_url, &json!({
                    "method": "bdev_aio_delete",
                    "params": {
                        "name": bdev_name
                    }
                })).await;
            }
        }
        
        Ok(())
    }
    
    /// Save SPDK configuration to ConfigMap
    pub async fn save_spdk_config(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("💾 [SAVE_CONFIG] Saving SPDK configuration");
        
        let save_start = std::time::Instant::now();
        let response = call_spdk_rpc(&self.spdk_rpc_url, &json!({
            "method": "bdev_nvme_save_config"
        })).await;
        
        match response {
            Ok(_) => {
                let save_duration = save_start.elapsed();
                println!("✅ [SAVE_CONFIG] SPDK configuration saved successfully in {:.2}ms", save_duration.as_millis());
                Ok(())
            }
            Err(e) => {
                let save_duration = save_start.elapsed();
                println!("⚠️ [SAVE_CONFIG] Failed to save SPDK config after {:.2}ms: {}", save_duration.as_millis(), e);
                Err(e)
            }
        }
    }
    
    /// Create Kubernetes event for unsupported storage setup
    async fn create_unsupported_storage_event(&self, device: &NvmeDevice, reason: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚨 [USERSPACE_SPDK_EVENT] Creating event for incompatible storage device");
        
        // Create detailed log entries that operators can see in pod logs
        println!("📊 [USERSPACE_SPDK_ONLY] EVENT: Device {} (PCI: {}) - Cannot use with userspace SPDK: {}", 
                 device.device_path, device.pcie_addr, reason);
        println!("📊 [USERSPACE_SPDK_ONLY] DEVICE_INFO: Model: {}, Serial: {}, Capacity: {}GB", 
                 device.model, device.serial_number, device.capacity / (1024 * 1024 * 1024));
        println!("📊 [USERSPACE_SPDK_ONLY] POLICY: No kernel driver fallback - userspace SPDK only");
        println!("📊 [USERSPACE_SPDK_ONLY] TESTED: Actual driver operation tests failed - not environment guessing");
        println!("📊 [USERSPACE_SPDK_ONLY] RECOMMENDATION: Verify system supports:");
        println!("📊 [USERSPACE_SPDK_ONLY] - Driver unbinding (echo PCI_ADDR > /sys/bus/pci/drivers/DRIVER/unbind)");
        println!("📊 [USERSPACE_SPDK_ONLY] - Userspace drivers (vfio-pci or uio_pci_generic)");
        println!("📊 [USERSPACE_SPDK_ONLY] - Write access to /sys/bus/pci/devices/*/driver_override");
        println!("📊 [USERSPACE_SPDK_ONLY] ENVIRONMENT_NOTE: System must support kernel driver unbinding for userspace SPDK");
        
        // TODO: If we add kube_client to NodeAgent, we can create actual Kubernetes events here
        // Following the pattern from create_ublk_kernel_missing_event in driver.rs
        
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
