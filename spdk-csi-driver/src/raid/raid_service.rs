// raid/raid_service.rs - RAID creation and management service

use serde_json::{json, Value};
use crate::minimal_models::MinimalStateError;
use crate::raid::raid_models::RaidHealthStatus;

/// Create RAID 1 bdev from base bdevs
pub async fn create_raid1_bdev(
    node_name: &str,
    raid_name: &str,
    base_bdevs: Vec<String>,
    rpc_call: impl Fn(&str, Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, Box<dyn std::error::Error + Send + Sync>>> + Send>>,
) -> Result<String, MinimalStateError> {
    tracing::info!("[RAID] Creating RAID 1 bdev: {} with {} base bdevs on node: {}",
             raid_name, base_bdevs.len(), node_name);

    if base_bdevs.len() < 2 {
        return Err(MinimalStateError::InternalError {
            message: format!("RAID 1 requires minimum 2 base bdevs, got {}", base_bdevs.len())
        });
    }

    let payload = json!({
        "method": "bdev_raid_create",
        "params": {
            "name": raid_name,
            "raid_level": "1",
            "base_bdevs": base_bdevs,
            "superblock": true,
        }
    });

    rpc_call(node_name, payload)
        .await
        .map_err(|e| MinimalStateError::SpdkRpcError {
            message: format!("Failed to create RAID bdev: {}", e)
        })?;

    tracing::info!("[RAID] RAID 1 bdev created: {}", raid_name);
    Ok(raid_name.to_string())
}

/// Delete RAID bdev
pub async fn delete_raid_bdev(
    node_name: &str,
    raid_name: &str,
    rpc_call: impl Fn(&str, Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, Box<dyn std::error::Error + Send + Sync>>> + Send>>,
) -> Result<(), MinimalStateError> {
    tracing::info!("[RAID] Deleting RAID bdev: {} on node: {}", raid_name, node_name);

    let payload = json!({
        "method": "bdev_raid_delete",
        "params": {
            "name": raid_name
        }
    });

    rpc_call(node_name, payload)
        .await
        .map_err(|e| MinimalStateError::SpdkRpcError {
            message: format!("Failed to delete RAID bdev: {}", e)
        })?;

    tracing::info!("[RAID] RAID bdev deleted: {}", raid_name);
    Ok(())
}

/// Get RAID bdev status
pub async fn get_raid_status(
    node_name: &str,
    raid_name: &str,
    rpc_call: impl Fn(&str, Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, Box<dyn std::error::Error + Send + Sync>>> + Send>>,
) -> Result<RaidHealthStatus, MinimalStateError> {
    let payload = json!({
        "method": "bdev_raid_get_bdevs",
        "params": {
            "category": "all"
        }
    });

    let response = rpc_call(node_name, payload)
        .await
        .map_err(|e| MinimalStateError::SpdkRpcError {
            message: format!("Failed to get RAID status: {}", e)
        })?;

    // Parse response to find our RAID bdev
    if let Some(raids) = response.as_array() {
        for raid in raids {
            if raid["name"].as_str() == Some(raid_name) {
                let state = raid["state"].as_str().unwrap_or("unknown");
                let base_bdevs = raid["base_bdevs_list"].as_array()
                    .map(|arr| arr.len() as u32)
                    .unwrap_or(0);
                
                let status = match state {
                    "online" => "online",
                    "degraded" => "degraded",
                    _ => "failed",
                };

                return Ok(RaidHealthStatus {
                    raid_name: raid_name.to_string(),
                    status: status.to_string(),
                    total_replicas: base_bdevs,
                    online_replicas: base_bdevs, // TODO: Calculate properly
                    failed_replicas: vec![],
                });
            }
        }
    }

    Err(MinimalStateError::InternalError {
        message: format!("RAID bdev not found: {}", raid_name)
    })
}

/// Add base bdev to existing RAID (for rebuild)
pub async fn add_base_bdev_to_raid(
    node_name: &str,
    raid_name: &str,
    base_bdev: &str,
    rpc_call: impl Fn(&str, Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, Box<dyn std::error::Error + Send + Sync>>> + Send>>,
) -> Result<(), MinimalStateError> {
    tracing::info!("[RAID] Adding base bdev {} to RAID {} on node: {}",
             base_bdev, raid_name, node_name);

    let payload = json!({
        "method": "bdev_raid_add_base_bdev",
        "params": {
            "raid_bdev": raid_name,
            "base_bdev": base_bdev
        }
    });

    rpc_call(node_name, payload)
        .await
        .map_err(|e| MinimalStateError::SpdkRpcError {
            message: format!("Failed to add base bdev to RAID: {}", e)
        })?;

    tracing::info!("[RAID] Base bdev added to RAID: {}", base_bdev);
    Ok(())
}

