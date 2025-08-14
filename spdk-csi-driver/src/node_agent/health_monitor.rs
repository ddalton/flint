// node_agent/health_monitor.rs - Device Health Monitoring
//
// This module provides health monitoring capabilities for NVMe devices,
// RAID arrays, and logical volumes.

use crate::node_agent::{NodeAgent, disk_discovery::NvmeDevice, rpc_client::call_spdk_rpc};
use serde_json::json;
use std::process::Command;

/// Check device health status
pub async fn check_device_health(agent: &NodeAgent, device: &NvmeDevice) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    println!("🏥 [HEALTH] Checking health for device: {}", device.controller_id);
    
    // Method 1: Check SPDK bdev status
    if let Ok(spdk_health) = check_spdk_bdev_health(agent, &device.controller_id).await {
        if !spdk_health {
            println!("⚠️ [HEALTH] SPDK reports device unhealthy: {}", device.controller_id);
            return Ok(false);
        }
    }
    
    // Method 2: Check NVMe SMART data if available
    if let Ok(smart_health) = check_nvme_smart_health(&device.device_path).await {
        if !smart_health {
            println!("⚠️ [HEALTH] SMART data indicates device issues: {}", device.controller_id);
            return Ok(false);
        }
    }
    
    // Method 3: Basic connectivity test
    if let Ok(connectivity) = check_device_connectivity(&device.device_path).await {
        if !connectivity {
            println!("⚠️ [HEALTH] Device connectivity test failed: {}", device.controller_id);
            return Ok(false);
        }
    }
    
    println!("✅ [HEALTH] Device health check passed: {}", device.controller_id);
    Ok(true)
}

/// Check SPDK bdev health status
async fn check_spdk_bdev_health(agent: &NodeAgent, controller_id: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔍 [SPDK_HEALTH] Checking SPDK bdev health for: {}", controller_id);
    
    // Get bdev information from SPDK
    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_bdevs"
    })).await?;
    
    if let Some(bdev_list) = bdevs["result"].as_array() {
        for bdev in bdev_list {
            if let Some(name) = bdev["name"].as_str() {
                if name.contains(controller_id) {
                    // Check if bdev is claimed or has any issues
                    if let Some(claimed) = bdev["claimed"].as_bool() {
                        if claimed {
                            println!("ℹ️ [SPDK_HEALTH] Bdev is claimed (in use): {}", name);
                        }
                    }
                    
                    // Check for any error conditions (this would be SPDK-specific)
                    // For now, assume healthy if bdev exists and is accessible
                    println!("✅ [SPDK_HEALTH] SPDK bdev appears healthy: {}", name);
                    return Ok(true);
                }
            }
        }
    }
    
    // If no matching bdev found, it might not be attached to SPDK yet
    println!("ℹ️ [SPDK_HEALTH] No SPDK bdev found for controller: {}", controller_id);
    Ok(true) // Not necessarily unhealthy, just not in SPDK
}

