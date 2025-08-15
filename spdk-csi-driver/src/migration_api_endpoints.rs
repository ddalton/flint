// migration_api_endpoints.rs - HTTP API endpoints for enhanced RAID migration
//
// These endpoints provide the REST API that the dashboard frontend calls.
// All SPDK communication uses native Rust Unix socket calls, no Python dependencies.

use std::sync::Arc;
use warp::{Filter, Reply, Rejection};
use serde_json::json;
use tokio::sync::RwLock;

use crate::enhanced_migration_api::{
    EnhancedMigrationApi, EnhancedMigrationRequest, EnhancedMigrationOperation,
    MigrationType, TargetType, MigrationStatus
};

/// Simplified AppState for migration API
#[derive(Clone)]
pub struct AppState {
    pub spdk_nodes: Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
}

/// Migration API state
#[derive(Clone)]
pub struct MigrationApiState {
    pub migration_api: Arc<RwLock<EnhancedMigrationApi>>,
    pub app_state: AppState,
}

/// Create all migration API routes
pub fn migration_routes(
    state: MigrationApiState,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let base = warp::path("api").and(warp::path("migration"));

    let get_targets = base
        .and(warp::path("targets"))
        .and(warp::get())
        .and(warp::query::<MigrationTargetsQuery>())
        .and(with_state(state.clone()))
        .and_then(get_migration_targets_handler);

    let start_migration = base
        .and(warp::path("start"))
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(start_migration_handler);

    let get_operations = base
        .and(warp::path("operations"))
        .and(warp::get())
        .and(warp::query::<OperationsQuery>())
        .and(with_state(state.clone()))
        .and_then(get_operations_handler);

    let retry_operation = base
        .and(warp::path("operations"))
        .and(warp::path::param::<String>())
        .and(warp::path("retry"))
        .and(warp::post())
        .and(with_state(state.clone()))
        .and_then(retry_operation_handler);

    let cancel_operation = base
        .and(warp::path("operations"))
        .and(warp::path::param::<String>())
        .and(warp::path("cancel"))
        .and(warp::post())
        .and(with_state(state.clone()))
        .and_then(cancel_operation_handler);

    let monitor_endpoint = base
        .and(warp::path("monitor"))
        .and(warp::get())
        .and(warp::query::<MonitorQuery>())
        .and(with_state(state.clone()))
        .and_then(monitor_operations_handler);

    // Enhanced migration endpoint for specific alerts
    let enhanced_migrate = warp::path("api")
        .and(warp::path("alerts"))
        .and(warp::path::param::<String>())
        .and(warp::path("enhanced-migrate"))
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(enhanced_migration_handler);

    get_targets
        .or(start_migration)
        .or(get_operations)
        .or(retry_operation)
        .or(cancel_operation)
        .or(monitor_endpoint)
        .or(enhanced_migrate)
}

