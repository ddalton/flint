// spdk_embedded.rs - Embedded SPDK integration using OpenEBS spdk-rs
use std::sync::{Arc, Mutex};
use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};
use spdk_rs::{
    blob::{BlobStore, BlobOptions},
    bdev::{Bdev, BdevOpts},
    nvme::{NvmeController, NvmeOpts},
    runtime::{Runtime, RuntimeOpts},
    lvol::{LvolStore, LvolStoreOpts, Lvol, LvolOpts},
};

/// Embedded SPDK instance for direct API access
pub struct SpdkEmbedded {
    runtime: Arc<Runtime>,
    initialized: Arc<Mutex<bool>>,
}

impl SpdkEmbedded {
    /// Initialize embedded SPDK instance
    pub fn new() -> Result<Self> {
        let runtime_opts = RuntimeOpts::default()
            .with_name("flint-node-agent")
            .with_rpc_sock("/var/tmp/spdk.sock");
            
        let runtime = Runtime::init(runtime_opts)?;
        
        Ok(Self {
            runtime: Arc::new(runtime),
            initialized: Arc::new(Mutex::new(true)),
        })
    }

    /// Create AIO bdev for kernel-bound devices
    pub async fn create_aio_bdev(&self, device_path: &str, bdev_name: &str) -> Result<()> {
        println!("🔗 [SPDK_EMBEDDED] Creating AIO bdev: {} -> {}", device_path, bdev_name);
        
        let bdev_opts = BdevOpts::aio()
            .with_name(bdev_name)
            .with_filename(device_path)
            .with_block_size(4096);
            
        match Bdev::create(bdev_opts).await {
            Ok(_) => {
                println!("✅ [SPDK_EMBEDDED] Successfully created AIO bdev: {}", bdev_name);
                Ok(())
            }
            Err(e) if e.to_string().contains("already exists") => {
                println!("✅ [SPDK_EMBEDDED] AIO bdev already exists: {}", bdev_name);
                Ok(())
            }
            Err(e) => {
                println!("❌ [SPDK_EMBEDDED] Failed to create AIO bdev {}: {}", bdev_name, e);
                Err(e.into())
            }
        }
    }

