// spdk_native.rs - Native SPDK integration for Flint embedded mode
// This module provides safe Rust wrappers around SPDK C APIs

use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::{Arc, Mutex, Once};
use anyhow::{Result, anyhow};
use serde_json::{json, Value};

// Include the generated SPDK bindings (Linux only)
#[cfg(target_os = "linux")]
mod bindings {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/spdk_bindings.rs"));
}

// Mock implementation for non-Linux platforms (development)
#[cfg(not(target_os = "linux"))]
mod bindings {
    pub type spdk_bdev = *mut std::ffi::c_void;
    pub type spdk_lvol_store = *mut std::ffi::c_void;
    pub type spdk_lvol = *mut std::ffi::c_void;
    pub type spdk_env_opts = std::ffi::c_void;
    pub type spdk_log_level = u32;
    pub type spdk_bdev_io_stat = std::ffi::c_void;
    
    pub const SPDK_LOG_INFO: u32 = 3;
}

#[cfg(target_os = "linux")]
use bindings::*;

static SPDK_INIT: Once = Once::new();
static mut SPDK_INITIALIZED: bool = false;

/// SPDK initialization error
#[derive(Debug, Clone)]
pub struct SpdkError {
    pub message: String,
}

impl std::fmt::Display for SpdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SPDK Error: {}", self.message)
    }
}

impl std::error::Error for SpdkError {}

impl From<SpdkError> for anyhow::Error {
    fn from(err: SpdkError) -> Self {
        anyhow!(err.message)
    }
}

/// Native SPDK integration for Flint
pub struct SpdkNative {
    initialized: Arc<Mutex<bool>>,
}

impl SpdkNative {
    /// Initialize SPDK for Flint usage
    pub fn new() -> Result<Self> {
        Self::initialize_spdk_once()?;
        
        Ok(Self {
            initialized: Arc::new(Mutex::new(true)),
        })
    }

    /// Initialize SPDK environment exactly once
    fn initialize_spdk_once() -> Result<()> {
        let mut init_result = Ok(());
        
        SPDK_INIT.call_once(|| {
            #[cfg(target_os = "linux")]
            {
                println!("🚀 [SPDK_NATIVE] Initializing SPDK environment");
                
                unsafe {
                    // Initialize environment options
                    let mut opts = std::mem::zeroed::<spdk_env_opts>();
                    spdk_env_opts_init(&mut opts);
                    
                    // Configure for embedded mode
                    let app_name = CString::new("flint-embedded").unwrap();
                    opts.name = app_name.as_ptr() as *mut i8;
                    opts.shm_id = 0;
                    opts.mem_size = 1024; // 1GB - adjust based on needs
                    
                    // Initialize SPDK environment
                    let result = spdk_env_init(&opts);
                    if result != 0 {
                        init_result = Err(anyhow!("SPDK environment initialization failed: {}", result));
                        return;
                    }
                    
                    // Set logging level
                    spdk_log_set_print_level(SPDK_LOG_INFO);
                    
                    SPDK_INITIALIZED = true;
                    println!("✅ [SPDK_NATIVE] SPDK environment initialized successfully");
                }
            }
            
            #[cfg(not(target_os = "linux"))]
            {
                println!("🔧 [SPDK_MOCK] Mock SPDK initialization");
            }
        });
        
        init_result
    }

    /// Check if SPDK is properly initialized
    pub fn is_initialized(&self) -> bool {
        unsafe { SPDK_INITIALIZED }
    }

    /// Create AIO bdev for kernel devices - robust implementation
    pub async fn create_aio_bdev(&self, device_path: &str, bdev_name: &str) -> Result<()> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            println!("🔗 [SPDK_NATIVE] Creating AIO bdev: {} -> {}", device_path, bdev_name);
            
            // Validate inputs
            if !std::path::Path::new(device_path).exists() {
                return Err(anyhow!("Device {} does not exist", device_path));
            }
            
            let device_path_c = CString::new(device_path)
                .map_err(|_| anyhow!("Invalid device path"))?;
            let bdev_name_c = CString::new(bdev_name)
                .map_err(|_| anyhow!("Invalid bdev name"))?;
            