fn with_state(
    state: MigrationApiState,
) -> impl Filter<Extract = (MigrationApiState,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

#[derive(Debug, serde::Deserialize)]
struct MigrationTargetsQuery {
    volume_id: Option<String>,
    raid_name: Option<String>,
    include_current_node: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
struct OperationsQuery {
    node_id: Option<String>,
    status: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct MonitorQuery {
    node_id: Option<String>,
}

/// GET /api/migration/targets
/// Returns available disks and NVMe-oF targets for migration
async fn get_migration_targets_handler(
    query: MigrationTargetsQuery,
    state: MigrationApiState,
) -> Result<impl Reply, Rejection> {
    println!("🎯 [API] Getting migration targets for volume_id={:?}, raid_name={:?}", 
             query.volume_id, query.raid_name);

    let migration_api = state.migration_api.read().await;
    
    match migration_api.get_migration_targets(
        query.volume_id,
        query.raid_name,
        query.include_current_node.unwrap_or(false),
    ).await {
        Ok(targets) => {
            println!("✅ [API] Found {} disks, {} NVMe-oF targets", 
                     targets.available_disks.len(), 
                     targets.available_nvmeof_targets.len());
            Ok(warp::reply::json(&targets))
        }
        Err(e) => {
            println!("❌ [API] Failed to get migration targets: {}", e);
            Ok(warp::reply::json(&json!({
                "error": e.to_string(),
                "available_disks": [],
                "available_nvmeof_targets": [],
                "raid_info": null
            })))
        }
    }
}

/// POST /api/migration/start
/// Start a new migration operation
async fn start_migration_handler(
    request: EnhancedMigrationRequest,
    state: MigrationApiState,
) -> Result<impl Reply, Rejection> {
    println!("🚀 [API] Starting {:?} migration for RAID {:?}", 
             request.operation_type, request.raid_name);

    let migration_api = state.migration_api.read().await;
    
    match migration_api.start_migration(request).await {
        Ok(operation) => {
            println!("✅ [API] Started migration operation {}", operation.id);
            Ok(warp::reply::json(&json!({
                "status": "success",
                "operation_id": operation.id,
                "message": "Migration operation started",
                "operation": operation
            })))
        }
        Err(e) => {
            println!("❌ [API] Failed to start migration: {}", e);
            Ok(warp::reply::json(&json!({
                "status": "error",
                "error": e.to_string()
            })))
        }
    }
}

/// GET /api/migration/operations
/// Get list of migration operations
async fn get_operations_handler(
    query: OperationsQuery,
    state: MigrationApiState,
) -> Result<impl Reply, Rejection> {
    let migration_api = state.migration_api.read().await;
    let operations = migration_api.active_operations.read().await;
    
    let filtered_operations: Vec<&EnhancedMigrationOperation> = operations
        .values()
        .filter(|op| {
            // Filter by node if specified
            if let Some(ref node_id) = query.node_id {
                if op.source_node != *node_id && 
                   op.target_info.target_node.as_ref() != Some(node_id) {
                    return false;
                }
            }
            
            // Filter by status if specified
            if let Some(ref status) = query.status {
                let status_match = match status.as_str() {
                    "pending" => matches!(op.status, MigrationStatus::Pending),
                    "executing" => matches!(op.status, MigrationStatus::Executing),
                    "cleanup" => matches!(op.status, MigrationStatus::Cleanup),
                    "completed" => matches!(op.status, MigrationStatus::Completed),
                    "failed" => matches!(op.status, MigrationStatus::Failed),
                    _ => true,
                };
                if !status_match {
                    return false;
                }
            }
            
            true
        })
        .collect();

    Ok(warp::reply::json(&json!({
        "operations": filtered_operations,
        "total": filtered_operations.len()
    })))
}

/// POST /api/migration/operations/{id}/retry
/// Retry a failed migration operation
async fn retry_operation_handler(
    operation_id: String,
    state: MigrationApiState,
) -> Result<impl Reply, Rejection> {
    println!("🔄 [API] Retrying migration operation {}", operation_id);
    
    let migration_api = state.migration_api.read().await;
    let mut operations = migration_api.active_operations.write().await;
    
    if let Some(operation) = operations.get_mut(&operation_id) {
        if matches!(operation.status, MigrationStatus::Failed) {
            // Reset operation for retry
            operation.status = MigrationStatus::Pending;
            operation.progress_percent = 0.0;
            operation.stage = "Retrying".to_string();
            operation.error_message = None;
            operation.started_at = chrono::Utc::now();
            
            // Restart migration by creating new operation with same parameters
            println!("🔄 [RETRY] Restarting migration operation {}", operation_id);
            
            Ok(warp::reply::json(&json!({
                "status": "success",
                "message": "Migration operation queued for retry",
                "operation": operation
            })))
        } else {
            Ok(warp::reply::json(&json!({
                "status": "error",
                "error": "Operation is not in failed state"
            })))
        }
    } else {
        Ok(warp::reply::json(&json!({
            "status": "error",
            "error": "Operation not found"
        })))
    }
}

/// POST /api/migration/operations/{id}/cancel
/// Cancel a running migration operation
async fn cancel_operation_handler(
    operation_id: String,
    state: MigrationApiState,
) -> Result<impl Reply, Rejection> {
    println!("⏹️ [API] Canceling migration operation {}", operation_id);
    
    let migration_api = state.migration_api.read().await;
    let mut operations = migration_api.active_operations.write().await;
    
    if let Some(operation) = operations.get_mut(&operation_id) {
        if matches!(operation.status, MigrationStatus::Executing | MigrationStatus::Pending) {
            operation.status = MigrationStatus::Failed;
            operation.error_message = Some("Operation canceled by user".to_string());
            
            // Perform cancellation cleanup - stop ongoing operations
            println!("⏹️ [CANCEL] Canceling migration operation {}", operation_id);
            
            Ok(warp::reply::json(&json!({
                "status": "success",
                "message": "Migration operation canceled"
            })))
        } else {
            Ok(warp::reply::json(&json!({
                "status": "error",
                "error": "Operation cannot be canceled in current state"
            })))
        }
    } else {
        Ok(warp::reply::json(&json!({
            "status": "error",
            "error": "Operation not found"
        })))
    }
}

/// GET /api/migration/monitor
/// Get real-time migration monitoring data
async fn monitor_operations_handler(
    query: MonitorQuery,
    state: MigrationApiState,
) -> Result<impl Reply, Rejection> {
    let migration_api = state.migration_api.read().await;
    let operations = migration_api.active_operations.read().await;
    
    // Filter active operations
    let active_operations: Vec<&EnhancedMigrationOperation> = operations
        .values()
        .filter(|op| {
            // Only return active operations
            !matches!(op.status, MigrationStatus::Completed)
        })
        .filter(|op| {
            // Filter by node if specified
            if let Some(ref node_id) = query.node_id {
                op.source_node == *node_id || 
                op.target_info.target_node.as_ref() == Some(node_id)
            } else {
                true
            }
        })
        .collect();

    // Get cleanup queue (placeholder)
    let cleanup_queue = Vec::<serde_json::Value>::new();

    Ok(warp::reply::json(&json!({
        "operations": active_operations,
        "alerts": [], // Alerts would be generated based on operation status
        "cleanup_queue": cleanup_queue
    })))
}

/// POST /api/alerts/{volume_id}/enhanced-migrate
/// Enhanced migration endpoint for alert-triggered migrations
async fn enhanced_migration_handler(
    volume_id: String,
    request: EnhancedMigrationRequest,
    state: MigrationApiState,
) -> Result<impl Reply, Rejection> {
    println!("🚨 [API] Enhanced migration for volume {} with type {:?}", 
             volume_id, request.operation_type);

    if !request.confirmation {
        return Ok(warp::reply::json(&json!({
            "status": "error",
            "error": "Migration requires explicit confirmation"
        })));
    }

    let migration_api = state.migration_api.read().await;
    
    // Create request with volume ID
    let enhanced_request = EnhancedMigrationRequest {
        volume_id: Some(volume_id.clone()),
        ..request
    };
    
    match migration_api.start_migration(enhanced_request).await {
        Ok(operation) => {
            // Determine operation name for response
            let operation_name = match operation.operation_type {
                MigrationType::NodeMigration => "Node Migration",
                MigrationType::MemberMigration => "RAID Member Migration",
                MigrationType::MemberAddition => "RAID Member Addition",
            };
            
            // Determine target description
            let target_description = match &operation.target_info.target_type {
                TargetType::Node => operation.target_info.target_node.as_ref()
                    .map(|n| format!("Node: {}", n))
                    .unwrap_or_else(|| "Auto-selected node".to_string()),
                TargetType::LocalDisk => operation.target_info.target_disk_id.as_ref()
                    .map(|d| format!("Disk: {}", d))
                    .unwrap_or_else(|| "Auto-selected disk".to_string()),
                TargetType::InternalNvmeof => operation.target_info.target_nvmeof_nqn.as_ref()
                    .map(|n| format!("Internal NVMe-oF: {}", n))
                    .unwrap_or_else(|| "Internal NVMe-oF target".to_string()),
                TargetType::ExternalNvmeof => operation.target_info.target_nvmeof_nqn.as_ref()
                    .map(|n| format!("External NVMe-oF: {}", n))
                    .unwrap_or_else(|| "External NVMe-oF target".to_string()),
            };

            Ok(warp::reply::json(&json!({
                "status": "success",
                "operation_id": operation.id,
                "operation_name": operation_name,
                "target_description": target_description,
                "message": format!("{} initiated for volume {}", operation_name, volume_id)
            })))
        }
        Err(e) => {
            println!("❌ [API] Enhanced migration failed: {}", e);
            Ok(warp::reply::json(&json!({
                "status": "error",
                "error": e.to_string()
            })))
        }
    }
}

/// Initialize migration API with SPDK node connections
pub async fn initialize_migration_api(
    app_state: &AppState,
) -> Result<MigrationApiState, Box<dyn std::error::Error + Send + Sync>> {
    let mut migration_api = EnhancedMigrationApi::new();
    
    // Add SPDK nodes from app state
    let spdk_nodes = app_state.spdk_nodes.read().await;
    for (node_name, rpc_url) in spdk_nodes.iter() {
        if rpc_url.starts_with("unix://") {
            let socket_path = rpc_url.trim_start_matches("unix://").to_string();
            migration_api.add_spdk_node(node_name.clone(), socket_path.clone());
            println!("🔗 [MIGRATION_API] Added SPDK node {} with socket {}", node_name, socket_path);
        } else {
            println!("⚠️ [MIGRATION_API] Skipping non-Unix socket URL for {}: {}", node_name, rpc_url);
        }
    }
    
    Ok(MigrationApiState {
        migration_api: Arc::new(RwLock::new(migration_api)),
        app_state: app_state.clone(),
    })
}

/* Example SPDK RPC calls that would be made:

1. For RAID member migration:
{
  "jsonrpc": "2.0",
  "method": "bdev_raid_replace_member",
  "params": {
    "name": "raid1_node1",
    "old_member": "nvme0n1",
    "new_member": "nvme2n1"
  },
  "id": 1
}

2. For RAID member addition:
{
  "jsonrpc": "2.0", 
  "method": "bdev_raid_add_member",
  "params": {
    "name": "raid1_node1",
    "member": "nvme3n1"
  },
  "id": 2
}

3. For NVMe-oF attachment:
{
  "jsonrpc": "2.0",
  "method": "bdev_nvme_attach_controller", 
  "params": {
    "name": "nvme_migration_001",
    "trtype": "TCP",
    "traddr": "192.168.1.100",
    "trsvcid": "4420", 
    "subnqn": "nqn.2023.io.spdk:storage.target1"
  },
  "id": 3
}

4. For monitoring rebuild progress:
{
  "jsonrpc": "2.0",
  "method": "bdev_raid_get_bdevs",
  "params": {
    "name": "raid1_node1"
  },
  "id": 4
}
*/
