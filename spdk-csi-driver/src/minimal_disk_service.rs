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

    /// Discover all disks on this node with auto-recovery (for startup/periodic discovery)
    pub async fn discover_local_disks(&self) -> Result<Vec<DiskInfo>, MinimalStateError> {
        self.discover_local_disks_internal(true).await
    }

    /// Fast disk discovery without auto-recovery (for API requests)
    pub async fn discover_local_disks_fast(&self) -> Result<Vec<DiskInfo>, MinimalStateError> {
        self.discover_local_disks_internal(false).await
    }

    /// Internal disk discovery with optional auto-recovery
    async fn discover_local_disks_internal(&self, with_auto_recovery: bool) -> Result<Vec<DiskInfo>, MinimalStateError> {
        println!("🔍 [MINIMAL_DISK] Starting disk discovery (auto-recovery: {}) on node: {}", with_auto_recovery, self.node_name);

        // Step 1: Auto-recover SPDK state for physical devices (only on startup/periodic)
        if with_auto_recovery {
            if let Err(e) = self.auto_recover_spdk_state().await {
                println!("⚠️ [MINIMAL_DISK] Auto-recovery failed (continuing anyway): {}", e);
            }
        }

        // Step 2: Get current SPDK state
        let bdevs = self.get_spdk_bdevs().await?;
        let lvstores = self.get_spdk_lvstores().await?;
        let controllers = self.get_spdk_nvme_controllers().await?;

        let mut disks = Vec::new();
        // Note: Full bdevs JSON not logged (too verbose). Count logged below.
        
        if let Some(bdev_list) = bdevs["result"].as_array() {
            println!("🔧 [DEBUG] Found {} bdevs in result array", bdev_list.len());
            for (i, bdev) in bdev_list.iter().enumerate() {
                // Note: Individual bdev JSON not logged (too verbose). Only extracted values logged below.
                
                if let Some(disk_info) = self.bdev_to_disk_info(bdev, &lvstores, &controllers).await? {
                    // Note: Per-disk details not logged (verbose). Summary logged at end.
                    
                    // Filter out system disks and non-storage devices
                    if self.is_storage_disk(&disk_info).await? {
                        disks.push(disk_info);
                    }
                    // Note: Filtered disks not logged (normal filtering)
                } else {
                    // Note: Skipped bdev (lvol, not physical storage) - not logged to reduce noise
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

        // Find the disk first - use fast discovery to avoid timeout
        let disk_found = self.discover_local_disks_fast().await?
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
    pub async fn create_lvol(&self, lvs_name: &str, volume_id: &str, size_bytes: u64, thin_provision: bool) -> Result<String, MinimalStateError> {
        println!("🔧 [MINIMAL_DISK] Creating lvol: {} in LVS: {} (size: {} bytes, thin: {})", 
                 volume_id, lvs_name, size_bytes, thin_provision);
        
        let lvol_name = format!("vol_{}", volume_id);
        let size_mib = (size_bytes + 1048575) / 1048576; // Round up to MiB

        println!("🔍 [LVOL_CREATE_DEBUG] Lvol name will be: {}", lvol_name);
        println!("🔍 [LVOL_CREATE_DEBUG] Size in MiB: {}", size_mib);
        
        // Check if lvol already exists before attempting to create
        println!("🔍 [LVOL_CREATE_DEBUG] Checking if lvol already exists...");
        let check_bdevs = self.call_spdk_rpc(&json!({"method": "bdev_get_bdevs"})).await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to check existing bdevs: {}", e)
            })?;
        
        if let Some(bdev_list) = check_bdevs["result"].as_array() {
            for bdev in bdev_list {
                if let Some(aliases) = bdev["aliases"].as_array() {
                    for alias in aliases {
                        if let Some(alias_str) = alias.as_str() {
                            // Check if alias contains our lvol name
                            if alias_str.ends_with(&lvol_name) {
                                println!("⚠️ [LVOL_CREATE_DEBUG] Found existing bdev with matching name:");
                                println!("   Alias: {}", alias_str);
                                println!("   UUID: {}", bdev["name"].as_str().unwrap_or("unknown"));
                                println!("   Product: {}", bdev.get("product_name").and_then(|p| p.as_str()).unwrap_or("unknown"));
                            }
                        }
                    }
                }
                // Also check the bdev name itself
                if let Some(name) = bdev["name"].as_str() {
                    if name.contains(&lvol_name) {
                        println!("⚠️ [LVOL_CREATE_DEBUG] Found bdev with name containing '{}': {}", lvol_name, name);
                    }
                }
            }
        }

        let create_params = json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size_in_mib": size_mib,
                "thin_provision": thin_provision
            }
        });

        // Note: Create params not logged (verbose). Key values logged above and result logged below.

        let response = self.call_spdk_rpc(&create_params).await
            .map_err(|e| {
                println!("❌ [LVOL_CREATE_DEBUG] SPDK returned error: {}", e);
                MinimalStateError::SpdkRpcError { 
                    message: format!("Failed to create lvol: {}", e) 
                }
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

    /// Resize logical volume (expand only)
    pub async fn resize_lvol(&self, lvol_uuid: &str, new_size_bytes: u64) -> Result<(), MinimalStateError> {
        println!("📏 [MINIMAL_DISK] Resizing lvol {} to {} bytes", lvol_uuid, new_size_bytes);
        
        let size_mib = (new_size_bytes + 1048575) / 1048576; // Round up to MiB

        let resize_params = json!({
            "method": "bdev_lvol_resize",
            "params": {
                "name": lvol_uuid,
                "size_in_mib": size_mib
            }
        });

        self.call_spdk_rpc(&resize_params).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to resize lvol: {}", e) 
            })?;

        println!("✅ [MINIMAL_DISK] Successfully resized lvol {} to {} MiB", lvol_uuid, size_mib);
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
                    // This is IDEMPOTENT - if auto-discovery fails, we explicitly load the LVS
                    // Timeout is 10 seconds to handle slow disks or examination delays
                    if let Some(lvs_name) = self.wait_for_lvs_discovery(&bdev_name, 10).await {
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
    ///
    /// Strategy for NVMe devices:
    /// 1. First try SPDK userspace driver (unbind kernel, bind vfio-pci, attach via bdev_nvme_attach_controller)
    /// 2. If userspace fails, fall back to io_uring (keeps kernel driver, less performance but works everywhere)
    ///
    /// Strategy for SATA devices:
    /// - Always use io_uring (SPDK userspace only supports NVMe)
    async fn ensure_device_bdev_exists(&self, device: &PhysicalDevice) -> Result<String, MinimalStateError> {
        println!("🔄 [BDEV_RECOVERY] Ensuring bdev exists for device: {} (driver: {})",
                 device.device_name, device.driver);

        let correlation_id = format!("{:08x}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u32);

        // Check if this is an NVMe device (by device name pattern)
        let is_nvme_device = device.device_name.starts_with("nvme");

        // For NVMe devices: try SPDK userspace driver first for maximum performance
        if is_nvme_device {
            println!("🚀 [BDEV_RECOVERY:{}] NVMe device detected, attempting SPDK userspace driver first", correlation_id);

            // Check if device is already bound to userspace driver (vfio-pci or uio)
            if device.driver == "vfio-pci" || device.driver == "uio_pci_generic" || device.driver == "igb_uio" {
                // Device already bound to userspace driver, try to attach via SPDK
                match self.try_spdk_nvme_attach(device, &correlation_id).await {
                    Ok(bdev_name) => return Ok(bdev_name),
                    Err(e) => {
                        println!("⚠️ [BDEV_RECOVERY:{}] SPDK NVMe attach failed: {}", correlation_id, e);
                        // Can't fall back to io_uring since device is unbound from kernel
                        return Err(e);
                    }
                }
            }

            // Device is kernel-bound (nvme driver), try to switch to userspace
            if device.driver == "nvme" {
                // First check if an SPDK NVMe bdev already exists for this PCI address
                let spdk_bdev_name = format!("nvme_{}", device.pci_address.replace(":", "_").replace(".", "_"));
                let bdevs = self.get_spdk_bdevs().await?;
                if let Some(bdev_list) = bdevs["result"].as_array() {
                    for bdev in bdev_list {
                        if let Some(name) = bdev["name"].as_str() {
                            if name.starts_with(&spdk_bdev_name) || name.contains(&device.pci_address.replace(":", "_")) {
                                println!("✅ [BDEV_RECOVERY:{}] SPDK NVMe bdev already exists: {}", correlation_id, name);
                                return Ok(name.to_string());
                            }
                        }
                    }
                }

                // Try to unbind from kernel and bind to userspace driver
                match self.try_unbind_and_attach_nvme(device, &correlation_id).await {
                    Ok(bdev_name) => {
                        println!("✅ [BDEV_RECOVERY:{}] Successfully using SPDK userspace NVMe driver: {}", correlation_id, bdev_name);
                        return Ok(bdev_name);
                    }
                    Err(e) => {
                        println!("⚠️ [BDEV_RECOVERY:{}] SPDK userspace driver failed: {}, falling back to io_uring", correlation_id, e);
                        // Fall through to io_uring fallback
                    }
                }
            }
        }

        // Fallback path: io_uring for kernel-bound devices (NVMe fallback or SATA)
        // This works with any block device that has a kernel driver
        if device.driver == "nvme" || device.driver == "ahci" || device.driver == "ata_piix" || device.driver.contains("sata") {
            let uring_bdev_name = format!("uring_{}", device.device_name);

            // Check if uring bdev already exists
            let bdevs = self.get_spdk_bdevs().await?;
            if let Some(bdev_list) = bdevs["result"].as_array() {
                for bdev in bdev_list {
                    if let Some(name) = bdev["name"].as_str() {
                        if name == uring_bdev_name {
                            println!("✅ [BDEV_RECOVERY:{}] uring bdev already exists: {}", correlation_id, uring_bdev_name);
                            return Ok(uring_bdev_name);
                        }
                    }
                }
            }

            println!("🔧 [BDEV_RECOVERY:{}] Creating io_uring bdev: {} (fallback path)", correlation_id, uring_bdev_name);
            println!("🔧 [BDEV_RECOVERY:{}] Device: /dev/{}, PCI: {}", correlation_id, device.device_name, device.pci_address);

            let device_path = format!("/dev/{}", device.device_name);

            let uring_params = serde_json::json!({
                "method": "bdev_uring_create",
                "params": {
                    "name": uring_bdev_name,
                    "filename": device_path
                    // Note: Not specifying block_size - let SPDK auto-detect from device
                }
            });

            match self.call_spdk_rpc(&uring_params).await {
                Ok(_) => {
                    println!("✅ [BDEV_RECOVERY:{}] Successfully created uring bdev: {}", correlation_id, uring_bdev_name);
                    return Ok(uring_bdev_name);
                }
                Err(e) if e.to_string().contains("File exists") => {
                    println!("✅ [BDEV_RECOVERY:{}] uring bdev already exists: {}", correlation_id, uring_bdev_name);
                    return Ok(uring_bdev_name);
                }
                Err(e) => {
                    println!("❌ [BDEV_RECOVERY:{}] Failed to create uring bdev: {}", correlation_id, e);
                    return Err(MinimalStateError::SpdkRpcError {
                        message: format!("Failed to create uring bdev: {}", e)
                    });
                }
            }
        }

        // Device has unknown/unbound driver and is not suitable for io_uring
        println!("⏭️ [BDEV_RECOVERY:{}] Skipping device with unsupported driver: {} (driver: {})",
                 correlation_id, device.device_name, device.driver);
        Err(MinimalStateError::InternalError {
            message: format!("Device {} has unsupported driver: {}", device.device_name, device.driver)
        })
    }

    /// Try to unbind NVMe device from kernel driver and attach via SPDK userspace driver
    async fn try_unbind_and_attach_nvme(&self, device: &PhysicalDevice, correlation_id: &str) -> Result<String, MinimalStateError> {
        use std::fs;

        println!("🔧 [SPDK_USERSPACE:{}] Attempting to bind {} to userspace driver", correlation_id, device.pci_address);

        // Step 1: Check if vfio-pci or uio_pci_generic is available
        let userspace_driver = self.detect_available_userspace_driver().await?;
        println!("🔧 [SPDK_USERSPACE:{}] Using userspace driver: {}", correlation_id, userspace_driver);

        // Step 2: Get device vendor/device IDs for driver binding
        let (vendor_id, device_id) = self.get_pci_ids(&device.pci_address)?;
        println!("🔧 [SPDK_USERSPACE:{}] Device IDs: vendor={}, device={}", correlation_id, vendor_id, device_id);

        // Step 3: Unbind from kernel nvme driver
        let unbind_path = format!("/sys/bus/pci/devices/{}/driver/unbind", device.pci_address);
        if std::path::Path::new(&unbind_path).exists() {
            println!("🔧 [SPDK_USERSPACE:{}] Unbinding from kernel driver...", correlation_id);
            fs::write(&unbind_path, &device.pci_address)
                .map_err(|e| MinimalStateError::InternalError {
                    message: format!("Failed to unbind device {}: {}", device.pci_address, e)
                })?;

            // Give kernel time to process unbind
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        // Step 4: Add device ID to userspace driver's new_id (required for vfio-pci)
        let new_id_path = format!("/sys/bus/pci/drivers/{}/new_id", userspace_driver);
        if std::path::Path::new(&new_id_path).exists() {
            let id_string = format!("{} {}", vendor_id, device_id);
            // Ignore error if ID already exists
            let _ = fs::write(&new_id_path, &id_string);
        }

        // Step 5: Bind to userspace driver
        let bind_path = format!("/sys/bus/pci/drivers/{}/bind", userspace_driver);
        println!("🔧 [SPDK_USERSPACE:{}] Binding to {}...", correlation_id, userspace_driver);
        fs::write(&bind_path, &device.pci_address)
            .map_err(|e| MinimalStateError::InternalError {
                message: format!("Failed to bind device {} to {}: {}", device.pci_address, userspace_driver, e)
            })?;

        // Give driver time to initialize
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Step 6: Verify binding succeeded
        let current_driver = self.get_current_driver(&device.pci_address).await
            .unwrap_or_else(|_| "unknown".to_string());
        if current_driver != userspace_driver {
            return Err(MinimalStateError::InternalError {
                message: format!("Driver binding failed: expected {}, got {}", userspace_driver, current_driver)
            });
        }

        println!("✅ [SPDK_USERSPACE:{}] Device bound to {}", correlation_id, userspace_driver);

        // Step 7: Attach via SPDK bdev_nvme_attach_controller
        self.try_spdk_nvme_attach(device, correlation_id).await
    }

    /// Try to attach an NVMe device via SPDK's bdev_nvme_attach_controller
    async fn try_spdk_nvme_attach(&self, device: &PhysicalDevice, correlation_id: &str) -> Result<String, MinimalStateError> {
        // Generate controller name based on PCI address
        let controller_name = format!("nvme_{}", device.pci_address.replace(":", "_").replace(".", "_"));

        println!("🔧 [SPDK_USERSPACE:{}] Attaching NVMe controller: {} (PCI: {})",
                 correlation_id, controller_name, device.pci_address);

        // SPDK expects PCI address in traddr format
        let attach_params = serde_json::json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": controller_name,
                "trtype": "pcie",
                "traddr": device.pci_address
            }
        });

        match self.call_spdk_rpc(&attach_params).await {
            Ok(response) => {
                // bdev_nvme_attach_controller returns array of created bdev names
                if let Some(bdevs) = response["result"].as_array() {
                    if let Some(first_bdev) = bdevs.first() {
                        let bdev_name = first_bdev.as_str().unwrap_or(&controller_name).to_string();
                        println!("✅ [SPDK_USERSPACE:{}] NVMe controller attached, bdev: {}", correlation_id, bdev_name);
                        return Ok(bdev_name);
                    }
                }
                // If result is a string (single bdev name)
                if let Some(bdev_name) = response["result"].as_str() {
                    println!("✅ [SPDK_USERSPACE:{}] NVMe controller attached, bdev: {}", correlation_id, bdev_name);
                    return Ok(bdev_name.to_string());
                }
                // Fallback to controller name + n1 (common SPDK naming)
                let bdev_name = format!("{}n1", controller_name);
                println!("✅ [SPDK_USERSPACE:{}] NVMe controller attached (assumed bdev: {})", correlation_id, bdev_name);
                Ok(bdev_name)
            }
            Err(e) if e.to_string().contains("already exists") || e.to_string().contains("already attached") => {
                let bdev_name = format!("{}n1", controller_name);
                println!("✅ [SPDK_USERSPACE:{}] NVMe controller already attached: {}", correlation_id, bdev_name);
                Ok(bdev_name)
            }
            Err(e) => {
                println!("❌ [SPDK_USERSPACE:{}] Failed to attach NVMe controller: {}", correlation_id, e);
                Err(MinimalStateError::SpdkRpcError {
                    message: format!("Failed to attach NVMe controller: {}", e)
                })
            }
        }
    }

    /// Detect which userspace driver is available (prefer vfio-pci if IOMMU available)
    async fn detect_available_userspace_driver(&self) -> Result<String, MinimalStateError> {
        use std::path::Path;

        // Check if IOMMU is available (vfio-pci requires it)
        let iommu_groups = std::fs::read_dir("/sys/kernel/iommu_groups")
            .map(|d| d.count())
            .unwrap_or(0);

        if iommu_groups > 0 {
            // IOMMU available, check if vfio-pci driver is loaded
            if Path::new("/sys/bus/pci/drivers/vfio-pci").exists() {
                return Ok("vfio-pci".to_string());
            }
        }

        // Fall back to uio_pci_generic (doesn't require IOMMU, but less secure)
        if Path::new("/sys/bus/pci/drivers/uio_pci_generic").exists() {
            return Ok("uio_pci_generic".to_string());
        }

        // Try igb_uio (legacy DPDK driver)
        if Path::new("/sys/bus/pci/drivers/igb_uio").exists() {
            return Ok("igb_uio".to_string());
        }

        Err(MinimalStateError::InternalError {
            message: "No userspace driver available (vfio-pci, uio_pci_generic, or igb_uio required)".to_string()
        })
    }

    /// Get PCI vendor and device IDs
    fn get_pci_ids(&self, pci_address: &str) -> Result<(String, String), MinimalStateError> {
        use std::fs;

        let vendor_path = format!("/sys/bus/pci/devices/{}/vendor", pci_address);
        let device_path = format!("/sys/bus/pci/devices/{}/device", pci_address);

        let vendor_id = fs::read_to_string(&vendor_path)
            .map_err(|e| MinimalStateError::InternalError {
                message: format!("Failed to read vendor ID: {}", e)
            })?
            .trim()
            .trim_start_matches("0x")
            .to_string();

        let device_id = fs::read_to_string(&device_path)
            .map_err(|e| MinimalStateError::InternalError {
                message: format!("Failed to read device ID: {}", e)
            })?
            .trim()
            .trim_start_matches("0x")
            .to_string();

        Ok((vendor_id, device_id))
    }

    /// Wait for SPDK to auto-discover LVS on a bdev (async examination)
    /// In modern SPDK, when a bdev is created, the lvol module asynchronously examines it for existing LVS
    /// This is IDEMPOTENT and will try multiple strategies to ensure LVS is discovered if it exists
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
            
            // SPDK v25.09.x has no explicit "load_lvstore" method
            // Auto-discovery happens via examine_disk callback when bdev is created
            // Just keep polling - if examination hasn't completed yet, it will soon
            
            // Wait 500ms before next check (reduced from 100ms to minimize SPDK log spam)
            // This reduces SPDK RPC calls from 100/disk to 20/disk during 10s timeout
            sleep(Duration::from_millis(500)).await;
        }
        
        println!("❌ [LVS_DISCOVERY:{}] TIMEOUT! No LVS discovered on bdev '{}' after {}s ({} iterations)", 
                 correlation_id, bdev_name, timeout_secs, iteration);
        println!("❌ [LVS_DISCOVERY:{}] CORRELATE: Check SPDK logs around this time for vbdev_lvol examination of '{}'", 
                 correlation_id, bdev_name);
        println!("❌ [LVS_DISCOVERY:{}] This may indicate: 1) No LVS on disk, 2) SPDK examination failed, or 3) Timing issue", 
                 correlation_id);
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

    /// Get list of mounted partitions for a device
    async fn get_mounted_partitions(&self, device_name: &str) -> Vec<String> {
        use std::process::Command;
        
        // Read host's /proc/mounts to find mounted partitions
        // Try /host/proc/mounts first (when running in container with host mount)
        // Fall back to /proc/mounts (for local testing)
        let mounts_path = if std::path::Path::new("/host/proc/mounts").exists() {
            "/host/proc/mounts"
        } else {
            "/proc/mounts"
        };
        
        match Command::new("cat").arg(mounts_path).output() {
            Ok(output) => {
                let mounts = String::from_utf8_lossy(&output.stdout);
                let mut partitions = Vec::new();
                
                for line in mounts.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let device = parts[0];
                        let mount_point = parts[1];
                        
                        // Check if this mount belongs to our device (e.g., /dev/nvme0n1p1)
                        if device.contains(device_name) {
                            partitions.push(mount_point.to_string());
                        }
                    }
                }
                
                partitions
            }
            Err(e) => {
                println!("⚠️ [DISK_DETECT] Failed to read mounts: {}", e);
                Vec::new()
            }
        }
    }

    /// Check if a device is a system disk (has critical system mounts)
    fn is_system_disk(&self, _device_name: &str, mounted_partitions: &[String]) -> bool {
        // A disk is a system disk if it has any of these critical mount points
        let system_mounts = ["/", "/boot", "/usr", "/var", "/etc", "/home"];
        
        for mount in mounted_partitions {
            if system_mounts.contains(&mount.as_str()) {
                return true;
            }
        }
        
        false
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
            
            // Extract params from the request
            let params = rpc_request.get("params").cloned();
            
            if let Some(ref p) = params {
                // Log params summary (not full JSON to reduce verbosity)
                let param_summary = if let Some(name) = p.get("name") {
                    format!("name={}", name)
                } else if let Some(obj) = p.as_object() {
                    format!("{} keys", obj.len())
                } else {
                    "complex".to_string()
                };
                eprintln!("🔍 [SPDK_PARAMS] Method: {}, params: {}", method, param_summary);
            } else {
                eprintln!("🔍 [SPDK_PARAMS] Method: {}, params: None", method);
            }
            
            let result = match method {
            "bdev_get_bdevs" => {
                // Forward params from request (e.g., {"name": "uuid"} to filter results)
                eprintln!("🔧 [SPDK_FIX] Calling bdev_get_bdevs with params: {:?}", params);
                
                let result = spdk.call_method("bdev_get_bdevs", params.clone()).await
                    .map_err(|e| {
                        let error_msg = format!("SPDK RPC call 'bdev_get_bdevs' failed: {}", e);
                        println!("❌ [SPDK_RPC] {}", error_msg);
                        Box::new(std::io::Error::new(std::io::ErrorKind::Other, error_msg)) as Box<dyn std::error::Error + Send + Sync>
                    })?;
                
                // Log result to verify filtering worked
                if let Some(result_array) = result.as_array() {
                    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                    eprintln!("✅ [SPDK_FIX] bdev_get_bdevs returned {} bdev(s)", result_array.len());
                    if let Some(requested_name) = params.as_ref().and_then(|p| p.get("name")) {
                        eprintln!("   Requested: name={}", requested_name);
                        eprintln!("   Expected: 1 bdev");
                        eprintln!("   Actual: {} bdev(s)", result_array.len());
                        if result_array.len() == 1 {
                            eprintln!("   ✅ FILTERING WORKED!");
                        } else if result_array.len() > 1 {
                            eprintln!("   ⚠️ FILTERING DID NOT WORK - got all bdevs");
                        }
                    }
                    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                }
                
                result
            }
            "bdev_lvol_get_lvstores" => {
                eprintln!("🔧 [SPDK_METHOD] bdev_lvol_get_lvstores (no params expected)");
                let lvstores = spdk.get_lvol_stores().await?;
                eprintln!("✅ [SPDK_METHOD] bdev_lvol_get_lvstores returned {} LVS", lvstores.len());
                // Convert LvsInfo to serializable format  
                let serializable_lvstores: Vec<serde_json::Value> = lvstores.into_iter().map(|lvs| {
                    json!({
                        "name": lvs.name,
                        "uuid": lvs.uuid,
                        "base_bdev": lvs.base_bdev,
                        "cluster_size": lvs.cluster_size,
                        "total_clusters": lvs.total_clusters,
                        "free_clusters": lvs.free_clusters,
                        "block_size": lvs.block_size
                    })
                }).collect();
                json!(serializable_lvstores)
            }
            "bdev_nvme_get_controllers" => {
                eprintln!("🔧 [SPDK_METHOD] bdev_nvme_get_controllers (no params expected)");
                let controllers = spdk.get_nvme_controllers().await?;
                eprintln!("✅ [SPDK_METHOD] bdev_nvme_get_controllers returned {} controllers", controllers.len());
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
                eprintln!("🔧 [SPDK_METHOD] bdev_lvol_create (manual param extraction)");
                let params = rpc_request["params"].as_object()
                    .ok_or("Missing params for lvol creation")?;
                let lvs_name = params["lvs_name"].as_str().unwrap_or("");
                let lvol_name = params["lvol_name"].as_str().unwrap_or("");  
                let size_mib = params["size_in_mib"].as_u64().unwrap_or(0);
                let size_bytes = size_mib * 1024 * 1024;
                let thin_provision = params["thin_provision"].as_bool().unwrap_or(false);
                
                eprintln!("   lvol_name: {}, size: {} MiB, thin: {}", lvol_name, size_mib, thin_provision);
                
                let uuid = spdk.create_lvol(lvs_name, lvol_name, size_bytes, 1048576, thin_provision).await?;
                eprintln!("✅ [SPDK_METHOD] bdev_lvol_create returned UUID: {}", uuid);
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
        // Note: Raw bdev JSON not logged (too verbose). Only log extracted values.
        
        let bdev_name = bdev["name"].as_str().unwrap_or("");
        let product_name = bdev["product_name"].as_str().unwrap_or("");
        let block_size = bdev["block_size"].as_u64().unwrap_or(0);
        let num_blocks = bdev["num_blocks"].as_u64().unwrap_or(0);
        let claimed = bdev["claimed"].as_bool().unwrap_or(false);
        
        // Note: Extracted values not logged (too verbose during discovery with 20+ bdevs)

        // Filter for storage devices (matches raid_over_lv pattern)
        // Use case-insensitive check for "uring" to match both "Uring" and "URING bdev"
        let product_upper = product_name.to_uppercase();
        if !product_upper.contains("NVME") && !product_upper.contains("SSD") && !product_upper.contains("URING") {
            // Note: Skipping non-storage bdevs (lvols) - not logged to reduce noise
            return Ok(None);
        }
        
        // Note: Storage bdev inclusion not logged per-bdev (too verbose). Summary logged at end.

        let size_bytes = block_size * num_blocks;
        
        // Try to get device name from uring filename if available, otherwise use bdev name
        let device_name = if let Some(filename) = bdev.get("driver_specific")
            .and_then(|ds| ds.get("uring"))
            .and_then(|uring| uring.get("filename"))
            .and_then(|f| f.as_str()) {
            // Extract device name from filename like "/dev/nvme0n1" -> "nvme0n1"
            filename.trim_start_matches("/dev/").to_string()
        } else {
            bdev_name.trim_start_matches("uring_").to_string()
        };
        
        let pci_address = self.extract_pci_from_bdev_name(bdev_name);
        
        // Find LVS information for this bdev
        let (lvs_name, free_space, lvol_count) = self.find_lvs_for_bdev(bdev_name, lvstores);
        
        // Check for mounted partitions on this device
        let mounted_partitions = self.get_mounted_partitions(&device_name).await;
        let is_system_disk = self.is_system_disk(&device_name, &mounted_partitions);
        
        Ok(Some(DiskInfo {
            node_name: self.node_name.clone(),
            pci_address,
            device_name,
            bdev_name: bdev_name.to_string(),
            size_bytes,
            // A disk is healthy if it's usable for storage
            // claimed=true means it has an LVS, which is GOOD (it's initialized and ready)
            // claimed=false means it's unclaimed (also healthy, just not initialized)
            // So all disks are healthy unless we detect specific problems
            healthy: true,
            blobstore_initialized: lvs_name.is_some(),
            free_space,
            lvs_name,
            lvol_count,
            firmware: Some("unknown".to_string()),
            model: product_name.to_string(),
            serial: Some("unknown".to_string()),
            is_system_disk,
            mounted_partitions,
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
        // For uring bdevs like "uring_nvme0n1", extract device name and map to PCI
        let device_name = bdev_name.trim_start_matches("uring_");
        
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
        // Note: Full lvstores response not logged (verbose). Count and matches logged below.
        
        if let Some(lvs_list) = lvstores["result"].as_array() {
            println!("✅ [LVS_SEARCH] Found {} LVS stores to check", lvs_list.len());
            
            for (i, lvs) in lvs_list.iter().enumerate() {
                // Note: Raw LVS JSON not logged (verbose). Only checking base_bdev match.
                
                if let Some(base_bdev) = lvs["base_bdev"].as_str() {
                    // Note: Per-LVS comparison not logged (verbose). Only log if match found.
                    
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
                    // Note: LVS without base_bdev field (rare) - not logged to reduce noise
                }
            }
            // Note: No LVS on this bdev (not logged - normal for uninitialized disks)
        } else {
            // Note: No LVS stores in cluster - not logged (normal for fresh deployment)
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
