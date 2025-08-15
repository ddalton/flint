// node_agent/raid_operations.rs - RAID Operations and Blobstore Management
//
// This module handles RAID bdev operations, LVS (Logical Volume Store) initialization,
// and blobstore management for SPDK storage.

use crate::node_agent::{NodeAgent, rpc_client::call_spdk_rpc};
use crate::models::SpdkRaidDisk;
use serde_json::json;

/// Initialize blobstore on a RAID device
pub async fn initialize_blobstore_on_device(agent: &NodeAgent, raid: &SpdkRaidDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let raid_name = raid.metadata.name.as_deref().unwrap_or("unknown");
    let lvs_name = raid.spec.lvs_name();
    let raid_bdev_name = raid.spec.raid_bdev_name();

    println!("🚀 [SPDK_INIT] Initializing LVS on RAID bdev for SpdkRaidDisk: {}", raid_name);
    println!("🔧 [SPDK_INIT] LVS name: {}, RAID bdev: {}", lvs_name, raid_bdev_name);

    // Ensure RAID bdev exists
    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_get_bdevs" })).await?;
    let Some(list) = bdevs["result"].as_array() else {
        return Err("Failed to get bdev list".into());
    };
    let raid_exists = list.iter().any(|b| b["name"].as_str() == Some(&raid_bdev_name));
    if !raid_exists {
        return Err(format!("RAID bdev '{}' not found on node; cannot initialize LVS", raid_bdev_name).into());
    }

    // Check for existing LVS on the RAID bdev
    if let Ok(resp) = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_lvol_get_lvstores" })).await {
        if let Some(lvstores) = resp["result"].as_array() {
            if lvstores.iter().any(|lvs| lvs["base_bdev"].as_str() == Some(&raid_bdev_name)) {
                println!("✅ [SPDK_INIT] LVS already exists on RAID bdev '{}'; nothing to do", raid_bdev_name);
                return Ok(());
            }
        }
    }

    // Create LVS on the RAID bdev
    println!("🏗️ [SPDK_INIT] Creating LVS '{}' on RAID bdev '{}'", lvs_name, raid_bdev_name);
    let create = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_create_lvstore",
        "params": { "bdev_name": raid_bdev_name, "lvs_name": lvs_name, "cluster_sz": 1048576 }
    })).await?;

    // SPDK configuration auto-save could be added here if needed

    if let Some(error) = create.get("error") {
        let code = error["code"].as_i64().unwrap_or(0);
        let msg = error["message"].as_str().unwrap_or("");
        if code == -17 || msg.contains("exists") {
            println!("ℹ️ [SPDK_INIT] LVS already exists by name; treating as success");
            return Ok(());
        }
        return Err(format!("SPDK RPC error creating LVS: {}", error).into());
    }

    println!("✅ [SPDK_INIT] Successfully created LVS '{}' on RAID bdev '{}'", lvs_name, raid_bdev_name);
    Ok(())
}

