//! HTTP endpoints for snapshot operations
//! 
//! These routes are registered separately in the node agent and don't interfere
//! with existing volume management routes. All endpoints follow REST conventions.

use warp::{Filter, Rejection, Reply};
use std::sync::Arc;
use std::collections::HashMap;
use super::SnapshotService;
use super::snapshot_models::*;

/// Register all snapshot routes - called once during node agent startup
/// 
/// Creates the following endpoints:
/// - POST /api/snapshots/create - Create a snapshot
/// - POST /api/snapshots/delete - Delete a snapshot
/// - POST /api/snapshots/clone  - Clone a snapshot to new volume
/// - GET  /api/snapshots/list   - List all snapshots
/// - GET  /api/snapshots/get_info?snapshot_uuid=xxx - Get snapshot info
pub fn register_snapshot_routes(
    snapshot_service: Arc<SnapshotService>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    create_snapshot_route(snapshot_service.clone())
        .or(delete_snapshot_route(snapshot_service.clone()))
        .or(clone_snapshot_route(snapshot_service.clone()))
        .or(list_snapshots_route(snapshot_service.clone()))
        .or(get_snapshot_info_route(snapshot_service))
}

/// POST /api/snapshots/create
fn create_snapshot_route(
    service: Arc<SnapshotService>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    warp::path!("api" / "snapshots" / "create")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_service(service))
        .and_then(handle_create_snapshot)
}

/// POST /api/snapshots/delete
fn delete_snapshot_route(
    service: Arc<SnapshotService>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    warp::path!("api" / "snapshots" / "delete")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_service(service))
        .and_then(handle_delete_snapshot)
}

/// POST /api/snapshots/clone
fn clone_snapshot_route(
    service: Arc<SnapshotService>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    warp::path!("api" / "snapshots" / "clone")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_service(service))
        .and_then(handle_clone_snapshot)
}

/// GET /api/snapshots/list
fn list_snapshots_route(
    service: Arc<SnapshotService>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    warp::path!("api" / "snapshots" / "list")
        .and(warp::get())
        .and(with_service(service))
        .and_then(handle_list_snapshots)
}

/// GET /api/snapshots/get_info?snapshot_uuid=xxx
fn get_snapshot_info_route(
    service: Arc<SnapshotService>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    warp::path!("api" / "snapshots" / "get_info")
        .and(warp::get())
        .and(warp::query::<HashMap<String, String>>())
        .and(with_service(service))
        .and_then(handle_get_snapshot_info)
}

/// Helper to inject service into handlers
fn with_service(
    service: Arc<SnapshotService>,
) -> impl Filter<Extract = (Arc<SnapshotService>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || service.clone())
}

// === HTTP Handler Implementations ===