/// Check NVMe SMART health data
async fn check_nvme_smart_health(device_path: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    println!("🧠 [SMART] Checking SMART health for: {}", device_path);
    
    // Try to get SMART health information using nvme-cli
    match Command::new("nvme").args(["smart-log", device_path]).output() {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                
                // Check for critical warnings
                if stdout.contains("Critical Warning") {
                    for line in stdout.lines() {
                        if line.contains("Critical Warning") && line.contains(": 0x") {
                            if let Some(hex_part) = line.split(": 0x").nth(1) {
                                if let Some(hex_value) = hex_part.split_whitespace().next() {
                                    if let Ok(warning_value) = u8::from_str_radix(hex_value, 16) {
                                        if warning_value != 0 {
                                            println!("⚠️ [SMART] Critical warning detected: 0x{:02x}", warning_value);
                                            return Ok(false);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                
                // Check temperature (basic threshold check)
                if stdout.contains("Temperature") {
                    for line in stdout.lines() {
                        if line.contains("Temperature") && line.contains("Celsius") {
                            // Extract temperature value - this is a simple heuristic
                            if line.contains("85") || line.contains("90") || line.contains("95") {
                                println!("⚠️ [SMART] High temperature detected: {}", line.trim());
                                // Don't fail on high temp alone, just warn
                            }
                        }
                    }
                }
                
                // Check available spare
                if stdout.contains("Available Spare") {
                    for line in stdout.lines() {
                        if line.contains("Available Spare") && line.contains("%") {
                            // Look for very low spare values
                            if line.contains(" 1%") || line.contains(" 2%") || line.contains(" 0%") {
                                println!("⚠️ [SMART] Low available spare detected: {}", line.trim());
                                return Ok(false);
                            }
                        }
                    }
                }
                
                println!("✅ [SMART] SMART health check passed for: {}", device_path);
                Ok(true)
            } else {
                println!("⚠️ [SMART] nvme smart-log command failed for: {}", device_path);
                Ok(true) // Don't fail health check just because SMART read failed
            }
        }
        Err(e) => {
            println!("ℹ️ [SMART] nvme-cli not available or failed: {}", e);
            Ok(true) // Don't fail health check if nvme-cli is not available
        }
    }
}

/// Check basic device connectivity
async fn check_device_connectivity(device_path: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔌 [CONNECTIVITY] Checking connectivity for: {}", device_path);
    
    // Check if device file exists
    if !std::path::Path::new(device_path).exists() {
        println!("❌ [CONNECTIVITY] Device file does not exist: {}", device_path);
        return Ok(false);
    }
    
    // Try to read device information
    match Command::new("blockdev").args(["--getsize64", device_path]).output() {
        Ok(output) => {
            if output.status.success() {
                let size_str = String::from_utf8_lossy(&output.stdout);
                if let Ok(size) = size_str.trim().parse::<u64>() {
                    if size > 0 {
                        println!("✅ [CONNECTIVITY] Device connectivity verified: {} bytes", size);
                        return Ok(true);
                    }
                }
            }
            println!("⚠️ [CONNECTIVITY] blockdev command failed for: {}", device_path);
        }
        Err(e) => {
            println!("⚠️ [CONNECTIVITY] Failed to run blockdev: {}", e);
        }
    }
    
    // Fallback: try reading sysfs size
    let device_name = device_path.strip_prefix("/dev/").unwrap_or(device_path);
    let size_path = format!("/sys/block/{}/size", device_name);
    
    match tokio::fs::read_to_string(&size_path).await {
        Ok(size_str) => {
            if let Ok(sectors) = size_str.trim().parse::<u64>() {
                if sectors > 0 {
                    println!("✅ [CONNECTIVITY] Device size verified via sysfs: {} sectors", sectors);
                    return Ok(true);
                }
            }
        }
        Err(_) => {
            println!("⚠️ [CONNECTIVITY] Could not read size from sysfs: {}", size_path);
        }
    }
    
    println!("❌ [CONNECTIVITY] Device connectivity check failed: {}", device_path);
    Ok(false)
}

/// Monitor RAID health status
pub async fn monitor_raid_health(agent: &NodeAgent, raid_name: &str) -> Result<RaidHealthStatus, Box<dyn std::error::Error + Send + Sync>> {
    println!("🛡️ [RAID_HEALTH] Monitoring RAID health: {}", raid_name);
    
    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_bdevs"
    })).await?;
    
    if let Some(bdev_list) = bdevs["result"].as_array() {
        for bdev in bdev_list {
            if let Some(name) = bdev["name"].as_str() {
                if name == raid_name {
                    if let Some(driver_specific) = bdev.get("driver_specific") {
                        if let Some(raid_info) = driver_specific.get("raid") {
                            return parse_raid_health_info(raid_info);
                        }
                    }
                }
            }
        }
    }
    
    Err(format!("RAID '{}' not found", raid_name).into())
}

/// RAID health status structure
#[derive(Debug, Clone)]
pub struct RaidHealthStatus {
    pub state: String,
    pub degraded: bool,
    pub rebuilding: bool,
    pub failed_members: Vec<String>,
    pub healthy_members: Vec<String>,
}

/// Parse RAID health information from SPDK response
fn parse_raid_health_info(raid_info: &serde_json::Value) -> Result<RaidHealthStatus, Box<dyn std::error::Error + Send + Sync>> {
    let state = raid_info["state"].as_str().unwrap_or("unknown").to_string();
    let degraded = state != "online";
    let rebuilding = state == "rebuilding";
    
    let mut failed_members = Vec::new();
    let mut healthy_members = Vec::new();
    
    if let Some(base_bdevs) = raid_info["base_bdevs"].as_array() {
        for (i, member) in base_bdevs.iter().enumerate() {
            if let Some(member_name) = member.as_str() {
                // Check member state if available
                if let Some(states) = raid_info["member_states"].as_array() {
                    if let Some(member_state) = states.get(i) {
                        if let Some(state_str) = member_state.as_str() {
                            if state_str == "online" {
                                healthy_members.push(member_name.to_string());
                            } else {
                                failed_members.push(member_name.to_string());
                            }
                            continue;
                        }
                    }
                }
                // Default to healthy if no state info
                healthy_members.push(member_name.to_string());
            }
        }
    }
    
    Ok(RaidHealthStatus {
        state,
        degraded,
        rebuilding,
        failed_members,
        healthy_members,
    })
}

/// Check LVS health and capacity
pub async fn check_lvs_health(agent: &NodeAgent, lvs_name: &str) -> Result<LvsHealthStatus, Box<dyn std::error::Error + Send + Sync>> {
    println!("💾 [LVS_HEALTH] Checking LVS health: {}", lvs_name);
    
    let lvstores = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_get_lvstores"
    })).await?;
    
    if let Some(lvs_list) = lvstores["result"].as_array() {
        for lvs in lvs_list {
            if let Some(name) = lvs["name"].as_str() {
                if name == lvs_name {
                    let total_bytes = lvs["total_data_clusters"].as_u64().unwrap_or(0) * 
                                     lvs["cluster_size"].as_u64().unwrap_or(1);
                    let free_bytes = lvs["free_clusters"].as_u64().unwrap_or(0) * 
                                    lvs["cluster_size"].as_u64().unwrap_or(1);
                    let used_bytes = total_bytes - free_bytes;
                    
                    let usage_percentage = if total_bytes > 0 {
                        (used_bytes as f64 / total_bytes as f64) * 100.0
                    } else {
                        0.0
                    };
                    
                    let health_status = if usage_percentage > 95.0 {
                        "Critical".to_string()
                    } else if usage_percentage > 85.0 {
                        "Warning".to_string()
                    } else {
                        "Healthy".to_string()
                    };
                    
                    return Ok(LvsHealthStatus {
                        name: lvs_name.to_string(),
                        total_bytes,
                        used_bytes,
                        free_bytes,
                        usage_percentage,
                        health_status,
                    });
                }
            }
        }
    }
    
    Err(format!("LVS '{}' not found", lvs_name).into())
}

