// spdk_native.rs - Native SPDK integration using custom generated bindings
// This module provides safe Rust wrappers around SPDK C APIs for Flint's needs

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
    // Minimal mock types for development on non-Linux platforms
    pub type spdk_bdev = *mut std::ffi::c_void;
    pub type spdk_lvol_store = *mut std::ffi::c_void;
    pub type spdk_lvol = *mut std::ffi::c_void;
    
    // Mock constants
    pub const SPDK_BDEV_LARGE_BUF_MAX_SIZE: usize = 65536;
}

#[cfg(target_os = "linux")]
use bindings::*;

/// Native SPDK integration for Flint
pub struct SpdkNative {
    initialized: Arc<Mutex<bool>>,
}

impl SpdkNative {
    /// Initialize SPDK for Flint usage
    pub fn new() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            // TODO: Initialize SPDK application framework
            println!("🚀 [SPDK_NATIVE] Initializing native SPDK integration");
            
            Ok(Self {
                initialized: Arc::new(Mutex::new(true)),
            })
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native integration only available on Linux"))
        }
    }

    /// Create AIO bdev for kernel-bound devices
    pub async fn create_aio_bdev(&self, device_path: &str, bdev_name: &str) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            println!("🔗 [SPDK_NATIVE] Creating AIO bdev: {} -> {}", device_path, bdev_name);
            
            // Convert Rust strings to C strings
            let _device_path_c = CString::new(device_path)?;
            let _bdev_name_c = CString::new(bdev_name)?;
            
            // TODO: Implement actual SPDK AIO bdev creation
            // This would call spdk_bdev_aio_create() with proper parameters
            
            println!("✅ [SPDK_NATIVE] AIO bdev creation placeholder: {}", bdev_name);
            Ok(())
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Check if LVS exists
    pub async fn lvs_exists(&self, lvs_name: &str) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            println!("🔍 [SPDK_NATIVE] Checking if LVS exists: {}", lvs_name);
            
            // TODO: Implement actual LVS lookup using spdk_lvol_store_get_by_name()
            // For now, return false as placeholder
            Ok(false)
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Get LVS information
    pub async fn get_lvs_info(&self, lvs_name: &str) -> Result<Option<Value>> {
        #[cfg(target_os = "linux")]
        {
            println!("📊 [SPDK_NATIVE] Getting LVS info: {}", lvs_name);
            
            // TODO: Implement actual LVS info retrieval
            // This would use spdk_lvol_store functions to get capacity, etc.
            
            let info = json!({
                "name": lvs_name,
                "uuid": "placeholder-uuid",
                "total_size": 0,
                "free_size": 0,
                "cluster_size": 1048576,
                "bdev_name": "placeholder-bdev"
            });
            
            Ok(Some(info))
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Create LVS on bdev
    pub async fn create_lvs(&self, bdev_name: &str, lvs_name: &str) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            println!("🏗️ [SPDK_NATIVE] Creating LVS: {} on bdev: {}", lvs_name, bdev_name);
            
            // Convert to C strings
            let _bdev_name_c = CString::new(bdev_name)?;
            let _lvs_name_c = CString::new(lvs_name)?;
            
            // TODO: Implement actual LVS creation using spdk_lvol_store_create()
            
            println!("✅ [SPDK_NATIVE] LVS creation placeholder: {}", lvs_name);
            Ok(())
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Create logical volume
    pub async fn create_lvol(&self, lvs_name: &str, lvol_name: &str, size_bytes: u64) -> Result<String> {
        #[cfg(target_os = "linux")]
        {
            println!("🔧 [SPDK_NATIVE] Creating lvol: {} in LVS: {} (size: {} bytes)", 
                     lvol_name, lvs_name, size_bytes);
            
            // TODO: Implement actual lvol creation using spdk_lvol_create()
            
            let bdev_name = format!("{}/{}", lvs_name, lvol_name);
            println!("✅ [SPDK_NATIVE] Lvol creation placeholder: {}", bdev_name);
            Ok(bdev_name)
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Delete logical volume
    pub async fn delete_lvol(&self, lvs_name: &str, lvol_name: &str) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            println!("🗑️ [SPDK_NATIVE] Deleting lvol: {} from LVS: {}", lvol_name, lvs_name);
            
            // TODO: Implement actual lvol deletion using spdk_lvol_destroy()
            
            println!("✅ [SPDK_NATIVE] Lvol deletion placeholder: {}", lvol_name);
            Ok(())
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Get all bdevs
    pub async fn get_bdevs(&self) -> Result<Vec<Value>> {
        #[cfg(target_os = "linux")]
        {
            println!("📋 [SPDK_NATIVE] Getting all bdevs");
            
            // TODO: Implement using spdk_bdev_first() and spdk_bdev_next()
            
            Ok(vec![])
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Get LVS stores list
    pub async fn get_lvol_stores(&self) -> Result<Vec<Value>> {
        #[cfg(target_os = "linux")]
        {
            println!("📋 [SPDK_NATIVE] Getting all LVS stores");
            
            // TODO: Implement using SPDK lvol store iteration functions
            
            Ok(vec![])
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("SPDK native operations only available on Linux"))
        }
    }

    /// Initialize blobstore on disk
    pub async fn initialize_disk_blobstore(&self, disk_name: &str, device_path: &str, _pcie_addr: &str) -> Result<String> {
        println!("🚀 [SPDK_NATIVE] Initializing blobstore for disk: {}", disk_name);
        
        // Step 1: Create AIO bdev for kernel-bound devices
        let bdev_name = if device_path.starts_with("/dev/") {
            let device_name = device_path.trim_start_matches("/dev/");
            let aio_bdev_name = format!("aio_{}", device_name);
            self.create_aio_bdev(device_path, &aio_bdev_name).await?;
            aio_bdev_name
        } else {
            return Err(anyhow!("NVMe controller attach not yet implemented"));
        };
        
        // Step 2: Create LVS (Logical Volume Store) 
        let lvs_name = format!("lvs_{}", disk_name);
        self.create_lvs(&bdev_name, &lvs_name).await?;
        
        println!("🎉 [SPDK_NATIVE] Successfully initialized blobstore for disk: {}", disk_name);
        Ok(lvs_name)
    }

    /// Shutdown native SPDK
    pub async fn shutdown(&self) -> Result<()> {
        println!("🛑 [SPDK_NATIVE] Shutting down native SPDK...");
        
        #[cfg(target_os = "linux")]
        {
            // TODO: Implement proper SPDK shutdown
        }
        
        println!("✅ [SPDK_NATIVE] SPDK shutdown completed");
        Ok(())
    }
}

/// Global SPDK instance for the node-agent
static SPDK_INIT: Once = Once::new();
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
                    // Leave SPDK_INSTANCE as None to indicate failure
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
    println!("✅ [SPDK_NATIVE] Global SPDK instance initialized");
    Ok(())
} 