/// Initialize disk blobstore (creates LVS on a single disk)
pub async fn initialize_disk_blobstore(
    agent: &NodeAgent,
    device_path: &str,
    lvs_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🚀 [DISK_INIT] Initializing blobstore on disk: {}", device_path);
    println!("🔧 [DISK_INIT] LVS name: {}", lvs_name);

    // Extract device name from path (e.g., "/dev/nvme0n1" -> "nvme0n1")
    let device_name = device_path.strip_prefix("/dev/").unwrap_or(device_path);
    let bdev_name = format!("nvme-{}", device_name);

    // Check if bdev exists in SPDK
    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_get_bdevs" })).await?;
    let Some(bdev_list) = bdevs["result"].as_array() else {
        return Err("Failed to get bdev list".into());
    };

    let bdev_exists = bdev_list.iter().any(|b| b["name"].as_str() == Some(&bdev_name));
    if !bdev_exists {
        // Create AIO bdev for the device
        println!("🔧 [DISK_INIT] Creating AIO bdev: {}", bdev_name);
        let create_bdev = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
            "method": "bdev_aio_create",
            "params": {
                "name": bdev_name,
                "filename": device_path
            }
        })).await;

        match create_bdev {
            Ok(_) => println!("✅ [DISK_INIT] Created AIO bdev: {}", bdev_name),
            Err(e) => {
                println!("⚠️ [DISK_INIT] Failed to create AIO bdev: {}", e);
                return Err(e);
            }
        }
    } else {
        println!("ℹ️ [DISK_INIT] Bdev already exists: {}", bdev_name);
    }

    // Check for existing LVS
    if let Ok(resp) = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_lvol_get_lvstores" })).await {
        if let Some(lvstores) = resp["result"].as_array() {
            if lvstores.iter().any(|lvs| lvs["base_bdev"].as_str() == Some(&bdev_name)) {
                println!("✅ [DISK_INIT] LVS already exists on bdev '{}'; nothing to do", bdev_name);
                return Ok(());
            }
        }
    }

    // Create LVS on the bdev
    println!("🏗️ [DISK_INIT] Creating LVS '{}' on bdev '{}'", lvs_name, bdev_name);
    let create_lvs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_create_lvstore",
        "params": {
            "bdev_name": bdev_name,
            "lvs_name": lvs_name,
            "cluster_sz": 1048576
        }
    })).await;

    match create_lvs {
        Ok(_) => {
            println!("✅ [DISK_INIT] Successfully created LVS '{}' on bdev '{}'", lvs_name, bdev_name);
            // SPDK configuration auto-save could be added here if needed
        }
        Err(e) => {
            // Check if it's an "already exists" error
            if e.to_string().contains("exists") || e.to_string().contains("-17") {
                println!("ℹ️ [DISK_INIT] LVS already exists; treating as success");
            } else {
                println!("⚠️ [DISK_INIT] Failed to create LVS: {}", e);
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Create logical volume on an existing LVS
pub async fn create_logical_volume(
    agent: &NodeAgent,
    lvs_name: &str,
    lvol_name: &str,
    size_bytes: u64,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    println!("📁 [LVOL] Creating logical volume: {} on LVS: {}", lvol_name, lvs_name);
    println!("📏 [LVOL] Size: {} bytes ({} GB)", size_bytes, size_bytes / (1024 * 1024 * 1024));

    // Check if logical volume already exists
    if let Ok(resp) = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_get_bdevs" })).await {
        if let Some(bdevs) = resp["result"].as_array() {
            let full_lvol_name = format!("{}/{}", lvs_name, lvol_name);
            if bdevs.iter().any(|b| b["name"].as_str() == Some(&full_lvol_name)) {
                println!("ℹ️ [LVOL] Logical volume already exists: {}", full_lvol_name);
                return Ok(full_lvol_name);
            }
        }
    }

    // Create the logical volume
    let create_lvol = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_create",
        "params": {
            "lvol_name": lvol_name,
            "size": size_bytes,
            "lvs_name": lvs_name,
            "thin_provision": false
        }
    })).await;

    match create_lvol {
        Ok(_response) => {
            let full_lvol_name = format!("{}/{}", lvs_name, lvol_name);
            println!("✅ [LVOL] Successfully created logical volume: {}", full_lvol_name);
            
            // SPDK configuration auto-save could be added here if needed
            
            Ok(full_lvol_name)
        }
        Err(e) => {
            if e.to_string().contains("exists") {
                let full_lvol_name = format!("{}/{}", lvs_name, lvol_name);
                println!("ℹ️ [LVOL] Logical volume already exists: {}", full_lvol_name);
                Ok(full_lvol_name)
            } else {
                println!("⚠️ [LVOL] Failed to create logical volume: {}", e);
                Err(e)
            }
        }
    }
}