/// Handle POST /api/snapshots/create
async fn handle_create_snapshot(
    req: CreateSnapshotRequest,
    service: Arc<SnapshotService>,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [SNAPSHOT_ROUTES] POST /api/snapshots/create: {}", req.snapshot_name);

    match service.create_snapshot(&req.lvol_name, &req.snapshot_name).await {
        Ok(response) => {
            println!("✅ [SNAPSHOT_ROUTES] Snapshot created: {}", response.snapshot_uuid);
            Ok(warp::reply::with_status(
                warp::reply::json(&response),
                warp::http::StatusCode::OK,
            ))
        }
        Err(e) => {
            println!("❌ [SNAPSHOT_ROUTES] Failed to create snapshot: {}", e);
            let error_response = serde_json::json!({
                "error": format!("Failed to create snapshot: {}", e),
                "snapshot_name": req.snapshot_name,
            });
            Ok(warp::reply::with_status(
                warp::reply::json(&error_response),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

/// Handle POST /api/snapshots/delete
async fn handle_delete_snapshot(
    req: DeleteSnapshotRequest,
    service: Arc<SnapshotService>,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [SNAPSHOT_ROUTES] POST /api/snapshots/delete: {}", req.snapshot_uuid);

    match service.delete_snapshot(&req.snapshot_uuid).await {
        Ok(response) => {
            println!("✅ [SNAPSHOT_ROUTES] Snapshot deleted: {}", req.snapshot_uuid);
            Ok(warp::reply::with_status(
                warp::reply::json(&response),
                warp::http::StatusCode::OK,
            ))
        }
        Err(e) => {
            // For delete, not found is OK (idempotent)
            if e.to_string().contains("not found") {
                println!("ℹ️ [SNAPSHOT_ROUTES] Snapshot not found (already deleted): {}", req.snapshot_uuid);
                let response = DeleteSnapshotResponse {
                    success: true,
                    message: Some("Snapshot not found (already deleted)".to_string()),
                };
                Ok(warp::reply::with_status(
                    warp::reply::json(&response),
                    warp::http::StatusCode::OK,
                ))
            } else {
                println!("❌ [SNAPSHOT_ROUTES] Failed to delete snapshot: {}", e);
                let error_response = serde_json::json!({
                    "error": format!("Failed to delete snapshot: {}", e),
                    "snapshot_uuid": req.snapshot_uuid,
                });
                Ok(warp::reply::with_status(
                    warp::reply::json(&error_response),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                ))
            }
        }
    }
}

/// Handle POST /api/snapshots/clone
async fn handle_clone_snapshot(
    req: CloneSnapshotRequest,
    service: Arc<SnapshotService>,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [SNAPSHOT_ROUTES] POST /api/snapshots/clone: {} -> {}", 
             req.snapshot_uuid, req.clone_name);

    match service.clone_snapshot(&req.snapshot_uuid, &req.clone_name).await {
        Ok(response) => {
            println!("✅ [SNAPSHOT_ROUTES] Clone created: {}", response.clone_uuid);
            Ok(warp::reply::with_status(
                warp::reply::json(&response),
                warp::http::StatusCode::OK,
            ))
        }
        Err(e) => {
            println!("❌ [SNAPSHOT_ROUTES] Failed to clone snapshot: {}", e);
            let error_response = serde_json::json!({
                "error": format!("Failed to clone snapshot: {}", e),
                "snapshot_uuid": req.snapshot_uuid,
                "clone_name": req.clone_name,
            });
            Ok(warp::reply::with_status(
                warp::reply::json(&error_response),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

/// Handle GET /api/snapshots/list
async fn handle_list_snapshots(
    service: Arc<SnapshotService>,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [SNAPSHOT_ROUTES] GET /api/snapshots/list");

    match service.list_snapshots().await {
        Ok(snapshots) => {
            println!("✅ [SNAPSHOT_ROUTES] Listed {} snapshots", snapshots.len());
            let response = ListSnapshotsResponse { snapshots };
            Ok(warp::reply::with_status(
                warp::reply::json(&response),
                warp::http::StatusCode::OK,
            ))
        }
        Err(e) => {
            println!("❌ [SNAPSHOT_ROUTES] Failed to list snapshots: {}", e);
            let error_response = serde_json::json!({
                "error": format!("Failed to list snapshots: {}", e),
            });
            Ok(warp::reply::with_status(
                warp::reply::json(&error_response),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

/// Handle GET /api/snapshots/get_info?snapshot_uuid=xxx
async fn handle_get_snapshot_info(
    params: HashMap<String, String>,
    service: Arc<SnapshotService>,
) -> Result<impl Reply, Rejection> {
    let snapshot_uuid = params.get("snapshot_uuid")
        .map(|s| s.as_str())
        .unwrap_or("");

    println!("🌐 [SNAPSHOT_ROUTES] GET /api/snapshots/get_info?snapshot_uuid={}", snapshot_uuid);

    if snapshot_uuid.is_empty() {
        let error_response = serde_json::json!({
            "error": "Missing snapshot_uuid parameter",
        });
        return Ok(warp::reply::with_status(
            warp::reply::json(&error_response),
            warp::http::StatusCode::BAD_REQUEST,
        ));
    }

    match service.find_snapshot(snapshot_uuid).await {
        Ok(Some(snapshot)) => {
            println!("✅ [SNAPSHOT_ROUTES] Found snapshot: {}", snapshot_uuid);
            Ok(warp::reply::with_status(
                warp::reply::json(&snapshot),
                warp::http::StatusCode::OK,
            ))
        }
        Ok(None) => {
            println!("ℹ️ [SNAPSHOT_ROUTES] Snapshot not found: {}", snapshot_uuid);
            let error_response = serde_json::json!({
                "error": "Snapshot not found",
                "snapshot_uuid": snapshot_uuid,
            });
            Ok(warp::reply::with_status(
                warp::reply::json(&error_response),
                warp::http::StatusCode::NOT_FOUND,
            ))
        }
        Err(e) => {
            println!("❌ [SNAPSHOT_ROUTES] Error querying snapshot: {}", e);
            let error_response = serde_json::json!({
                "error": format!("Failed to query snapshot: {}", e),
                "snapshot_uuid": snapshot_uuid,
            });
            Ok(warp::reply::with_status(
                warp::reply::json(&error_response),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_routes_registration() {
        // Test that all routes can be registered without panic
        let service = Arc::new(SnapshotService::new(
            "test-node".to_string(),
            "/tmp/spdk.sock".to_string(),
        ));
        
        let _routes = register_snapshot_routes(service);
        // If we get here without panic, routes registered successfully
    }
}

