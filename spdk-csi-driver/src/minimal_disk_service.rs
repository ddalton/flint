// minimal_disk_service.rs - Minimal State Disk Service
// FOR NODE AGENTS ONLY - Uses direct Unix socket communication with SPDK

use serde_json::{json, Value};
use reqwest::Client as HttpClient;

use crate::minimal_models::{DiskInfo, MinimalStateError};

/// Pure SPDK disk discovery and management service
/// FOR NODE AGENTS ONLY - Uses direct Unix socket communication with SPDK  
/// Replaces all Kubernetes CRD operations with direct SPDK queries
#[derive(Clone)]
pub struct MinimalDiskService {
    pub node_name: String,
    pub spdk_socket_path: String,  // Unix socket path (e.g., "/tmp/spdk.sock")
    pub http_client: HttpClient,
}

impl MinimalDiskService {
    pub fn new(node_name: String, spdk_socket_path: String) -> Self {
        Self {
            node_name,
            spdk_socket_path,
            http_client: HttpClient::new(),
        }
    }

    /// Discover all disks on this node with auto-recovery
    pub async fn discover_local_disks(&self) -> Result<Vec<DiskInfo>, MinimalStateError> {
        println!("🔍 [MINIMAL_DISK] Starting disk discovery with auto-recovery on node: {}", self.node_name);

        // Step 1: Auto-recover SPDK state for physical devices
        if let Err(e) = self.auto_recover_spdk_state().await {
            println!("⚠️ [MINIMAL_DISK] Auto-recovery failed (continuing anyway): {}", e);
        }

        // Step 2: Get current SPDK state after recovery
        let bdevs = self.get_spdk_bdevs().await?;
        let lvstores = self.get_spdk_lvstores().await?;
        let controllers = self.get_spdk_nvme_controllers().await?;

        let mut disks = Vec::new();
        println!("🔧 [DEBUG] bdevs JSON structure: {}", serde_json::to_string_pretty(&bdevs).unwrap_or_else(|_| "JSON error".to_string()));
        
        if let Some(bdev_list) = bdevs["result"].as_array() {
            println!("🔧 [DEBUG] Found {} bdevs in result array", bdev_list.len());
            for (i, bdev) in bdev_list.iter().enumerate() {
                println!("🔧 [DEBUG] Processing bdev {}: {}", i, serde_json::to_string(bdev).unwrap_or_else(|_| "JSON error".to_string()));
                
                if let Some(disk_info) = self.bdev_to_disk_info(bdev, &lvstores, &controllers).await? {
                    println!("🔧 [DEBUG] Created DiskInfo: name={}, pci={}, healthy={}", disk_info.device_name, disk_info.pci_address, disk_info.healthy);
                    
                    // Filter out system disks and non-storage devices
                    if self.is_storage_disk(&disk_info).await? {
                        println!("✅ [DEBUG] Added disk to list: {}", disk_info.device_name);
                        disks.push(disk_info);
                    } else {
                        println!("❌ [DEBUG] Filtered out disk: {}", disk_info.device_name);
                    }
                } else {
                    println!("❌ [DEBUG] bdev_to_disk_info returned None for bdev {}", i);
                }
            }
        } else {
            println!("❌ [DEBUG] No bdev array found in result!");
            println!("🔧 [DEBUG] bdevs keys: {:?}", bdevs.as_object().map(|o| o.keys().collect::<Vec<_>>()));
        }

        println!("✅ [MINIMAL_DISK] Discovered {} local storage disks", disks.len());
        Ok(disks)
    }

