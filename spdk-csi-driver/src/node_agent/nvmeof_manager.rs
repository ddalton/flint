// node_agent/nvmeof_manager.rs - NVMe-oF Export Management
//
// This module handles intelligent NVMe-oF export management, including
// export creation, cleanup, and RAID conflict resolution.

use crate::node_agent::{NodeAgent, rpc_client::call_spdk_rpc};
use crate::models::{NvmeofDisk, NvmeofDiskSpec, NvmeofDiskStatus, SpdkRaidDisk};
use crate::nvmeof_export_manager::NvmeofExportManager;
use crate::models::NvmeofEndpoint;
use kube::Api;
use serde_json::json;
use chrono::Utc;

/// Intelligently manage NVMe-oF exports to avoid RAID conflicts
pub async fn manage_nvmeof_exports_intelligently(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔧 [NVMEOF_MGMT] Starting intelligent NVMe-oF export management for node: {}", agent.node_name);
    
    // Create export manager
    let export_manager = NvmeofExportManager::new(
        agent.spdk_rpc_url.clone(),
        agent.node_name.clone(),
    );
    
    // Get current SPDK configuration to understand RAID usage
    let bdevs_response = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_bdevs"
    })).await?;
    
    let mut raid_members = Vec::new();
    
    if let Some(bdevs) = bdevs_response["result"].as_array() {
        for bdev in bdevs {
            if let Some(driver_specific) = bdev.get("driver_specific") {
                if let Some(raid_info) = driver_specific.get("raid") {
                    if let Some(base_bdevs) = raid_info["base_bdevs"].as_array() {
                        for base_bdev in base_bdevs {
                            if let Some(member_name) = base_bdev.as_str() {
                                raid_members.push(member_name.to_string());
                                println!("🛡️ [NVMEOF_MGMT] Found RAID member: {}", member_name);
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Cleanup exports for devices that are now RAID members
    if !raid_members.is_empty() {
        println!("🧹 [NVMEOF_MGMT] Cleaning up exports for {} RAID members", raid_members.len());
        if let Err(e) = export_manager.cleanup_conflicting_exports(&raid_members).await {
            println!("⚠️ [NVMEOF_MGMT] Failed to cleanup exports: {}", e);
        }
    }
    
    println!("✅ [NVMEOF_MGMT] Intelligent NVMe-oF export management completed");
    Ok(())
}

/// Legacy disk discovery and publishing (temporarily disabled due to model changes)
pub async fn discover_and_publish_nvmeof_disks_legacy(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // TODO: Re-implement with updated models
    println!("Legacy NVMe-oF disk discovery temporarily disabled - needs model updates");
    Ok(())
}


/// Repair RAID disk members for local disks (temporarily disabled due to model changes)
pub async fn repair_spdkraiddisk_members_for_local_disk(
    _agent: &NodeAgent,
    _local_pci_addr: &str,
    _local_device_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // TODO: Re-implement with updated models
    println!("RAID member repair temporarily disabled - needs model updates");
    Ok(())
}
