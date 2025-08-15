// node_agent/nvmeof_manager.rs - NVMe-oF Export Management
//
// This module handles intelligent NVMe-oF export management, including
// export creation, cleanup, and RAID conflict resolution.

use crate::node_agent::{NodeAgent, rpc_client::call_spdk_rpc};

use crate::nvmeof_export_manager::NvmeofExportManager;


use serde_json::json;


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




/// Automatic RAID member repair for local disk failures
/// This provides fast, local-only repair using spare disks on the same node
pub async fn repair_spdkraiddisk_members_for_local_disk(
    agent: &NodeAgent,
    _local_pci_addr: &str,
    local_device_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔧 [AUTO_REPAIR] Starting automatic RAID member repair for failed disk {}", local_device_path);

    // Step 1: Find all RAID devices that contain this failed member
    let affected_raids = find_raids_containing_disk(agent, local_device_path).await?;
    
    if affected_raids.is_empty() {
        println!("ℹ️ [AUTO_REPAIR] No RAID devices found containing disk {}", local_device_path);
        return Ok(());
    }

    println!("🛡️ [AUTO_REPAIR] Found {} RAID device(s) affected by disk failure", affected_raids.len());

    // Step 2: Attempt repair for each affected RAID
    for raid_info in affected_raids {
        println!("🔄 [AUTO_REPAIR] Repairing RAID '{}' affected by failed member '{}'", 
                 raid_info.name, local_device_path);

        match attempt_local_raid_repair(agent, &raid_info, local_device_path).await {
            Ok(replacement_disk) => {
                println!("✅ [AUTO_REPAIR] Successfully replaced failed member in RAID '{}' with '{}'", 
                         raid_info.name, replacement_disk);
                
                // Log the repair for monitoring/alerting
                log_successful_repair(&raid_info.name, local_device_path, &replacement_disk).await;
            }
            Err(e) => {
                println!("❌ [AUTO_REPAIR] Failed to repair RAID '{}': {}", raid_info.name, e);
                
                // Log failure for controller/operator intervention
                log_repair_failure(&raid_info.name, local_device_path, &e.to_string()).await;
            }
        }
    }

    println!("🎉 [AUTO_REPAIR] Automatic repair process completed for disk {}", local_device_path);
    Ok(())
}

/// Information about a RAID device for repair operations
#[derive(Debug, Clone)]
struct RaidRepairInfo {
    name: String,
    failed_member_name: String,
    raid_level: u32,
    total_members: u32,
    healthy_members: u32,
}

/// Find all RAID devices that contain the specified failed disk
async fn find_raids_containing_disk(
    agent: &NodeAgent,
    failed_device_path: &str,
) -> Result<Vec<RaidRepairInfo>, Box<dyn std::error::Error + Send + Sync>> {
    use crate::node_agent::rpc_client::call_spdk_rpc;
    use serde_json::json;

    println!("🔍 [AUTO_REPAIR] Scanning for RAID devices containing failed disk {}", failed_device_path);

    // Query all RAID devices from SPDK
    let raids_response = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_raid_get_bdevs",
        "params": {}
    })).await?;

    let mut affected_raids = Vec::new();
    
    if let Some(raid_list) = raids_response.get("result").and_then(|r| r.as_array()) {
        for raid in raid_list {
            if let Some(raid_name) = raid.get("name").and_then(|n| n.as_str()) {
                // Check if this RAID contains the failed device
                if let Some(members) = raid.get("base_bdevs").and_then(|m| m.as_array()) {
                    let mut found_failed_member = false;
                    let mut failed_member_name = String::new();
                    let mut healthy_count = 0u32;
                    
                    for member in members {
                        if let Some(member_name) = member.get("name").and_then(|n| n.as_str()) {
                            // Check if this member corresponds to our failed device
                            if member_name.contains(&extract_device_name(failed_device_path)) {
                                found_failed_member = true;
                                failed_member_name = member_name.to_string();
                            } else if member.get("state").and_then(|s| s.as_str()) == Some("online") {
                                healthy_count += 1;
                            }
                        }
                    }
                    
                    if found_failed_member {
                        let raid_info = RaidRepairInfo {
                            name: raid_name.to_string(),
                            failed_member_name,
                            raid_level: raid.get("raid_level").and_then(|l| l.as_u64()).unwrap_or(1) as u32,
                            total_members: members.len() as u32,
                            healthy_members: healthy_count,
                        };
                        
                        println!("🛡️ [AUTO_REPAIR] Found affected RAID: {} (level {}, {}/{} members healthy)", 
                                 raid_info.name, raid_info.raid_level, raid_info.healthy_members, raid_info.total_members);
                        
                        affected_raids.push(raid_info);
                    }
                }
            }
        }
    }

    Ok(affected_raids)
}