    /// Initialize blobstore (LVS) on a disk by PCI address
    pub async fn initialize_blobstore(&self, pci_address: &str) -> Result<String, MinimalStateError> {
        println!("🔧 [MINIMAL_DISK] Initializing blobstore on disk with PCI: {}", pci_address);

        // Find the disk first
        let disk_found = self.discover_local_disks().await?
            .into_iter()
            .find(|d| d.pci_address == pci_address)
            .ok_or_else(|| MinimalStateError::DiskNotFound { 
                node: self.node_name.clone(), 
                pci: pci_address.to_string() 
            })?;

        // Check if LVS already exists
        if disk_found.blobstore_initialized {
            println!("✅ [MINIMAL_DISK] LVS already exists: {:?}", disk_found.lvs_name);
            return Ok(disk_found.lvs_name.unwrap_or_else(|| "unknown".to_string()));
        }

        // Create new LVS
        let lvs_name = format!("lvs_{}_{}", self.node_name, pci_address.replace(":", "-").replace(".", "-"));
        let bdev_name = &disk_found.bdev_name;

        let create_lvs_params = json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": bdev_name,
                "lvs_name": lvs_name,
                "cluster_sz": 1048576  // 1MB clusters
            }
        });

        let response = self.call_spdk_rpc(&create_lvs_params).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to create LVS: {}", e) 
            })?;

        if let Some(error) = response.get("error") {
            return Err(MinimalStateError::SpdkRpcError { 
                message: format!("SPDK LVS creation failed: {}", error) 
            });
        }

        println!("✅ [MINIMAL_DISK] Successfully created LVS: {}", lvs_name);
        Ok(lvs_name)
    }

    /// Create logical volume on a disk
    pub async fn create_lvol(&self, lvs_name: &str, volume_id: &str, size_bytes: u64) -> Result<String, MinimalStateError> {
        println!("🔧 [MINIMAL_DISK] Creating lvol: {} in LVS: {} (size: {} bytes)", volume_id, lvs_name, size_bytes);
        
        let lvol_name = format!("vol_{}", volume_id);
        let size_mib = (size_bytes + 1048575) / 1048576; // Round up to MiB

        let create_params = json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size_in_mib": size_mib
            }
        });

        let response = self.call_spdk_rpc(&create_params).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to create lvol: {}", e) 
            })?;

        let lvol_uuid = response["result"].as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError { 
                message: "No UUID in SPDK lvol create response".to_string() 
            })?
            .to_string();

        println!("✅ [MINIMAL_DISK] Created lvol {} with UUID: {}", lvol_name, lvol_uuid);
        Ok(lvol_uuid)
    }

    /// Delete logical volume 
    pub async fn delete_lvol(&self, lvol_uuid: &str) -> Result<(), MinimalStateError> {
        println!("🗑️ [MINIMAL_DISK] Deleting lvol with UUID: {}", lvol_uuid);

        let delete_params = json!({
            "method": "bdev_lvol_delete",
            "params": {
                "name": lvol_uuid
            }
        });

        self.call_spdk_rpc(&delete_params).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to delete lvol: {}", e) 
            })?;

        println!("✅ [MINIMAL_DISK] Successfully deleted lvol: {}", lvol_uuid);  
        Ok(())
    }

    // === PRIVATE HELPER METHODS ===

    /// Auto-recover SPDK state for physical NVMe devices
    async fn auto_recover_spdk_state(&self) -> Result<(), MinimalStateError> {
        println!("🔄 [AUTO_RECOVERY] Starting SPDK state recovery for node: {}", self.node_name);

        // Get all physical NVMe devices
        let nvme_devices = self.discover_physical_nvme_devices().await?;
        println!("🔄 [AUTO_RECOVERY] Found {} physical NVMe devices", nvme_devices.len());

        for device in nvme_devices {
            println!("🔄 [AUTO_RECOVERY] Processing device: {} ({})", device.device_name, device.pci_address);
            
            // Skip system disks
            if self.is_system_disk_physical(&device).await {
                println!("⏭️ [AUTO_RECOVERY] Skipping system disk: {}", device.device_name);
                continue;
            }

            // Auto-create bdev if device should have SPDK access
            match self.ensure_device_bdev_exists(&device).await {
                Ok(bdev_name) => {
                    println!("🔍 [AUTO_RECOVERY] Bdev ready: {}, now checking for existing LVS...", bdev_name);
                    println!("🔍 [AUTO_RECOVERY] Device details: {} ({}), Size: {}GB", 
                             device.device_name, device.pci_address, device.size_bytes / (1024*1024*1024));
                    
                    // CRITICAL: Wait for SPDK to auto-discover existing LVS from this bdev
                    // In modern SPDK, the lvol module asynchronously examines new bdevs for LVS
                    if let Some(lvs_name) = self.wait_for_lvs_discovery(&bdev_name, 5).await {
                        println!("✅ [AUTO_RECOVERY] Auto-discovered existing LVS: {} on {}", lvs_name, bdev_name);
                    } else {
                        println!("ℹ️ [AUTO_RECOVERY] No LVS found on bdev: {} (disk is clean)", bdev_name);
                    }
                }
                Err(e) => {
                    println!("⚠️ [AUTO_RECOVERY] Failed to ensure bdev for {}: {}", device.device_name, e);
                }
            }
        }

        println!("✅ [AUTO_RECOVERY] SPDK state recovery completed");
        Ok(())
    }

    /// Discover physical NVMe devices via system inspection
    async fn discover_physical_nvme_devices(&self) -> Result<Vec<PhysicalDevice>, MinimalStateError> {
        use std::process::Command;

        println!("🔍 [PHYSICAL_DISCOVERY] Scanning for NVMe devices via lspci...");
        
        let output = Command::new("lspci")
            .args(["-D", "-d", "::0108"]) // NVMe class code
            .output()
            .map_err(|e| MinimalStateError::InternalError { 
                message: format!("Failed to run lspci: {}", e) 
            })?;

        let stdout = String::from_utf8(output.stdout)
            .map_err(|e| MinimalStateError::InternalError { 
                message: format!("Invalid lspci output: {}", e) 
            })?;

        let mut devices = Vec::new();
        for line in stdout.lines() {
            if let Some(pci_addr) = line.split_whitespace().next() {
                println!("🔍 [PHYSICAL_DISCOVERY] Found NVMe device: {}", pci_addr);
                
                // Get device info
                if let Ok(device_info) = self.get_physical_device_info(pci_addr).await {
                    devices.push(device_info);
                }
            }
        }

        println!("✅ [PHYSICAL_DISCOVERY] Found {} NVMe devices", devices.len());
        Ok(devices)
    }

    /// Get physical device information from system
    async fn get_physical_device_info(&self, pci_address: &str) -> Result<PhysicalDevice, MinimalStateError> {
        // Get current driver
        let driver = self.get_current_driver(pci_address).await
            .unwrap_or_else(|_| "unbound".to_string());

        // Try to find device name if bound to nvme driver
        let device_name = if driver == "nvme" {
            self.find_nvme_device_name(pci_address).await
                .unwrap_or_else(|_| format!("nvme-{}", pci_address.replace(":", "-")))
        } else {
            format!("nvme-{}", pci_address.replace(":", "-"))
        };

        // Get size - estimate for unbound devices
        let size_bytes = if driver == "nvme" {
            self.get_device_size_from_blockdev(&device_name).await
                .unwrap_or(1_000_000_000_000) // 1TB default
        } else {
            1_000_000_000_000 // 1TB default for unbound
        };

        Ok(PhysicalDevice {
            pci_address: pci_address.to_string(),
            device_name,
            driver,
            size_bytes,
            model: "NVMe Device".to_string(), // Could enhance with PCI ID lookup
        })
    }

    /// Ensure a physical device has appropriate SPDK bdev
    async fn ensure_device_bdev_exists(&self, device: &PhysicalDevice) -> Result<String, MinimalStateError> {
        println!("🔄 [BDEV_RECOVERY] Ensuring bdev exists for device: {}", device.device_name);

        let expected_bdev_name = if device.driver == "nvme" {
            // Kernel-bound device -> AIO bdev
            format!("kernel_{}", device.device_name)
        } else {
            // Unbound/SPDK-bound device -> would need NVMe controller attach
            // For now, skip unbound devices in auto-recovery
            println!("⏭️ [BDEV_RECOVERY] Skipping unbound device: {}", device.device_name);
            return Err(MinimalStateError::InternalError { 
                message: "Unbound device skipped".to_string() 
            });
        };

        // Check if bdev already exists
        let bdevs = self.get_spdk_bdevs().await?;
        if let Some(bdev_list) = bdevs["result"].as_array() {
            for bdev in bdev_list {
                if let Some(name) = bdev["name"].as_str() {
                    if name == expected_bdev_name {
                        println!("✅ [BDEV_RECOVERY] Bdev already exists: {}", expected_bdev_name);
                        return Ok(expected_bdev_name);
                    }
                }
            }
        }

        // Create missing AIO bdev for kernel-bound device
        if device.driver == "nvme" {
            let correlation_id = format!("{:08x}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros() as u32);
            println!("🔧 [BDEV_RECOVERY:{}] Creating AIO bdev: {}", correlation_id, expected_bdev_name);
            println!("🔧 [BDEV_RECOVERY:{}] Device: /dev/{}, PCI: {}", correlation_id, device.device_name, device.pci_address);
            println!("🔧 [BDEV_RECOVERY:{}] CORRELATE: Watch SPDK logs for bdev_aio.c create messages and vbdev_lvol.c examine", correlation_id);
            
            let device_path = format!("/dev/{}", device.device_name);
            
            let aio_params = serde_json::json!({
                "method": "bdev_aio_create",
                "params": {
                    "name": expected_bdev_name,
                    "filename": device_path
                    // Note: Not specifying block_size - let SPDK auto-detect from device
                    // This prevents errors when disk size is not a multiple of 4096
                }
            });

            match self.call_spdk_rpc(&aio_params).await {
                Ok(_) => {
                    println!("✅ [BDEV_RECOVERY:{}] Successfully created AIO bdev: {}", correlation_id, expected_bdev_name);
                    println!("✅ [BDEV_RECOVERY:{}] SPDK will now asynchronously examine this bdev for existing LVS", correlation_id);
                    return Ok(expected_bdev_name);
                }
                Err(e) if e.to_string().contains("File exists") => {
                    println!("✅ [BDEV_RECOVERY:{}] AIO bdev already exists: {}", correlation_id, expected_bdev_name);
                    return Ok(expected_bdev_name);
                }
                Err(e) => {
                    println!("⚠️ [BDEV_RECOVERY:{}] Failed to create AIO bdev {}: {}", correlation_id, expected_bdev_name, e);
                    println!("⚠️ [BDEV_RECOVERY:{}] Error details: {}", correlation_id, e);
                    return Err(MinimalStateError::SpdkRpcError { 
                        message: format!("Failed to create AIO bdev: {}", e) 
                    });
                }
            }
        }

        Err(MinimalStateError::InternalError { 
            message: "Unexpected code path in ensure_device_bdev_exists".to_string() 
        })
    }

    /// Wait for SPDK to auto-discover LVS on a bdev (async examination)
    /// In modern SPDK, when a bdev is created, the lvol module asynchronously examines it for existing LVS
    async fn wait_for_lvs_discovery(&self, bdev_name: &str, timeout_secs: u64) -> Option<String> {
        let correlation_id = format!("{:08x}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u32);
        println!("🔍 [LVS_DISCOVERY:{}] Waiting for SPDK to auto-discover LVS on bdev: {} (timeout: {}s)", 
                 correlation_id, bdev_name, timeout_secs);
        println!("🔍 [LVS_DISCOVERY:{}] CORRELATE: Check SPDK logs for vbdev_lvol.c messages about '{}'", 
                 correlation_id, bdev_name);
        
        use tokio::time::{sleep, Duration};
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        
        let mut iteration = 0;
        while start.elapsed() < timeout {
            iteration += 1;
            
            if iteration % 10 == 1 {
                println!("🔄 [LVS_DISCOVERY:{}] Polling iteration {} (elapsed: {}ms)", 
                         correlation_id, iteration, start.elapsed().as_millis());
            }
            
            // Query for all lvstores
            match self.call_spdk_rpc(&json!({
                "method": "bdev_lvol_get_lvstores"
            })).await {
                Ok(response) => {
                    if let Some(lvstore_list) = response["result"].as_array() {
                        if iteration % 10 == 1 && !lvstore_list.is_empty() {
                            println!("🔍 [LVS_DISCOVERY:{}] Found {} total LVS in system", correlation_id, lvstore_list.len());
                            for (idx, lvstore) in lvstore_list.iter().enumerate() {
                                let name = lvstore["name"].as_str().unwrap_or("unknown");
                                let base = lvstore["base_bdev"].as_str().unwrap_or("unknown");
                                println!("🔍 [LVS_DISCOVERY:{}]   LVS[{}]: name='{}', base_bdev='{}'", 
                                         correlation_id, idx, name, base);
                            }
                        }
                        
                        for lvstore in lvstore_list {
                            if let Some(base_bdev) = lvstore["base_bdev"].as_str() {
                                if base_bdev == bdev_name {
                                    let lvs_name = lvstore["name"].as_str().unwrap_or("unknown").to_string();
                                    let lvs_uuid = lvstore["uuid"].as_str().unwrap_or("unknown");
                                    let free_clusters = lvstore["free_clusters"].as_u64().unwrap_or(0);
                                    let cluster_size = lvstore["cluster_size"].as_u64().unwrap_or(0);
                                    let free_gb = (free_clusters * cluster_size) / (1024*1024*1024);
                                    
                                    println!("✅ [LVS_DISCOVERY:{}] SUCCESS! Found LVS after {} iterations ({}ms)", 
                                             correlation_id, iteration, start.elapsed().as_millis());
                                    println!("✅ [LVS_DISCOVERY:{}]   LVS Name: {}", correlation_id, lvs_name);
                                    println!("✅ [LVS_DISCOVERY:{}]   LVS UUID: {}", correlation_id, lvs_uuid);
                                    println!("✅ [LVS_DISCOVERY:{}]   Base Bdev: {}", correlation_id, base_bdev);
                                    println!("✅ [LVS_DISCOVERY:{}]   Free Space: {}GB", correlation_id, free_gb);
                                    
                                    return Some(lvs_name);
                                }
                            }
                        }
                    } else if iteration % 10 == 1 {
                        println!("🔍 [LVS_DISCOVERY:{}] Query returned no lvstores array", correlation_id);
                    }
                }
                Err(e) => {
                    println!("⚠️ [LVS_DISCOVERY:{}] Failed to query lvstores (iteration {}): {}", correlation_id, iteration, e);
                }
            }
            
            // Wait 100ms before next check
            sleep(Duration::from_millis(100)).await;
        }
        
        println!("❌ [LVS_DISCOVERY:{}] TIMEOUT! No LVS discovered on bdev '{}' after {}s ({} iterations)", 
                 correlation_id, bdev_name, timeout_secs, iteration);
        println!("❌ [LVS_DISCOVERY:{}] CORRELATE: Check SPDK logs around this time for vbdev_lvol examination of '{}'", 
                 correlation_id, bdev_name);
        None
    }

    /// Helper methods for physical device discovery
    async fn get_current_driver(&self, pci_addr: &str) -> Result<String, MinimalStateError> {
        let driver_path = format!("/sys/bus/pci/devices/{}/driver", pci_addr);
        
        match std::fs::read_link(&driver_path) {
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

    async fn find_nvme_device_name(&self, pci_addr: &str) -> Result<String, MinimalStateError> {
        use std::fs;
        let devices_dir = "/sys/block";
        
        println!("🔍 [DEVICE_MAPPING] Looking for device name for PCI: {}", pci_addr);
        
        for entry in fs::read_dir(devices_dir).map_err(|e| MinimalStateError::InternalError { 
            message: format!("Failed to read /sys/block: {}", e) 
        })? {
            let entry = entry.map_err(|e| MinimalStateError::InternalError { 
                message: format!("Failed to read directory entry: {}", e) 
            })?;
            
            if let Some(device_name) = entry.file_name().to_str() {
                if device_name.starts_with("nvme") && device_name.ends_with("n1") {
                    // Check if this device corresponds to our PCI address via symlink
                    let device_symlink = format!("/sys/block/{}", device_name);
                    if let Ok(link) = fs::read_link(&device_symlink) {
                        let link_str = link.to_string_lossy();
                        println!("🔍 [DEVICE_MAPPING] Checking {} -> {}", device_name, link_str);
                        if link_str.contains(pci_addr) {
                            println!("✅ [DEVICE_MAPPING] Found match: {} -> {}", pci_addr, device_name);
                            return Ok(device_name.to_string());
                        }
                    }
                }
            }
        }
        
        println!("❌ [DEVICE_MAPPING] No device found for PCI: {}", pci_addr);
        Err(MinimalStateError::DiskNotFound { 
            node: self.node_name.clone(), 
            pci: pci_addr.to_string() 
        })
    }

    async fn get_device_size_from_blockdev(&self, device_name: &str) -> Result<u64, MinimalStateError> {
        use std::process::Command;
        
        let output = Command::new("blockdev")
            .args(["--getsize64", &format!("/dev/{}", device_name)])
            .output()
            .map_err(|e| MinimalStateError::InternalError { 
                message: format!("Failed to run blockdev: {}", e) 
            })?;

        let size_str = String::from_utf8(output.stdout)
            .map_err(|e| MinimalStateError::InternalError { 
                message: format!("Invalid blockdev output: {}", e) 
            })?;
        
        size_str.trim().parse().map_err(|e| MinimalStateError::InternalError { 
            message: format!("Failed to parse device size: {}", e) 
        })
    }

    async fn is_system_disk_physical(&self, device: &PhysicalDevice) -> bool {
        // Simple heuristic: if device contains root filesystem, it's a system disk
        // This could be enhanced with more sophisticated detection
        if device.driver == "nvme" {
            // Check if any partition is mounted on /
            // This is a simplified check - production would be more thorough
            false // For now, assume no system disks in our test environment
        } else {
            false
        }
    }

    /// Call SPDK RPC via Unix socket (NODE AGENT pattern)
        pub async fn call_spdk_rpc(&self, rpc_request: &Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
            use crate::spdk_native::SpdkNative;

            let method = rpc_request["method"].as_str().unwrap_or("unknown");
            println!("🔧 [NODE_AGENT_SPDK] Calling SPDK method via Unix socket: {}", method);
            println!("🔧 [SPDK_RPC] Original socket URL: {}", self.spdk_socket_path);
            
            // Handle socket path like raid_over_lv branch
            let spdk_socket = self.spdk_socket_path.trim_start_matches("unix://");
            println!("🔧 [SPDK_RPC] Cleaned socket path: {}", spdk_socket);
            
            // Check if socket file exists before attempting connection (from raid_over_lv)
            if !std::path::Path::new(spdk_socket).exists() {
                let error_msg = format!("SPDK socket file does not exist: {}", spdk_socket);
                println!("❌ [SPDK_RPC] {}", error_msg);
                return Err(error_msg.into());
            }
            
            println!("✅ [SPDK_RPC] Socket file exists, creating SPDK client...");

            // Use Unix socket connection (matches raid_over_lv pattern)
            let spdk = SpdkNative::new(Some(spdk_socket.to_string())).await
                .map_err(|e| {
                    let error_msg = format!("Failed to create SPDK client for socket {}: {}", spdk_socket, e);
                    println!("❌ [SPDK_RPC] {}", error_msg);
                    error_msg
                })?;

            // Call method using the new persistent socket client (matches raid_over_lv)
            println!("🔧 [SPDK_RPC] Calling method '{}' with params: {:?}", method, rpc_request.get("params"));
            let result = match method {
            "bdev_get_bdevs" => {
                // Use generic RPC call to get full bdev objects, not just names
                spdk.call_method("bdev_get_bdevs", None).await
                    .map_err(|e| {
                        let error_msg = format!("SPDK RPC call 'bdev_get_bdevs' failed: {}", e);
                        println!("❌ [SPDK_RPC] {}", error_msg);
                        Box::new(std::io::Error::new(std::io::ErrorKind::Other, error_msg)) as Box<dyn std::error::Error + Send + Sync>
                    })?
            }
            "bdev_lvol_get_lvstores" => {
                let lvstores = spdk.get_lvol_stores().await?;
                // Convert LvsInfo to serializable format  
                let serializable_lvstores: Vec<serde_json::Value> = lvstores.into_iter().map(|lvs| {
                    json!({
                        "name": lvs.name,
                        "uuid": lvs.uuid,
                        "cluster_size": lvs.cluster_size,
                        "total_clusters": lvs.total_clusters,
                        "free_clusters": lvs.free_clusters,
                        "block_size": lvs.block_size
                    })
                }).collect();
                json!(serializable_lvstores)
            }
            "bdev_nvme_get_controllers" => {
                let controllers = spdk.get_nvme_controllers().await?;
                json!(controllers)
            }
            "bdev_lvol_create_lvstore" => {
                let params = rpc_request["params"].as_object()
                    .ok_or("Missing params for lvstore creation")?;
                let bdev_name = params["bdev_name"].as_str().unwrap_or("");
                let lvs_name = params["lvs_name"].as_str().unwrap_or("");
                let cluster_sz = params["cluster_sz"].as_u64().unwrap_or(1048576);
                
                spdk.create_lvs(bdev_name, lvs_name, cluster_sz).await?;
                json!("success")
            }
            "bdev_lvol_create" => {
                let params = rpc_request["params"].as_object()
                    .ok_or("Missing params for lvol creation")?;
                let lvs_name = params["lvs_name"].as_str().unwrap_or("");
                let lvol_name = params["lvol_name"].as_str().unwrap_or("");  
                let size_mib = params["size_in_mib"].as_u64().unwrap_or(0);
                let size_bytes = size_mib * 1024 * 1024;
                
                let uuid = spdk.create_lvol(lvs_name, lvol_name, size_bytes, 1048576).await?;
                json!(uuid)
            }
            "bdev_lvol_delete" => {
                let params = rpc_request["params"].as_object()
                    .ok_or("Missing params for lvol deletion")?;
                let name = params["name"].as_str().unwrap_or("");
                
                spdk.delete_lvol("", name).await?;
                json!("success")
            }
            _ => {
                // For other methods, use generic RPC call (matches raid_over_lv)
                let params = rpc_request.get("params").cloned();
                spdk.call_method(method, params).await
                    .map_err(|e| {
                        let error_msg = format!("SPDK RPC call '{}' failed: {}", method, e);
                        println!("❌ [SPDK_RPC] {}", error_msg);
                        Box::new(std::io::Error::new(std::io::ErrorKind::Other, error_msg)) as Box<dyn std::error::Error + Send + Sync>
                    })?
            }
        };
        
        println!("✅ [SPDK_RPC] Method '{}' completed successfully", method);
        
        // Return the direct SPDK response (already in JSON-RPC 2.0 format)
        Ok(json!({"result": result}))
    }

    /// Get all bdevs from SPDK
    async fn get_spdk_bdevs(&self) -> Result<Value, MinimalStateError> {
        self.call_spdk_rpc(&json!({
            "method": "bdev_get_bdevs"
        })).await.map_err(|e| MinimalStateError::SpdkRpcError { 
            message: format!("Failed to get bdevs: {}", e) 
        })
    }

    /// Get all LVS from SPDK
    async fn get_spdk_lvstores(&self) -> Result<Value, MinimalStateError> {
        self.call_spdk_rpc(&json!({
            "method": "bdev_lvol_get_lvstores"
        })).await.map_err(|e| MinimalStateError::SpdkRpcError { 
            message: format!("Failed to get lvstores: {}", e) 
        })
    }

    /// Get NVMe controller information from SPDK
    async fn get_spdk_nvme_controllers(&self) -> Result<Value, MinimalStateError> {
        self.call_spdk_rpc(&json!({
            "method": "bdev_nvme_get_controllers"
        })).await.map_err(|e| MinimalStateError::SpdkRpcError { 
            message: format!("Failed to get controllers: {}", e) 
        })
    }

    /// Convert SPDK bdev JSON to our DiskInfo structure
    async fn bdev_to_disk_info(&self, bdev: &Value, lvstores: &Value, _controllers: &Value) -> Result<Option<DiskInfo>, MinimalStateError> {
        println!("🔧 [BDEV_TO_DISK] Raw bdev JSON: {}", serde_json::to_string(bdev).unwrap_or_else(|_| "JSON error".to_string()));
        
        let bdev_name = bdev["name"].as_str().unwrap_or("");
        let product_name = bdev["product_name"].as_str().unwrap_or("");
        let block_size = bdev["block_size"].as_u64().unwrap_or(0);
        let num_blocks = bdev["num_blocks"].as_u64().unwrap_or(0);
        let claimed = bdev["claimed"].as_bool().unwrap_or(false);
        
        println!("🔧 [BDEV_TO_DISK] Extracted values: name='{}', product='{}', block_size={}, num_blocks={}, claimed={}", 
                 bdev_name, product_name, block_size, num_blocks, claimed);

        // Filter for storage devices (matches raid_over_lv pattern)
        if !product_name.contains("NVMe") && !product_name.contains("SSD") && !product_name.contains("AIO") {
            println!("🔍 [DISK_FILTER] Skipping bdev '{}' with product: '{}' (not storage)", bdev_name, product_name);
            return Ok(None);
        }
        
        println!("✅ [DISK_FILTER] Including storage bdev: '{}' (product: '{}')", bdev_name, product_name);

        let size_bytes = block_size * num_blocks;
        
        // Try to get device name from AIO filename if available, otherwise use bdev name
        let device_name = if let Some(filename) = bdev.get("driver_specific")
            .and_then(|ds| ds.get("aio"))
            .and_then(|aio| aio.get("filename"))
            .and_then(|f| f.as_str()) {
            // Extract device name from filename like "/dev/nvme0n1" -> "nvme0n1"
            filename.trim_start_matches("/dev/").to_string()
        } else {
            bdev_name.trim_start_matches("kernel_").to_string()
        };
        
        let pci_address = self.extract_pci_from_bdev_name(bdev_name);
        
        // Find LVS information for this bdev
        let (lvs_name, free_space, lvol_count) = self.find_lvs_for_bdev(bdev_name, lvstores);
        
        Ok(Some(DiskInfo {
            node_name: self.node_name.clone(),
            pci_address,
            device_name,
            bdev_name: bdev_name.to_string(),
            size_bytes,
            healthy: !claimed, // Simple heuristic: unclaimed = healthy
            blobstore_initialized: lvs_name.is_some(),
            free_space,
            lvs_name,
            lvol_count,
            firmware: Some("unknown".to_string()),
            model: product_name.to_string(),
            serial: Some("unknown".to_string()),
        }))
    }

    /// Check if this is a storage disk (not a system disk)
    async fn is_storage_disk(&self, _disk: &DiskInfo) -> Result<bool, MinimalStateError> {
        // TODO: Implement proper storage disk filtering
        // For now, accept all disks
        Ok(true)
    }

    /// Extract real PCI address from bdev name using system information
    fn extract_pci_from_bdev_name(&self, bdev_name: &str) -> String {
        // For AIO bdevs like "kernel_nvme0n1", extract device name and map to PCI
        let device_name = bdev_name.trim_start_matches("kernel_");
        
        if device_name.starts_with("nvme") && device_name.ends_with("n1") {
            // Try to read the actual PCI address from sysfs
            let symlink_path = format!("/sys/block/{}", device_name);
            if let Ok(link) = std::fs::read_link(&symlink_path) {
                let link_str = link.to_string_lossy();
                // Extract PCI address from path like "../devices/pci0000:00/0000:00:04.0/nvme/nvme0/nvme0n1"
                // Look for the device PCI address (second occurrence), not the domain (first occurrence)
                if let Some(domain_end) = link_str.find("/0000:") {
                    // Start after the domain part: "pci0000:00/0000:00:04.0" -> "0000:00:04.0"
                    let device_start = domain_end + 1; // Skip the "/"
                    let device_part = &link_str[device_start..];
                    
                    if let Some(device_end) = device_part.find("/") {
                        let pci_addr = &device_part[..device_end];
                        println!("✅ [PCI_EXTRACT] Mapped {} -> {}", device_name, pci_addr);
                        return pci_addr.to_string();
                    }
                }
            }
            
            println!("❌ [PCI_EXTRACT] Failed to map {} to PCI address", device_name);
        }
        
        // Fallback to placeholder for non-nvme or failed lookups
        "0000:00:00:0".to_string()
    }

    /// Find LVS information for a bdev - Enhanced with recovery logic
    fn find_lvs_for_bdev(&self, bdev_name: &str, lvstores: &Value) -> (Option<String>, u64, u32) {
        println!("🔍 [LVS_SEARCH] Looking for LVS on bdev: {}", bdev_name);
        println!("🔧 [LVS_SEARCH_DEBUG] Full lvstores response: {}", serde_json::to_string(lvstores).unwrap_or_else(|_| "JSON error".to_string()));
        
        if let Some(lvs_list) = lvstores["result"].as_array() {
            println!("✅ [LVS_SEARCH] Found {} LVS stores to check", lvs_list.len());
            
            for (i, lvs) in lvs_list.iter().enumerate() {
                println!("🔧 [LVS_SEARCH_DEBUG] LVS[{}] raw: {}", i, serde_json::to_string(lvs).unwrap_or_else(|_| "JSON error".to_string()));
                
                if let Some(base_bdev) = lvs["base_bdev"].as_str() {
                    println!("🔍 [LVS_SEARCH] LVS[{}]: name='{}', base_bdev='{}' (looking for: '{}')", 
                             i, 
                             lvs["name"].as_str().unwrap_or("unknown"), 
                             base_bdev,
                             bdev_name);
                    
                    if base_bdev == bdev_name {
                        let lvs_name = lvs["name"].as_str().unwrap_or("").to_string();
                        let free_clusters = lvs["free_clusters"].as_u64().unwrap_or(0);
                        let cluster_size = lvs["cluster_size"].as_u64().unwrap_or(0);
                        let free_space = free_clusters * cluster_size;
                        let lvol_count = 0; // TODO: Count lvols
                        
                        println!("✅ [LVS_RECOVERY] Found existing LVS '{}' on bdev '{}' (free: {}MB)", 
                                 lvs_name, bdev_name, free_space / 1024 / 1024);
                        return (Some(lvs_name), free_space, lvol_count);
                    }
                } else {
                    println!("⚠️ [LVS_SEARCH] LVS[{}] has no base_bdev field!", i);
                }
            }
            println!("❌ [LVS_SEARCH] No LVS found for bdev: {}", bdev_name);
        } else {
            println!("❌ [LVS_SEARCH] No LVS stores found in SPDK response");
            println!("🔧 [DEBUG] lvstores structure: {}", serde_json::to_string(lvstores).unwrap_or_else(|_| "JSON error".to_string()));
        }
        
        (None, 0, 0)
    }
}

/// Physical device information from system discovery
#[derive(Debug, Clone)]
struct PhysicalDevice {
    pub pci_address: String,
    pub device_name: String,
    pub driver: String,
    pub size_bytes: u64,
    pub model: String,
}
