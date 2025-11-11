// hybrid_controller.rs - Hybrid controller that can use both CRDs and SPDK queries
// This enables gradual migration from CRD-based state to minimal SPDK state

use std::sync::Arc;
use crate::driver::SpdkCsiDriver;
use crate::spdk_state::{SpdkStateService, SpdkStateDisk};
use crate::models::*;
use kube::{Api, api::ListParams};
use tonic::Status;

/// Migration modes for gradual transition
#[derive(Debug, Clone, PartialEq)]
pub enum MigrationMode {
    /// Use CRDs only (current behavior)
    CrdOnly,
    /// Use SPDK queries to verify CRD data  
    CrdVerified,
    /// Use SPDK queries with CRD fallback
    SpdkPreferred,
    /// Use SPDK queries only (minimal state)
    SpdkOnly,
}

impl MigrationMode {
    pub fn from_env() -> Self {
        match std::env::var("FLINT_MIGRATION_MODE").unwrap_or("crd_only".to_string()).as_str() {
            "crd_only" => MigrationMode::CrdOnly,
            "crd_verified" => MigrationMode::CrdVerified,
            "spdk_preferred" => MigrationMode::SpdkPreferred,
            "spdk_only" => MigrationMode::SpdkOnly,
            _ => {
                eprintln!("Unknown migration mode, defaulting to crd_only");
                MigrationMode::CrdOnly
            }
        }
    }
}

/// Hybrid Controller Service - supports gradual migration
pub struct HybridControllerService {
    driver: Arc<SpdkCsiDriver>,
    spdk_state: SpdkStateService,
    migration_mode: MigrationMode,
}

impl HybridControllerService {
    pub fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        let migration_mode = MigrationMode::from_env();
        println!("🔄 [HYBRID] Operating in migration mode: {:?}", migration_mode);
        