/// Delete logical volume
pub async fn delete_logical_volume(
    agent: &NodeAgent,
    lvol_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🗑️ [LVOL] Deleting logical volume: {}", lvol_name);

    let delete_lvol = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_delete",
        "params": {
            "name": lvol_name
        }
    })).await;

    match delete_lvol {
        Ok(_) => {
            println!("✅ [LVOL] Successfully deleted logical volume: {}", lvol_name);
            // SPDK configuration auto-save could be added here if needed
        }
        Err(e) => {
            if e.to_string().contains("not found") || e.to_string().contains("does not exist") {
                println!("ℹ️ [LVOL] Logical volume not found (already deleted): {}", lvol_name);
            } else {
                println!("⚠️ [LVOL] Failed to delete logical volume: {}", e);
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Create RAID bdev from member devices
pub async fn create_raid_bdev(
    agent: &NodeAgent,
    raid_name: &str,
    raid_level: &str,
    member_bdevs: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🛡️ [RAID] Creating RAID bdev: {}", raid_name);
    println!("🔧 [RAID] Level: {}, Members: {:?}", raid_level, member_bdevs);

    // Check if RAID already exists
    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_get_bdevs" })).await?;
    if let Some(bdev_list) = bdevs["result"].as_array() {
        if bdev_list.iter().any(|b| b["name"].as_str() == Some(raid_name)) {
            println!("ℹ️ [RAID] RAID bdev already exists: {}", raid_name);
            return Ok(());
        }
    }

    // Create RAID bdev
    let create_raid = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_raid_create",
        "params": {
            "name": raid_name,
            "raid_level": raid_level,
            "base_bdevs": member_bdevs,
            "superblock": true
        }
    })).await;

    match create_raid {
        Ok(_) => {
            println!("✅ [RAID] Successfully created RAID bdev: {}", raid_name);
            // SPDK configuration auto-save could be added here if needed
        }
        Err(e) => {
            if e.to_string().contains("exists") {
                println!("ℹ️ [RAID] RAID bdev already exists: {}", raid_name);
            } else {
                println!("⚠️ [RAID] Failed to create RAID bdev: {}", e);
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Delete RAID bdev
pub async fn delete_raid_bdev(
    agent: &NodeAgent,
    raid_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🗑️ [RAID] Deleting RAID bdev: {}", raid_name);

    let delete_raid = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_raid_delete",
        "params": {
            "name": raid_name
        }
    })).await;

    match delete_raid {
        Ok(_) => {
            println!("✅ [RAID] Successfully deleted RAID bdev: {}", raid_name);
            // SPDK configuration auto-save could be added here if needed
        }
        Err(e) => {
            if e.to_string().contains("not found") || e.to_string().contains("does not exist") {
                println!("ℹ️ [RAID] RAID bdev not found (already deleted): {}", raid_name);
            } else {
                println!("⚠️ [RAID] Failed to delete RAID bdev: {}", e);
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Get RAID status and health information
pub async fn get_raid_status(
    agent: &NodeAgent,
    raid_name: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    println!("📊 [RAID] Getting RAID status for: {}", raid_name);

    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_get_bdevs" })).await?;
    
    if let Some(bdev_list) = bdevs["result"].as_array() {
        for bdev in bdev_list {
            if let Some(name) = bdev["name"].as_str() {
                if name == raid_name {
                    if let Some(driver_specific) = bdev.get("driver_specific") {
                        if let Some(raid_info) = driver_specific.get("raid") {
                            println!("✅ [RAID] Found RAID status for: {}", raid_name);
                            return Ok(raid_info.clone());
                        }
                    }
                }
            }
        }
    }

    Err(format!("RAID bdev '{}' not found", raid_name).into())
}

/// Get LVS information
pub async fn get_lvs_info(
    agent: &NodeAgent,
    lvs_name: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    println!("📊 [LVS] Getting LVS information for: {}", lvs_name);

    let lvstores = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_lvol_get_lvstores" })).await?;
    
    if let Some(lvs_list) = lvstores["result"].as_array() {
        for lvs in lvs_list {
            if let Some(name) = lvs["name"].as_str() {
                if name == lvs_name {
                    println!("✅ [LVS] Found LVS information for: {}", lvs_name);
                    return Ok(lvs.clone());
                }
            }
        }
    }

    Err(format!("LVS '{}' not found", lvs_name).into())
}

/// List all logical volumes in an LVS
pub async fn list_logical_volumes(
    agent: &NodeAgent,
    lvs_name: &str,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
    println!("📋 [LVS] Listing logical volumes in LVS: {}", lvs_name);

    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_get_bdevs" })).await?;
    let mut logical_volumes = Vec::new();
    
    if let Some(bdev_list) = bdevs["result"].as_array() {
        for bdev in bdev_list {
            if let Some(name) = bdev["name"].as_str() {
                // Check if this is a logical volume (format: lvs_name/lvol_name)
                if name.starts_with(&format!("{}/", lvs_name)) {
                    logical_volumes.push(bdev.clone());
                }
            }
        }
    }

    println!("✅ [LVS] Found {} logical volumes in LVS: {}", logical_volumes.len(), lvs_name);
    Ok(logical_volumes)
}