/// Attempt to repair a RAID using local spare disks
async fn attempt_local_raid_repair(
    agent: &NodeAgent,
    raid_info: &RaidRepairInfo,
    failed_device_path: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use crate::node_agent::rpc_client::call_spdk_rpc;
    use serde_json::json;

    // Step 1: Check if RAID can survive with current healthy members
    let min_members_required = match raid_info.raid_level {
        0 => raid_info.total_members, // RAID 0 requires all members
        1 => 1,                       // RAID 1 can survive with 1 member
        5 => raid_info.total_members - 1, // RAID 5 can lose 1 member
        6 => raid_info.total_members - 2, // RAID 6 can lose 2 members
        _ => raid_info.total_members, // Conservative default
    };

    if raid_info.healthy_members < min_members_required {
        return Err(format!("RAID {} is critically degraded ({} healthy, {} required)", 
                          raid_info.name, raid_info.healthy_members, min_members_required).into());
    }

    // Step 2: Find a suitable local spare disk
    let spare_disk = find_local_spare_disk(agent, failed_device_path).await?;
    
    println!("💾 [AUTO_REPAIR] Selected spare disk '{}' for RAID '{}' repair", spare_disk, raid_info.name);

    // Step 3: Replace the failed member using SPDK
    let replace_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_raid_replace_member",
        "params": {
            "name": raid_info.name,
            "old_member": raid_info.failed_member_name,
            "new_member": spare_disk
        }
    })).await;

    match replace_result {
        Ok(_) => {
            println!("🔄 [AUTO_REPAIR] SPDK member replacement initiated, rebuild will start automatically");
            Ok(spare_disk)
        }
        Err(e) => {
            // Try alternative method if replace_member fails
            println!("⚠️ [AUTO_REPAIR] Direct replacement failed, trying remove+add method: {}", e);
            
            // Remove failed member first
            call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                "method": "bdev_raid_remove_base_bdev", 
                "params": {
                    "name": raid_info.failed_member_name
                }
            })).await?;

            // Add new member
            call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                "method": "bdev_raid_add_base_bdev",
                "params": {
                    "raid_bdev": raid_info.name,
                    "base_bdev": spare_disk
                }
            })).await?;

            println!("✅ [AUTO_REPAIR] Successfully used remove+add method for RAID repair");
            Ok(spare_disk)
        }
    }
}

/// Find a local spare disk suitable for RAID member replacement
async fn find_local_spare_disk(
    agent: &NodeAgent,
    failed_device_path: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use crate::node_agent::rpc_client::call_spdk_rpc;
    use serde_json::json;

    println!("🔍 [AUTO_REPAIR] Searching for local spare disk to replace {}", failed_device_path);

    // Get target size from failed device (if possible)
    let target_size = get_device_size(failed_device_path).await.unwrap_or(1_000_000_000_000); // 1TB default

    // Query all available block devices
    let bdevs_response = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_bdevs",
        "params": {}
    })).await?;

    if let Some(bdev_list) = bdevs_response.get("result").and_then(|r| r.as_array()) {
        for bdev in bdev_list {
            if let Some(bdev_name) = bdev.get("name").and_then(|n| n.as_str()) {
                // Check if this bdev is suitable as a spare
                let is_nvme = bdev_name.starts_with("nvme") || 
                             bdev.get("product_name").and_then(|p| p.as_str())
                                 .map(|p| p.to_lowercase().contains("nvme")).unwrap_or(false);
                
                let is_unclaimed = !bdev.get("claimed").and_then(|c| c.as_bool()).unwrap_or(true);
                
                let size_bytes = bdev.get("num_blocks").and_then(|n| n.as_u64()).unwrap_or(0) *
                               bdev.get("block_size").and_then(|b| b.as_u64()).unwrap_or(512);
                
                let is_adequate_size = size_bytes >= (target_size as f64 * 0.9) as u64; // 90% of original size

                if is_nvme && is_unclaimed && is_adequate_size {
                    println!("✅ [AUTO_REPAIR] Found suitable spare disk: {} (size: {:.1} GB)", 
                             bdev_name, size_bytes as f64 / 1_000_000_000.0);
                    return Ok(bdev_name.to_string());
                }
            }
        }
    }

    Err(format!("No suitable local spare disk found for replacement of {}", failed_device_path).into())
}

/// Get device size in bytes
async fn get_device_size(device_path: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::fs;
    
    let device_name = extract_device_name(device_path);
    let size_path = format!("/sys/block/{}/size", device_name);
    
    match fs::read_to_string(&size_path).await {
        Ok(size_str) => {
            let sectors = size_str.trim().parse::<u64>()?;
            Ok(sectors * 512) // Convert sectors to bytes
        }
        Err(_) => {
            // Fallback: use blockdev command
            use std::process::Command;
            let output = Command::new("blockdev")
                .args(["--getsize64", device_path])
                .output()?;
                
            if output.status.success() {
                let size_str = String::from_utf8_lossy(&output.stdout);
                Ok(size_str.trim().parse::<u64>()?)
            } else {
                Err("Could not determine device size".into())
            }
        }
    }
}

/// Extract device name from device path (e.g., "/dev/nvme0n1" -> "nvme0n1")
fn extract_device_name(device_path: &str) -> String {
    device_path.split('/').last().unwrap_or(device_path).to_string()
}

/// Log successful repair for monitoring systems
async fn log_successful_repair(raid_name: &str, failed_device: &str, replacement_device: &str) {
    println!("📊 [REPAIR_LOG] SUCCESS: RAID '{}' member '{}' replaced with '{}'", 
             raid_name, failed_device, replacement_device);
    
    // In production, this would send metrics to monitoring systems:
    // - Prometheus metrics
    // - Kubernetes events  
    // - Alert manager notifications
    // - Dashboard notifications
}

/// Log repair failure for operator intervention
async fn log_repair_failure(raid_name: &str, failed_device: &str, error: &str) {
    println!("📊 [REPAIR_LOG] FAILURE: RAID '{}' member '{}' repair failed: {}", 
             raid_name, failed_device, error);
    
    // In production, this would:
    // - Create Kubernetes events
    // - Send alerts to operators
    // - Update dashboard with failure status
    // - Trigger controller-level repair as fallback
}