/// LVS health status structure
#[derive(Debug, Clone)]
pub struct LvsHealthStatus {
    pub name: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub usage_percentage: f64,
    pub health_status: String,
}

/// Comprehensive health check for all components
pub async fn comprehensive_health_check(agent: &NodeAgent) -> Result<SystemHealthStatus, Box<dyn std::error::Error + Send + Sync>> {
    println!("🏥 [SYSTEM_HEALTH] Running comprehensive health check");
    
    let mut healthy_devices = 0;
    let mut unhealthy_devices = 0;
    let mut healthy_raids = 0;
    let mut degraded_raids = 0;
    let mut healthy_lvs = 0;
    let mut warning_lvs = 0;
    
    // Check device health
    let devices = agent.discover_devices_by_persistent_paths().await.unwrap_or_default();
    for device in devices {
        match check_device_health(agent, &device).await {
            Ok(true) => healthy_devices += 1,
            Ok(false) => unhealthy_devices += 1,
            Err(_) => unhealthy_devices += 1,
        }
    }
    
    // Check RAID health
    if let Ok(bdevs) = call_spdk_rpc(&agent.spdk_rpc_url, &json!({"method": "bdev_get_bdevs"})).await {
        if let Some(bdev_list) = bdevs["result"].as_array() {
            for bdev in bdev_list {
                if let Some(driver_specific) = bdev.get("driver_specific") {
                    if driver_specific.get("raid").is_some() {
                        if let Some(name) = bdev["name"].as_str() {
                            match monitor_raid_health(agent, name).await {
                                Ok(status) => {
                                    if status.degraded {
                                        degraded_raids += 1;
                                    } else {
                                        healthy_raids += 1;
                                    }
                                }
                                Err(_) => degraded_raids += 1,
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Check LVS health
    if let Ok(lvstores) = call_spdk_rpc(&agent.spdk_rpc_url, &json!({"method": "bdev_lvol_get_lvstores"})).await {
        if let Some(lvs_list) = lvstores["result"].as_array() {
            for lvs in lvs_list {
                if let Some(name) = lvs["name"].as_str() {
                    match check_lvs_health(agent, name).await {
                        Ok(status) => {
                            if status.health_status == "Healthy" {
                                healthy_lvs += 1;
                            } else {
                                warning_lvs += 1;
                            }
                        }
                        Err(_) => warning_lvs += 1,
                    }
                }
            }
        }
    }
    
    let overall_status = if unhealthy_devices > 0 || degraded_raids > 0 {
        "Critical".to_string()
    } else if warning_lvs > 0 {
        "Warning".to_string()
    } else {
        "Healthy".to_string()
    };
    
    println!("✅ [SYSTEM_HEALTH] Health check completed: {}", overall_status);
    
    Ok(SystemHealthStatus {
        overall_status,
        healthy_devices,
        unhealthy_devices,
        healthy_raids,
        degraded_raids,
        healthy_lvs,
        warning_lvs,
    })
}

/// System health status summary
#[derive(Debug, Clone)]
pub struct SystemHealthStatus {
    pub overall_status: String,
    pub healthy_devices: u32,
    pub unhealthy_devices: u32,
    pub healthy_raids: u32,
    pub degraded_raids: u32,
    pub healthy_lvs: u32,
    pub warning_lvs: u32,
}