        Self {
            spdk_state: SpdkStateService::new(driver.clone()),
            driver,
            migration_mode,
        }
    }

    /// Get available disks - hybrid approach based on migration mode
    pub async fn get_available_disks_hybrid(&self, capacity: i64) -> Result<Vec<AvailableDisk>, Status> {
        match self.migration_mode {
            MigrationMode::CrdOnly => {
                self.get_available_disks_crd_only(capacity).await
            }
            MigrationMode::CrdVerified => {
                self.get_available_disks_crd_verified(capacity).await
            }
            MigrationMode::SpdkPreferred => {
                // Try SPDK first, fall back to CRDs
                match self.get_available_disks_spdk_only(capacity).await {
                    Ok(disks) => Ok(disks),
                    Err(_) => {
                        println!("⚠️ [HYBRID] SPDK query failed, falling back to CRDs");
                        self.get_available_disks_crd_only(capacity).await
                    }
                }
            }
            MigrationMode::SpdkOnly => {
                self.get_available_disks_spdk_only(capacity).await
            }
        }
    }

    /// Get volume information - hybrid approach
    pub async fn get_volume_hybrid(&self, volume_id: &str) -> Result<Option<VolumeInfo>, Status> {
        match self.migration_mode {
            MigrationMode::CrdOnly => {
                self.get_volume_crd_only(volume_id).await
            }
            MigrationMode::CrdVerified => {
                self.get_volume_crd_verified(volume_id).await  
            }
            MigrationMode::SpdkPreferred => {
                // Try SPDK first, fall back to CRDs
                match self.get_volume_spdk_only(volume_id).await {
                    Ok(Some(volume)) => Ok(Some(volume)),
                    Ok(None) | Err(_) => {
                        println!("⚠️ [HYBRID] SPDK volume query failed, falling back to CRDs");
                        self.get_volume_crd_only(volume_id).await
                    }
                }
            }
            MigrationMode::SpdkOnly => {
                self.get_volume_spdk_only(volume_id).await
            }
        }
    }

    /// CRD-only implementation (current behavior)
    async fn get_available_disks_crd_only(&self, capacity: i64) -> Result<Vec<AvailableDisk>, Status> {
        let disks: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        let crd_disks = disks
            .list(&ListParams::default())
            .await
            .map_err(|e| Status::internal(format!("Failed to list SpdkDisks: {}", e)))?
            .items
            .into_iter()
            .filter(|d| {
                d.status.as_ref().map_or(false, |s| 
                    s.healthy && s.blobstore_initialized && s.free_space >= capacity as u64)
            })
            .collect::<Vec<_>>();

        // Convert SpdkDisk CRDs to AvailableDisk format
        let available_disks: Vec<AvailableDisk> = crd_disks.into_iter().map(|crd| {
            AvailableDisk {
                node_name: crd.spec.node_id,
                pci_address: crd.spec.pcie_addr,
                device_name: crd.spec.nvme_controller_id.unwrap_or_else(|| "nvme0n1".to_string()),
                size_bytes: crd.status.as_ref().map(|s| s.total_capacity).unwrap_or(0),
                free_space: crd.status.as_ref().map(|s| s.free_space).unwrap_or(0),
                disk_ref: crd.metadata.name.unwrap_or_else(|| "unknown".to_string()),
            }
        }).collect();

        println!("📊 [HYBRID_CRD] Found {} available disks", available_disks.len());
        Ok(available_disks)
    }

    /// CRD with SPDK verification - cross-check CRD data against SPDK reality
    async fn get_available_disks_crd_verified(&self, capacity: i64) -> Result<Vec<AvailableDisk>, Status> {
        // Get CRD data first  
        let mut crd_disks = self.get_available_disks_crd_only(capacity).await?;
        
        // Verify each disk against SPDK
        println!("🔍 [HYBRID_VERIFIED] Verifying {} CRD disks against SPDK", crd_disks.len());
        
        let spdk_disks = self.spdk_state.get_all_disks().await
            .map_err(|e| Status::internal(format!("SPDK verification failed: {}", e)))?;
        
        // Create lookup map for SPDK disks
        let spdk_map: std::collections::HashMap<String, &SpdkStateDisk> = spdk_disks.iter()
            .map(|d| (format!("{}:{}", d.node_name, d.pci_address), d))
            .collect();
        
        // Verify and update CRD data with SPDK reality
        crd_disks.retain_mut(|crd_disk| {
            let key = format!("{}:{}", crd_disk.node_name, crd_disk.pci_address);
            
            if let Some(spdk_disk) = spdk_map.get(&key) {
                // Update with real SPDK data
                crd_disk.size_bytes = spdk_disk.size_bytes;
                crd_disk.free_space = spdk_disk.free_space;
                
                if !spdk_disk.healthy || !spdk_disk.blobstore_initialized {
                    println!("⚠️ [HYBRID_VERIFIED] Disk {} failed SPDK verification", key);
                    return false; // Remove unhealthy disk
                }
                
                println!("✅ [HYBRID_VERIFIED] Disk {} verified against SPDK", key);
                true
            } else {
                println!("❌ [HYBRID_VERIFIED] Disk {} not found in SPDK, removing", key);
                false // Remove disk not found in SPDK
            }
        });
        
        println!("✅ [HYBRID_VERIFIED] {} disks passed verification", crd_disks.len());
        Ok(crd_disks)
    }

    /// SPDK-only implementation (minimal state)
    async fn get_available_disks_spdk_only(&self, capacity: i64) -> Result<Vec<AvailableDisk>, Status> {
        let spdk_disks = self.spdk_state.get_available_disks(capacity as u64).await
            .map_err(|e| Status::internal(format!("SPDK query failed: {}", e)))?;
        
        // Convert SpdkStateDisk to AvailableDisk format
        let available_disks: Vec<AvailableDisk> = spdk_disks.into_iter().map(|spdk_disk| {
            let disk_ref = format!("{}-pci-{}", spdk_disk.node_name, spdk_disk.pci_address.replace(":", "-").replace(".", "-"));
            AvailableDisk {
                node_name: spdk_disk.node_name,
                pci_address: spdk_disk.pci_address,
                device_name: spdk_disk.device_name,
                size_bytes: spdk_disk.size_bytes,
                free_space: spdk_disk.free_space,
                disk_ref,
            }
        }).collect();

        println!("📊 [HYBRID_SPDK] Found {} available disks via SPDK", available_disks.len());
        Ok(available_disks)
    }

    /// Get volume from CRDs only
    async fn get_volume_crd_only(&self, volume_id: &str) -> Result<Option<VolumeInfo>, Status> {
        let volumes: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        match volumes.get(volume_id).await {
            Ok(crd_volume) => {
                let volume_info = VolumeInfo {
                    volume_id: crd_volume.spec.volume_id,
                    size_bytes: crd_volume.spec.size_bytes as u64,
                    replica_count: crd_volume.spec.num_replicas as u32,
                    health: "unknown".to_string(), // TODO: Determine from replicas
                };
                Ok(Some(volume_info))
            }
            Err(_) => Ok(None)
        }
    }

    /// Get volume with CRD/SPDK verification
    async fn get_volume_crd_verified(&self, volume_id: &str) -> Result<Option<VolumeInfo>, Status> {
        // Get from CRD first
        let crd_result = self.get_volume_crd_only(volume_id).await?;
        
        // Verify against SPDK
        if let Some(mut crd_volume) = crd_result {
            if let Ok(Some(spdk_volume)) = self.spdk_state.get_volume(volume_id).await {
                // Update with SPDK reality
                crd_volume.replica_count = spdk_volume.replicas.len() as u32;
                crd_volume.health = spdk_volume.health;
                
                println!("✅ [HYBRID_VERIFIED] Volume {} verified against SPDK", volume_id);
            } else {
                println!("⚠️ [HYBRID_VERIFIED] Volume {} not found in SPDK", volume_id);
                crd_volume.health = "missing".to_string();
            }
            
            Ok(Some(crd_volume))
        } else {
            Ok(None)
        }
    }

    /// Get volume from SPDK only
    async fn get_volume_spdk_only(&self, volume_id: &str) -> Result<Option<VolumeInfo>, Status> {
        match self.spdk_state.get_volume(volume_id).await {
            Ok(Some(spdk_volume)) => {
                let volume_info = VolumeInfo {
                    volume_id: spdk_volume.volume_id,
                    size_bytes: spdk_volume.size_bytes,
                    replica_count: spdk_volume.replicas.len() as u32,
                    health: spdk_volume.health,
                };
                Ok(Some(volume_info))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Status::internal(format!("SPDK query failed: {}", e)))
        }
    }
}

/// Unified disk representation for hybrid controller
#[derive(Debug, Clone)]
pub struct AvailableDisk {
    pub node_name: String,
    pub pci_address: String,
    pub device_name: String,
    pub size_bytes: u64,
    pub free_space: u64,
    pub disk_ref: String, // For compatibility with existing code
}

/// Unified volume representation for hybrid controller  
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub volume_id: String,
    pub size_bytes: u64,
    pub replica_count: u32,
    pub health: String,
}