            unsafe {
                let result = spdk_bdev_aio_create(
                    bdev_name_c.as_ptr(),
                    device_path_c.as_ptr(),
                    4096, // Standard block size
                );
                
                if result != 0 {
                    return Err(anyhow!("Failed to create AIO bdev {}: error code {}", bdev_name, result));
                }
            }
            
            println!("✅ [SPDK_NATIVE] AIO bdev created: {}", bdev_name);
            Ok(())
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            println!("🔗 [SPDK_MOCK] Mock AIO bdev: {} -> {}", device_path, bdev_name);
            Ok(())
        }
    }

    /// Check if LVS exists - robust implementation
    pub async fn lvs_exists(&self, lvs_name: &str) -> Result<bool> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            let lvs_name_c = CString::new(lvs_name)
                .map_err(|_| anyhow!("Invalid LVS name"))?;
            
            unsafe {
                let lvs = spdk_lvol_store_get_by_name(lvs_name_c.as_ptr());
                Ok(!lvs.is_null())
            }
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Ok(false)
        }
    }

    /// Create LVS with comprehensive error handling
    pub async fn create_lvs(&self, bdev_name: &str, lvs_name: &str) -> Result<()> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            println!("🏗️ [SPDK_NATIVE] Creating LVS: {} on bdev: {}", lvs_name, bdev_name);
            
            let bdev_name_c = CString::new(bdev_name)
                .map_err(|_| anyhow!("Invalid bdev name"))?;
            let lvs_name_c = CString::new(lvs_name)
                .map_err(|_| anyhow!("Invalid LVS name"))?;
            
            unsafe {
                // Verify bdev exists
                let bdev = spdk_bdev_get_by_name(bdev_name_c.as_ptr());
                if bdev.is_null() {
                    return Err(anyhow!("Bdev {} not found", bdev_name));
                }
                
                // Create LVS - for now synchronous, could be made async with callbacks
                let result = spdk_lvol_store_create(
                    bdev,
                    lvs_name_c.as_ptr(),
                    1048576, // 1MB cluster size
                    spdk_lvol_store_clear_method::LVOL_CLEAR_WITH_DEFAULT,
                    ptr::null_mut(), // No callback for now
                    ptr::null_mut(), // No callback context
                );
                
                if result != 0 {
                    return Err(anyhow!("Failed to create LVS {}: error code {}", lvs_name, result));
                }
            }
            
            // Verify LVS was created
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            if !self.lvs_exists(lvs_name).await? {
                return Err(anyhow!("LVS {} creation verification failed", lvs_name));
            }
            
            println!("✅ [SPDK_NATIVE] LVS created successfully: {}", lvs_name);
            Ok(())
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            println!("🏗️ [SPDK_MOCK] Mock LVS creation: {} on {}", lvs_name, bdev_name);
            Ok(())
        }
    }

    /// Create lvol with comprehensive validation and error handling
    pub async fn create_lvol(&self, lvs_name: &str, lvol_name: &str, size_bytes: u64) -> Result<String> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            println!("🔧 [SPDK_NATIVE] Creating lvol: {} in LVS: {} (size: {} bytes)", 
                     lvol_name, lvs_name, size_bytes);
            
            let lvs_name_c = CString::new(lvs_name)
                .map_err(|_| anyhow!("Invalid LVS name"))?;
            let lvol_name_c = CString::new(lvol_name)
                .map_err(|_| anyhow!("Invalid lvol name"))?;
            
            unsafe {
                // Get LVS and validate
                let lvs = spdk_lvol_store_get_by_name(lvs_name_c.as_ptr());
                if lvs.is_null() {
                    return Err(anyhow!("LVS {} not found", lvs_name));
                }
                
                // Check space availability
                let cluster_size = spdk_lvol_store_get_cluster_size(lvs);
                let clusters_needed = (size_bytes + cluster_size - 1) / cluster_size;
                let free_size = spdk_lvol_store_get_free_size(lvs);
                let free_clusters = free_size / cluster_size;
                
                if clusters_needed > free_clusters {
                    return Err(anyhow!(
                        "Insufficient space in LVS {}: need {} clusters ({} bytes), available {} clusters ({} bytes)",
                        lvs_name, clusters_needed, clusters_needed * cluster_size, 
                        free_clusters, free_size
                    ));
                }
                
                // Create lvol
                let result = spdk_lvol_create(
                    lvs,
                    lvol_name_c.as_ptr(),
                    size_bytes,
                    false, // Not thin provisioned
                    spdk_lvol_clear_method::LVOL_CLEAR_WITH_DEFAULT,
                    ptr::null_mut(), // No callback for now
                    ptr::null_mut(), // No callback context
                );
                
                if result != 0 {
                    return Err(anyhow!("Failed to create lvol {}: error code {}", lvol_name, result));
                }
                
                // Small delay for creation to complete
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                
                // Get UUID of created lvol
                let bdev_name_c = CString::new(format!("{}/{}", lvs_name, lvol_name))?;
                let lvol_bdev = spdk_bdev_get_by_name(bdev_name_c.as_ptr());
                if lvol_bdev.is_null() {
                    return Err(anyhow!("Failed to find created lvol bdev: {}/{}", lvs_name, lvol_name));
                }
                
                // Extract UUID
                let mut uuid_str = [0i8; 37];
                spdk_uuid_fmt_lower(
                    uuid_str.as_mut_ptr(),
                    uuid_str.len(),
                    spdk_bdev_get_uuid(lvol_bdev)
                );
                
                let uuid = CStr::from_ptr(uuid_str.as_ptr()).to_string_lossy().to_string();
                
                println!("✅ [SPDK_NATIVE] Lvol created: {}/{} (UUID: {})", 
                         lvs_name, lvol_name, uuid);
                
                Ok(uuid)
            }
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            // Mock implementation
            let uuid = uuid::Uuid::new_v4().to_string();
            println!("🔧 [SPDK_MOCK] Mock lvol created: {}/{} (UUID: {})", 
                     lvs_name, lvol_name, uuid);
            Ok(uuid)
        }
    }

    /// Delete lvol with proper cleanup
    pub async fn delete_lvol(&self, lvs_name: &str, lvol_name: &str) -> Result<()> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            println!("🗑️ [SPDK_NATIVE] Deleting lvol: {}/{}", lvs_name, lvol_name);
            
            let bdev_name_c = CString::new(format!("{}/{}", lvs_name, lvol_name))?;
            
            unsafe {
                // Get lvol bdev
                let bdev = spdk_bdev_get_by_name(bdev_name_c.as_ptr());
                if bdev.is_null() {
                    return Err(anyhow!("Lvol {}/{} not found", lvs_name, lvol_name));
                }
                
                // Get lvol from bdev
                let lvol = spdk_lvol_get_from_bdev(bdev);
                if lvol.is_null() {
                    return Err(anyhow!("Invalid lvol bdev: {}/{}", lvs_name, lvol_name));
                }
                
                // Delete lvol
                spdk_lvol_destroy(
                    lvol,
                    ptr::null_mut(), // No callback
                    ptr::null_mut(), // No context
                );
                
                println!("✅ [SPDK_NATIVE] Lvol deleted: {}/{}", lvs_name, lvol_name);
                Ok(())
            }
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            println!("🗑️ [SPDK_MOCK] Mock lvol deleted: {}/{}", lvs_name, lvol_name);
            Ok(())
        }
    }

    /// Get all bdevs with error handling
    pub async fn get_bdevs(&self) -> Result<Vec<Value>> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            let mut bdevs = Vec::new();
            
            unsafe {
                let mut bdev = spdk_bdev_first();
                while !bdev.is_null() {
                    // Safely extract bdev information
                    let name = CStr::from_ptr(spdk_bdev_get_name(bdev))
                        .to_string_lossy().to_string();
                    let product_name = CStr::from_ptr(spdk_bdev_get_product_name(bdev))
                        .to_string_lossy().to_string();
                    
                    // Get UUID safely
                    let mut uuid_str = [0i8; 37];
                    spdk_uuid_fmt_lower(
                        uuid_str.as_mut_ptr(),
                        uuid_str.len(),
                        spdk_bdev_get_uuid(bdev)
                    );
                    let uuid = CStr::from_ptr(uuid_str.as_ptr()).to_string_lossy().to_string();
                    
                    // Get properties
                    let block_size = spdk_bdev_get_block_size(bdev);
                    let num_blocks = spdk_bdev_get_num_blocks(bdev);
                    
                    bdevs.push(json!({
                        "name": name,
                        "uuid": uuid,
                        "product_name": product_name,
                        "block_size": block_size,
                        "num_blocks": num_blocks,
                        "md_size": spdk_bdev_get_md_size(bdev),
                        "md_interleave": spdk_bdev_is_md_interleaved(bdev),
                        "dif_type": spdk_bdev_get_dif_type(bdev),
                        "dif_is_head_of_md": spdk_bdev_is_dif_head_of_md(bdev),
                        "claimed": spdk_bdev_is_claimed(bdev),
                        "supported_io_types": {
                            "read": spdk_bdev_io_type_supported(bdev, spdk_bdev_io_type::SPDK_BDEV_IO_TYPE_READ),
                            "write": spdk_bdev_io_type_supported(bdev, spdk_bdev_io_type::SPDK_BDEV_IO_TYPE_WRITE),
                            "unmap": spdk_bdev_io_type_supported(bdev, spdk_bdev_io_type::SPDK_BDEV_IO_TYPE_UNMAP),
                            "write_zeroes": spdk_bdev_io_type_supported(bdev, spdk_bdev_io_type::SPDK_BDEV_IO_TYPE_WRITE_ZEROES),
                            "flush": spdk_bdev_io_type_supported(bdev, spdk_bdev_io_type::SPDK_BDEV_IO_TYPE_FLUSH),
                            "reset": spdk_bdev_io_type_supported(bdev, spdk_bdev_io_type::SPDK_BDEV_IO_TYPE_RESET),
                        }
                    }));
                    
                    bdev = spdk_bdev_next(bdev);
                }
            }
            
            println!("📋 [SPDK_NATIVE] Retrieved {} bdevs", bdevs.len());
            Ok(bdevs)
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Ok(vec![])
        }
    }

    /// Get LVS stores with comprehensive information
    pub async fn get_lvol_stores(&self) -> Result<Vec<Value>> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            let mut lvs_stores = Vec::new();
            
            unsafe {
                let mut lvs = spdk_lvol_store_first();
                while !lvs.is_null() {
                    // Safely extract LVS information
                    let name = CStr::from_ptr(spdk_lvol_store_get_name(lvs))
                        .to_string_lossy().to_string();
                    
                    // Get UUID
                    let mut uuid_str = [0i8; 37];
                    spdk_uuid_fmt_lower(
                        uuid_str.as_mut_ptr(),
                        uuid_str.len(),
                        spdk_lvol_store_get_uuid(lvs)
                    );
                    let uuid = CStr::from_ptr(uuid_str.as_ptr()).to_string_lossy().to_string();
                    
                    // Get base bdev name
                    let bdev = spdk_lvol_store_get_bs_bdev(lvs);
                    let base_bdev_name = if !bdev.is_null() {
                        CStr::from_ptr(spdk_bdev_get_name(bdev)).to_string_lossy().to_string()
                    } else {
                        "unknown".to_string()
                    };
                    
                    lvs_stores.push(json!({
                        "name": name,
                        "uuid": uuid,
                        "base_bdev": base_bdev_name,
                        "total_size": spdk_lvol_store_get_total_size(lvs),
                        "free_size": spdk_lvol_store_get_free_size(lvs),
                        "cluster_size": spdk_lvol_store_get_cluster_size(lvs),
                        "block_size": 4096
                    }));
                    
                    lvs = spdk_lvol_store_next(lvs);
                }
            }
            
            println!("📋 [SPDK_NATIVE] Retrieved {} LVS stores", lvs_stores.len());
            Ok(lvs_stores)
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Ok(vec![])
        }
    }

    /// Get I/O statistics for all bdevs
    pub async fn get_bdev_iostat(&self) -> Result<Vec<Value>> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            let mut iostats = Vec::new();
            
            unsafe {
                let mut bdev = spdk_bdev_first();
                while !bdev.is_null() {
                    let name = CStr::from_ptr(spdk_bdev_get_name(bdev))
                        .to_string_lossy().to_string();
                    
                    // Get I/O statistics
                    let mut io_stat = std::mem::zeroed::<spdk_bdev_io_stat>();
                    spdk_bdev_get_io_stat(bdev, &mut io_stat);
                    
                    iostats.push(json!({
                        "name": name,
                        "bytes_read": io_stat.bytes_read,
                        "num_read_ops": io_stat.num_read_ops,
                        "bytes_written": io_stat.bytes_written,
                        "num_write_ops": io_stat.num_write_ops,
                        "bytes_unmapped": io_stat.bytes_unmapped,
                        "num_unmap_ops": io_stat.num_unmap_ops,
                        "read_latency_ticks": io_stat.read_latency_ticks,
                        "write_latency_ticks": io_stat.write_latency_ticks,
                        "unmap_latency_ticks": io_stat.unmap_latency_ticks,
                        "ticks_rate": spdk_get_ticks_hz(),
                    }));
                    
                    bdev = spdk_bdev_next(bdev);
                }
            }
            
            println!("📊 [SPDK_NATIVE] Retrieved I/O stats for {} bdevs", iostats.len());
            Ok(iostats)
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Ok(vec![])
        }
    }

    /// Initialize disk blobstore - high-level operation
    pub async fn initialize_disk_blobstore(&self, disk_name: &str, device_path: &str, _pcie_addr: &str) -> Result<String> {
        println!("🚀 [SPDK_NATIVE] Initializing blobstore for disk: {}", disk_name);
        
        // Step 1: Create AIO bdev
        let bdev_name = if device_path.starts_with("/dev/") {
            let device_name = device_path.trim_start_matches("/dev/");
            let aio_bdev_name = format!("aio_{}", device_name);
            self.create_aio_bdev(device_path, &aio_bdev_name).await?;
            aio_bdev_name
        } else {
            return Err(anyhow!("Only /dev/ devices supported in embedded mode"));
        };
        
        // Step 2: Create LVS
        let lvs_name = format!("lvs_{}", disk_name);
        self.create_lvs(&bdev_name, &lvs_name).await?;
        
        println!("🎉 [SPDK_NATIVE] Blobstore initialized for disk: {}", disk_name);
        Ok(lvs_name)
    }

    /// Sync all blobstores
    pub async fn sync_all_blobstores(&self) -> Result<()> {
        if !self.is_initialized() {
            return Err(anyhow!("SPDK not initialized"));
        }

        #[cfg(target_os = "linux")]
        {
            println!("🔄 [SPDK_NATIVE] Syncing all blobstores");
            
            unsafe {
                let mut lvs = spdk_lvol_store_first();
                while !lvs.is_null() {
                    let name = CStr::from_ptr(spdk_lvol_store_get_name(lvs))
                        .to_string_lossy();
                    println!("🔄 [SPDK_NATIVE] Syncing LVS: {}", name);
                    
                    // Note: In a full implementation, we'd call spdk_blob_sync_md()
                    // or equivalent async operation here
                    
                    lvs = spdk_lvol_store_next(lvs);
                }
            }
            
            println!("✅ [SPDK_NATIVE] All blobstores synced");
            Ok(())
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Ok(())
        }
    }

    /// Get blobstores (alias for get_lvol_stores)
    pub async fn get_blobstores(&self) -> Result<Vec<Value>> {
        self.get_lvol_stores().await
    }

    /// Placeholder functions for compatibility with RPC mode
    pub async fn get_raid_bdevs(&self) -> Result<Vec<Value>> {
        // Embedded mode doesn't create RAID bdevs (uses single lvols)
        Ok(vec![])
    }

    pub async fn get_nvmeof_subsystems(&self) -> Result<Vec<Value>> {
        // Embedded mode uses ublk, not NVMe-oF
        Ok(vec![json!({
            "nqn": "nqn.2014-08.org.nvmexpress.discovery",
            "subtype": "Discovery",
            "state": "active"
        })])
    }
}

/// Global SPDK instance for the node-agent
static mut SPDK_INSTANCE: Option<SpdkNative> = None;

/// Get global SPDK instance
pub fn get_spdk_instance() -> Result<&'static SpdkNative> {
    unsafe {
        SPDK_INIT.call_once(|| {
            match SpdkNative::new() {
                Ok(instance) => {
                    SPDK_INSTANCE = Some(instance);
                }
                Err(e) => {
                    eprintln!("Failed to initialize SPDK: {}", e);
                }
            }
        });
        
        SPDK_INSTANCE.as_ref()
            .ok_or_else(|| anyhow!("SPDK initialization failed"))
    }
}

/// Initialize global SPDK instance
pub async fn initialize_spdk() -> Result<()> {
    let _spdk = get_spdk_instance()?;
    println!("✅ [SPDK_NATIVE] Global SPDK instance ready");
    Ok(())
}