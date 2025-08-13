// nvmeof_export_manager.rs - Manages NVMe-oF export lifecycle and prevents resource conflicts
use serde_json::{json, Value};
use std::collections::HashSet;
use reqwest::Client as HttpClient;

/// Manages NVMe-oF exports and prevents resource conflicts with RAID usage
pub struct NvmeofExportManager {
    spdk_rpc_url: String,
    node_id: String,
}

impl NvmeofExportManager {
    pub fn new(spdk_rpc_url: String, node_id: String) -> Self {
        Self {
            spdk_rpc_url,
            node_id,
        }
    }

    /// Check if a disk is currently exported via NVMe-oF
    pub async fn is_disk_exported(&self, disk_bdev_name: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let expected_nqn = self.generate_raw_disk_nqn(disk_bdev_name);
        self.nvmeof_subsystem_exists(&expected_nqn).await
    }

    /// Check if a disk is currently used as a RAID member
    pub async fn is_disk_in_raid(&self, disk_bdev_name: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        
        // Get all RAID bdevs
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_get_bdevs",
                "params": {}
            }))
            .send()
            .await?;
            
        let raid_data: Value = response.json().await?;
        
        if let Some(raids) = raid_data.get("result").and_then(|r| r.as_array()) {
            for raid in raids {
                if let Some(base_bdevs) = raid.get("base_bdevs").and_then(|bb| bb.as_array()) {
                    for base_bdev in base_bdevs {
                        if let Some(name) = base_bdev.as_str() {
                            if name == disk_bdev_name {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
        
        Ok(false)
    }

    /// Remove NVMe-oF export for a disk that's becoming a RAID member
    pub async fn remove_export_for_raid_usage(&self, disk_bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let export_nqn = self.generate_raw_disk_nqn(disk_bdev_name);
        
        if self.nvmeof_subsystem_exists(&export_nqn).await? {
            println!("🔧 [EXPORT_CLEANUP] Removing NVMe-oF export {} - disk becoming RAID member", export_nqn);
            self.delete_nvmeof_subsystem(&export_nqn).await?;
            println!("✅ [EXPORT_CLEANUP] Successfully removed conflicting export: {}", export_nqn);
        } else {
            println!("ℹ️ [EXPORT_CLEANUP] No existing export found for {}", disk_bdev_name);
        }
        
        Ok(())
    }

    /// Create NVMe-oF export for standalone disk usage (only if not in RAID)
    pub async fn create_export_if_standalone(&self, disk_bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if disk is already in RAID
        if self.is_disk_in_raid(disk_bdev_name).await? {
            println!("ℹ️ [EXPORT_MANAGER] Skipping export for {} - disk is RAID member", disk_bdev_name);
            return Ok(());
        }

        // Check if already exported
        if self.is_disk_exported(disk_bdev_name).await? {
            println!("ℹ️ [EXPORT_MANAGER] Export already exists for {}", disk_bdev_name);
            return Ok(());
        }

        // Safe to create export
        println!("🌐 [EXPORT_MANAGER] Creating NVMe-oF export for standalone disk: {}", disk_bdev_name);
        self.create_raw_disk_export(disk_bdev_name).await?;
        
        Ok(())
    }

    /// Batch cleanup of conflicting exports before RAID creation
    pub async fn cleanup_conflicting_exports(&self, raid_member_bdevs: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [BATCH_CLEANUP] Cleaning up conflicting exports for {} RAID members", raid_member_bdevs.len());
        
        for bdev_name in raid_member_bdevs {
            if let Err(e) = self.remove_export_for_raid_usage(bdev_name).await {
                println!("⚠️ [BATCH_CLEANUP] Failed to remove export for {}: {}", bdev_name, e);
                // Continue with other disks rather than failing entire operation
            }
        }
        
        println!("✅ [BATCH_CLEANUP] Completed export cleanup for RAID creation");
        Ok(())
    }

    /// Get all currently exported disk bdev names
    pub async fn get_exported_disk_bdevs(&self) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        let mut exported_bdevs = HashSet::new();
        
        // Get all NVMe-oF subsystems
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_get_subsystems",
                "params": {}
            }))
            .send()
            .await?;
            
        let subsystems_data: Value = response.json().await?;
        
        if let Some(subsystems) = subsystems_data.get("result").and_then(|r| r.as_array()) {
            for subsystem in subsystems {
                if let Some(nqn) = subsystem.get("nqn").and_then(|n| n.as_str()) {
                    // Check if this is a raw disk export (not a logical volume export)
                    if nqn.contains(&format!(":raw-{}-", self.node_id)) {
                        // Extract bdev name from NQN: nqn.2025-05.io.flint:raw-nodeA-Nvme1n1 -> Nvme1n1
                        if let Some(bdev_name) = nqn.split('-').last() {
                            exported_bdevs.insert(bdev_name.to_string());
                        }
                    }
                }
            }
        }
        
        Ok(exported_bdevs)
    }

    /// Reconcile exports - remove conflicts and add missing exports for standalone disks
    pub async fn reconcile_exports(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔄 [EXPORT_RECONCILE] Starting NVMe-oF export reconciliation");
        
        // Get all available bdevs
        let available_bdevs = self.get_nvme_bdevs().await?;
        
        // Get currently exported bdevs
        let exported_bdevs = self.get_exported_disk_bdevs().await?;
        
        // Check each exported bdev for conflicts
        for exported_bdev in &exported_bdevs {
            if self.is_disk_in_raid(exported_bdev).await? {
                println!("🔧 [EXPORT_RECONCILE] Removing conflicting export for RAID member: {}", exported_bdev);
                if let Err(e) = self.remove_export_for_raid_usage(exported_bdev).await {
                    println!("⚠️ [EXPORT_RECONCILE] Failed to remove conflicting export for {}: {}", exported_bdev, e);
                }
            }
        }
        
        // Check each available bdev for missing exports
        for bdev_name in &available_bdevs {
            if !exported_bdevs.contains(bdev_name) && !self.is_disk_in_raid(bdev_name).await? {
                println!("🌐 [EXPORT_RECONCILE] Creating missing export for standalone disk: {}", bdev_name);
                if let Err(e) = self.create_raw_disk_export(bdev_name).await {
                    println!("⚠️ [EXPORT_RECONCILE] Failed to create export for {}: {}", bdev_name, e);
                }
            }
        }
        
        println!("✅ [EXPORT_RECONCILE] Export reconciliation completed");
        Ok(())
    }

    // Private helper methods

    fn generate_raw_disk_nqn(&self, disk_bdev_name: &str) -> String {
        format!("nqn.2025-05.io.flint:raw-{}-{}", self.node_id, disk_bdev_name)
    }

    async fn nvmeof_subsystem_exists(&self, nqn: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_get_subsystems",
                "params": {}
            }))
            .send()
            .await?;
            
        let subsystems_data: Value = response.json().await?;
        
        if let Some(subsystems) = subsystems_data.get("result").and_then(|r| r.as_array()) {
            for subsystem in subsystems {
                if let Some(existing_nqn) = subsystem.get("nqn").and_then(|n| n.as_str()) {
                    if existing_nqn == nqn {
                        return Ok(true);
                    }
                }
            }
        }
        
        Ok(false)
    }

    async fn delete_nvmeof_subsystem(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_delete_subsystem",
                "params": {
                    "nqn": nqn
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            if !error_text.contains("does not exist") {
                return Err(format!("Failed to delete NVMe-oF subsystem {}: {}", nqn, error_text).into());
            }
        }

        Ok(())
    }

    async fn create_raw_disk_export(&self, disk_bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        let nqn = self.generate_raw_disk_nqn(disk_bdev_name);
        
        // Create subsystem
        let create_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_create_subsystem",
                "params": {
                    "nqn": nqn,
                    "allow_any_host": true,
                    "serial_number": format!("FLINT-RAW-{}", disk_bdev_name),
                    "model_number": "Flint Raw Disk"
                }
            }))
            .send()
            .await?;

        if !create_response.status().is_success() {
            let error_text = create_response.text().await?;
            if !error_text.contains("already exists") {
                return Err(format!("Failed to create NVMe-oF subsystem {}: {}", nqn, error_text).into());
            }
        }

        // Add namespace
        let namespace_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_subsystem_add_ns",
                "params": {
                    "nqn": nqn,
                    "namespace": {
                        "nsid": 1,
                        "bdev_name": disk_bdev_name
                    }
                }
            }))
            .send()
            .await?;

        if !namespace_response.status().is_success() {
            let error_text = namespace_response.text().await?;
            return Err(format!("Failed to add namespace to {}: {}", nqn, error_text).into());
        }

        // Add listener
        let listener_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_subsystem_add_listener",
                "params": {
                    "nqn": nqn,
                    "listen_address": {
                        "trtype": "TCP",
                        "traddr": "0.0.0.0",
                        "trsvcid": "4420"
                    }
                }
            }))
            .send()
            .await?;

        if !listener_response.status().is_success() {
            let error_text = listener_response.text().await?;
            if !error_text.contains("already exists") {
                return Err(format!("Failed to add listener to {}: {}", nqn, error_text).into());
            }
        }

        println!("✅ [EXPORT_MANAGER] Successfully created NVMe-oF export: {}", nqn);
        Ok(())
    }

    async fn get_nvme_bdevs(&self) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        let mut nvme_bdevs = HashSet::new();
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_get_bdevs",
                "params": {}
            }))
            .send()
            .await?;
            
        let bdevs_data: Value = response.json().await?;
        
        if let Some(bdevs) = bdevs_data.get("result").and_then(|r| r.as_array()) {
            for bdev in bdevs {
                if let Some(product) = bdev.get("product_name").and_then(|p| p.as_str()) {
                    if product.to_lowercase().contains("nvme") {
                        if let Some(name) = bdev.get("name").and_then(|n| n.as_str()) {
                            nvme_bdevs.insert(name.to_string());
                        }
                    }
                }
            }
        }
        
        Ok(nvme_bdevs)
    }
}