    /// Check if LVS exists
    pub async fn lvs_exists(&self, lvs_name: &str) -> Result<bool> {
        match LvolStore::get_by_name(lvs_name).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Get LVS information
    pub async fn get_lvs_info(&self, lvs_name: &str) -> Result<Option<Value>> {
        match LvolStore::get_by_name(lvs_name).await {
            Ok(lvs) => {
                let info = json!({
                    "name": lvs.name(),
                    "uuid": lvs.uuid(),
                    "total_size": lvs.total_size(),
                    "free_size": lvs.free_size(),
                    "cluster_size": lvs.cluster_size(),
                    "bdev_name": lvs.base_bdev_name()
                });
                println!("📊 [SPDK_EMBEDDED] LVS info for {}: {}", lvs_name, serde_json::to_string_pretty(&info)?);
                Ok(Some(info))
            }
            Err(_) => Ok(None),
        }
    }

    /// Create LVS on bdev
    pub async fn create_lvs(&self, bdev_name: &str, lvs_name: &str) -> Result<()> {
        println!("🏗️ [SPDK_EMBEDDED] Creating LVS: {} on bdev: {}", lvs_name, bdev_name);
        
        // Check if LVS already exists
        if self.lvs_exists(lvs_name).await? {
            println!("✅ [SPDK_EMBEDDED] LVS already exists: {}", lvs_name);
            return Ok(());
        }
        
        let lvs_opts = LvolStoreOpts::new()
            .with_bdev_name(bdev_name)
            .with_name(lvs_name)
            .with_cluster_size(1048576); // 1MB clusters
            
        match LvolStore::create(lvs_opts).await {
            Ok(lvs) => {
                println!("✅ [SPDK_EMBEDDED] Successfully created LVS: {} (UUID: {})", 
                         lvs.name(), lvs.uuid());
                println!("📊 [SPDK_EMBEDDED] LVS capacity: {} bytes free of {} total", 
                         lvs.free_size(), lvs.total_size());
                Ok(())
            }
            Err(e) => {
                println!("❌ [SPDK_EMBEDDED] Failed to create LVS {}: {}", lvs_name, e);
                Err(e.into())
            }
        }
    }

    /// Create logical volume
    pub async fn create_lvol(&self, lvs_name: &str, lvol_name: &str, size_bytes: u64) -> Result<String> {
        println!("🔧 [SPDK_EMBEDDED] Creating lvol: {} in LVS: {} (size: {} bytes)", 
                 lvol_name, lvs_name, size_bytes);
        
        let lvs = LvolStore::get_by_name(lvs_name).await?;
        
        let lvol_opts = LvolOpts::new()
            .with_name(lvol_name)
            .with_size(size_bytes)
            .with_thin_provision(false);
            
        match lvs.create_lvol(lvol_opts).await {
            Ok(lvol) => {
                let bdev_name = format!("{}/{}", lvs_name, lvol_name);
                println!("✅ [SPDK_EMBEDDED] Successfully created lvol: {} (UUID: {})", 
                         bdev_name, lvol.uuid());
                Ok(bdev_name)
            }
            Err(e) => {
                println!("❌ [SPDK_EMBEDDED] Failed to create lvol {}: {}", lvol_name, e);
                Err(e.into())
            }
        }
    }

    /// Delete logical volume
    pub async fn delete_lvol(&self, lvs_name: &str, lvol_name: &str) -> Result<()> {
        println!("🗑️ [SPDK_EMBEDDED] Deleting lvol: {} from LVS: {}", lvol_name, lvs_name);
        
        let lvs = LvolStore::get_by_name(lvs_name).await?;
        let lvol = lvs.get_lvol_by_name(lvol_name).await?;
        
        match lvol.destroy().await {
            Ok(_) => {
                println!("✅ [SPDK_EMBEDDED] Successfully deleted lvol: {}", lvol_name);
                Ok(())
            }
            Err(e) => {
                println!("❌ [SPDK_EMBEDDED] Failed to delete lvol {}: {}", lvol_name, e);
                Err(e.into())
            }
        }
    }

    /// Attach NVMe controller
    pub async fn attach_nvme_controller(&self, pcie_addr: &str) -> Result<String> {
        println!("🔌 [SPDK_EMBEDDED] Attaching NVMe controller: {}", pcie_addr);
        
        let nvme_opts = NvmeOpts::new()
            .with_traddr(pcie_addr)
            .with_name(&format!("nvme_{}", pcie_addr.replace(":", "_")));
            
        match NvmeController::attach(nvme_opts).await {
            Ok(controller) => {
                println!("✅ [SPDK_EMBEDDED] Successfully attached NVMe controller: {}", controller.name());
                Ok(controller.name().to_string())
            }
            Err(e) if e.to_string().contains("already exists") => {
                let controller_name = format!("nvme_{}", pcie_addr.replace(":", "_"));
                println!("✅ [SPDK_EMBEDDED] NVMe controller already attached: {}", controller_name);
                Ok(controller_name)
            }
            Err(e) => {
                println!("❌ [SPDK_EMBEDDED] Failed to attach NVMe controller {}: {}", pcie_addr, e);
                Err(e.into())
            }
        }
    }

    /// Get all bdevs
    pub async fn get_bdevs(&self) -> Result<Vec<Value>> {
        let bdevs = Bdev::list().await?;
        let mut result = Vec::new();
        
        for bdev in bdevs {
            let info = json!({
                "name": bdev.name(),
                "uuid": bdev.uuid(),
                "block_size": bdev.block_size(),
                "num_blocks": bdev.num_blocks(),
                "size_bytes": bdev.size_bytes(),
                "product_name": bdev.product_name(),
                "claimed": bdev.is_claimed()
            });
            result.push(info);
        }
        
        Ok(result)
    }

    /// Get LVS stores list
    pub async fn get_lvol_stores(&self) -> Result<Vec<Value>> {
        let stores = LvolStore::list().await?;
        let mut result = Vec::new();
        
        for lvs in stores {
            let info = json!({
                "name": lvs.name(),
                "uuid": lvs.uuid(),
                "total_size": lvs.total_size(),
                "free_size": lvs.free_size(),
                "cluster_size": lvs.cluster_size(),
                "base_bdev": lvs.base_bdev_name()
            });
            result.push(info);
        }
        
        Ok(result)
    }

    /// Initialize blobstore on disk
    pub async fn initialize_disk_blobstore(&self, disk_name: &str, device_path: &str, pcie_addr: &str) -> Result<String> {
        println!("🚀 [SPDK_EMBEDDED] Initializing blobstore for disk: {}", disk_name);
        
        // Step 1: Create AIO bdev for kernel-bound devices
        let bdev_name = if device_path.starts_with("/dev/") {
            let device_name = device_path.trim_start_matches("/dev/");
            let aio_bdev_name = format!("aio_{}", device_name);
            self.create_aio_bdev(device_path, &aio_bdev_name).await?;
            aio_bdev_name
        } else {
            // Attach NVMe controller for PCIe devices  
            self.attach_nvme_controller(pcie_addr).await?
        };
        
        // Step 2: Create LVS (Logical Volume Store) 
        let lvs_name = format!("lvs_{}", disk_name);
        self.create_lvs(&bdev_name, &lvs_name).await?;
        
        println!("🎉 [SPDK_EMBEDDED] Successfully initialized blobstore for disk: {}", disk_name);
        Ok(lvs_name)
    }

    /// Shutdown embedded SPDK
    pub async fn shutdown(&self) -> Result<()> {
        println!("🛑 [SPDK_EMBEDDED] Shutting down embedded SPDK...");
        
        // Graceful shutdown of SPDK runtime
        self.runtime.shutdown().await?;
        
        println!("✅ [SPDK_EMBEDDED] SPDK shutdown completed");
        Ok(())
    }
}

/// Global SPDK instance for the node-agent
static SPDK_INSTANCE: std::sync::OnceLock<SpdkEmbedded> = std::sync::OnceLock::new();

/// Get global SPDK instance
pub fn get_spdk_instance() -> Result<&'static SpdkEmbedded> {
    SPDK_INSTANCE.get_or_try_init(|| SpdkEmbedded::new())
}

/// Initialize global SPDK instance
pub async fn initialize_spdk() -> Result<()> {
    let _spdk = get_spdk_instance()?;
    println!("✅ [SPDK_EMBEDDED] Global SPDK instance initialized");
    Ok(())
} 