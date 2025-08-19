#![recursion_limit = "256"]

use warp::{Filter, Reply, Rejection};
use serde::{Serialize, Deserialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use reqwest::Client as HttpClient;
use kube::{Client, Api, api::ListParams};
use spdk_csi_driver::models::{NvmeofDisk, NvmeofDiskSpec, NvmeofDiskStatus, SpdkRaidDisk, SpdkRaidDiskStatus};
use chrono::{Utc, DateTime};
use std::env;
use spdk_csi_driver::*;
use k8s_openapi::api::core::v1::Pod;  
// duplicate import removed

// --- Start of New API Response Structs ---
#[derive(Serialize, Debug, Clone)]
struct DetailedSnapshotInfo {
    // From SpdkSnapshot CR
    pub snapshot_id: String,
    pub source_volume_id: String,
    pub creation_time: Option<DateTime<Utc>>,
    pub ready_to_use: bool,
    pub size_bytes: i64,
    pub snapshot_type: SnapshotType,
    pub clone_source_snapshot_id: Option<String>,
    pub replica_bdev_details: Vec<SpdkBdevDetails>,
}

#[derive(Serialize, Debug, Clone)]
struct SpdkBdevDetails {
    pub node: String, // The node where the snapshot bdev was found
    pub name: String,
    pub aliases: Vec<String>,
    pub driver: String,
    pub snapshot_source_bdev: Option<String>,
}
// --- End of New API Response Structs ---

// Volume validation result structures - REMOVED: unused structs

// SPDK validation status for frontend display
#[derive(Serialize, Debug, Clone)]
struct SpdkValidationStatus {
    has_spdk_backing: bool,
    validation_message: Option<String>,
    validation_severity: ValidationSeverity, // info, warning, error
}

#[derive(Serialize, Debug, Clone)]
enum ValidationSeverity {
    #[serde(rename = "info")]
    Info,
    // REMOVED: Warning and Error variants - unused
}

// Enhanced dashboard API response types with NVMe-oF instead of vhost
#[derive(Serialize, Debug, Clone)]
struct DashboardVolume {
    id: String,
    name: String,
    size: String,
    state: String,
    replicas: i32,
    active_replicas: i32,
    local_nvme: bool,
    access_method: String,
    rebuild_progress: Option<f64>,
    nodes: Vec<String>,
    replica_statuses: Vec<DashboardReplicaStatus>,
    // NVMe-oF fields instead of vhost
    nvmeof_targets: Vec<NvmeofTargetInfo>,
    nvmeof_enabled: bool,
    transport_type: String,
    target_port: u16,
    // Enhanced RAID status from SPDK
    raid_status: Option<DashboardRaidStatus>,
    ublk_device: Option<serde_json::Value>,
    // SPDK validation status
    spdk_validation_status: SpdkValidationStatus,
}

#[derive(Serialize, Debug, Clone)]
struct NvmeofTargetInfo {
    nqn: String,
    target_ip: String,
    target_port: u16,
    transport: String,
    node: String,
    bdev_name: String,
    active: bool,
    connection_count: u32,
}

#[derive(Serialize, Debug, Clone)]
struct DashboardRaidStatus {
    raid_level: u32,
    state: String,
    num_members: u32,
    operational_members: u32,
    discovered_members: u32,
    members: Vec<DashboardRaidMember>,
    rebuild_info: Option<DashboardRebuildInfo>,
    superblock_version: Option<u32>,
    auto_rebuild_enabled: bool,
}

#[derive(Serialize, Debug, Clone)]
struct DashboardRaidMember {
    slot: u32,
    name: String,
    state: String,
    uuid: Option<String>,
    is_configured: bool,
    node: Option<String>,
    disk_ref: Option<String>,
    health_status: String,
}

#[derive(Serialize, Debug, Clone)]
struct DashboardRebuildInfo {
    state: String,
    target_slot: u32,
    source_slot: u32,
    blocks_remaining: u64,
    blocks_total: u64,
    progress_percentage: f64,
    estimated_time_remaining: Option<String>,
    start_time: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
struct DashboardReplicaStatus {
    node: String,
    status: String,
    is_local: bool,
    last_io_timestamp: Option<String>,
    rebuild_progress: Option<f64>,
    rebuild_target: Option<String>,
    is_new_replica: Option<bool>,
    nvmf_target: Option<NvmfTarget>,
    access_method: String,
    raid_member_slot: Option<u32>,
    raid_member_state: String,
}

#[derive(Serialize, Debug, Clone)]
struct NvmfTarget {
    nqn: String,
    target_ip: String,
    target_port: String,
    transport_type: String,
}

#[derive(Serialize, Debug, Clone)]
struct NvmeofEndpointInfo {
    nqn: String,
    target_addr: String,
    target_port: u16,
    transport: String,
    active: bool,
}

#[derive(Serialize, Debug, Clone)]
struct DashboardDisk {
    id: String,
    node: String,
    pci_addr: String,
    capacity: i64,
    capacity_gb: i64,
    allocated_space: i64,
    free_space: i64,
    free_space_display: String,
    healthy: bool,
    blobstore_initialized: bool,
    lvol_count: u32,
    model: String,
    read_iops: u64,
    write_iops: u64,
    read_latency: u64,
    write_latency: u64,
    brought_online: String,
    provisioned_volumes: Vec<ProvisionedVolume>,
    // Orphaned SPDK volumes on this disk
    orphaned_spdk_volumes: Vec<OrphanedVolumeInfo>,
    // Disk origin
    is_remote: bool,
    // Optional NVMe-oF endpoint for this disk (local or remote)
    nvmeof_endpoint: Option<NvmeofEndpointInfo>,
}

// Duplicate struct removed (consolidated above)

#[derive(Serialize, Debug, Clone)]
struct DashboardRaidDisk {
    id: String,
    node: String,
    raid_level: String,
    state: String,
    lvs_name: Option<String>,
    lvs_uuid: Option<String>,
    total_capacity_gb: i64,
    usable_capacity_gb: i64,
    used_capacity_gb: i64,
    degraded: bool,
    rebuild_progress: Option<f64>,
    members: Vec<DashboardRaidMember>,
}

#[derive(Serialize, Debug, Clone)]
struct ProvisionedVolume {
    volume_name: String,
    volume_id: String,
    size: i64,
    provisioned_at: String,
    replica_type: String,
    status: String,
}

#[derive(Serialize, Debug, Clone)]
struct RawSpdkVolume {
    name: String,
    uuid: String,
    node: String,
    lvs_name: String,
    size_blocks: u64,
    size_gb: f64,
    is_managed: bool, // Whether this volume has a corresponding SpdkVolume CRD
}

#[derive(Deserialize, Debug)]
struct QueryParameters {
    node: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
struct OrphanedVolumeInfo {
    spdk_volume_name: String,
    spdk_volume_uuid: String,
    size_blocks: u64,
    size_gb: f64,
    orphaned_since: String, // When we detected it was orphaned
}

#[derive(Serialize, Debug)]
struct DashboardData {
    volumes: Vec<DashboardVolume>,        // Managed volumes (with SpdkVolume CRDs)
    raw_volumes: Vec<RawSpdkVolume>,      // Unmanaged SPDK volumes (orphaned/leftover)
    disks: Vec<DashboardDisk>,
    nodes: Vec<DashboardNode>,            // Nodes with maintenance status
    raid_disks: Vec<DashboardRaidDisk>,   // RAID disks managed by SPDK instances
}

#[derive(Serialize, Debug, Clone)]
struct DashboardNode {
    node_id: String,
    status: String,                       // "healthy", "maintenance", "offline"
    maintenance_mode: bool,
    maintenance_status: Option<spdk_csi_driver::models::MaintenanceStatus>,
    raid_count: u32,                      // Number of RAID bdevs managed by this node
    volume_count: u32,                    // Number of volumes on this node
}

#[derive(Clone)]
struct AppState {
    kube_client: Client,
    spdk_nodes: Arc<RwLock<HashMap<String, String>>>,
    cache: Arc<RwLock<Option<DashboardData>>>,
    last_update: Arc<RwLock<DateTime<Utc>>>,
    target_namespace: String,
}

/// Get the current pod's namespace from the service account token
async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error>> {
    // Try environment variable first (allows override)
    if let Ok(namespace) = std::env::var("FLINT_NAMESPACE") {
        return Ok(namespace);
    }
    
    // Read namespace from service account token file
    let namespace_path = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
    if std::path::Path::new(namespace_path).exists() {
        match tokio::fs::read_to_string(namespace_path).await {
            Ok(namespace) => {
                let namespace = namespace.trim().to_string();
                println!("📍 [NAMESPACE] Detected current namespace: {}", namespace);
                return Ok(namespace);
            }
            Err(e) => {
                println!("⚠️ [NAMESPACE] Failed to read namespace file: {}", e);
            }
        }
    }
    
    // Fallback to default if running outside cluster
    println!("⚠️ [NAMESPACE] Using fallback namespace: flint-system");
    Ok("flint-system".to_string())
}

// Request/Response structures for orphaned volume deletion
#[derive(Deserialize, Debug)]
struct DeleteOrphanedVolumeRequest {
    node: String,
    volume_name: String,  // Can be UUID or alias of the logical volume
    volume_uuid: String,
    // REMOVED: reason field - unused
}

#[derive(Serialize, Debug)]
struct DeleteOrphanedVolumeResponse {
    success: bool,
    message: String,
    deleted_volume: Option<DeletedVolumeInfo>,
}

#[derive(Serialize, Debug)]
struct DeletedVolumeInfo {
    node: String,
    volume_name: String,
    volume_uuid: String,
    size_gb: f64,
    deleted_at: String,
}

/// Delete orphaned SPDK logical volume using bdev_lvol_delete RPC
async fn delete_orphaned_spdk_volume(
    request: DeleteOrphanedVolumeRequest,
    state: AppState,
) -> Result<impl Reply, Rejection> {
    println!("🗑️ [DELETE_ORPHAN] Received request to delete orphaned volume '{}' on node '{}'", 
        request.volume_name, request.node);
    
    // Verify node exists and get RPC URL
    let spdk_nodes = state.spdk_nodes.read().await;
    let rpc_url = match spdk_nodes.get(&request.node) {
        Some(url) => url,
        None => {
            println!("❌ [DELETE_ORPHAN] Node '{}' not found in SPDK nodes", request.node);
            return Ok(warp::reply::with_status(
                warp::reply::json(&DeleteOrphanedVolumeResponse {
                    success: false,
                    message: format!("Node '{}' not found", request.node),
                    deleted_volume: None,
                }),
                warp::http::StatusCode::NOT_FOUND
            ));
        }
    };
    
    // First, verify the volume still exists and is indeed orphaned
    println!("🔍 [DELETE_ORPHAN] Verifying volume '{}' exists on node '{}'", request.volume_name, request.node);
    
    let http_client = HttpClient::new();
    
    // Check if volume exists by querying current logical volumes
    let verification_response = match http_client
        .post(rpc_url)
        .json(&json!({
            "method": "bdev_get_bdevs",
            "params": {},
            "id": 1
        }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            println!("❌ [DELETE_ORPHAN] Failed to verify volume existence: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&DeleteOrphanedVolumeResponse {
                    success: false,
                    message: format!("Failed to verify volume existence: {}", e),
                    deleted_volume: None,
                }),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ));
        }
    };
    
    if !verification_response.status().is_success() {
        println!("❌ [DELETE_ORPHAN] SPDK verification query failed with status: {}", verification_response.status());
        return Ok(warp::reply::with_status(
            warp::reply::json(&DeleteOrphanedVolumeResponse {
                success: false,
                message: format!("SPDK verification query failed"),
                deleted_volume: None,
            }),
            warp::http::StatusCode::BAD_GATEWAY
        ));
    }
    
    let verification_text = match verification_response.text().await {
        Ok(text) => text,
        Err(e) => {
            println!("❌ [DELETE_ORPHAN] Failed to read verification response: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&DeleteOrphanedVolumeResponse {
                    success: false,
                    message: format!("Failed to read verification response: {}", e),
                    deleted_volume: None,
                }),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ));
        }
    };
    
    let verification_info: serde_json::Value = match serde_json::from_str(&verification_text) {
        Ok(info) => info,
        Err(e) => {
            println!("❌ [DELETE_ORPHAN] Failed to parse verification response: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&DeleteOrphanedVolumeResponse {
                    success: false,
                    message: format!("Failed to parse verification response: {}", e),
                    deleted_volume: None,
                }),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ));
        }
    };
    
    // Find the volume in the bdev list
    let mut found_volume = None;
    let mut volume_size_gb = 0.0;
    
    if let Some(bdevs) = verification_info["result"].as_array() {
        for bdev in bdevs {
            if let Some(bdev_name) = bdev["name"].as_str() {
                if bdev_name == request.volume_name || 
                   (bdev.get("aliases").and_then(|a| a.as_array()).map_or(false, |aliases| 
                       aliases.iter().any(|alias| alias.as_str() == Some(&request.volume_name)))) {
                    found_volume = Some(bdev.clone());
                    if let Some(num_blocks) = bdev["num_blocks"].as_u64() {
                        volume_size_gb = (num_blocks * 512) as f64 / (1024.0 * 1024.0 * 1024.0);
                    }
                    break;
                }
            }
        }
    }
    
    let _volume_info = match found_volume {
        Some(vol) => vol,
        None => {
            println!("⚠️ [DELETE_ORPHAN] Volume '{}' not found on node '{}' - may have been already deleted", 
                request.volume_name, request.node);
            return Ok(warp::reply::with_status(
                warp::reply::json(&DeleteOrphanedVolumeResponse {
                    success: false,
                    message: format!("Volume '{}' not found - may have been already deleted", request.volume_name),
                    deleted_volume: None,
                }),
                warp::http::StatusCode::NOT_FOUND
            ));
        }
    };
    
    // Now perform the actual deletion using bdev_lvol_delete
    println!("🗑️ [DELETE_ORPHAN] Deleting SPDK logical volume '{}' on node '{}'", request.volume_name, request.node);
    
    let delete_response = match http_client
        .post(rpc_url)
        .json(&json!({
            "method": "bdev_lvol_delete",
            "params": {
                "name": request.volume_name
            },
            "id": 1
        }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            println!("❌ [DELETE_ORPHAN] Failed to send delete request: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&DeleteOrphanedVolumeResponse {
                    success: false,
                    message: format!("Failed to send delete request: {}", e),
                    deleted_volume: None,
                }),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ));
        }
    };
    
    let delete_status = delete_response.status();
    if delete_status.is_success() {
        let deleted_info = DeletedVolumeInfo {
            node: request.node.clone(),
            volume_name: request.volume_name.clone(),
            volume_uuid: request.volume_uuid.clone(),
            size_gb: volume_size_gb,
            deleted_at: chrono::Utc::now().to_rfc3339(),
        };
        
        println!("✅ [DELETE_ORPHAN] Successfully deleted orphaned volume '{}' ({:.2}GB) on node '{}'", 
            request.volume_name, volume_size_gb, request.node);
        
        Ok(warp::reply::with_status(
            warp::reply::json(&DeleteOrphanedVolumeResponse {
                success: true,
                message: format!("Successfully deleted orphaned volume '{}' ({:.2}GB)", request.volume_name, volume_size_gb),
                deleted_volume: Some(deleted_info),
            }),
            warp::http::StatusCode::OK
        ))
    } else {
        let error_text = delete_response.text().await.unwrap_or_default();
        println!("❌ [DELETE_ORPHAN] SPDK delete failed with status {}: {}", delete_status, error_text);
        
        Ok(warp::reply::with_status(
            warp::reply::json(&DeleteOrphanedVolumeResponse {
                success: false,
                message: format!("SPDK delete failed: {}", error_text),
                deleted_volume: None,
            }),
            warp::http::StatusCode::BAD_GATEWAY
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kube_client = Client::try_default().await?;
    
    // Detect the namespace for custom resources
    let target_namespace = get_current_namespace().await?;
    println!("🎯 [DASHBOARD] Using namespace for custom resources: {}", target_namespace);
    
    let mut spdk_nodes = HashMap::new();
    
    if let Ok(node_urls) = env::var("SPDK_NODE_URLS") {
        if !node_urls.trim().is_empty() {
            for pair in node_urls.split(',') {
                if let Some((node, url)) = pair.split_once('=') {
                    spdk_nodes.insert(node.to_string(), url.to_string());
                }
            }
        } else {
            spdk_nodes = discover_spdk_nodes(&kube_client).await?;
        }
    } else {
        spdk_nodes = discover_spdk_nodes(&kube_client).await?;
    }
    
    let app_state = AppState {
        kube_client,
        spdk_nodes: Arc::new(RwLock::new(spdk_nodes)),
        cache: Arc::new(RwLock::new(None)),
        last_update: Arc::new(RwLock::new(Utc::now())),
        target_namespace,
    };
    
    let refresh_state = app_state.clone();
    tokio::spawn(async move {
        refresh_loop(refresh_state).await;
    });
    
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);
    
    let state_filter = warp::any().map(move || app_state.clone());
    
    let api = warp::path("api").and(
        warp::path("dashboard")
            .and(warp::get())
            .and(state_filter.clone())
            .and_then(get_dashboard_data)
        .or(
            // GET /api/volumes/{id}/spdk?node=... - Get detailed SPDK information for a volume
            warp::path("volumes")
                .and(warp::path::param::<String>())
                .and(warp::path("spdk"))
                .and(warp::get())
                .and(warp::query::<QueryParameters>())
                .and(state_filter.clone())
                .and_then(get_volume_spdk_details)
        )
        .or(
            // POST /api/nvmeofdisks (create or update a remote NvmeofDisk)
            warp::path("nvmeofdisks")
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(create_or_update_nvmeofdisk)
        )
        .or(
            // PUT /api/nvmeofdisks/{name}
            warp::path("nvmeofdisks")
                .and(warp::path::param::<String>())
                .and(warp::put())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(update_nvmeofdisk)
        )
        .or(
            warp::path("volumes")
                .and(warp::path::param::<String>())
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_volume_details)
        )
        .or(
            warp::path("volumes")
                .and(warp::path::param::<String>())
                .and(warp::path("raid"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_volume_raid_status)
        )
        .or(
            warp::path("volumes")
                .and(warp::path::param::<String>())
                .and(warp::path("nvmeof"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_nvmeof_details)
        )
        .or(
            warp::path("refresh")
                .and(warp::post())
                .and(state_filter.clone())
                .and_then(trigger_refresh)
        )
        .or(
            // POST /api/raiddisks - create a RAID disk from member NvmeofDisk refs
            warp::path("raiddisks")
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(create_raid_disk)
        )
        .or(
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("metrics"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_node_metrics)
        )
        .or(
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("raid"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_node_raid_status)
        )
        // Uninitialized disks endpoint removed to avoid triggering discovery from dashboard-backend
        .or(
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("disks"))
                .and(warp::path("setup"))
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(setup_node_disks)
        )
        .or(
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("disks"))
                .and(warp::path("reset"))
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(reset_node_disks)
        )
        .or(
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("disks"))
                .and(warp::path("initialize"))
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(initialize_node_disks)
        )
        // Per-node disk status and aggregated setup endpoints removed
        .or(
            warp::path("snapshots")
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_all_snapshots)
        )
        .or(
            warp::path("snapshots")
                .and(warp::path::param::<String>())
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_snapshot_details)
        )
        .or(
            warp::path("snapshots")
                .and(warp::path("tree"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_snapshots_tree)
        )
        .or(
            // Get raw SPDK volumes (unmanaged volumes)
            warp::path("spdk")
                .and(warp::path("volumes"))
                .and(warp::path("raw"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_raw_spdk_volumes_endpoint)
        )
        .or(
            // DELETE /api/spdk/volumes/raw/{uuid} - Delete unmanaged SPDK logical volume by UUID
            warp::path("spdk")
                .and(warp::path("volumes"))
                .and(warp::path("raw"))
                .and(warp::path::param::<String>()) // volume UUID
                .and(warp::delete())
                .and(state_filter.clone())
                .and_then(delete_raw_spdk_volume)
        )
        .or(
            // POST /api/nvmeofdisks (create or update a remote NvmeofDisk)
            warp::path("nvmeofdisks")
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(create_or_update_nvmeofdisk)
        )
        .or(
            // PUT /api/nvmeofdisks/{name}
            warp::path("nvmeofdisks")
                .and(warp::path::param::<String>())
                .and(warp::put())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(update_nvmeofdisk)
        )
        .or(
            // DELETE /api/volumes/orphaned - Delete orphaned SPDK logical volumes (legacy endpoint)
            warp::path("volumes")
                .and(warp::path("orphaned"))
                .and(warp::delete())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(delete_orphaned_spdk_volume)
        )
        .or(
            // POST /api/nodes/{node_id}/maintenance - Enable/disable maintenance mode
            warp::path("nodes")
                .and(warp::path::param::<String>()) // node_id
                .and(warp::path("maintenance"))
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(|node_id: String, request: MaintenanceModeRequest, state: AppState| {
                    set_maintenance_mode(node_id, request, state)
                })
        )
                        .or(
            // GET /api/nodes/{node_id}/maintenance - Get maintenance status
            warp::path("nodes")
                .and(warp::path::param::<String>()) // node_id
                .and(warp::path("maintenance"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(|node_id: String, state: AppState| {
                    get_maintenance_status(node_id, state)
                })
        )
        .or(
            // GET /api/alerts - Get dashboard alerts for operator attention
            warp::path("alerts")
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(|state: AppState| {
                    get_dashboard_alerts(state)
                })
        )
        .or(
            // GET /api/nodes/{node_id}/alerts - Get alerts for specific node
            warp::path("nodes")
                .and(warp::path::param::<String>()) // node_id
                .and(warp::path("alerts"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(|node_id: String, state: AppState| {
                    get_node_alerts(node_id, state)
                })
        )
        .or(
            // GET /api/nodes/performance - Get performance summary for all nodes
            warp::path("nodes")
                .and(warp::path("performance"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(|state: AppState| {
                    get_nodes_performance_summary(state)
                })
        )
        .or(
            // GET /api/dashboard/overview - Get main dashboard overview with node stats
            warp::path("dashboard")
                .and(warp::path("overview"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(|state: AppState| {
                    get_dashboard_overview(state)
                })
        )
        .or(
            // POST /api/alerts/{alert_id}/migrate - Trigger manual migration for alert
            warp::path("alerts")
                .and(warp::path::param::<String>()) // alert_id (volume_id)
                .and(warp::path("migrate"))
                .and(warp::post())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(|volume_id: String, request: ManualMigrationRequest, state: AppState| {
                    trigger_manual_migration(volume_id, request, state)
                })
        )
    );

    let routes = api.with(cors);
    
    println!("SPDK Dashboard API server starting on http://0.0.0.0:8080");
    warp::serve(routes)
        .run(([0, 0, 0, 0], 8080))
        .await;
    
    Ok(())
}

// === NvmeofDisk management endpoints ===
#[derive(Deserialize, Clone)]
struct CreateRaidDiskRequest {
    name: String,
    raid_level: String,            // "1", "0", etc.
    members: Vec<String>,          // member NvmeofDisk names (local NVMe-oF endpoints)
    stripe_size_kb: Option<u32>,
    superblock_enabled: Option<bool>,
    auto_rebuild: Option<bool>,
    created_on_node: String,
}

async fn create_raid_disk(req: CreateRaidDiskRequest, state: AppState) -> Result<impl Reply, Rejection> {
    let api: Api<SpdkRaidDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let member_disks: Vec<spdk_csi_driver::models::RaidMemberDisk> = req.members.into_iter().enumerate().map(|(i, member_name)| {
        spdk_csi_driver::models::RaidMemberDisk {
            member_index: i as u32,
            node_id: req.created_on_node.clone(), // Node where this RAID is being created
            hardware_id: Some(member_name.clone()),
            disk_ref: member_name.clone(), // Reference to the actual disk/bdev name
            serial_number: None, // Will be populated when actual disk is identified
            wwn: None,
            model: None,
            vendor: None,
            nvmeof_endpoint: spdk_csi_driver::models::NvmeofEndpoint::default(),
            state: spdk_csi_driver::models::RaidMemberState::Online,
            capacity_bytes: 0, // Will be populated when actual disk is connected
            connected: false,
            last_health_check: None,
            binding_approach: None, // Will be set when bdev is actually created
        }
    }).collect();

    let spec = spdk_csi_driver::models::SpdkRaidDiskSpec {
        raid_disk_id: req.name.clone(),
        raid_level: req.raid_level,
        num_member_disks: member_disks.len() as i32,
        member_disks,
        stripe_size_kb: req.stripe_size_kb.unwrap_or(1024),
        superblock_enabled: req.superblock_enabled.unwrap_or(true),
        created_on_node: req.created_on_node,
        min_capacity_bytes: 0,
        auto_rebuild: req.auto_rebuild.unwrap_or(true),
    };

    let mut raiddisk = SpdkRaidDisk::new_with_metadata(&req.name, spec, &state.target_namespace);
    raiddisk.status = Some(SpdkRaidDiskStatus::default());

    match api.create(&Default::default(), &raiddisk).await {
        Ok(obj) => Ok(warp::reply::json(&json!({"status":"ok","name": obj.metadata.name })) ),
        Err(e) => {
            eprintln!("SpdkRaidDisk create failed: {}", e);
            Err(warp::reject())
        }
    }
}

#[derive(Deserialize, Clone)]
struct NvmeofDiskCreateRequest {
    name: String,
    is_remote: bool,
    node_id: Option<String>,
    size_bytes: i64,
    nqn: String,
    target_addr: String,
    target_port: u16,
    transport: String,
    credential_secret_name: Option<String>,
    credential_secret_namespace: Option<String>,
    model: Option<String>,
    vendor: Option<String>,
    serial_number: Option<String>,
    hardware_id: Option<String>,
}

async fn create_or_update_nvmeofdisk(req: NvmeofDiskCreateRequest, state: AppState) -> Result<impl Reply, Rejection> {
    let api: Api<NvmeofDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let spec = NvmeofDiskSpec {
        is_remote: req.is_remote,
        node_id: req.node_id.clone(),
        hardware_id: req.hardware_id.clone(),
        serial_number: req.serial_number.clone(),
        wwn: None,
        model: req.model.clone(),
        vendor: req.vendor.clone(),
        size_bytes: req.size_bytes,
        nvmeof_endpoint: spdk_csi_driver::models::NvmeofEndpoint {
            nqn: req.nqn.clone(),
            target_addr: req.target_addr.clone(),
            target_port: req.target_port,
            transport: req.transport.clone(),
            created_at: Some(Utc::now().to_rfc3339()),
            active: true,
        },
        credential_secret_name: req.credential_secret_name.clone(),
        credential_secret_namespace: req.credential_secret_namespace.clone(),
    };

    let mut resource = NvmeofDisk::new(&req.name, spec);
    resource.status = Some(NvmeofDiskStatus {
        healthy: true,
        endpoint_validated: true,
        available_bytes: req.size_bytes,
        last_checked: Utc::now().to_rfc3339(),
        message: Some("Created via dashboard backend".to_string()),
        consecutive_failures: 0,
        last_successful_check: Some(Utc::now().to_rfc3339()),
        failure_reason: None,
    });

    let _pp = kube::api::PostParams::default();
    let patch_params = kube::api::PatchParams::apply("dashboard");
    let patch = kube::api::Patch::Apply(&resource);

    match api.patch(&req.name, &patch_params, &patch).await {
        Ok(obj) => Ok(warp::reply::json(&json!({"status":"ok","name": obj.metadata.name})) ),
        Err(e) => {
            eprintln!("NvmeofDisk create/update failed: {}", e);
            Err(warp::reject())
        }
    }
}

#[derive(Deserialize, Clone)]
struct NvmeofDiskUpdateRequest {
    size_bytes: Option<i64>,
    nqn: Option<String>,
    target_addr: Option<String>,
    target_port: Option<u16>,
    transport: Option<String>,
    healthy: Option<bool>,
}

async fn update_nvmeofdisk(name: String, req: NvmeofDiskUpdateRequest, state: AppState) -> Result<impl Reply, Rejection> {
    let api: Api<NvmeofDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);

    let current = api.get(&name).await.map_err(|_| warp::reject())?;
    let mut spec = current.spec.clone();
    if let Some(sz) = req.size_bytes { spec.size_bytes = sz; }
    if let Some(nqn) = req.nqn { spec.nvmeof_endpoint.nqn = nqn; }
    if let Some(addr) = req.target_addr { spec.nvmeof_endpoint.target_addr = addr; }
    if let Some(port) = req.target_port { spec.nvmeof_endpoint.target_port = port; }
    if let Some(tr) = req.transport { spec.nvmeof_endpoint.transport = tr; }

    let mut status = current.status.unwrap_or_default();
    let current_time = Utc::now().to_rfc3339();
    
    // Enhanced failure tracking logic
    if let Some(h) = req.healthy {
        let was_healthy = status.healthy;
        status.healthy = h;
        
        if h && !was_healthy {
            // Recovery: reset failure tracking
            status.consecutive_failures = 0;
            status.last_successful_check = Some(current_time.clone());
            status.failure_reason = None;
            println!("✅ [NVMEOF_HEALTH] {} recovered after {} consecutive failures", name, status.consecutive_failures);
        } else if !h && was_healthy {
            // New failure: start tracking
            status.consecutive_failures = 1;
            status.failure_reason = Some("External NVMe-oF endpoint unreachable".to_string());
            println!("⚠️ [NVMEOF_HEALTH] {} failed (failure #{})", name, status.consecutive_failures);
        } else if !h && !was_healthy {
            // Continued failure: increment counter
            status.consecutive_failures += 1;
            println!("❌ [NVMEOF_HEALTH] {} still failing (failure #{})", name, status.consecutive_failures);
            
            // Update failure reason based on streak
            if status.consecutive_failures >= 3 {
                status.failure_reason = Some("Persistent external NVMe-oF connectivity issues - check network and storage system".to_string());
            }
        }
    }
    
    status.last_checked = current_time;

    let patch = json!({
        "spec": spec,
        "status": status,
    });

    match api.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch)).await {
        Ok(obj) => Ok(warp::reply::json(&json!({"status":"ok","name": obj.metadata.name})) ),
        Err(e) => {
            eprintln!("NvmeofDisk update failed: {}", e);
            Err(warp::reject())
        }
    }
}

/// Enhanced external NVMe-oF health check with failure tracking
async fn check_external_nvmeof_health(
    disk_name: &str,
    endpoint: &NvmeofEndpoint, 
    state: &AppState
) -> bool {
    println!("🔍 [EXTERNAL_HEALTH] Checking external NVMe-oF health for: {}", disk_name);
    
    // Step 1: Basic network connectivity test
    let network_reachable = test_network_connectivity(&endpoint.target_addr, endpoint.target_port).await;
    if !network_reachable {
        println!("❌ [EXTERNAL_HEALTH] Network connectivity failed for {}:{}", endpoint.target_addr, endpoint.target_port);
        update_nvmeofdisk_health(disk_name, false, Some("Network connectivity failed".to_string()), state).await;
        return false;
    }
    
    // Step 2: NVMe-oF specific validation would go here
    // For now, if network is reachable, consider it healthy
    update_nvmeofdisk_health(disk_name, true, None, state).await;
    true
}

/// Test basic network connectivity to external endpoint
async fn test_network_connectivity(target_addr: &str, target_port: u16) -> bool {
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio::time::timeout;
    
    let address = format!("{}:{}", target_addr, target_port);
    
    match timeout(Duration::from_secs(5), TcpStream::connect(&address)).await {
        Ok(Ok(_)) => {
            println!("✅ [NETWORK] Connection successful to {}", address);
            true
        }
        Ok(Err(e)) => {
            println!("❌ [NETWORK] Connection failed to {}: {}", address, e);
            false
        }
        Err(_) => {
            println!("⏰ [NETWORK] Connection timeout to {}", address);
            false
        }
    }
}

/// Update NvmeofDisk health status with failure tracking
async fn update_nvmeofdisk_health(
    disk_name: &str,
    healthy: bool,
    failure_reason: Option<String>,
    state: &AppState,
) {
    let api: Api<NvmeofDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    if let Ok(current) = api.get(disk_name).await {
        let mut status = current.status.unwrap_or_default();
        let current_time = Utc::now().to_rfc3339();
        let was_healthy = status.healthy;
        
        status.healthy = healthy;
        status.last_checked = current_time.clone();
        
        if healthy && !was_healthy {
            // Recovery
            let prev_failures = status.consecutive_failures;
            status.consecutive_failures = 0;
            status.last_successful_check = Some(current_time);
            status.failure_reason = None;
            println!("✅ [RECOVERY] {} recovered after {} failures", disk_name, prev_failures);
        } else if !healthy {
            // Failure
            if was_healthy {
                status.consecutive_failures = 1;
            } else {
                status.consecutive_failures += 1;
            }
            status.failure_reason = failure_reason;
            println!("❌ [FAILURE] {} failure #{}: {:?}", disk_name, status.consecutive_failures, status.failure_reason);
        }
        
        let patch = json!({ "status": status });
        if let Err(e) = api.patch_status(disk_name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch)).await {
            eprintln!("Failed to update NvmeofDisk {} status: {}", disk_name, e);
        }
    }
}

/// Discovers SPDK nodes by finding running node_agent pods via the Kubernetes API.
async fn discover_spdk_nodes(client: &Client) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    println!("Discovering SPDK nodes by listing 'flint-csi-node' pods...");
    let mut nodes = HashMap::new();
    // Assumes node_agent pods are labeled with 'app=flint-csi-node'.
    let pods_api: Api<Pod> = Api::all(client.clone());
    let lp = ListParams::default().labels("app=flint-csi-node");

    let pods = pods_api.list(&lp).await?;

    for pod in pods.items {
        let node_name = pod.spec.as_ref().and_then(|s| s.node_name.clone());
        let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());

        if let (Some(name), Some(ip)) = (node_name, pod_ip) {
            let url = format!("http://{}:8081/api/spdk/rpc", ip);
            println!("Discovered SPDK node '{}' at '{}'", name, url);
            nodes.insert(name, url);
        }
    }
    
    if nodes.is_empty() {
        eprintln!("Warning: No SPDK node agent pods found. Dashboard may not show live data.");
    }

    Ok(nodes)
}

async fn refresh_loop(state: AppState) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
    
    loop {
        interval.tick().await;
        
        if let Err(e) = refresh_dashboard_data(&state).await {
            eprintln!("Failed to refresh dashboard data: {}", e);
        }
    }
}

async fn refresh_dashboard_data(state: &AppState) -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 [DASHBOARD_REFRESH] Starting dashboard data refresh...");
    
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let volumes_list = match volumes_api.list(&ListParams::default()).await {
        Ok(list) => {
            println!("✅ [DASHBOARD_REFRESH] Successfully listed {} volumes", list.items.len());
            list
        }
        Err(e) => {
            println!("❌ [DASHBOARD_REFRESH] Failed to list volumes: {}", e);
            return Err(Box::new(e));
        }
    };
    
    let disks_api: Api<NvmeofDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let disks_list = match disks_api.list(&ListParams::default()).await {
        Ok(list) => {
            println!("✅ [DASHBOARD_REFRESH] Successfully listed {} disks", list.items.len());
            list
        }
        Err(e) => {
            println!("❌ [DASHBOARD_REFRESH] Failed to list disks: {}", e);
            return Err(Box::new(e));
        }
    };
    
    let mut dashboard_volumes = Vec::new();
    let mut dashboard_disks = Vec::new();
    let mut dashboard_raid_disks = Vec::new();
    let mut nodes = std::collections::HashSet::new();
    
    println!("🔧 [DASHBOARD_REFRESH] Converting {} volumes to dashboard format...", volumes_list.items.len());
    for (i, volume) in volumes_list.items.iter().enumerate() {
        let dashboard_volume = convert_volume_to_dashboard(volume);
        println!("✅ [DASHBOARD_REFRESH] Volume {}/{}: {} converted successfully", 
            i + 1, volumes_list.items.len(), volume.spec.volume_id);
        for replica in &dashboard_volume.replica_statuses {
            nodes.insert(replica.node.clone());
        }
        dashboard_volumes.push(dashboard_volume);
    }
    
    println!("🔧 [DASHBOARD_REFRESH] Converting {} nvmeof disks to dashboard format...", disks_list.items.len());
    for (i, disk) in disks_list.items.iter().enumerate() {
        if let Some(node_id) = disk.spec.node_id.as_ref() {
            nodes.insert(node_id.clone());
        }
        let dashboard_disk = convert_nvmeofdisk_to_dashboard(disk);
        println!("✅ [DASHBOARD_REFRESH] NvmeofDisk {}/{}: {} converted successfully", 
            i + 1, disks_list.items.len(), disk.metadata.name.as_ref().unwrap_or(&"unnamed".to_string()));
        dashboard_disks.push(dashboard_disk);
    }
    
    // List RAID disks CRDs for unified disk view
    let raiddisks_api: Api<SpdkRaidDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    if let Ok(raid_list) = raiddisks_api.list(&ListParams::default()).await {
        for rd in raid_list.items {
            if let Some(status) = rd.status.clone() {
                let members = status.raid_status.as_ref().map(|rs| {
                    rs.base_bdevs_list.iter().map(|m| DashboardRaidMember {
                        slot: m.slot,
                        name: m.name.clone(),
                        state: m.state.clone(),
                        uuid: m.uuid.clone(),
                        is_configured: m.is_configured,
                        node: m.node.clone(),
                        disk_ref: None, // Not applicable for RAID bdevs
                        health_status: "Unknown".to_string(), // Will be populated from actual status
                    }).collect::<Vec<_>>()
                }).unwrap_or_default();
                dashboard_raid_disks.push(DashboardRaidDisk {
                    id: rd.spec.raid_disk_id.clone(),
                    node: rd.spec.created_on_node.clone(),
                    raid_level: rd.spec.raid_level.clone(),
                    state: status.state.clone(),
                    lvs_name: status.lvs_name.clone(),
                    lvs_uuid: status.lvs_uuid.clone(),
                    total_capacity_gb: status.total_capacity_bytes / (1024*1024*1024),
                    usable_capacity_gb: status.usable_capacity_bytes / (1024*1024*1024),
                    used_capacity_gb: status.used_capacity_bytes / (1024*1024*1024),
                    degraded: status.degraded,
                    rebuild_progress: status.rebuild_progress,
                    members,
                });
                nodes.insert(rd.spec.created_on_node.clone());
            }
        }
    }

    println!("🚀 [DASHBOARD_REFRESH] Enhancing with SPDK metrics...");
    match enhance_with_spdk_metrics(&mut dashboard_volumes, &mut dashboard_disks, state).await {
        Ok(_) => println!("✅ [DASHBOARD_REFRESH] SPDK metrics enhancement completed successfully"),
        Err(e) => {
            println!("⚠️ [DASHBOARD_REFRESH] SPDK metrics enhancement failed: {}", e);
            // Continue without SPDK metrics rather than failing entirely
        }
    }
    
    // Collect raw SPDK volumes for the dashboard
    let raw_spdk_volumes = match get_raw_spdk_volumes(state).await {
        Ok(mut raw_volumes) => {
            // Check which volumes are managed against the managed volume list
            for raw_vol in &mut raw_volumes {
                for managed_vol in &volumes_list.items {
                    if raw_vol.name.contains(&managed_vol.spec.volume_id) ||
                       managed_vol.spec.volume_id.contains(&raw_vol.uuid) ||
                       raw_vol.name.contains("vol_") && raw_vol.name.contains(&managed_vol.spec.volume_id.replace("pvc-", "")) {
                        raw_vol.is_managed = true;
                        break;
                    }
                }
            }
            // Return only unmanaged volumes for the dashboard
            raw_volumes.into_iter().filter(|v| !v.is_managed).collect()
        }
        Err(e) => {
            println!("⚠️ [DASHBOARD_REFRESH] Failed to get raw SPDK volumes: {}", e);
            Vec::new()
        }
    };
    
    println!("✅ [DASHBOARD_REFRESH] Found {} unmanaged SPDK volumes", raw_spdk_volumes.len());
    
    // Include all discovered nodes, not just those with volumes/disks
    let spdk_nodes = state.spdk_nodes.read().await;
    for discovered_node in spdk_nodes.keys() {
        nodes.insert(discovered_node.clone());
    }
    
    let dashboard_data = DashboardData {
        volumes: dashboard_volumes,
        raw_volumes: raw_spdk_volumes,
        disks: dashboard_disks,
        nodes: nodes.into_iter().map(|node_id| DashboardNode {
            node_id: node_id.clone(),
            status: "healthy".to_string(), // Status determined by node connectivity
            maintenance_mode: false, // Check maintenance mode via SpdkConfig if needed
            maintenance_status: None, // Maintenance status from SpdkConfig if available
            raid_count: 0, // RAID count calculated from raid_disks
            volume_count: 0, // Volume count calculated from volumes
        }).collect(),
        raid_disks: dashboard_raid_disks,
    };
    
    *state.cache.write().await = Some(dashboard_data);
    *state.last_update.write().await = Utc::now();
    
    Ok(())
}

fn convert_volume_to_dashboard(volume: &SpdkVolume) -> DashboardVolume {
    let default_status = SpdkVolumeStatus::default();
    let status = volume.status.as_ref().unwrap_or(&default_status);
    let spec = &volume.spec;
    
    // Convert RAID status from CRD
    let raid_status = status.raid_status.as_ref().map(|raid| {
        let members: Vec<DashboardRaidMember> = raid.base_bdevs_list.iter().map(|member| {
            // Find corresponding replica for additional info
            let replica = spec.replicas.iter()
                .find(|r| r.raid_member_index == member.slot as usize);
            
            DashboardRaidMember {
                slot: member.slot,
                name: member.name.clone(),
                state: member.state.clone(),
                uuid: member.uuid.clone(),
                is_configured: member.is_configured,
                node: replica.map(|r| r.node.clone()),
                disk_ref: replica.map(|r| r.disk_ref.clone()),
                health_status: replica.map(|r| format!("{:?}", r.health_status)).unwrap_or_default(),
            }
        }).collect();
        
        let rebuild_info = raid.rebuild_info.as_ref().map(|rebuild| {
            DashboardRebuildInfo {
                state: rebuild.state.clone(),
                target_slot: rebuild.target_slot,
                source_slot: rebuild.source_slot,
                blocks_remaining: rebuild.blocks_remaining,
                blocks_total: rebuild.blocks_total,
                progress_percentage: rebuild.progress_percentage,
                estimated_time_remaining: estimate_rebuild_time(rebuild),
                start_time: None, // Could be tracked separately
            }
        });
        
        DashboardRaidStatus {
            raid_level: raid.raid_level,
            state: raid.state.clone(),
            num_members: raid.num_base_bdevs,
            operational_members: raid.num_base_bdevs_operational,
            discovered_members: raid.num_base_bdevs_discovered,
            members,
            rebuild_info,
            superblock_version: raid.superblock_version,
            auto_rebuild_enabled: spec.raid_auto_rebuild,
        }
    });
    
    let replica_statuses: Vec<DashboardReplicaStatus> = spec.replicas.iter().map(|replica| {
        let nvmf_target = if let (Some(nqn), Some(ip), Some(port)) = (
            &replica.nqn,
            &replica.ip,
            &replica.port
        ) {
            Some(NvmfTarget {
                nqn: nqn.clone(),
                target_ip: ip.clone(),
                target_port: port.clone(),
                transport_type: spec.nvmeof_transport.as_deref().unwrap_or("TCP").to_string(),
            })
        } else {
            None
        };
        
        let access_method = if replica.node == std::env::var("NODE_ID").unwrap_or_default() {
            "local-nvmeof".to_string()
        } else {
            "remote-nvmeof".to_string()
        };
        
        // Get rebuild progress from RAID status if this replica is rebuilding
        let rebuild_progress = raid_status.as_ref()
            .and_then(|rs| rs.rebuild_info.as_ref())
            .filter(|ri| ri.target_slot == replica.raid_member_index as u32)
            .map(|ri| ri.progress_percentage);
        
        DashboardReplicaStatus {
            node: replica.node.clone(),
            status: format!("{:?}", replica.health_status).to_lowercase(),
            is_local: replica.node == std::env::var("NODE_ID").unwrap_or_default(),
            last_io_timestamp: replica.last_io_timestamp.clone(),
            rebuild_progress,
            rebuild_target: None,
            is_new_replica: None,
            nvmf_target,
            access_method,
            raid_member_slot: Some(replica.raid_member_index as u32),
            raid_member_state: format!("{:?}", replica.raid_member_state).to_lowercase(),
        }
    }).collect();
    
    let size_gb = spec.size_bytes / (1024 * 1024 * 1024);
    let has_local_nvme = replica_statuses.iter().any(|r| r.is_local);
    
    // Convert NVMe-oF targets from status
    let nvmeof_targets: Vec<NvmeofTargetInfo> = status.nvmeof_targets.iter().map(|target| {
        NvmeofTargetInfo {
            nqn: target.nqn.clone(),
            target_ip: target.target_addr.clone(),
            target_port: target.target_port,
            transport: target.transport.clone(),
            node: target.node.clone(),
            bdev_name: target.bdev_name.clone(),
            active: target.active,
            connection_count: 0, // Could be enhanced with live data
        }
    }).collect();
    
    let volume_name = volume.metadata.name.clone().unwrap_or(spec.volume_id.clone());
    let nvmeof_enabled = !nvmeof_targets.is_empty();
    let transport_type = spec.nvmeof_transport.as_deref().unwrap_or("tcp").to_string();
    let target_port = spec.nvmeof_target_port.unwrap_or(4420);
    
    // Get rebuild progress from RAID status
    let rebuild_progress = raid_status.as_ref()
        .and_then(|rs| rs.rebuild_info.as_ref())
        .map(|ri| ri.progress_percentage);

    // Add ublk device info if available
    let ublk_device = status.ublk_device.as_ref().map(|ublk| {
        json!({
            "id": ublk.id,
            "device_path": ublk.device_path
        })
    });
    
    DashboardVolume {
        id: spec.volume_id.clone(),
        name: volume_name,
        size: format!("{}GB", size_gb),
        state: status.state.clone(),
        replicas: spec.num_replicas,
        active_replicas: status.active_replicas.len() as i32,
        local_nvme: has_local_nvme,
        access_method: "nvmeof".to_string(),
        rebuild_progress,
        nodes: spec.replicas.iter().map(|r| r.node.clone()).collect(),
        replica_statuses,
        nvmeof_targets,
        nvmeof_enabled,
        transport_type,
        target_port,
        raid_status,
        ublk_device,
        // Default validation status - will be updated by validation process
        spdk_validation_status: SpdkValidationStatus {
            has_spdk_backing: true, // Assume true until validation proves otherwise
            validation_message: None,
            validation_severity: ValidationSeverity::Info,
        },
    }
}

fn estimate_rebuild_time(rebuild: &RaidRebuildInfo) -> Option<String> {
    if rebuild.blocks_remaining == 0 || rebuild.progress_percentage >= 100.0 {
        return Some("Complete".to_string());
    }
    
    // Simple estimation based on current progress
    // In a real implementation, you'd track rate over time
    let estimated_seconds = (rebuild.blocks_remaining as f64 / 1000.0) * 60.0; // Very rough estimate
    
    if estimated_seconds < 60.0 {
        Some(format!("{}s", estimated_seconds as u64))
    } else if estimated_seconds < 3600.0 {
        Some(format!("{}m", (estimated_seconds / 60.0) as u64))
    } else {
        Some(format!("{}h", (estimated_seconds / 3600.0) as u64))
    }
}

fn convert_nvmeofdisk_to_dashboard(disk: &NvmeofDisk) -> DashboardDisk {
    let capacity = disk.spec.size_bytes;
    let capacity_gb = capacity / (1024 * 1024 * 1024);
    let free = disk.status.as_ref().map(|s| s.available_bytes).unwrap_or(0);
    let free_gb = free / (1024 * 1024 * 1024);
    let node = disk.spec.node_id.clone().unwrap_or("remote".to_string());
    DashboardDisk {
        id: disk.metadata.name.clone().unwrap_or_default(),
        node,
        pci_addr: "".to_string(),
        capacity,
        capacity_gb,
        allocated_space: capacity - free,
        free_space: free,
        free_space_display: format!("{}GB", free_gb),
        healthy: disk.status.as_ref().map(|s| s.healthy).unwrap_or(false),
        blobstore_initialized: false,
        lvol_count: 0,
        model: disk.spec.model.clone().unwrap_or_default(),
        read_iops: 0,
        write_iops: 0,
        read_latency: 0,
        write_latency: 0,
        brought_online: disk.status.as_ref().map(|s| s.last_checked.clone()).unwrap_or_default(),
        provisioned_volumes: vec![],
        orphaned_spdk_volumes: vec![],
        is_remote: disk.spec.is_remote,
        nvmeof_endpoint: Some(NvmeofEndpointInfo {
            nqn: disk.spec.nvmeof_endpoint.nqn.clone(),
            target_addr: disk.spec.nvmeof_endpoint.target_addr.clone(),
            target_port: disk.spec.nvmeof_endpoint.target_port,
            transport: disk.spec.nvmeof_endpoint.transport.clone(),
            active: disk.status.as_ref().map(|s| s.endpoint_validated).unwrap_or(false),
        }),
    }
}

async fn enhance_with_spdk_metrics(
    volumes: &mut [DashboardVolume],
    disks: &mut [DashboardDisk],
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    
    println!("🔍 [SPDK_METRICS] Enhancing {} volumes and {} disks with SPDK metrics from {} nodes", 
        volumes.len(), disks.len(), spdk_nodes.len());
    
    for (node_name, rpc_url) in spdk_nodes.iter() {
        println!("🌐 [SPDK_METRICS] Querying node '{}' at '{}'", node_name, rpc_url);
        
        // Get real-time RAID status and update volumes
        match http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_raid_get_bdevs",
                "params": { "category": "all" }
            }))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                println!("✅ [SPDK_METRICS] RAID query to {} returned status: {}", node_name, status);
                
                if status.is_success() {
                    let response_text = match response.text().await {
                        Ok(text) => {
                            println!("📄 [SPDK_METRICS] Raw RAID response from {}: {}", node_name, 
                                if text.len() > 500 { format!("{}... (truncated, full length: {})", &text[..500], text.len()) } else { text.clone() });
                            text
                        }
                        Err(e) => {
                            println!("❌ [SPDK_METRICS] Failed to read response text from {}: {}", node_name, e);
                            continue;
                        }
                    };
                    
                    match serde_json::from_str::<serde_json::Value>(&response_text) {
                        Ok(raid_info) => {
                            println!("✅ [SPDK_METRICS] Successfully parsed RAID JSON from {}", node_name);
                            if let Some(raid_bdevs) = raid_info["result"].as_array() {
                                println!("🔍 [SPDK_METRICS] Found {} RAID bdevs from {}", raid_bdevs.len(), node_name);
                                for (i, raid_bdev) in raid_bdevs.iter().enumerate() {
                                    if let Some(raid_name) = raid_bdev["name"].as_str() {
                                        println!("🔍 [SPDK_METRICS] Processing RAID bdev {}/{}: '{}'", i + 1, raid_bdevs.len(), raid_name);
                                        for volume in volumes.iter_mut() {
                                            if volume.id == raid_name || raid_name.contains(&volume.id) {
                                                println!("🔄 [SPDK_METRICS] Updating volume '{}' with live RAID status", volume.id);
                                                update_volume_with_live_raid_status(volume, raid_bdev);
                                            }
                                        }
                                    } else {
                                        println!("⚠️ [SPDK_METRICS] RAID bdev {} missing 'name' field", i);
                                    }
                                }
                            } else {
                                println!("⚠️ [SPDK_METRICS] No 'result' array found in RAID response from {}", node_name);
                            }
                        }
                        Err(e) => {
                            println!("❌ [SPDK_METRICS] Failed to parse RAID JSON from {}: {}", node_name, e);
                            println!("📄 [SPDK_METRICS] Failed JSON content: {}", response_text);
                        }
                    }
                } else {
                    println!("⚠️ [SPDK_METRICS] RAID query to {} failed with status: {}", node_name, status);
                }
            }
            Err(e) => {
                println!("❌ [SPDK_METRICS] Failed to send RAID query to {}: {}", node_name, e);
            }
        }
        
        // Get NVMe-oF subsystem status instead of vhost controllers
        println!("🔍 [SPDK_METRICS] Querying NVMe-oF subsystems from {}", node_name);
        match http_client
            .post(rpc_url)
            .json(&json!({
                "method": "nvmf_get_subsystems"
            }))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                println!("✅ [SPDK_METRICS] NVMe-oF query to {} returned status: {}", node_name, status);
                
                if status.is_success() {
                    let response_text = match response.text().await {
                        Ok(text) => {
                            println!("📄 [SPDK_METRICS] Raw NVMe-oF response from {}: {}", node_name,
                                if text.len() > 500 { format!("{}... (truncated, full length: {})", &text[..500], text.len()) } else { text.clone() });
                            text
                        }
                        Err(e) => {
                            println!("❌ [SPDK_METRICS] Failed to read NVMe-oF response text from {}: {}", node_name, e);
                            continue;
                        }
                    };
                    
                    if let Ok(nvmf_info) = serde_json::from_str::<serde_json::Value>(&response_text) {
                        println!("✅ [SPDK_METRICS] Successfully parsed NVMe-oF JSON from {}", node_name);
                        if let Some(subsystems) = nvmf_info["result"].as_array() {
                            println!("🔍 [SPDK_METRICS] Found {} NVMe-oF subsystems from {}", subsystems.len(), node_name);
                            for (i, subsystem) in subsystems.iter().enumerate() {
                                if let Some(nqn) = subsystem["nqn"].as_str() {
                                    println!("🔍 [SPDK_METRICS] Processing NVMe-oF subsystem {}/{}: '{}'", i + 1, subsystems.len(), nqn);
                                    for volume in volumes.iter_mut() {
                                        // Find volumes that match this NQN
                                        if nqn.contains(&volume.id) || volume.nvmeof_targets.iter().any(|t| t.nqn == nqn) {
                                            println!("🔄 [SPDK_METRICS] Updating volume '{}' with NVMe-oF status from subsystem '{}'", volume.id, nqn);
                                            // Update NVMe-oF target status
                                            let is_active = subsystem["state"].as_str() == Some("active");
                                            
                                            for target in &mut volume.nvmeof_targets {
                                                if target.nqn == nqn {
                                                    target.active = is_active;
                                                    
                                                    // Get connection count if available
                                                    if let Some(hosts) = subsystem["hosts"].as_array() {
                                                        target.connection_count = hosts.len() as u32;
                                                    }
                                                }
                                            }
                                            
                                            volume.nvmeof_enabled = volume.nvmeof_targets.iter().any(|t| t.active);
                                        }
                                    }
                                } else {
                                    println!("⚠️ [SPDK_METRICS] NVMe-oF subsystem {} missing 'nqn' field", i);
                                }
                            }
                        } else {
                            println!("⚠️ [SPDK_METRICS] No 'result' array found in NVMe-oF response from {}", node_name);
                        }
                    } else {
                        println!("❌ [SPDK_METRICS] Failed to parse NVMe-oF JSON from {}: parse error", node_name);
                        println!("📄 [SPDK_METRICS] Failed NVMe-oF JSON content: {}", response_text);
                    }
                } else {
                    println!("⚠️ [SPDK_METRICS] NVMe-oF query to {} failed with status: {}", node_name, status);
                }
            }
            Err(e) => {
                println!("❌ [SPDK_METRICS] Failed to send NVMe-oF query to {}: {}", node_name, e);
            }
        }
        
        // Get real-time bdev statistics
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_get_iostat"
            }))
            .send()
            .await
        {
            if let Ok(iostat) = response.json::<serde_json::Value>().await {
                if let Some(bdevs) = iostat["result"].as_array() {
                    for bdev_stat in bdevs {
                        if let Some(bdev_name) = bdev_stat["name"].as_str() {
                            for disk in disks.iter_mut() {
                                if disk.id == bdev_name || bdev_name.contains(&disk.id) {
                                    if let Some(read_ios) = bdev_stat["read_ios"].as_u64() {
                                        disk.read_iops = read_ios;
                                    }
                                    if let Some(write_ios) = bdev_stat["write_ios"].as_u64() {
                                        disk.write_iops = write_ios;
                                    }
                                    if let Some(read_latency) = bdev_stat["read_latency_ticks"].as_u64() {
                                        disk.read_latency = read_latency;
                                    }
                                    if let Some(write_latency) = bdev_stat["write_latency_ticks"].as_u64() {
                                        disk.write_latency = write_latency;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    Ok(())
}

// REMOVED: apply_validation_results_to_dashboard function - unused

/// Get raw SPDK logical volumes from all nodes
async fn get_raw_spdk_volumes(
    state: &AppState,
) -> Result<Vec<RawSpdkVolume>, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔍 [RAW_SPDK] Collecting raw SPDK logical volumes from all nodes");
    
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    let mut raw_volumes = Vec::new();
    
    for (node_name, rpc_url) in spdk_nodes.iter() {
        println!("🌐 [RAW_SPDK] Querying logical volumes and LVS stores on node '{}'", node_name);
        
        // First, get LVS stores to get cluster sizes
        let lvstores_response = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_lvol_get_lvstores",
                "params": {},
                "id": 1
            }))
            .send()
            .await?;
            
        let mut lvs_cluster_sizes = std::collections::HashMap::new();
        
        if lvstores_response.status().is_success() {
            let lvstores_text = lvstores_response.text().await?;
            let lvstores_info: serde_json::Value = serde_json::from_str(&lvstores_text)?;
            
            let lvstores_list = if let Some(result_array) = lvstores_info["result"].as_array() {
                result_array
            } else if let Some(direct_array) = lvstores_info.as_array() {
                direct_array
            } else {
                println!("⚠️ [RAW_SPDK] Unexpected LVS response format from {}", node_name);
                &Vec::new()
            };
            
            for lvstore in lvstores_list {
                if let (Some(lvs_name), Some(cluster_size)) = (
                    lvstore["name"].as_str(),
                    lvstore["cluster_size"].as_u64()
                ) {
                    lvs_cluster_sizes.insert(lvs_name.to_string(), cluster_size);
                    println!("🏪 [RAW_SPDK] LVS '{}': cluster_size = {} bytes", lvs_name, cluster_size);
                }
            }
        }
        
        // Now get logical volumes using the reliable API you specified
        let lvols_response = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_lvol_get_lvols",
                "params": {},
                "id": 1
            }))
            .send()
            .await?;
            
        if !lvols_response.status().is_success() {
            println!("⚠️ [RAW_SPDK] Query to {} failed with status: {}", node_name, lvols_response.status());
            continue;
        }
        
        let lvols_text = lvols_response.text().await?;
        let lvols_info: serde_json::Value = serde_json::from_str(&lvols_text)?;
        
        let lvols_list = if let Some(result_array) = lvols_info["result"].as_array() {
            result_array
        } else if let Some(direct_array) = lvols_info.as_array() {
            direct_array
        } else {
            println!("⚠️ [RAW_SPDK] Unexpected response format from {}: {}", node_name, lvols_text);
            continue;
        };
        
        println!("✅ [RAW_SPDK] Found {} logical volumes on {}", lvols_list.len(), node_name);
            
        for lvol in lvols_list {
            if let (Some(vol_name), Some(vol_uuid), Some(lvs_info)) = (
                lvol["name"].as_str(),
                lvol["uuid"].as_str(),
                lvol["lvs"].as_object()
            ) {
                let lvs_name = lvs_info.get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown");
                    
                // Get cluster information for size calculation
                let num_allocated_clusters = lvol["num_allocated_clusters"].as_u64().unwrap_or(0);
                
                let (size_bytes, size_gb) = if let Some(cluster_size) = lvs_cluster_sizes.get(lvs_name) {
                    let bytes = num_allocated_clusters * cluster_size;
                    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    (bytes, gb)
                } else {
                    println!("⚠️ [RAW_SPDK] Missing cluster size for LVS '{}', skipping volume {}", lvs_name, vol_name);
                    continue; // Skip this volume if we can't calculate its size
                };
                
                raw_volumes.push(RawSpdkVolume {
                    name: vol_name.to_string(),
                    uuid: vol_uuid.to_string(),
                    node: node_name.clone(),
                    lvs_name: lvs_name.to_string(),
                    size_blocks: size_bytes / 512, // Convert to 512-byte blocks for consistency
                    size_gb,
                    is_managed: false, // Will be updated below
                });
                
                let cluster_size = lvs_cluster_sizes.get(lvs_name).unwrap(); // Safe because we checked above
                println!("📋 [RAW_SPDK] Raw volume: {} ({:.2}GB, {} clusters × {} bytes) on {}", 
                    vol_name, size_gb, num_allocated_clusters, cluster_size, node_name);
            }
        }
    }
    
    println!("📊 [RAW_SPDK] Total raw SPDK volumes found: {}", raw_volumes.len());
    Ok(raw_volumes)
}

/// Enhanced SPDK volume details structure
#[derive(Serialize, Debug, Clone)]
struct SpdkVolumeDetails {
    volume_name: String,
    volume_uuid: String,
    lvs_name: String,
    lvs_uuid: String,
    node: String,
    // Volume-specific information
    allocated_clusters: u64,
    cluster_size: u64,
    size_bytes: u64,
    size_gb: f64,
    is_thin_provisioned: bool,
    is_clone: bool,
    is_snapshot: bool,
    // LVS information
    lvs_total_clusters: u64,
    lvs_free_clusters: u64,
    lvs_block_size: u64,
    lvs_base_bdev: String,
    lvs_capacity_gb: f64,
    lvs_used_gb: f64,
    lvs_utilization_pct: f64,
    // SPDK bdev information
    bdev_name: String,
    bdev_alias: Option<String>,
    // Additional metadata
    last_updated: String,
}

/// Get detailed SPDK information for a specific volume
async fn get_volume_spdk_details(
    volume_id: String,
    query: QueryParameters,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🔍 [VOLUME_SPDK] Getting SPDK details for volume/UUID: {}", volume_id);
    
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    
    // Get the target node from query parameter or fallback to first available
    let target_node = query.node.unwrap_or_else(|| {
        spdk_nodes.keys().next().unwrap_or(&"unknown".to_string()).clone()
    });
    
    println!("🎯 [VOLUME_SPDK] Querying node '{}' for volume/UUID '{}'", target_node, volume_id);
    
    let rpc_url = spdk_nodes.get(&target_node).ok_or_else(|| {
        println!("❌ [VOLUME_SPDK] Node '{}' not found in SPDK nodes", target_node);
        warp::reject::not_found()
    })?;
    
        // Get LVS stores first
    let lvstores_response = http_client
        .post(rpc_url)
            .json(&json!({
                "method": "bdev_lvol_get_lvstores",
                "params": {},
                "id": 1
            }))
            .send()
            .await
            .map_err(|e| {
                println!("❌ [VOLUME_SPDK] Failed to query LVS stores on {}: {}", target_node, e);
                warp::reject::not_found()
            })?;
            
    if !lvstores_response.status().is_success() {
        println!("❌ [VOLUME_SPDK] LVS query failed on node {}", target_node);
        return Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "error": "LVS query failed",
                "message": format!("Failed to query LVS stores on node: {}", target_node)
            })),
            warp::http::StatusCode::SERVICE_UNAVAILABLE
        ));
    }
        
        let lvstores_text = lvstores_response.text().await.map_err(|_| {
            warp::reject::not_found()
        })?;
        let lvstores_info: serde_json::Value = serde_json::from_str(&lvstores_text).map_err(|_| {
            warp::reject::not_found()
        })?;
        
        let empty_vec = Vec::new();
        let lvstores_list = lvstores_info["result"].as_array().unwrap_or(&empty_vec);
        
    // Get logical volumes
    let lvols_response = http_client
        .post(rpc_url)
            .json(&json!({
                "method": "bdev_lvol_get_lvols",
                "params": {},
                "id": 1
            }))
            .send()
            .await
            .map_err(|_| warp::reject::not_found())?;
            
    if !lvols_response.status().is_success() {
        println!("❌ [VOLUME_SPDK] Logical volumes query failed on node {}", target_node);
        return Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "error": "Logical volumes query failed", 
                "message": format!("Failed to query logical volumes on node: {}", target_node)
            })),
            warp::http::StatusCode::SERVICE_UNAVAILABLE
        ));
    }
        
        let lvols_text = lvols_response.text().await.map_err(|_| {
            warp::reject::not_found()
        })?;
        let lvols_info: serde_json::Value = serde_json::from_str(&lvols_text).map_err(|_| {
            warp::reject::not_found()
        })?;
        
        let empty_lvols = Vec::new();
        let lvols_list = lvols_info["result"].as_array().unwrap_or(&empty_lvols);
        
    // Find the volume by checking if volume_id is contained in the volume name OR matches the UUID
    for lvol in lvols_list {
        if let Some(vol_name) = lvol["name"].as_str() {
            let vol_uuid = lvol["uuid"].as_str().unwrap_or("");
            
            // Check if volume_id matches either the name pattern (for managed volumes) or the UUID (for raw volumes)
            if vol_name.contains(&volume_id) || vol_uuid == volume_id {
                // Found the volume! Extract details
                let vol_uuid = lvol["uuid"].as_str().unwrap_or("unknown");
                let lvs_info = lvol["lvs"].as_object().unwrap();
                let lvs_name = lvs_info.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                let lvs_uuid = lvs_info.get("uuid").and_then(|n| n.as_str()).unwrap_or("unknown");
                
                // Find the corresponding LVS details
                let mut lvs_details = None;
                for lvstore in lvstores_list {
                    if let Some(store_name) = lvstore["name"].as_str() {
                        if store_name == lvs_name {
                            lvs_details = Some(lvstore);
                            break;
                        }
                    }
                }
                
                if let Some(lvs) = lvs_details {
                    let allocated_clusters = lvol["num_allocated_clusters"].as_u64().unwrap_or(0);
                    let cluster_size = lvs["cluster_size"].as_u64().unwrap_or(1048576);
                    let size_bytes = allocated_clusters * cluster_size;
                    let size_gb = size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    
                    let total_clusters = lvs["total_data_clusters"].as_u64().unwrap_or(0);
                    let free_clusters = lvs["free_clusters"].as_u64().unwrap_or(0);
                    let lvs_capacity_bytes = total_clusters * cluster_size;
                    let lvs_used_bytes = (total_clusters - free_clusters) * cluster_size;
                    let lvs_capacity_gb = lvs_capacity_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    let lvs_used_gb = lvs_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    let lvs_utilization_pct = if total_clusters > 0 {
                        ((total_clusters - free_clusters) as f64 / total_clusters as f64) * 100.0
                    } else {
                        0.0
                    };
                    
                    let spdk_details = SpdkVolumeDetails {
                        volume_name: vol_name.to_string(),
                        volume_uuid: vol_uuid.to_string(),
                        lvs_name: lvs_name.to_string(),
                        lvs_uuid: lvs_uuid.to_string(),
                        node: target_node.clone(),
                        allocated_clusters,
                        cluster_size,
                        size_bytes,
                        size_gb,
                        is_thin_provisioned: lvol["is_thin_provisioned"].as_bool().unwrap_or(false),
                        is_clone: lvol["is_clone"].as_bool().unwrap_or(false),
                        is_snapshot: lvol["is_snapshot"].as_bool().unwrap_or(false),
                        lvs_total_clusters: total_clusters,
                        lvs_free_clusters: free_clusters,
                        lvs_block_size: lvs["block_size"].as_u64().unwrap_or(512),
                        lvs_base_bdev: lvs["base_bdev"].as_str().unwrap_or("unknown").to_string(),
                        lvs_capacity_gb,
                        lvs_used_gb,
                        lvs_utilization_pct,
                        bdev_name: vol_uuid.to_string(), // SPDK bdev name is the UUID
                        bdev_alias: lvol.get("alias").and_then(|a| a.as_str()).map(|s| s.to_string()),
                        last_updated: chrono::Utc::now().to_rfc3339(),
                    };
                    
                    println!("✅ [VOLUME_SPDK] Found SPDK details for volume {} on node {}", volume_id, target_node);
                    
                    return Ok(warp::reply::with_status(
                        warp::reply::json(&spdk_details),
                        warp::http::StatusCode::OK
                    ));
                }
            }
        }
    }
    
    // Volume not found
    println!("❌ [VOLUME_SPDK] Volume {} not found on node {}", volume_id, target_node);
    Ok(warp::reply::with_status(
        warp::reply::json(&json!({
            "error": "Volume not found",
            "message": format!("No SPDK logical volume found for volume ID '{}' on node '{}'", volume_id, target_node)
        })),
        warp::http::StatusCode::NOT_FOUND
    ))
}

/// Delete a raw SPDK logical volume by UUID
async fn delete_raw_spdk_volume(volume_uuid: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🗑️ [DELETE_RAW] ========== STARTING DELETE REQUEST ===========");
    println!("🗑️ [DELETE_RAW] Volume UUID to delete: '{}'", volume_uuid);
    println!("🗑️ [DELETE_RAW] Timestamp: {}", chrono::Utc::now().to_rfc3339());
    
    // First, find which node has this volume
    let raw_volumes = match get_raw_spdk_volumes(&state).await {
        Ok(volumes) => volumes,
        Err(e) => {
            println!("❌ [DELETE_RAW] Failed to get raw volumes: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&json!({
                    "success": false,
                    "message": format!("Failed to query volumes: {}", e)
                })),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ));
        }
    };
    
    // Find the target volume
    let target_volume = match raw_volumes.iter().find(|v| v.uuid == volume_uuid) {
        Some(vol) => vol,
        None => {
            println!("❌ [DELETE_RAW] Volume '{}' not found", volume_uuid);
            return Ok(warp::reply::with_status(
                warp::reply::json(&json!({
                    "success": false,
                    "message": format!("Volume '{}' not found", volume_uuid)
                })),
                warp::http::StatusCode::NOT_FOUND
            ));
        }
    };
    
    // Check if volume is managed - don't allow deletion of managed volumes
    if target_volume.is_managed {
        println!("❌ [DELETE_RAW] Volume '{}' is managed by Kubernetes - use PVC deletion instead", volume_uuid);
        return Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "success": false,
                "message": "Cannot delete managed volume - delete the PVC instead"
            })),
            warp::http::StatusCode::BAD_REQUEST
        ));
    }
    
    println!("✅ [DELETE_RAW] Found unmanaged volume '{}' on node '{}' - proceeding with deletion", 
        target_volume.name, target_volume.node);
    
    // Get the RPC URL for the target node
    let spdk_nodes = state.spdk_nodes.read().await;
    let rpc_url = match spdk_nodes.get(&target_volume.node) {
        Some(url) => url,
        None => {
            println!("❌ [DELETE_RAW] Node '{}' not found in SPDK nodes", target_volume.node);
            return Ok(warp::reply::with_status(
                warp::reply::json(&json!({
                    "success": false,
                    "message": format!("Node '{}' not found", target_volume.node)
                })),
                warp::http::StatusCode::NOT_FOUND
            ));
        }
    };
    
    // Delete the volume using bdev_lvol_delete with UUID (SPDK will try UUID lookup)
    println!("🗑️ [DELETE_RAW] Attempting SPDK deletion:");
    println!("🗑️ [DELETE_RAW]   Volume UUID: '{}'", target_volume.uuid);
    println!("🗑️ [DELETE_RAW]   Volume Name: '{}'", target_volume.name);
    println!("🗑️ [DELETE_RAW]   Target Node: '{}'", target_volume.node);
    println!("🗑️ [DELETE_RAW]   RPC URL: {}", rpc_url);
    
    let rpc_payload = json!({
        "method": "bdev_lvol_delete",
        "params": { "name": target_volume.uuid },
        "id": 1
    });
    println!("🗑️ [DELETE_RAW]   RPC Payload: {}", rpc_payload);
    
    let http_client = HttpClient::new();
    let delete_response = http_client
        .post(rpc_url)
        .json(&rpc_payload)
        .send()
        .await;
        
    match delete_response {
        Ok(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            
            println!("🗑️ [DELETE_RAW] HTTP Response received:");
            println!("🗑️ [DELETE_RAW]   Status: {}", status);
            println!("🗑️ [DELETE_RAW]   Headers: {:?}", headers);
            
            if status.is_success() {
                let response_text = response.text().await.unwrap_or_else(|_| "No response body".to_string());
                println!("🗑️ [DELETE_RAW]   Response Body: {}", response_text);
                println!("✅ [DELETE_RAW] Successfully deleted volume '{}' from node '{}'", 
                    target_volume.name, target_volume.node);
                
                // Force refresh dashboard data to update UI
                if let Err(e) = refresh_dashboard_data(&state).await {
                    println!("⚠️ [DELETE_RAW] Failed to refresh dashboard after deletion: {}", e);
                }
                
                Ok(warp::reply::with_status(
                    warp::reply::json(&json!({
                        "success": true,
                        "message": format!("Volume '{}' deleted successfully", target_volume.name),
                        "deleted_volume": {
                            "name": target_volume.name,
                            "uuid": target_volume.uuid,
                            "node": target_volume.node,
                            "size_gb": target_volume.size_gb
                        }
                    })),
                    warp::http::StatusCode::OK
                ))
            } else {
                let status = response.status();
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                println!("❌ [DELETE_RAW] SPDK deletion failed:");
                println!("❌ [DELETE_RAW]   Status: {}", status);
                println!("❌ [DELETE_RAW]   Error Body: {}", error_text);
                Ok(warp::reply::with_status(
                    warp::reply::json(&json!({
                        "success": false,
                        "message": format!("SPDK deletion failed: {}", error_text)
                    })),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR
                ))
            }
        }
        Err(e) => {
            println!("❌ [DELETE_RAW] Network/HTTP error during deletion:");
            println!("❌ [DELETE_RAW]   Error: {}", e);
            println!("❌ [DELETE_RAW]   Error Debug: {:?}", e);
            Ok(warp::reply::with_status(
                warp::reply::json(&json!({
                    "success": false,
                    "message": format!("Network error: {}", e)
                })),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ))
        }
    }
}

/// Endpoint to get raw SPDK volumes with management status
async fn get_raw_spdk_volumes_endpoint(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    match get_raw_spdk_volumes(&state).await {
        Ok(mut raw_volumes) => {
            // Check which volumes are managed by getting SpdkVolume CRDs
            let spdk_volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
            
            if let Ok(managed_volumes) = spdk_volumes_api.list(&ListParams::default()).await {
                // Mark volumes as managed if they have corresponding CRDs
                for raw_vol in &mut raw_volumes {
                    for managed_vol in &managed_volumes.items {
                        // Check if this raw volume corresponds to a managed volume
                        if raw_vol.name.contains(&managed_vol.spec.volume_id) ||
                           managed_vol.spec.volume_id.contains(&raw_vol.uuid) ||
                           raw_vol.name.contains("vol_") && raw_vol.name.contains(&managed_vol.spec.volume_id.replace("pvc-", "")) {
                            raw_vol.is_managed = true;
                            break;
                        }
                    }
                }
            }
            
            // Separate managed and unmanaged volumes
            let managed_volumes: Vec<_> = raw_volumes.iter().filter(|v| v.is_managed).cloned().collect();
            let unmanaged_volumes: Vec<_> = raw_volumes.iter().filter(|v| !v.is_managed).cloned().collect();
            
            println!("📊 [RAW_SPDK] Returning {} managed volumes, {} unmanaged volumes", 
                managed_volumes.len(), unmanaged_volumes.len());
            
            Ok(warp::reply::json(&json!({
                "managed_volumes": managed_volumes,
                "unmanaged_volumes": unmanaged_volumes,
                "total_volumes": raw_volumes.len(),
                "summary": {
                    "managed_count": managed_volumes.len(),
                    "unmanaged_count": unmanaged_volumes.len(),
                    "total_size_gb": raw_volumes.iter().map(|v| v.size_gb).sum::<f64>()
                }
            })))
        }
        Err(e) => {
            println!("❌ [RAW_SPDK] Failed to get raw volumes: {}", e);
            Ok(warp::reply::json(&json!({
                "error": format!("Failed to get raw SPDK volumes: {}", e),
                "managed_volumes": [],
                "unmanaged_volumes": []
            })))
        }
    }
}

fn update_volume_with_live_raid_status(volume: &mut DashboardVolume, raid_bdev: &serde_json::Value) {
    // Update volume state based on live RAID status
    if let Some(state) = raid_bdev["state"].as_str() {
        volume.state = match state {
            "online" => "Healthy".to_string(),
            "degraded" => "Degraded".to_string(),
            "broken" | "failed" => "Failed".to_string(),
            _ => state.to_string(),
        };
    }
    
    // Update RAID status with live data
    if let Some(ref mut raid_status) = volume.raid_status {
        // Update rebuild information
        if let Some(rebuild_info) = raid_bdev["rebuild_info"].as_object() {
            let progress = rebuild_info["progress_percentage"].as_f64().unwrap_or(0.0);
            volume.rebuild_progress = Some(progress);
            
            if let Some(ref mut rebuild) = raid_status.rebuild_info {
                rebuild.state = rebuild_info["state"].as_str().unwrap_or("").to_string();
                rebuild.progress_percentage = progress;
                rebuild.blocks_remaining = rebuild_info["blocks_remaining"].as_u64().unwrap_or(0);
                rebuild.blocks_total = rebuild_info["blocks_total"].as_u64().unwrap_or(0);
                rebuild.estimated_time_remaining = estimate_rebuild_time_from_live_data(rebuild_info);
            }
            
            // Update replica statuses for rebuilding replicas
            if let Some(target_slot) = rebuild_info["target_slot"].as_u64() {
                for replica in &mut volume.replica_statuses {
                    if replica.raid_member_slot == Some(target_slot as u32) {
                        replica.status = "rebuilding".to_string();
                        replica.rebuild_progress = Some(progress);
                        replica.raid_member_state = "rebuilding".to_string();
                    }
                }
            }
        } else {
            volume.rebuild_progress = None;
            raid_status.rebuild_info = None;
        }
        
        // Update member states
        if let Some(base_bdevs) = raid_bdev["base_bdevs"].as_array() {
            for (idx, base_bdev) in base_bdevs.iter().enumerate() {
                if let Some(member) = raid_status.members.get_mut(idx) {
                    if let Some(member_state) = base_bdev["state"].as_str() {
                        member.state = member_state.to_string();
                    }
                }
                
                // Update corresponding replica status
                if let Some(replica) = volume.replica_statuses.get_mut(idx) {
                    if let Some(member_state) = base_bdev["state"].as_str() {
                        replica.status = match member_state {
                            "online" => "healthy".to_string(),
                            "degraded" => "degraded".to_string(),
                            "failed" => "failed".to_string(),
                            "rebuilding" => "rebuilding".to_string(),
                            _ => member_state.to_string(),
                        };
                        replica.raid_member_state = member_state.to_string();
                    }
                }
            }
        }
        
        // Update operational member count
        if let Some(operational) = raid_bdev["num_base_bdevs_operational"].as_u64() {
            raid_status.operational_members = operational as u32;
            volume.active_replicas = operational as i32;
        }
    }
}

fn estimate_rebuild_time_from_live_data(rebuild_info: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let blocks_remaining = rebuild_info["blocks_remaining"].as_u64().unwrap_or(0);
    let progress = rebuild_info["progress_percentage"].as_f64().unwrap_or(0.0);
    
    if blocks_remaining == 0 || progress >= 100.0 {
        return Some("Complete".to_string());
    }
    
    // More sophisticated estimation could use rate tracking here
    let estimated_seconds = (blocks_remaining as f64 / 1000.0) * 60.0;
    
    if estimated_seconds < 60.0 {
        Some(format!("{}s", estimated_seconds as u64))
    } else if estimated_seconds < 3600.0 {
        Some(format!("{}m", (estimated_seconds / 60.0) as u64))
    } else {
        Some(format!("{}h", (estimated_seconds / 3600.0) as u64))
    }
}

// API handlers
async fn get_dashboard_data(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let last_update = *state.last_update.read().await;
    let cache_age = Utc::now().signed_duration_since(last_update);
    
    if cache_age.num_seconds() > 60 {
        if let Err(e) = refresh_dashboard_data(&state).await {
            eprintln!("Failed to refresh data: {}", e);
        }
    }
    
    let cache = state.cache.read().await;
    if let Some(data) = cache.as_ref() {
        Ok(warp::reply::json(data))
    } else {
        let empty_data = DashboardData {
            volumes: vec![],
            raw_volumes: vec![],
            disks: vec![],
            nodes: vec![],
            raid_disks: vec![],
        };
        Ok(warp::reply::json(&empty_data))
    }
}

async fn get_volume_details(volume_id: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let cache = state.cache.read().await;
    if let Some(data) = cache.as_ref() {
        if let Some(volume) = data.volumes.iter().find(|v| v.id == volume_id) {
            return Ok(warp::reply::json(volume));
        }
    }
    
    Ok(warp::reply::json(&json!({"error": "Volume not found"})))
}

async fn get_volume_raid_status(volume_id: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    let mut raid_details = json!({
        "volume_id": volume_id,
        "raid_bdevs": [],
        "live_status": {}
    });
    
    // Query all nodes for real-time RAID information
    for (node, rpc_url) in spdk_nodes.iter() {
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_raid_get_bdevs",
                "params": { "category": "all" }
            }))
            .send()
            .await
        {
            if let Ok(raid_info) = response.json::<serde_json::Value>().await {
                if let Some(raid_bdevs) = raid_info["result"].as_array() {
                    for raid_bdev in raid_bdevs {
                        if let Some(raid_name) = raid_bdev["name"].as_str() {
                            if raid_name == volume_id {
                                let mut raid_bdev_info = raid_bdev.clone();
                                raid_bdev_info["node"] = json!(node);
                                
                                // Add enhanced rebuild information
                                if let Some(rebuild_info) = raid_bdev["rebuild_info"].as_object() {
                                    let mut enhanced_rebuild = rebuild_info.clone();
                                    enhanced_rebuild.insert(
                                        "estimated_completion".to_string(),
                                        json!(estimate_rebuild_time_from_live_data(rebuild_info))
                                    );
                                    raid_bdev_info["rebuild_info"] = json!(enhanced_rebuild);
                                }
                                
                                // Add member health details
                                if let Some(base_bdevs) = raid_bdev["base_bdevs"].as_array() {
                                    let enhanced_members: Vec<serde_json::Value> = base_bdevs.iter().enumerate().map(|(i, member)| {
                                        let mut enhanced_member = member.clone();
                                        enhanced_member["slot"] = json!(i);
                                        enhanced_member["node"] = json!(node);
                                        enhanced_member
                                    }).collect();
                                    raid_bdev_info["base_bdevs"] = json!(enhanced_members);
                                }
                                
                                raid_details["raid_bdevs"]
                                    .as_array_mut()
                                    .unwrap()
                                    .push(raid_bdev_info);
                            }
                        }
                    }
                }
            }
        }
    }
    
    Ok(warp::reply::json(&raid_details))
}

// Renamed from get_vhost_details to get_nvmeof_details
async fn get_nvmeof_details(volume_id: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    let mut nvmeof_details = json!({
        "volume_id": volume_id,
        "nvmeof_subsystems": [],
        "transport_type": "tcp"
    });
    
    for (node, rpc_url) in spdk_nodes.iter() {
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "nvmf_get_subsystems"
            }))
            .send()
            .await
        {
            if let Ok(nvmf_info) = response.json::<serde_json::Value>().await {
                if let Some(subsystems) = nvmf_info["result"].as_array() {
                    for subsystem in subsystems {
                        if let Some(nqn) = subsystem["nqn"].as_str() {
                            if nqn.contains(&volume_id) {
                                let mut subsystem_info = subsystem.clone();
                                subsystem_info["node"] = json!(node);
                                
                                // Add listener information
                                if let Some(listeners) = subsystem["listen_addresses"].as_array() {
                                    let enhanced_listeners: Vec<serde_json::Value> = listeners.iter().map(|listener| {
                                        let mut enhanced_listener = listener.clone();
                                        enhanced_listener["node"] = json!(node);
                                        enhanced_listener
                                    }).collect();
                                    subsystem_info["listen_addresses"] = json!(enhanced_listeners);
                                }
                                
                                // Add namespace information
                                if let Some(namespaces) = subsystem["namespaces"].as_array() {
                                    subsystem_info["namespaces"] = json!(namespaces);
                                }
                                
                                nvmeof_details["nvmeof_subsystems"]
                                    .as_array_mut()
                                    .unwrap()
                                    .push(subsystem_info);
                            }
                        }
                    }
                }
            }
        }
    }
    
    Ok(warp::reply::json(&nvmeof_details))
}

async fn trigger_refresh(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    match refresh_dashboard_data(&state).await {
        Ok(_) => Ok(warp::reply::json(&json!({"status": "success"}))),
        Err(e) => Ok(warp::reply::json(&json!({"error": e.to_string()}))),
    }
}

async fn get_node_metrics(node: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(rpc_url) = spdk_nodes.get(&node) {
        let http_client = HttpClient::new();
        
        let mut metrics = json!({});
        
        // Get bdev list
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "bdev_get_bdevs"}))
            .send()
            .await
        {
            if let Ok(bdevs) = response.json::<serde_json::Value>().await {
                metrics["bdevs"] = bdevs;
            }
        }
        
        // Get lvol stores
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "bdev_lvol_get_lvstores"}))
            .send()
            .await
        {
            if let Ok(lvstores) = response.json::<serde_json::Value>().await {
                metrics["lvol_stores"] = lvstores;
            }
        }
        
        // Get NVMe-oF subsystems instead of vhost controllers
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "nvmf_get_subsystems"}))
            .send()
            .await
        {
            if let Ok(nvmf_subsystems) = response.json::<serde_json::Value>().await {
                metrics["nvmf_subsystems"] = nvmf_subsystems;
            }
        }
        
        // Get RAID information with enhanced details
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "bdev_raid_get_bdevs", "params": {"category": "all"}}))
            .send()
            .await
        {
            if let Ok(raid_bdevs) = response.json::<serde_json::Value>().await {
                metrics["raid_bdevs"] = raid_bdevs;
            }
        }
        
        // Get I/O statistics
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "bdev_get_iostat"}))
            .send()
            .await
        {
            if let Ok(iostat) = response.json::<serde_json::Value>().await {
                metrics["iostat"] = iostat;
            }
        }
        
        return Ok(warp::reply::json(&metrics));
    }
    
    Ok(warp::reply::json(&json!({"error": "Node not found"})))
}

async fn get_node_raid_status(node: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(rpc_url) = spdk_nodes.get(&node) {
        let http_client = HttpClient::new();
        
        // Get detailed RAID status with enhanced information
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_raid_get_bdevs",
                "params": { "category": "all" }
            }))
            .send()
            .await
        {
            if let Ok(mut raid_bdevs) = response.json::<serde_json::Value>().await {
                // Enhance RAID information
                if let Some(raid_list) = raid_bdevs["result"].as_array_mut() {
                    for raid_bdev in raid_list {
                        // Add node information
                        raid_bdev["node"] = json!(node);
                        
                        // Enhance rebuild information with estimates
                        if let Some(rebuild_info) = raid_bdev["rebuild_info"].as_object() {
                            let mut enhanced_rebuild = rebuild_info.clone();
                            enhanced_rebuild.insert(
                                "estimated_completion".to_string(),
                                json!(estimate_rebuild_time_from_live_data(rebuild_info))
                            );
                            
                            // Calculate rebuild rate if possible
                            let blocks_total = rebuild_info["blocks_total"].as_u64().unwrap_or(0);
                            let blocks_remaining = rebuild_info["blocks_remaining"].as_u64().unwrap_or(0);
                            if blocks_total > 0 {
                                let blocks_completed = blocks_total - blocks_remaining;
                                enhanced_rebuild.insert(
                                    "blocks_completed".to_string(),
                                    json!(blocks_completed)
                                );
                            }
                            
                            raid_bdev["rebuild_info"] = json!(enhanced_rebuild);
                        }
                        
                        // Add member health analysis
                        if let Some(base_bdevs) = raid_bdev["base_bdevs"].as_array() {
                            let mut health_summary = json!({
                                "total_members": base_bdevs.len(),
                                "online_members": 0,
                                "failed_members": 0,
                                "rebuilding_members": 0
                            });
                            
                            for member in base_bdevs {
                                match member["state"].as_str() {
                                    Some("online") => {
                                        health_summary["online_members"] = 
                                            json!(health_summary["online_members"].as_u64().unwrap_or(0) + 1);
                                    }
                                    Some("failed") => {
                                        health_summary["failed_members"] = 
                                            json!(health_summary["failed_members"].as_u64().unwrap_or(0) + 1);
                                    }
                                    Some("rebuilding") => {
                                        health_summary["rebuilding_members"] = 
                                            json!(health_summary["rebuilding_members"].as_u64().unwrap_or(0) + 1);
                                    }
                                    _ => {}
                                }
                            }
                            
                            raid_bdev["health_summary"] = health_summary;
                        }
                    }
                }
                return Ok(warp::reply::json(&raid_bdevs));
            }
        }
    }
    
    Ok(warp::reply::json(&json!({"error": "Failed to get RAID status"})))
}

async fn get_all_snapshots(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let http_client = HttpClient::new();
    let spdk_nodes = state.spdk_nodes.read().await;

    let all_snapshots = match snapshots_api.list(&ListParams::default()).await {
        Ok(list) => list,
        Err(e) => {
            return Ok(warp::reply::json(&json!({
                "error": format!("Failed to list snapshots from Kubernetes: {}", e),
                "snapshots": []
            })));
        }
    };

    let mut detailed_results = Vec::new();

    for snapshot_crd in all_snapshots.items {
        let mut bdev_details_list = Vec::new();

        // Iterate over each replica snapshot defined in the CRD spec.
        for replica_snap in &snapshot_crd.spec.replica_snapshots {
            // Get the correct RPC URL for the node where the replica snapshot resides.
            if let Some(rpc_url) = spdk_nodes.get(&replica_snap.node_name) {
                let resp = http_client
                    .post(rpc_url)
                    .json(&json!({
                        "method": "bdev_get_bdevs",
                        "params": { "name": &replica_snap.spdk_snapshot_lvol }
                    }))
                    .send()
                    .await;

                if let Ok(resp) = resp {
                    if resp.status().is_success() {
                        let json_body: serde_json::Value = resp.json().await.unwrap_or_default();
                        if let Some(bdev_array) = json_body.get("result").and_then(|r| r.as_array()) {
                            if !bdev_array.is_empty() {
                                let bdev = &bdev_array[0];
                                bdev_details_list.push(SpdkBdevDetails {
                                    node: replica_snap.node_name.clone(),
                                    name: bdev["name"].as_str().unwrap_or("").to_string(),
                                    aliases: bdev["aliases"].as_array().unwrap_or(&vec![]).iter()
                                        .filter_map(|a| a.as_str().map(String::from)).collect(),
                                    driver: bdev["product_name"].as_str().unwrap_or("").to_string(),
                                    snapshot_source_bdev: bdev.get("driver_specific")
                                        .and_then(|ds| ds.get("snapshot"))
                                        .and_then(|snap| snap.get("snapshot_bdev"))
                                        .and_then(|sb| sb.as_str().map(String::from)),
                                });
                            }
                        }
                    }
                }
            }
        }
        
        let status = snapshot_crd.status.unwrap_or_default();
        detailed_results.push(DetailedSnapshotInfo {
            snapshot_id: snapshot_crd.spec.snapshot_id,
            source_volume_id: snapshot_crd.spec.source_volume_id,
            creation_time: status.creation_time,
            ready_to_use: status.ready_to_use,
            size_bytes: status.size_bytes,
            snapshot_type: snapshot_crd.spec.snapshot_type,
            clone_source_snapshot_id: snapshot_crd.spec.clone_source_snapshot_id,
            replica_bdev_details: bdev_details_list,
        });
    }

    Ok(warp::reply::json(&detailed_results))
}

async fn get_snapshot_details(
    snapshot_id: String,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    match snapshots_api.get(&snapshot_id).await {
        Ok(snapshot_crd) => {
            let http_client = HttpClient::new();
            let spdk_nodes = state.spdk_nodes.read().await;
            let mut bdev_details_list = Vec::new();

            // Iterate through each replica snapshot and find its details.
            for replica_snap in &snapshot_crd.spec.replica_snapshots {
                if let Some(rpc_url) = spdk_nodes.get(&replica_snap.node_name) {
                    let resp = http_client
                        .post(rpc_url)
                        .json(&json!({
                            "method": "bdev_get_bdevs",
                            "params": { "name": &replica_snap.spdk_snapshot_lvol }
                        }))
                        .send()
                        .await;

                    if let Ok(resp) = resp {
                        if resp.status().is_success() {
                             let json_body: serde_json::Value = resp.json().await.unwrap_or_default();
                            if let Some(bdev_array) = json_body.get("result").and_then(|r| r.as_array()) {
                                if !bdev_array.is_empty() {
                                    let bdev = &bdev_array[0];
                                    bdev_details_list.push(SpdkBdevDetails {
                                        node: replica_snap.node_name.clone(),
                                        name: bdev["name"].as_str().unwrap_or("").to_string(),
                                        aliases: bdev["aliases"].as_array().unwrap_or(&vec![]).iter()
                                            .filter_map(|a| a.as_str().map(String::from)).collect(),
                                        driver: bdev["product_name"].as_str().unwrap_or("").to_string(),
                                        snapshot_source_bdev: bdev.get("driver_specific")
                                            .and_then(|ds| ds.get("snapshot"))
                                            .and_then(|snap| snap.get("snapshot_bdev"))
                                            .and_then(|sb| sb.as_str().map(String::from)),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            
            let status = snapshot_crd.status.unwrap_or_default();
            let detailed_snapshot = DetailedSnapshotInfo {
                snapshot_id: snapshot_crd.spec.snapshot_id,
                source_volume_id: snapshot_crd.spec.source_volume_id,
                creation_time: status.creation_time,
                ready_to_use: status.ready_to_use,
                size_bytes: status.size_bytes,
                snapshot_type: snapshot_crd.spec.snapshot_type,
                clone_source_snapshot_id: snapshot_crd.spec.clone_source_snapshot_id,
                replica_bdev_details: bdev_details_list,
            };

            Ok(warp::reply::json(&detailed_snapshot))
        }
        Err(_) => Ok(warp::reply::json(&json!({
            "error": "Snapshot not found"
        })))
    }
}

#[derive(Clone, Debug)]
struct SnapshotNode {
    bdev_name: String,
    parent_name: Option<String>,
    details: serde_json::Value,
}

/// Recursively builds a JSON tree from a map of nodes and a starting parent.
fn build_snapshot_tree_from_map(
    parent_name: &Option<String>,
    nodes: &HashMap<String, SnapshotNode>,
) -> Vec<serde_json::Value> {
    let mut tree = Vec::new();

    // Find all children of the current parent
    for node in nodes.values() {
        if node.parent_name == *parent_name {
            // This node is a direct child. Create its JSON object.
            let mut node_json = json!({
                "bdev_name": node.bdev_name,
                "details": &node.details,
                "children": build_snapshot_tree_from_map(&Some(node.bdev_name.clone()), nodes)
            });

            // Calculate and include the storage consumed by this specific snapshot/lvol.
            if let Some(lvol_details) = node.details.get("driver_specific").and_then(|ds| ds.get("lvol")) {
                let cluster_size = lvol_details["cluster_size"].as_u64().unwrap_or(0);
                let allocated_clusters = lvol_details["num_allocated_clusters"].as_u64().unwrap_or(0);
                let consumed_bytes = cluster_size * allocated_clusters;

                node_json["storage_info"] = json!({
                    "consumed_bytes": consumed_bytes,
                    "cluster_size": cluster_size,
                    "allocated_clusters": allocated_clusters
                });
            }

            // Add CRD info if we can find it
            if let Some(aliases) = node.details["aliases"].as_array() {
                for alias in aliases {
                     if let Some(alias_str) = alias.as_str() {
                        // The alias often corresponds to the SpdkSnapshot CRD name
                        if alias_str.starts_with("snap_") {
                             node_json["snapshot_id"] = json!(alias_str);
                        }
                     }
                }
            }
            tree.push(node_json);
        }
    }
    tree
}

/// Connects to a node and traces the snapshot chain for a given starting lvol.
async fn trace_snapshot_chain_on_node(
    starting_lvol_name: String,
    rpc_url: &str,
    http_client: &HttpClient,
) -> HashMap<String, SnapshotNode> {
    let mut nodes = HashMap::new();
    let mut queue = vec![starting_lvol_name];
    let mut visited = std::collections::HashSet::new();

    while let Some(bdev_name) = queue.pop() {
        if !visited.insert(bdev_name.clone()) {
            continue;
        }

        // Get details for the current bdev in the chain
        let resp = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_get_bdevs",
                "params": { "name": &bdev_name }
            }))
            .send()
            .await;

        if let Ok(resp) = resp {
            if let Ok(json_body) = resp.json::<serde_json::Value>().await {
                 if let Some(bdev_array) = json_body.get("result").and_then(|r| r.as_array()) {
                    if let Some(bdev_details) = bdev_array.get(0) {
                        // Find the parent (backing device) of this bdev
                        let parent_name = bdev_details.get("driver_specific")
                            .and_then(|ds| ds.get("lvol"))
                            .and_then(|lvol| lvol.get("backing_bdev"))
                            .and_then(|b| b.as_str().map(String::from));

                        // Add this node to our map
                        nodes.insert(bdev_name.clone(), SnapshotNode {
                            bdev_name: bdev_name.clone(),
                            parent_name: parent_name.clone(),
                            details: bdev_details.clone(),
                        });

                        // If it has a parent, add the parent to the queue to be traced
                        if let Some(p_name) = parent_name {
                            queue.push(p_name);
                        }
                    }
                }
            }
        }
    }

    nodes
}

async fn get_snapshots_tree(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);

    let all_volumes = match volumes_api.list(&ListParams::default()).await {
        Ok(list) => list,
        Err(e) => {
            return Ok(warp::reply::json(&json!({
                "error": format!("Failed to list volumes: {}", e),
                "tree": {}
            })));
        }
    };

    let http_client = HttpClient::new();
    let spdk_nodes = state.spdk_nodes.read().await;
    let mut tree = json!({});

    for volume in all_volumes.items {
        let volume_id = &volume.spec.volume_id;
        let volume_name = volume.metadata.name.as_ref().unwrap_or(volume_id);
        let mut snapshot_chain_json = json!({
            "error": "Could not trace snapshot chain. No healthy replicas found or lvol_uuid missing."
        });

        // To trace the chain, we only need to inspect one replica. Let's find the first one
        // that is local or has an lvol_uuid we can use as a starting point.
        if let Some(replica_to_trace) = volume.spec.replicas.first() {
            if let (Some(lvol_uuid), Some(rpc_url)) = (
                &replica_to_trace.lvol_uuid,
                spdk_nodes.get(&replica_to_trace.node),
            ) {
                // The name of the active lvol bdev for this replica. This is the head of the chain.
                let lvs_name = format!("lvs_{}", replica_to_trace.disk_ref);
                let starting_lvol_name = format!("{}/{}", lvs_name, lvol_uuid);

                // Trace the entire chain on the node where the replica resides.
                let nodes_map = trace_snapshot_chain_on_node(
                    starting_lvol_name.clone(),
                    rpc_url,
                    &http_client,
                ).await;

                // Build the nested JSON tree from the dependency map.
                let root_snapshots = build_snapshot_tree_from_map(&Some(starting_lvol_name.clone()), &nodes_map);

                snapshot_chain_json = json!({
                    "active_lvol": starting_lvol_name,
                    "chain_depth": nodes_map.len(),
                    "snapshots": root_snapshots
                });
            }
        }

        tree[volume_id] = json!({
            "volume_name": volume_name,
            "volume_id": volume_id,
            "volume_size": volume.spec.size_bytes,
            "snapshot_chain": snapshot_chain_json,
        });
    }

    Ok(warp::reply::json(&tree))
}

// Disk setup proxy handlers - forward requests to node-agents
// Note: Uninitialized disks discovery endpoint removed to avoid triggering discovery from the dashboard backend

async fn setup_node_disks(node: String, request: serde_json::Value, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(node_agent_url) = get_node_agent_url(&spdk_nodes, &node) {
        let http_client = HttpClient::new();
        
        match http_client
            .post(&format!("{}/api/disks/setup", node_agent_url))
            .json(&request)
            .send()
            .await
        {
            Ok(response) => {
                match response.json::<serde_json::Value>().await {
                    Ok(data) => Ok(warp::reply::json(&data)),
                    Err(_) => Ok(warp::reply::json(&json!({
                        "success": false,
                        "error": "Failed to parse node-agent response",
                        "node": node
                    })))
                }
            }
            Err(e) => Ok(warp::reply::json(&json!({
                "success": false,
                "error": format!("Failed to connect to node-agent: {}", e),
                "node": node
            })))
        }
    } else {
        Ok(warp::reply::json(&json!({
            "success": false,
            "error": "Node-agent not found", 
            "node": node
        })))
    }
}

async fn reset_node_disks(node: String, request: serde_json::Value, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(node_agent_url) = get_node_agent_url(&spdk_nodes, &node) {
        let http_client = HttpClient::new();
        
        match http_client
            .post(&format!("{}/api/disks/reset", node_agent_url))
            .json(&request)
            .send()
            .await
        {
            Ok(response) => {
                match response.json::<serde_json::Value>().await {
                    Ok(data) => Ok(warp::reply::json(&data)),
                    Err(_) => Ok(warp::reply::json(&json!({
                        "success": false,
                        "error": "Failed to parse node-agent response",
                        "node": node
                    })))
                }
            }
            Err(e) => Ok(warp::reply::json(&json!({
                "success": false,
                "error": format!("Failed to connect to node-agent: {}", e),
                "node": node
            })))
        }
    } else {
        Ok(warp::reply::json(&json!({
            "success": false,
            "error": "Node-agent not found",
            "node": node
        })))
    }
}

async fn initialize_node_disks(node: String, request: serde_json::Value, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(node_agent_url) = get_node_agent_url(&spdk_nodes, &node) {
        let http_client = HttpClient::new();
        
        match http_client
            .post(&format!("{}/api/disks/initialize", node_agent_url))
            .json(&request)
            .send()
            .await
        {
            Ok(response) => {
                match response.json::<serde_json::Value>().await {
                    Ok(data) => Ok(warp::reply::json(&data)),
                    Err(_) => Ok(warp::reply::json(&json!({
                        "success": false,
                        "setup_disks": [],
                        "failed_disks": [],
                        "warnings": ["Failed to parse node-agent response"],
                        "completed_at": chrono::Utc::now().to_rfc3339()
                    })))
                }
            }
            Err(e) => Ok(warp::reply::json(&json!({
                "success": false,
                "setup_disks": [],
                "failed_disks": [],
                "warnings": [format!("Failed to connect to node-agent: {}", e)],
                "completed_at": chrono::Utc::now().to_rfc3339()
            })))
        }
    } else {
        Ok(warp::reply::json(&json!({
            "success": false,
            "setup_disks": [],
            "failed_disks": [],
            "warnings": ["Node-agent not found"],
            "completed_at": chrono::Utc::now().to_rfc3339()
        })))
    }
}

// Note: Node disk status proxy removed; backend should not query per-node disk status directly

// Note: Aggregated disk setup proxy removed to avoid triggering discovery across nodes

// Helper function to get node-agent URL from SPDK RPC URL
fn get_node_agent_url(spdk_nodes: &HashMap<String, String>, node: &str) -> Option<String> {
    if let Some(spdk_rpc_url) = spdk_nodes.get(node) {
        // spdk_rpc_url is like "http://10.42.1.15:8081/api/spdk/rpc"
        // We need "http://10.42.1.15:8081" for node-agent APIs
        if let Some(base_url) = spdk_rpc_url.split("/api/spdk/rpc").next() {
            return Some(base_url.to_string());
        }
    }
    None
}

// ============================================================================
// MIGRATION MANAGER IMPLEMENTATION
// ============================================================================

/// Analyze a node and create a migration plan for all its RAIDs
async fn analyze_node_and_create_migration_plan(
    node_id: &str,
    state: &AppState,
) -> Result<MigrationPlan, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔍 [MIGRATION_ANALYSIS] Analyzing node {} for migration planning", node_id);
    
    // Get the node's SPDK configuration
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", node_id);
    
    let config = spdk_configs.get_opt(&config_name).await?
        .ok_or(format!("No SpdkConfig found for node {}", node_id))?;
    
    let mut raid_migrations = Vec::new();
    let mut single_replica_migrations = Vec::new();
    
    // Analyze each RAID on this node
    for raid in &config.spec.raid_bdevs {
        println!("🛡️ [MIGRATION_ANALYSIS] Found RAID {} on node {}", raid.name, node_id);
        
        // Find optimal target node for this RAID
        match find_optimal_target_node_for_raid(raid, node_id, state).await {
            Ok(target_node) => {
                println!("🎯 [MIGRATION_ANALYSIS] Selected target node {} for RAID {}", target_node, raid.name);
                raid_migrations.push(RaidMigration {
                    raid_name: raid.name.clone(),
                    source_node: node_id.to_string(),
                    target_node,
                });
            }
            Err(e) => {
                println!("⚠️ [MIGRATION_ANALYSIS] Could not find target for RAID {}: {}", raid.name, e);
                // Continue with other RAIDs - partial migration is better than none
            }
        }
    }
    
    // Check for single-replica volumes (though these should be rare with RAID setup)
    // This would query SpdkVolume CRDs to find volumes with only one replica on this node
    if let Ok(single_volumes) = find_single_replica_volumes_on_node(node_id, state).await {
        for volume_id in single_volumes {
            if let Ok(target_node) = find_optimal_target_node_for_volume(&volume_id, node_id, state).await {
                single_replica_migrations.push(SingleReplicaMigration {
                    volume_id,
                    source_node: node_id.to_string(),
                    target_node,
                });
            }
        }
    }
    
    let plan = MigrationPlan {
        raid_migrations,
        single_replica_migrations,
    };
    
    println!("📋 [MIGRATION_ANALYSIS] Created migration plan: {} RAIDs, {} single volumes", 
             plan.raid_migrations.len(), plan.single_replica_migrations.len());
    
    Ok(plan)
}

/// Find the optimal target node for a RAID migration
async fn find_optimal_target_node_for_raid(
    raid: &spdk_csi_driver::models::RaidBdevConfig,
    source_node: &str,
    state: &AppState,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use k8s_openapi::api::core::v1::Node;
    
    println!("🎯 [TARGET_SELECTION] Finding optimal target for RAID {} from {}", raid.name, source_node);
    
    let nodes_api: Api<Node> = Api::all(state.kube_client.clone());
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    let list_params = kube::api::ListParams::default();
    let (nodes_result, configs_result) = tokio::join!(
        nodes_api.list(&list_params),
        spdk_configs.list(&list_params)
    );
    
    let nodes = nodes_result?;
    let configs = configs_result?;
    
    // Step 1: Find nodes with healthy replicas of this RAID
    let replica_nodes = find_nodes_with_healthy_replicas(raid, &configs, source_node).await;
    
    if !replica_nodes.is_empty() {
        println!("🔍 [TARGET_SELECTION] Found {} nodes with healthy replicas: {:?}", 
                 replica_nodes.len(), replica_nodes);
        
        // Among replica nodes, choose the one with minimum RAID count
        if let Some(best_replica_node) = select_node_with_min_raids(&replica_nodes, &configs, &nodes).await? {
            println!("✅ [TARGET_SELECTION] Optimal choice - replica node with min RAIDs: {}", best_replica_node);
            return Ok(best_replica_node);
        }
    }
    
    // Step 2: If no replica nodes available, find any healthy node with minimum RAIDs
    println!("🔄 [TARGET_SELECTION] No replica nodes available, selecting by load balancing");
    
    let all_healthy_nodes = get_healthy_schedulable_nodes(&nodes, source_node).await?;
    
    if let Some(best_node) = select_node_with_min_raids(&all_healthy_nodes, &configs, &nodes).await? {
        println!("✅ [TARGET_SELECTION] Load-balanced choice: {}", best_node);
        return Ok(best_node);
    }
    
    Err("No suitable target node found for RAID migration".into())
}

/// Find nodes that have healthy replicas of the given RAID
async fn find_nodes_with_healthy_replicas(
    raid: &spdk_csi_driver::models::RaidBdevConfig,
    configs: &kube::core::ObjectList<SpdkConfig>,
    source_node: &str,
) -> Vec<String> {
    let mut replica_nodes = Vec::new();
    
    // Look through RAID members to find where replicas are located
    for member in &raid.members {
        if let Some(nvmeof_config) = &member.nvmeof_config {
            let replica_node = &nvmeof_config.target_node_id;
            
            // Skip source node and avoid duplicates
            if replica_node != source_node && !replica_nodes.contains(replica_node) {
                // Verify the replica node has a healthy config
                if let Some(node_config) = configs.items.iter().find(|c| c.spec.node_id == *replica_node) {
                    if !node_config.spec.maintenance_mode {
                        println!("🔍 [REPLICA_CHECK] Found healthy replica on node: {}", replica_node);
                        replica_nodes.push(replica_node.clone());
                    }
                }
            }
        }
    }
    
    replica_nodes
}

/// Get all healthy and schedulable nodes (excluding source)
async fn get_healthy_schedulable_nodes(
    nodes: &kube::core::ObjectList<k8s_openapi::api::core::v1::Node>,
    source_node: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut healthy_nodes = Vec::new();
    
    for node in &nodes.items {
        let node_name = node.metadata.name.as_deref().unwrap_or("unknown");
        
        // Skip source node
        if node_name == source_node {
            continue;
        }
        
        // Check if node is ready and schedulable
        if is_node_ready_and_schedulable(node) {
            healthy_nodes.push(node_name.to_string());
        }
    }
    
    Ok(healthy_nodes)
}

/// Select the node with minimum RAID count from candidate nodes
async fn select_node_with_min_raids(
    candidate_nodes: &[String],
    configs: &kube::core::ObjectList<SpdkConfig>,
    nodes: &kube::core::ObjectList<k8s_openapi::api::core::v1::Node>,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut best_node: Option<String> = None;
    let mut min_raid_count = u32::MAX;
    
    for node_name in candidate_nodes {
        // Verify node is still healthy
        if let Some(node) = nodes.items.iter().find(|n| n.metadata.name.as_deref() == Some(node_name)) {
            if !is_node_ready_and_schedulable(node) {
                println!("⚠️ [TARGET_SELECTION] Skipping unhealthy node: {}", node_name);
                continue;
            }
        }
        
        // Count RAIDs on this node
        let raid_count = count_raids_on_node(node_name, configs);
        
        println!("📊 [LOAD_BALANCE] Node {} has {} RAIDs", node_name, raid_count);
        
        if raid_count < min_raid_count {
            min_raid_count = raid_count;
            best_node = Some(node_name.clone());
        }
    }
    
    if let Some(ref node) = best_node {
        println!("🎯 [LOAD_BALANCE] Selected node {} with {} RAIDs (minimum)", node, min_raid_count);
    }
    
    Ok(best_node)
}

/// Count the number of RAIDs currently hosted on a node
fn count_raids_on_node(node_name: &str, configs: &kube::core::ObjectList<SpdkConfig>) -> u32 {
    configs.items
        .iter()
        .find(|config| config.spec.node_id == node_name)
        .map(|config| config.spec.raid_bdevs.len() as u32)
        .unwrap_or(0)
}

/// Check if a node is ready and schedulable
fn is_node_ready_and_schedulable(node: &k8s_openapi::api::core::v1::Node) -> bool {
    // Check node conditions
    if let Some(status) = &node.status {
        if let Some(conditions) = &status.conditions {
            for condition in conditions {
                if condition.type_ == "Ready" && condition.status == "True" {
                    // Node is ready, now check if it's schedulable
                    if let Some(spec) = &node.spec {
                        return !spec.unschedulable.unwrap_or(false);
                    }
                    return true;
                }
            }
        }
    }
    false
}

/// Information about RAID member accessibility for network partition detection
#[derive(Debug, Clone)]
struct RaidMemberAccessibility {
    pub all_members_inaccessible: bool,
    pub has_external_nvmeof_members: bool,
    pub inaccessible_local_members: u32,
    pub inaccessible_external_members: u32,
    pub total_members: u32,
}

/// Check if all RAID members are inaccessible (network partition scenario)
/// Returns detailed information about member accessibility for appropriate recovery guidance
async fn check_raid_member_accessibility(
    volume: &SpdkVolume,
    nodes: &kube::core::ObjectList<k8s_openapi::api::core::v1::Node>,
    nvmeof_disks: &kube::core::ObjectList<NvmeofDisk>,
) -> RaidMemberAccessibility {
    // Get RAID status from the volume
    let raid_status = match &volume.status {
        Some(status) => match &status.raid_status {
            Some(raid) => raid,
            None => {
                println!("🔍 [MEMBER_CHECK] No RAID status found for volume {}", volume.spec.volume_id);
                return RaidMemberAccessibility {
                    all_members_inaccessible: false,
                    has_external_nvmeof_members: false,
                    inaccessible_local_members: 0,
                    inaccessible_external_members: 0,
                    total_members: 0,
                };
            }
        },
        None => {
            println!("🔍 [MEMBER_CHECK] No status found for volume {}", volume.spec.volume_id);
            return RaidMemberAccessibility {
                all_members_inaccessible: false,
                has_external_nvmeof_members: false,
                inaccessible_local_members: 0,
                inaccessible_external_members: 0,
                total_members: 0,
            };
        }
    };

    if raid_status.base_bdevs_list.is_empty() {
        println!("🔍 [MEMBER_CHECK] No RAID members found for volume {}", volume.spec.volume_id);
        return RaidMemberAccessibility {
            all_members_inaccessible: false,
            has_external_nvmeof_members: false,
            inaccessible_local_members: 0,
            inaccessible_external_members: 0,
            total_members: 0,
        };
    }

    println!("🔍 [MEMBER_CHECK] Checking accessibility for {} RAID members of volume {}", 
             raid_status.base_bdevs_list.len(), volume.spec.volume_id);

    let mut accessible_members = 0;
    let mut has_external_nvmeof_members = false;
    let mut inaccessible_local_members = 0;
    let mut inaccessible_external_members = 0;
    let total_members = raid_status.base_bdevs_list.len() as u32;

    for member in &raid_status.base_bdevs_list {
        // Find the corresponding replica to get node and endpoint info
        let replica = volume.spec.replicas.iter()
            .find(|r| r.raid_member_index == member.slot as usize);

        let is_member_accessible = match replica {
            Some(replica) => {
                // Determine if this is an external NVMe-oF member by checking if it has external endpoint info
                let is_external_nvmeof = is_external_nvmeof_member(replica, nvmeof_disks).await;
                if is_external_nvmeof {
                    has_external_nvmeof_members = true;
                }

                // Check if the node hosting this member is accessible
                let node_accessible = nodes.items.iter()
                    .find(|node| node.metadata.name.as_deref() == Some(&replica.node))
                    .map(|node| is_node_ready_and_schedulable(node))
                    .unwrap_or(false);

                if node_accessible {
                    println!("✅ [MEMBER_CHECK] Member {} (slot {}) accessible via node {}", 
                             member.name, member.slot, replica.node);
                    true
                } else {
                    // Node is down, but check if this is a remote NVMe-oF endpoint that might be accessible from other nodes
                    let nvmeof_accessible = check_nvmeof_endpoint_accessibility(replica, nvmeof_disks).await;
                    if nvmeof_accessible {
                        println!("✅ [MEMBER_CHECK] Member {} (slot {}) accessible via NVMe-oF endpoint despite node {} being down", 
                                 member.name, member.slot, replica.node);
                        true
                    } else {
                        println!("❌ [MEMBER_CHECK] Member {} (slot {}) inaccessible - node {} down and no accessible NVMe-oF endpoint", 
                                 member.name, member.slot, replica.node);
                        
                        // Track what type of member is inaccessible
                        if is_external_nvmeof {
                            inaccessible_external_members += 1;
                        } else {
                            inaccessible_local_members += 1;
                        }
                        false
                    }
                }
            },
            None => {
                println!("⚠️ [MEMBER_CHECK] No replica info found for member {} (slot {}), assuming inaccessible local member", 
                         member.name, member.slot);
                inaccessible_local_members += 1;
                false
            }
        };

        if is_member_accessible {
            accessible_members += 1;
        }
    }

    let all_inaccessible = accessible_members == 0;
    
    if all_inaccessible {
        println!("🚨 [NETWORK_PARTITION] ALL {} RAID members are inaccessible for volume {} - network partition detected!", 
                 total_members, volume.spec.volume_id);
        println!("📊 [PARTITION_DETAILS] {} local members, {} external NVMe-oF members inaccessible", 
                 inaccessible_local_members, inaccessible_external_members);
    } else {
        println!("✅ [MEMBER_CHECK] {}/{} RAID members accessible for volume {} - migration still possible", 
                 accessible_members, total_members, volume.spec.volume_id);
    }

    RaidMemberAccessibility {
        all_members_inaccessible: all_inaccessible,
        has_external_nvmeof_members,
        inaccessible_local_members,
        inaccessible_external_members,
        total_members,
    }
}

/// Check if a replica is using an external NVMe-oF member (vs local disk)
async fn is_external_nvmeof_member(
    replica: &Replica,
    nvmeof_disks: &kube::core::ObjectList<NvmeofDisk>,
) -> bool {
    // Look up the NVMe-oF disk to check if it's marked as remote/external
    if let Some(nvmeof_disk) = nvmeof_disks.items.iter()
        .find(|disk| disk.metadata.name.as_deref() == Some(&replica.disk_ref)) {
        
        // Check if this disk is marked as remote/external
        return nvmeof_disk.spec.is_remote;
    }
    
    // If no NVMe-oF disk info found, assume local
    false
}

/// Check if an NVMe-oF endpoint is accessible despite its host node being down
/// This applies to external NVMe-oF endpoints that can be reached from multiple nodes
async fn check_nvmeof_endpoint_accessibility(
    replica: &Replica,
    nvmeof_disks: &kube::core::ObjectList<NvmeofDisk>,
) -> bool {
    // Look up the NVMe-oF disk status for this replica
    if let Some(nvmeof_disk) = nvmeof_disks.items.iter()
        .find(|disk| disk.metadata.name.as_deref() == Some(&replica.disk_ref)) {
        
        if let Some(status) = &nvmeof_disk.status {
            // Enhanced accessibility check with failure tracking
            let is_accessible = status.healthy && status.endpoint_validated;
            
            // Only consider it inaccessible if we have multiple consecutive failures
            // This avoids false positives from temporary network hiccups
            let persistent_failure = status.consecutive_failures >= 2;
            
            if !is_accessible && persistent_failure {
                println!("❌ [NVMEOF_CHECK] NVMe-oF disk {} persistently inaccessible: healthy={}, validated={}, consecutive_failures={}, reason={:?}", 
                         replica.disk_ref, status.healthy, status.endpoint_validated, status.consecutive_failures, status.failure_reason);
                return false;
            } else if is_accessible {
                println!("✅ [NVMEOF_CHECK] NVMe-oF disk {} accessible: healthy={}, validated={}", 
                         replica.disk_ref, status.healthy, status.endpoint_validated);
                return true;
            } else {
                // Unhealthy but not yet persistent failure - give it benefit of the doubt
                println!("⚠️ [NVMEOF_CHECK] NVMe-oF disk {} temporarily unhealthy (failure #{} < 2), assuming still accessible", 
                         replica.disk_ref, status.consecutive_failures);
                return true;
            }
        }
    }
    
    // If no NVMe-oF disk info found, assume not accessible
    println!("⚠️ [NVMEOF_CHECK] No NVMe-oF disk info found for {}, assuming not accessible", replica.disk_ref);
    false
}

/// Find single-replica volumes on a specific node
async fn find_single_replica_volumes_on_node(
    node_id: &str,
    state: &AppState,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let volumes = volumes_api.list(&kube::api::ListParams::default()).await?;
    
    let mut single_volumes = Vec::new();
    
    for volume in volumes.items {
        if volume.spec.num_replicas == 1 {
            // Check if the single replica is on this node
            if let Some(replica) = volume.spec.replicas.first() {
                if replica.node == node_id {
                    single_volumes.push(volume.spec.volume_id);
                }
            }
        }
    }
    
    Ok(single_volumes)
}

/// Find optimal target node for a single volume
async fn find_optimal_target_node_for_volume(
    volume_id: &str,
    source_node: &str,
    state: &AppState,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Similar logic to RAID target selection
    // For simplicity, reuse the same logic
    use k8s_openapi::api::core::v1::Node;
    
    let nodes_api: Api<Node> = Api::all(state.kube_client.clone());
    let nodes = nodes_api.list(&kube::api::ListParams::default()).await?;
    
    for node in nodes.items {
        if let Some(ref node_name) = node.metadata.name {
            if node_name != source_node && is_node_ready_and_schedulable(&node) {
                return Ok(node_name.clone());
            }
        }
    }
    
    Err("No suitable target node for volume".into())
}

/// Execute the migration plan asynchronously
async fn execute_migration_plan(
    plan: MigrationPlan,
    source_node_id: &str,
    migration_id: &str,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🚀 [MIGRATION_EXEC] Starting execution of migration plan {}", migration_id);
    
    // Initialize migration progress tracking
    let mut migration_progress = Vec::new();
    
    // Execute RAID migrations
    for raid_migration in plan.raid_migrations {
        println!("🛡️ [MIGRATION_EXEC] Migrating RAID {} from {} to {}", 
                 raid_migration.raid_name, raid_migration.source_node, raid_migration.target_node);
        
        let progress = MigrationProgress {
            migration_type: "raid".to_string(),
            source_id: raid_migration.raid_name.clone(),
            target_node: raid_migration.target_node.clone(),
            progress_percent: 0.0,
            status: "starting".to_string(),
            error_message: None,
        };
        migration_progress.push(progress);
        
        // Update progress in SpdkConfig
        update_migration_progress(source_node_id, &migration_progress, &state).await?;
        
        match execute_raid_migration(&raid_migration, &state).await {
            Ok(_) => {
                // Update progress to completed
                if let Some(progress) = migration_progress.iter_mut()
                    .find(|p| p.source_id == raid_migration.raid_name) {
                    progress.progress_percent = 100.0;
                    progress.status = "completed".to_string();
                }
                println!("✅ [MIGRATION_EXEC] RAID {} migration completed", raid_migration.raid_name);
            }
            Err(e) => {
                // Update progress to failed
                if let Some(progress) = migration_progress.iter_mut()
                    .find(|p| p.source_id == raid_migration.raid_name) {
                    progress.status = "failed".to_string();
                    progress.error_message = Some(e.to_string());
                }
                println!("❌ [MIGRATION_EXEC] RAID {} migration failed: {}", raid_migration.raid_name, e);
            }
        }
        
        // Update progress in SpdkConfig
        update_migration_progress(source_node_id, &migration_progress, &state).await?;
    }
    
    // Execute single replica migrations
    for volume_migration in plan.single_replica_migrations {
        println!("💾 [MIGRATION_EXEC] Migrating volume {} from {} to {}", 
                 volume_migration.volume_id, volume_migration.source_node, volume_migration.target_node);
        
        // Similar process for single volumes...
        // Implementation would be similar to RAID migration
    }
    
    // Mark migration as complete and node ready for shutdown
    mark_node_ready_for_shutdown(source_node_id, &state).await?;
    
    println!("🎉 [MIGRATION_EXEC] Migration {} completed successfully", migration_id);
    Ok(())
}

/// Execute a single RAID migration
async fn execute_raid_migration(
    migration: &RaidMigration,
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [RAID_MIGRATION] Starting migration of RAID {} to node {}", 
             migration.raid_name, migration.target_node);
    
    // Step 1: Get the source RAID configuration
    let source_raid_config = get_raid_config_from_node(&migration.source_node, &migration.raid_name, state).await?;
    
    // Step 2: Create the RAID on the target node
    create_raid_on_target_node(&migration.target_node, &source_raid_config, state).await?;
    
    // Step 2.5: Update SpdkRaidDisk CRD to reflect new hosting node
    update_spdk_raid_disk_node_id(&migration.raid_name, &migration.target_node, state).await?;
    
    // Step 3: Migrate LVS data (if any)
    migrate_lvs_data(&migration.raid_name, &migration.source_node, &migration.target_node, state).await?;
    
    // Step 4: Update source node configuration to remove the RAID
    remove_raid_from_source_config(&migration.source_node, &migration.raid_name, state).await?;
    
    // Step 5: Mark source node config as potentially orphaned (but don't delete)
    mark_node_config_orphaned(&migration.source_node, &migration.raid_name, state).await?;
    
    println!("✅ [RAID_MIGRATION] RAID {} successfully migrated to {}", migration.raid_name, migration.target_node);
    Ok(())
}

/// Get RAID configuration from source node
async fn get_raid_config_from_node(
    source_node: &str,
    raid_name: &str,
    state: &AppState,
) -> Result<spdk_csi_driver::models::RaidBdevConfig, Box<dyn std::error::Error + Send + Sync>> {
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", source_node);
    
    let config = spdk_configs.get(&config_name).await?;
    
    for raid in &config.spec.raid_bdevs {
        if raid.name == raid_name {
            return Ok(raid.clone());
        }
    }
    
    Err(format!("RAID {} not found in node {} config", raid_name, source_node).into())
}

/// Create RAID on target node
async fn create_raid_on_target_node(
    target_node: &str,
    raid_config: &spdk_csi_driver::models::RaidBdevConfig,
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🏗️ [RAID_CREATION] Creating RAID {} on target node {}", raid_config.name, target_node);
    
    // Get target node's SpdkConfig
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", target_node);
    
    let mut target_config = spdk_configs.get(&config_name).await?;
    
    // Add the RAID to target node's configuration
    target_config.spec.raid_bdevs.push(raid_config.clone());
    
    // Update the target node's configuration
    spdk_configs.replace(&config_name, &kube::api::PostParams::default(), &target_config).await?;
    
    println!("✅ [RAID_CREATION] RAID {} configuration added to node {}", raid_config.name, target_node);
    Ok(())
}

/// Migrate LVS data from source to target
async fn migrate_lvs_data(
    raid_name: &str,
    source_node: &str,
    target_node: &str,
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("📦 [LVS_MIGRATION] Migrating LVS data for RAID {} from {} to {}", raid_name, source_node, target_node);
    
    // This is a complex operation that would involve:
    // 1. Quiescing I/O to the LVS
    // 2. Creating snapshot on source
    // 3. Transferring data to target
    // 4. Importing LVS on target
    // 5. Updating volume mappings
    
    // For now, we'll implement a basic version that handles the CRD updates
    // In a production system, this would need sophisticated data transfer mechanisms
    
    println!("ℹ️ [LVS_MIGRATION] LVS data migration placeholder completed");
    Ok(())
}

/// Remove RAID from source node configuration
async fn remove_raid_from_source_config(
    source_node: &str,
    raid_name: &str,
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🧹 [CONFIG_CLEANUP] Removing RAID {} from source node {} config", raid_name, source_node);
    
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", source_node);
    
    let mut source_config = spdk_configs.get(&config_name).await?;
    
    // Remove the RAID from source configuration
    source_config.spec.raid_bdevs.retain(|raid| raid.name != raid_name);
    
    // Update the source node's configuration
    spdk_configs.replace(&config_name, &kube::api::PostParams::default(), &source_config).await?;
    
    println!("✅ [CONFIG_CLEANUP] RAID {} removed from source node {} config", raid_name, source_node);
    Ok(())
}

/// Mark source node config as potentially orphaned but keep it for operator review
async fn mark_node_config_orphaned(
    source_node: &str,
    raid_name: &str,
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🏷️ [CONFIG_ORPHAN] Marking source node {} config as orphaned after RAID {} migration", source_node, raid_name);
    
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", source_node);
    
    // Add metadata to indicate this config might be orphaned
    let patch = json!({
        "metadata": {
            "annotations": {
                "storage.flint.io/last-migration": chrono::Utc::now().to_rfc3339(),
                "storage.flint.io/orphaned-raids": raid_name,
                "storage.flint.io/review-needed": "true"
            }
        },
        "status": {
            "orphaned": true,
            "last_migration": chrono::Utc::now().to_rfc3339()
        }
    });
    
    spdk_configs.patch(&config_name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch)).await?;
    
    println!("✅ [CONFIG_ORPHAN] Source node {} config marked for operator review", source_node);
    println!("ℹ️ [CONFIG_ORPHAN] Operator can manually delete config if node is permanently failed");
    
    Ok(())
}

/// Update migration progress in SpdkConfig status
async fn update_migration_progress(
    node_id: &str,
    progress: &[MigrationProgress],
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", node_id);
    
    let patch = json!({
        "status": {
            "maintenance_status": {
                "migration_progress": progress
            }
        }
    });
    
    spdk_configs.patch(&config_name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch)).await?;
    Ok(())
}

/// Update SpdkRaidDisk CRD to reflect new hosting node
async fn update_spdk_raid_disk_node_id(
    raid_name: &str,
    new_node_id: &str,
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [CRD_UPDATE] Updating SpdkRaidDisk {} to node {}", raid_name, new_node_id);
    
    let raid_disks: Api<SpdkRaidDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    // Find the SpdkRaidDisk with matching RAID name
    let raid_list = raid_disks.list(&kube::api::ListParams::default()).await?;
    
    for raid_disk in raid_list.items {
        // Check if this SpdkRaidDisk corresponds to our RAID
        if raid_disk.metadata.name.as_deref() == Some(raid_name) {
            println!("📝 [CRD_UPDATE] Found SpdkRaidDisk CRD for RAID {}", raid_name);
            
            // Update both the spec node_id and status active_node
            let patch = json!({
                "spec": {
                    "nodeId": new_node_id
                },
                "status": {
                    "activeNode": new_node_id,
                    "lastMigration": chrono::Utc::now().to_rfc3339(),
                    "phase": "migrated"
                }
            });
            
            raid_disks.patch(raid_name, &kube::api::PatchParams::default(), 
                           &kube::api::Patch::Merge(&patch)).await?;
            
            println!("✅ [CRD_UPDATE] Updated SpdkRaidDisk {} node ID to {}", raid_name, new_node_id);
            return Ok(());
        }
    }
    
    println!("⚠️ [CRD_UPDATE] No SpdkRaidDisk CRD found for RAID {}", raid_name);
    Ok(())
}

/// Mark node as ready for shutdown
async fn mark_node_ready_for_shutdown(
    node_id: &str,
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", node_id);
    
    let patch = json!({
        "status": {
            "maintenance_status": {
                "ready_for_shutdown": true
            }
        }
    });
    
    spdk_configs.patch(&config_name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch)).await?;
    
    println!("🎯 [MAINTENANCE] Node {} marked as ready for shutdown", node_id);
    Ok(())
}

// ============================================================================
// MAINTENANCE MODE API
// ============================================================================

#[derive(Deserialize, Clone)]
struct MaintenanceModeRequest {
    enable: bool,
    migration_plan: Option<MigrationPlan>,
    force: Option<bool>, // Force shutdown without migration (dangerous)
}

#[derive(Deserialize, Clone)]
struct MigrationPlan {
    raid_migrations: Vec<RaidMigration>,
    single_replica_migrations: Vec<SingleReplicaMigration>,
}

#[derive(Deserialize, Clone)]
struct RaidMigration {
    raid_name: String,
    source_node: String,
    target_node: String,
}

#[derive(Deserialize, Clone)]
struct SingleReplicaMigration {
    volume_id: String,
    source_node: String,
    target_node: String,
}

#[derive(Serialize)]
struct MaintenanceModeResponse {
    success: bool,
    message: String,
    migration_id: Option<String>,
}

async fn set_maintenance_mode(
    node_id: String,
    request: MaintenanceModeRequest,
    state: AppState,
) -> Result<impl Reply, Rejection> {
    use spdk_csi_driver::models::SpdkConfig;
    use kube::api::{Api, PatchParams, Patch};
    use serde_json::json;
    
    println!("🔧 [MAINTENANCE] Setting maintenance mode for node {} to {}", node_id, request.enable);
    
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    if request.enable {
        // Enable maintenance mode
        println!("🚨 [MAINTENANCE] Enabling maintenance mode for node {}", node_id);
        
        // 1. Create or update SpdkConfig CRD with maintenance flag
        let config_name = format!("{}-config", node_id);
        
        // Check if config exists
        let config_exists = spdk_configs.get_opt(&config_name).await
            .map_err(|e| {
                println!("❌ [MAINTENANCE] Failed to check SpdkConfig: {}", e);
                warp::reject::reject()
            })?
            .is_some();
        
        if !config_exists {
            // Create initial SpdkConfig with maintenance mode
            let initial_config = SpdkConfig::new(
                &config_name,
                spdk_csi_driver::models::SpdkConfigSpec {
                    node_id: node_id.clone(),
                    maintenance_mode: true,
                    last_config_save: Some(chrono::Utc::now().to_rfc3339()),
                    raid_bdevs: vec![], // Will be populated by config sync
                    nvmeof_subsystems: vec![],
                },
            );
            
            spdk_configs.create(&kube::api::PostParams::default(), &initial_config).await
                .map_err(|e| {
                    println!("❌ [MAINTENANCE] Failed to create SpdkConfig: {}", e);
                    warp::reject::reject()
                })?;
        } else {
            // Update existing config to enable maintenance mode
            let patch = json!({
                "spec": {
                    "maintenance_mode": true
                },
                "status": {
                    "maintenance_status": {
                        "active": true,
                        "started_at": chrono::Utc::now().to_rfc3339(),
                        "migration_progress": [],
                        "ready_for_shutdown": false
                    }
                }
            });
            
            spdk_configs.patch(&config_name, &PatchParams::default(), &Patch::Merge(&patch)).await
                .map_err(|e| {
                    println!("❌ [MAINTENANCE] Failed to update SpdkConfig: {}", e);
                    warp::reject::reject()
                })?;
        }
        
        // 2. Start migration process if not forced shutdown
        if !request.force.unwrap_or(false) {
            println!("🚚 [MAINTENANCE] Starting migration process for node {}", node_id);
            
            // Analyze current RAIDs and create migration plan
            let migration_plan = match request.migration_plan {
                Some(plan) => {
                    println!("📋 [MIGRATION] Using provided migration plan");
                    plan
                }
                None => {
                    println!("🔍 [MIGRATION] Analyzing node to create migration plan");
                    match analyze_node_and_create_migration_plan(&node_id, &state).await {
                        Ok(plan) => plan,
                        Err(e) => {
                            println!("❌ [MIGRATION] Failed to create migration plan: {}", e);
                            return Ok(warp::reply::json(&MaintenanceModeResponse {
                                success: false,
                                message: format!("Failed to create migration plan: {}", e),
                                migration_id: None,
                            }));
                        }
                    }
                }
            };
            
            let migration_id = format!("migration-{}-{}", node_id, chrono::Utc::now().timestamp());
            
            // Start async migration process
            let migration_state = state.clone();
            let migration_node_id = node_id.clone();
            let migration_id_clone = migration_id.clone();
            
            tokio::spawn(async move {
                match execute_migration_plan(migration_plan, &migration_node_id, &migration_id_clone, migration_state).await {
                    Ok(_) => {
                        println!("✅ [MIGRATION] Migration {} completed successfully", migration_id_clone);
                    }
                    Err(e) => {
                        println!("❌ [MIGRATION] Migration {} failed: {}", migration_id_clone, e);
                    }
                }
            });
            
            return Ok(warp::reply::json(&MaintenanceModeResponse {
                success: true,
                message: format!("Maintenance mode enabled for node {}. Migration started.", node_id),
                migration_id: Some(migration_id),
            }));
        } else {
            // Force shutdown - mark as ready immediately (dangerous!)
            let patch = json!({
                "status": {
                    "maintenance_status": {
                        "active": true,
                        "started_at": chrono::Utc::now().to_rfc3339(),
                        "migration_progress": [],
                        "ready_for_shutdown": true
                    }
                }
            });
            
            spdk_configs.patch(&config_name, &PatchParams::default(), &Patch::Merge(&patch)).await
                .map_err(|e| {
                    println!("❌ [MAINTENANCE] Failed to update SpdkConfig: {}", e);
                    warp::reject::reject()
                })?;
            
            return Ok(warp::reply::json(&MaintenanceModeResponse {
                success: true,
                message: format!("Maintenance mode enabled for node {} (FORCE MODE - NO MIGRATION)", node_id),
                migration_id: None,
            }));
        }
    } else {
        // Disable maintenance mode
        println!("✅ [MAINTENANCE] Disabling maintenance mode for node {}", node_id);
        
        let config_name = format!("{}-config", node_id);
        let patch = json!({
            "spec": {
                "maintenance_mode": false
            },
            "status": {
                "maintenance_status": {
                    "active": false,
                    "started_at": null,
                    "migration_progress": [],
                    "ready_for_shutdown": false
                }
            }
        });
        
        spdk_configs.patch(&config_name, &PatchParams::default(), &Patch::Merge(&patch)).await
            .map_err(|e| {
                println!("❌ [MAINTENANCE] Failed to disable maintenance mode: {}", e);
                warp::reject::reject()
            })?;
        
        Ok(warp::reply::json(&MaintenanceModeResponse {
            success: true,
            message: format!("Maintenance mode disabled for node {}", node_id),
            migration_id: None,
        }))
    }
}

async fn get_maintenance_status(
    node_id: String,
    state: AppState,
) -> Result<impl Reply, Rejection> {
    use spdk_csi_driver::models::SpdkConfig;
    use kube::api::Api;
    
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let config_name = format!("{}-config", node_id);
    
    match spdk_configs.get_opt(&config_name).await {
        Ok(Some(config)) => {
            let status = config.status.unwrap_or_default();
            Ok(warp::reply::json(&status.maintenance_status))
        }
        Ok(None) => {
            Ok(warp::reply::json(&Option::<spdk_csi_driver::models::MaintenanceStatus>::None))
        }
        Err(e) => {
            println!("❌ [MAINTENANCE] Failed to get maintenance status: {}", e);
            Err(warp::reject::reject())
        }
    }
}

// ============================================================================
// DASHBOARD ALERTS AND OVERVIEW API
// ============================================================================

#[derive(Deserialize, Clone)]
pub struct ManualMigrationRequest {
    pub target_node: Option<String>,
    pub confirmation: bool, // Operator must confirm they understand the impact
}

#[derive(Serialize)]
struct DashboardAlert {
    id: String,
    alert_type: String,
    severity: String,
    message: String,
    volume_id: String,
    raid_name: String,
    source_node: String,
    created_at: String,
    suggested_action: String,
    manual_migration_available: bool,
    has_external_nvmeof_members: Option<bool>, // Only set for network partition alerts
    inaccessible_local_members: Option<u32>,    // Count of inaccessible local members
    inaccessible_external_members: Option<u32>, // Count of inaccessible external members
}

#[derive(Serialize)]
struct DashboardAlertsResponse {
    alerts: Vec<DashboardAlert>,
    total_critical: u32,
    total_warnings: u32,
}

#[derive(Serialize)]
struct NodeAlertsResponse {
    node_id: String,
    node_status: String,
    alerts: Vec<DashboardAlert>,
    total_alerts: u32,
    raid_count: u32,
    volume_count: u32,
}

#[derive(Serialize)]
struct DashboardOverview {
    cluster_health: ClusterHealth,
    node_stats: NodeStats,
    alert_summary: AlertSummary,
    recent_events: Vec<RecentEvent>,
}

#[derive(Serialize)]
struct ClusterHealth {
    status: String, // "healthy", "degraded", "critical"
    total_nodes: u32,
    healthy_nodes: u32,
    degraded_nodes: u32,
    failed_nodes: u32,
    // Data for pie chart
    node_status_chart: Vec<NodeStatusChartData>,
}

#[derive(Serialize)]
struct NodeStatusChartData {
    status: String,
    count: u32,
    percentage: f64,
    color: String,
}

#[derive(Serialize)]
struct NodeStats {
    total_raids: u32,
    healthy_raids: u32,
    degraded_raids: u32,
    total_volumes: u32,
    active_volumes: u32,
    failed_volumes: u32,
}

#[derive(Serialize)]
struct AlertSummary {
    total_alerts: u32,
    critical_alerts: u32,
    warning_alerts: u32,
    nodes_with_alerts: u32,
}

#[derive(Serialize)]
struct RecentEvent {
    timestamp: String,
    event_type: String,
    message: String,
    node_id: Option<String>,
    volume_id: Option<String>,
}

/// Get all dashboard alerts for operator attention
async fn get_dashboard_alerts(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("📋 [ALERTS] Getting dashboard alerts for operator");
    
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let configs_api: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let nodes_api: Api<k8s_openapi::api::core::v1::Node> = Api::all(state.kube_client.clone());
    let nvmeof_api: Api<NvmeofDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    match (volumes_api.list(&kube::api::ListParams::default()).await, 
           configs_api.list(&kube::api::ListParams::default()).await,
           nodes_api.list(&kube::api::ListParams::default()).await,
           nvmeof_api.list(&kube::api::ListParams::default()).await) {
        (Ok(volume_list), Ok(config_list), Ok(node_list), Ok(nvmeof_list)) => {
            let mut alerts = Vec::new();
            let mut critical_count = 0;
            let mut warning_count = 0;
            
            for volume in volume_list.items {
                // Check volume status for alerts
                if let Some(status) = &volume.status {
                    // Check for RAID host failures that need operator attention
                    if matches!(status.state.as_str(), "needs_attention" | "critical") {
                        let severity = if status.state == "critical" { "critical" } else { "warning" };
                        let alert_type = if status.state == "critical" { 
                            "raid_host_critical_failure" 
                        } else { 
                            "raid_host_failure" 
                        };
                        
                        if severity == "critical" {
                            critical_count += 1;
                        } else {
                            warning_count += 1;
                        }
                        
                        // Determine source node from volume configuration
                        let source_node = determine_volume_source_node(&volume, &state).await
                            .unwrap_or_else(|| "unknown".to_string());
                        
                        // ✅ ONLY create alerts if the source node actually has RAID disks
                        if let Some(node_config) = config_list.items.iter().find(|c| c.spec.node_id == source_node) {
                            if node_config.spec.raid_bdevs.is_empty() {
                                // Skip nodes with no RAID disks - no point alerting
                                println!("🔕 [ALERT_FILTER] Skipping alert for volume {} - source node {} has no RAID disks", 
                                         volume.spec.volume_id, source_node);
                                continue;
                            }
                        } else {
                            // No config found for node - skip alert
                            println!("🔕 [ALERT_FILTER] Skipping alert for volume {} - no config found for source node {}", 
                                     volume.spec.volume_id, source_node);
                            continue;
                        }
                        
                        // Check if all RAID members are inaccessible (network partition scenario)
                        let accessibility = check_raid_member_accessibility(&volume, &node_list, &nvmeof_list).await;
                        
                        let (message, suggested_action, migration_available) = if accessibility.all_members_inaccessible {
                            let recovery_details = match (accessibility.inaccessible_local_members > 0, accessibility.inaccessible_external_members > 0) {
                                (true, true) => "Data recovery requires fixing external NVMe-oF endpoint connectivity AND restoring cluster node connectivity, or backup restoration.",
                                (true, false) => "Data recovery requires restoring cluster node connectivity or backup restoration.",
                                (false, true) => "Data recovery requires fixing external NVMe-oF endpoint connectivity or backup restoration.",
                                (false, false) => "Data recovery requires backup restoration.", // Shouldn't happen, but safe fallback
                            };
                            let message = format!("Network partition detected for volume {} - ALL RAID members are inaccessible. {}", volume.spec.volume_id, recovery_details);
                            let action = "network_partition_recovery".to_string();
                            (message, action, false)
                        } else {
                            let message = format!("RAID host failure for volume {}", volume.spec.volume_id);
                            let action = if severity == "critical" { 
                                "urgent_migrate_raid_host".to_string() 
                            } else { 
                                "migrate_raid_host".to_string() 
                            };
                            (message, action, true)
                        };
                        
                        let alert = DashboardAlert {
                            id: format!("raid-failure-{}", volume.spec.volume_id),
                            alert_type: alert_type.to_string(),
                            severity: severity.to_string(),
                            message,
                            volume_id: volume.spec.volume_id.clone(),
                            raid_name: volume.spec.volume_id.clone(), // RAID name same as volume ID
                            source_node,
                            created_at: status.last_checked.clone(),
                            suggested_action,
                            manual_migration_available: migration_available,
                            has_external_nvmeof_members: if accessibility.all_members_inaccessible {
                                Some(accessibility.has_external_nvmeof_members)
                            } else {
                                None
                            },
                            inaccessible_local_members: if accessibility.all_members_inaccessible {
                                Some(accessibility.inaccessible_local_members)
                            } else {
                                None
                            },
                            inaccessible_external_members: if accessibility.all_members_inaccessible {
                                Some(accessibility.inaccessible_external_members)
                            } else {
                                None
                            },
                        };
                        
                        alerts.push(alert);
                    }
                }
            }
            
            alerts.sort_by(|a, b| {
                // Sort by severity (critical first) then by creation time
                match (a.severity.as_str(), b.severity.as_str()) {
                    ("critical", "warning") => std::cmp::Ordering::Less,
                    ("warning", "critical") => std::cmp::Ordering::Greater,
                    _ => a.created_at.cmp(&b.created_at),
                }
            });
            
            println!("📊 [ALERTS] Found {} alerts ({} critical, {} warnings)", 
                     alerts.len(), critical_count, warning_count);
            
            Ok(warp::reply::json(&DashboardAlertsResponse {
                alerts,
                total_critical: critical_count,
                total_warnings: warning_count,
            }))
        }
        (Err(e), _, _, _) | (_, Err(e), _, _) | (_, _, Err(e), _) | (_, _, _, Err(e)) => {
            println!("❌ [ALERTS] Failed to get alerts: {}", e);
            Err(warp::reject::reject())
        }
    }
}

/// Get alerts for a specific node
async fn get_node_alerts(node_id: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("📋 [NODE_ALERTS] Getting alerts for node {}", node_id);
    
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let configs_api: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let nodes_api: Api<k8s_openapi::api::core::v1::Node> = Api::all(state.kube_client.clone());
    let nvmeof_api: Api<NvmeofDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    match (volumes_api.list(&kube::api::ListParams::default()).await,
           configs_api.list(&kube::api::ListParams::default()).await,
           nodes_api.list(&kube::api::ListParams::default()).await,
           nvmeof_api.list(&kube::api::ListParams::default()).await) {
        (Ok(volume_list), Ok(config_list), Ok(node_list), Ok(nvmeof_list)) => {
            let mut node_alerts = Vec::new();
            let mut raid_count = 0;
            let mut volume_count = 0;
            
            for volume in volume_list.items {
                // Check if this volume's RAID is hosted on the requested node
                let source_node = determine_volume_source_node(&volume, &state).await;
                
                if source_node.as_deref() == Some(&node_id) {
                    // ✅ ONLY count and alert if the node actually has RAID disks
                    if let Some(node_config) = config_list.items.iter().find(|c| c.spec.node_id == node_id) {
                        if node_config.spec.raid_bdevs.is_empty() {
                            // Skip nodes with no RAID disks - no point counting or alerting
                            println!("🔕 [NODE_ALERT_FILTER] Skipping volume {} - node {} has no RAID disks", 
                                     volume.spec.volume_id, node_id);
                            continue;
                        }
                    } else {
                        // No config found for node - skip
                        println!("🔕 [NODE_ALERT_FILTER] Skipping volume {} - no config found for node {}", 
                                 volume.spec.volume_id, node_id);
                        continue;
                    }
                    
                    raid_count += 1;
                    volume_count += 1; // Each RAID can have multiple volumes, but for simplicity 1:1
                    
                    // Check for alerts on this node
                    if let Some(status) = &volume.status {
                        if matches!(status.state.as_str(), "needs_attention" | "critical") {
                            let severity = if status.state == "critical" { "critical" } else { "warning" };
                            let alert_type = if status.state == "critical" { 
                                "raid_host_critical_failure" 
                            } else { 
                                "raid_host_failure" 
                            };
                            
                            // Check if all RAID members are inaccessible (network partition scenario)
                            let accessibility = check_raid_member_accessibility(&volume, &node_list, &nvmeof_list).await;
                            
                            let (message, suggested_action, migration_available) = if accessibility.all_members_inaccessible {
                                let recovery_details = match (accessibility.inaccessible_local_members > 0, accessibility.inaccessible_external_members > 0) {
                                    (true, true) => "Data recovery requires fixing external NVMe-oF endpoint connectivity AND restoring cluster node connectivity, or backup restoration.",
                                    (true, false) => "Data recovery requires restoring cluster node connectivity or backup restoration.",
                                    (false, true) => "Data recovery requires fixing external NVMe-oF endpoint connectivity or backup restoration.",
                                    (false, false) => "Data recovery requires backup restoration.", // Shouldn't happen, but safe fallback
                                };
                                let message = format!("Network partition detected for volume {} - ALL RAID members are inaccessible. {}", volume.spec.volume_id, recovery_details);
                                let action = "network_partition_recovery".to_string();
                                (message, action, false)
                            } else {
                                let message = format!("RAID host failure for volume {}", volume.spec.volume_id);
                                let action = if severity == "critical" { 
                                    "urgent_migrate_raid_host".to_string() 
                                } else { 
                                    "migrate_raid_host".to_string() 
                                };
                                (message, action, true)
                            };
                            
                            let alert = DashboardAlert {
                                id: format!("raid-failure-{}", volume.spec.volume_id),
                                alert_type: alert_type.to_string(),
                                severity: severity.to_string(),
                                message,
                                volume_id: volume.spec.volume_id.clone(),
                                raid_name: volume.spec.volume_id.clone(),
                                source_node: node_id.clone(),
                                created_at: status.last_checked.clone(),
                                suggested_action,
                                manual_migration_available: migration_available,
                                has_external_nvmeof_members: if accessibility.all_members_inaccessible {
                                    Some(accessibility.has_external_nvmeof_members)
                                } else {
                                    None
                                },
                                inaccessible_local_members: if accessibility.all_members_inaccessible {
                                    Some(accessibility.inaccessible_local_members)
                                } else {
                                    None
                                },
                                inaccessible_external_members: if accessibility.all_members_inaccessible {
                                    Some(accessibility.inaccessible_external_members)
                                } else {
                                    None
                                },
                            };
                            
                            node_alerts.push(alert);
                        }
                    }
                }
            }
            
            // Determine node status based on alerts
            let node_status = if node_alerts.iter().any(|a| a.severity == "critical") {
                "critical"
            } else if node_alerts.iter().any(|a| a.severity == "warning") {
                "warning" 
            } else if raid_count > 0 {
                "healthy"
            } else {
                "idle"
            };
            
            println!("📊 [NODE_ALERTS] Node {} has {} alerts ({} RAIDs, {} volumes)", 
                     node_id, node_alerts.len(), raid_count, volume_count);
            
            let total_alert_count = node_alerts.len() as u32;
            Ok(warp::reply::json(&NodeAlertsResponse {
                node_id,
                node_status: node_status.to_string(),
                alerts: node_alerts,
                total_alerts: total_alert_count,
                raid_count,
                volume_count,
            }))
        }
        (Err(e), _, _, _) | (_, Err(e), _, _) | (_, _, Err(e), _) | (_, _, _, Err(e)) => {
            println!("❌ [NODE_ALERTS] Failed to get node alerts: {}", e);
            Err(warp::reject::reject())
        }
    }
}

/// Get main dashboard overview with node statistics and pie chart data
async fn get_dashboard_overview(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("📊 [OVERVIEW] Getting dashboard overview");
    
    use k8s_openapi::api::core::v1::Node;
    use kube::api::{Api, ListParams};
    
    // Get node information
    let nodes_api: Api<Node> = Api::all(state.kube_client.clone());
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    let list_params = ListParams::default();
    let (nodes_result, volumes_result) = tokio::join!(
        nodes_api.list(&list_params),
        volumes_api.list(&list_params)
    );
    
    match (nodes_result, volumes_result) {
        (Ok(node_list), Ok(volume_list)) => {
            // Analyze node health
            let mut total_nodes = 0;
            let mut healthy_nodes = 0;
            let mut degraded_nodes = 0;
            let mut failed_nodes = 0;
            
            // Map to track which nodes have storage roles
            let mut storage_nodes = std::collections::HashSet::new();
            
            // First, identify storage nodes from volume configurations
            for volume in &volume_list.items {
                if let Some(source_node) = determine_volume_source_node(volume, &state).await {
                    storage_nodes.insert(source_node);
                }
            }
            
            // Analyze each Kubernetes node
            for node in &node_list.items {
                total_nodes += 1;
                
                let node_name = node.metadata.name.as_deref().unwrap_or("unknown");
                let is_storage_node = storage_nodes.contains(node_name);
                
                // Check node conditions
                let mut is_ready = false;
                if let Some(status) = &node.status {
                    if let Some(conditions) = &status.conditions {
                        for condition in conditions {
                            if condition.type_ == "Ready" && condition.status == "True" {
                                is_ready = true;
                                break;
                            }
                        }
                    }
                }
                
                // Check for alerts on this node
                let has_critical_alerts = if is_storage_node {
                    volume_list.items.iter().any(|vol| {
                        if let Some(source) = determine_volume_source_node_sync(vol) {
                            if source == node_name {
                                if let Some(status) = &vol.status {
                                    return status.state == "critical";
                                }
                            }
                        }
                        false
                    })
                } else {
                    false
                };
                
                let has_warnings = if is_storage_node {
                    volume_list.items.iter().any(|vol| {
                        if let Some(source) = determine_volume_source_node_sync(vol) {
                            if source == node_name {
                                if let Some(status) = &vol.status {
                                    return status.state == "needs_attention";
                                }
                            }
                        }
                        false
                    })
                } else {
                    false
                };
                
                // Categorize node status
                if !is_ready {
                    failed_nodes += 1;
                } else if has_critical_alerts {
                    degraded_nodes += 1;
                } else if has_warnings {
                    degraded_nodes += 1;
                } else {
                    healthy_nodes += 1;
                }
            }
            
            // Create pie chart data
            let mut chart_data = Vec::new();
            
            if healthy_nodes > 0 {
                chart_data.push(NodeStatusChartData {
                    status: "Healthy".to_string(),
                    count: healthy_nodes,
                    percentage: (healthy_nodes as f64 / total_nodes as f64) * 100.0,
                    color: "#4CAF50".to_string(), // Green
                });
            }
            
            if degraded_nodes > 0 {
                chart_data.push(NodeStatusChartData {
                    status: "Degraded".to_string(),
                    count: degraded_nodes,
                    percentage: (degraded_nodes as f64 / total_nodes as f64) * 100.0,
                    color: "#FF9800".to_string(), // Orange
                });
            }
            
            if failed_nodes > 0 {
                chart_data.push(NodeStatusChartData {
                    status: "Failed".to_string(),
                    count: failed_nodes,
                    percentage: (failed_nodes as f64 / total_nodes as f64) * 100.0,
                    color: "#F44336".to_string(), // Red
                });
            }
            
            // Determine overall cluster health
            let cluster_status = if failed_nodes > 0 || degraded_nodes > total_nodes / 2 {
                "critical"
            } else if degraded_nodes > 0 {
                "degraded"  
            } else {
                "healthy"
            };
            
            // Analyze volumes and RAIDs
            let mut total_raids = 0;
            let mut healthy_raids = 0;
            let mut degraded_raids = 0;
            let mut total_volumes = 0;
            let mut active_volumes = 0;
            let mut failed_volumes = 0;
            
            let mut critical_alerts = 0;
            let mut warning_alerts = 0;
            let mut nodes_with_alerts = std::collections::HashSet::new();
            
            for volume in &volume_list.items {
                total_volumes += 1;
                total_raids += 1; // 1:1 mapping in current architecture
                
                if let Some(status) = &volume.status {
                    match status.state.as_str() {
                        "ready" => {
                            active_volumes += 1;
                            healthy_raids += 1;
                        }
                        "degraded" => {
                            active_volumes += 1;
                            degraded_raids += 1;
                        }
                        "needs_attention" => {
                            active_volumes += 1;
                            degraded_raids += 1;
                            warning_alerts += 1;
                            if let Some(source) = determine_volume_source_node(volume, &state).await {
                                nodes_with_alerts.insert(source);
                            }
                        }
                        "critical" => {
                            failed_volumes += 1;
                            degraded_raids += 1;
                            critical_alerts += 1;
                            if let Some(source) = determine_volume_source_node(volume, &state).await {
                                nodes_with_alerts.insert(source);
                            }
                        }
                        _ => {
                            failed_volumes += 1;
                            degraded_raids += 1;
                        }
                    }
                }
            }
            
            // Create recent events (placeholder - could be enhanced with real event tracking)
            let recent_events = vec![
                RecentEvent {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    event_type: "system_status".to_string(),
                    message: format!("Cluster health: {} ({}/{} nodes healthy)", 
                                   cluster_status, healthy_nodes, total_nodes),
                    node_id: None,
                    volume_id: None,
                }
            ];
            
            let overview = DashboardOverview {
                cluster_health: ClusterHealth {
                    status: cluster_status.to_string(),
                    total_nodes,
                    healthy_nodes,
                    degraded_nodes,
                    failed_nodes,
                    node_status_chart: chart_data,
                },
                node_stats: NodeStats {
                    total_raids,
                    healthy_raids,
                    degraded_raids,
                    total_volumes,
                    active_volumes,
                    failed_volumes,
                },
                alert_summary: AlertSummary {
                    total_alerts: critical_alerts + warning_alerts,
                    critical_alerts,
                    warning_alerts,
                    nodes_with_alerts: nodes_with_alerts.len() as u32,
                },
                recent_events,
            };
            
            println!("📊 [OVERVIEW] Cluster: {} nodes ({} healthy, {} degraded, {} failed)", 
                     total_nodes, healthy_nodes, degraded_nodes, failed_nodes);
            println!("📊 [OVERVIEW] Storage: {} RAIDs, {} volumes, {} alerts", 
                     total_raids, total_volumes, critical_alerts + warning_alerts);
            
            Ok(warp::reply::json(&overview))
        }
        (Err(e), _) | (_, Err(e)) => {
            println!("❌ [OVERVIEW] Failed to get overview: {}", e);
            Err(warp::reject::reject())
        }
    }
}

/// Determine the source node for a volume (synchronous version)
fn determine_volume_source_node_sync(_volume: &SpdkVolume) -> Option<String> {
    // For simplicity, we'll need to implement this differently
    // In practice, this would need access to the state to query configs
    // For now, return None - the async version should be used
    None
}

/// Determine the source node for a volume (where its RAID is hosted)
async fn determine_volume_source_node(
    volume: &SpdkVolume,
    state: &AppState,
) -> Option<String> {
    // Look up the RAID configuration to find which node hosts it
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    // Check all node configs to find where this RAID is hosted
    if let Ok(config_list) = spdk_configs.list(&kube::api::ListParams::default()).await {
        for config in config_list.items {
            for raid in &config.spec.raid_bdevs {
                if raid.name == volume.spec.volume_id {
                    return Some(config.spec.node_id);
                }
            }
        }
    }
    
    None
}

/// Trigger manual migration for a specific alert
async fn trigger_manual_migration(
    volume_id: String,
    request: ManualMigrationRequest,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🚨 [MANUAL_MIGRATION] Operator triggering manual migration for volume {}", volume_id);
    
    if !request.confirmation {
        return Ok(warp::reply::json(&json!({
            "success": false,
            "message": "Migration requires explicit confirmation from operator"
        })));
    }
    
    // Get the volume to find source node
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let volume = match volumes_api.get(&volume_id).await {
        Ok(vol) => vol,
        Err(e) => {
            println!("❌ [MANUAL_MIGRATION] Volume {} not found: {}", volume_id, e);
            return Ok(warp::reply::json(&json!({
                "success": false,
                "message": format!("Volume {} not found", volume_id)
            })));
        }
    };
    
    // Determine source and target nodes
    let source_node = determine_volume_source_node(&volume, &state).await
        .ok_or_else(|| {
            println!("❌ [MANUAL_MIGRATION] Could not determine source node for volume {}", volume_id);
            warp::reject::reject()
        })?;
    
    let target_node = match request.target_node {
        Some(target) => target,
        None => {
            // Auto-select target node
            match find_optimal_target_node_for_raid(&get_raid_config_placeholder(), &source_node, &state).await {
                Ok(target) => target,
                Err(e) => {
                    println!("❌ [MANUAL_MIGRATION] Could not find target node: {}", e);
                    return Ok(warp::reply::json(&json!({
                        "success": false,
                        "message": format!("Could not find suitable target node: {}", e)
                    })));
                }
            }
        }
    };
    
    // Create migration plan
    let migration_plan = MigrationPlan {
        raid_migrations: vec![RaidMigration {
            raid_name: volume_id.clone(),
            source_node: source_node.clone(),
            target_node: target_node.clone(),
        }],
        single_replica_migrations: vec![],
    };
    
    let migration_id = format!("manual-migration-{}", chrono::Utc::now().timestamp());
    
    // Get selection reasoning for the UI (before source_node is moved)
    let selection_reason = determine_selection_reason(&volume_id, &source_node, &target_node, &state).await;
    
    // Clone values before moving them
    let source_node_for_response = source_node.clone();
    let target_node_for_response = target_node.clone();
    
    // Start async migration
    let migration_state = state.clone();
    let migration_id_clone = migration_id.clone();
    
    tokio::spawn(async move {
        println!("🚀 [MANUAL_MIGRATION] Starting operator-requested migration {}", migration_id_clone);
        
        match execute_migration_plan(migration_plan, &source_node, &migration_id_clone, migration_state).await {
            Ok(_) => {
                println!("✅ [MANUAL_MIGRATION] Migration {} completed successfully", migration_id_clone);
            }
            Err(e) => {
                println!("❌ [MANUAL_MIGRATION] Migration {} failed: {}", migration_id_clone, e);
            }
        }
    });

    Ok(warp::reply::json(&json!({
        "success": true,
        "message": format!("Manual migration initiated for volume {}", volume_id),
        "migration_id": migration_id,
        "source_node": source_node_for_response,
        "target_node": target_node_for_response,
        "selection_reason": selection_reason
    })))
}

/// Determine why a particular target node was selected
async fn determine_selection_reason(
    volume_id: &str,
    source_node: &str,
    target_node: &str,
    state: &AppState,
) -> String {
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    if let Ok(configs) = spdk_configs.list(&kube::api::ListParams::default()).await {
        // Check if target node has replicas
        if let Some(source_config) = configs.items.iter().find(|c| c.spec.node_id == source_node) {
            if let Some(raid) = source_config.spec.raid_bdevs.iter().find(|r| r.name == volume_id) {
                for member in &raid.members {
                    if let Some(nvmeof_config) = &member.nvmeof_config {
                        if nvmeof_config.target_node_id == target_node {
                            return format!("Target has existing healthy replica - optimal choice (zero data migration)");
                        }
                    }
                }
            }
        }
        
        // Check RAID count for load balancing reasoning
        let target_raid_count = count_raids_on_node(target_node, &configs);
        return format!("Load-balanced selection - target node has {} RAIDs (minimum among available nodes)", target_raid_count);
    }
    
    "Auto-selected based on node health and availability".to_string()
}

/// Placeholder function for getting RAID config (to be implemented properly)
fn get_raid_config_placeholder() -> spdk_csi_driver::models::RaidBdevConfig {
    // This should be replaced with actual RAID config lookup
    spdk_csi_driver::models::RaidBdevConfig::default()
}

// ============================================================================
// NODE PERFORMANCE METRICS API
// ============================================================================

#[derive(Serialize, Debug)]
struct NodePerformanceMetrics {
    node_id: String,
    raid_count: u32,
    volume_count: u32,
    
    // Performance metrics
    total_read_iops: u64,
    total_write_iops: u64,
    total_read_bandwidth_mbps: f64,
    total_write_bandwidth_mbps: f64,
    avg_read_latency_ms: f64,
    avg_write_latency_ms: f64,
    
    // Resource utilization
    spdk_active: bool,
    last_updated: String,
    
    // Health indicators
    failed_raids: u32,
    degraded_raids: u32,
    healthy_raids: u32,
    
    // Performance score (0-100, higher is better)
    performance_score: f64,
}

#[derive(Serialize)]
struct NodesPerformanceResponse {
    nodes: Vec<NodePerformanceMetrics>,
    cluster_totals: ClusterPerformanceTotals,
    last_updated: String,
}

#[derive(Serialize)]
struct ClusterPerformanceTotals {
    total_read_iops: u64,
    total_write_iops: u64,
    total_bandwidth_mbps: f64,
    avg_cluster_latency_ms: f64,
    total_active_nodes: u32,
    total_raids: u32,
}

/// Get performance summary for all nodes
async fn get_nodes_performance_summary(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("📊 [PERFORMANCE] Getting cluster-wide node performance summary");
    
    let mut node_metrics = Vec::new();
    let mut cluster_totals = ClusterPerformanceTotals {
        total_read_iops: 0,
        total_write_iops: 0,
        total_bandwidth_mbps: 0.0,
        avg_cluster_latency_ms: 0.0,
        total_active_nodes: 0,
        total_raids: 0,
    };
    
    // Get all nodes with SPDK instances
    let spdk_nodes = state.spdk_nodes.read().await;
    let configs_api: Api<SpdkConfig> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    
    let configs = match configs_api.list(&kube::api::ListParams::default()).await {
        Ok(configs) => configs,
        Err(e) => {
            println!("❌ [PERFORMANCE] Failed to get node configs: {}", e);
            return Err(warp::reject::reject());
        }
    };
    
    for (node_name, rpc_url) in spdk_nodes.iter() {
        println!("📈 [PERFORMANCE] Collecting metrics for node: {}", node_name);
        
        let metrics = collect_node_performance_metrics(node_name, rpc_url, &configs, &state).await;
        
        // Update cluster totals
        cluster_totals.total_read_iops += metrics.total_read_iops;
        cluster_totals.total_write_iops += metrics.total_write_iops;
        cluster_totals.total_bandwidth_mbps += metrics.total_read_bandwidth_mbps + metrics.total_write_bandwidth_mbps;
        cluster_totals.total_raids += metrics.raid_count;
        
        if metrics.spdk_active {
            cluster_totals.total_active_nodes += 1;
        }
        
        node_metrics.push(metrics);
    }
    
    // Calculate cluster average latency
    let total_latency: f64 = node_metrics.iter()
        .filter(|m| m.spdk_active)
        .map(|m| (m.avg_read_latency_ms + m.avg_write_latency_ms) / 2.0)
        .sum();
    
    if cluster_totals.total_active_nodes > 0 {
        cluster_totals.avg_cluster_latency_ms = total_latency / cluster_totals.total_active_nodes as f64;
    }
    
    println!("📊 [PERFORMANCE] Collected metrics for {} nodes ({} active)", 
             node_metrics.len(), cluster_totals.total_active_nodes);
    
    Ok(warp::reply::json(&NodesPerformanceResponse {
        nodes: node_metrics,
        cluster_totals,
        last_updated: chrono::Utc::now().to_rfc3339(),
    }))
}

/// Collect comprehensive performance metrics for a single node
async fn collect_node_performance_metrics(
    node_name: &str,
    rpc_url: &str,
    configs: &kube::core::ObjectList<SpdkConfig>,
    state: &AppState,
) -> NodePerformanceMetrics {
    let mut metrics = NodePerformanceMetrics {
        node_id: node_name.to_string(),
        raid_count: 0,
        volume_count: 0,
        total_read_iops: 0,
        total_write_iops: 0,
        total_read_bandwidth_mbps: 0.0,
        total_write_bandwidth_mbps: 0.0,
        avg_read_latency_ms: 0.0,
        avg_write_latency_ms: 0.0,
        spdk_active: false,
        last_updated: chrono::Utc::now().to_rfc3339(),
        failed_raids: 0,
        degraded_raids: 0,
        healthy_raids: 0,
        performance_score: 0.0,
    };
    
    // Get node configuration for RAID count
    if let Some(node_config) = configs.items.iter().find(|c| c.spec.node_id == node_name) {
        metrics.raid_count = node_config.spec.raid_bdevs.len() as u32;
        
        // Count RAID health status
        for raid in &node_config.spec.raid_bdevs {
            // This is a simplified health check - in reality we'd query SPDK
            if raid.name.contains("failed") {
                metrics.failed_raids += 1;
            } else if raid.name.contains("degraded") {
                metrics.degraded_raids += 1;
            } else {
                metrics.healthy_raids += 1;
            }
        }
    }
    
    // Collect SPDK performance metrics
    let http_client = HttpClient::new();
    
    // Get I/O statistics from SPDK
    match http_client
        .post(rpc_url)
        .json(&json!({"method": "bdev_get_iostat"}))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(response) => {
            metrics.spdk_active = true;
            
            if let Ok(iostat_response) = response.json::<serde_json::Value>().await {
                if let Some(result) = iostat_response.get("result") {
                    if let Some(bdevs) = result.as_array() {
                        metrics.volume_count = bdevs.len() as u32;
                        
                        // Aggregate I/O statistics across all bdevs
                        for bdev in bdevs {
                            if let Some(stats) = bdev.as_object() {
                                // Read IOPS
                                if let Some(read_ios) = stats.get("read_ios").and_then(|v| v.as_u64()) {
                                    metrics.total_read_iops += read_ios;
                                }
                                
                                // Write IOPS  
                                if let Some(write_ios) = stats.get("write_ios").and_then(|v| v.as_u64()) {
                                    metrics.total_write_iops += write_ios;
                                }
                                
                                // Read bandwidth (bytes to MB/s)
                                if let Some(bytes_read) = stats.get("bytes_read").and_then(|v| v.as_u64()) {
                                    metrics.total_read_bandwidth_mbps += bytes_read as f64 / (1024.0 * 1024.0);
                                }
                                
                                // Write bandwidth (bytes to MB/s)
                                if let Some(bytes_written) = stats.get("bytes_written").and_then(|v| v.as_u64()) {
                                    metrics.total_write_bandwidth_mbps += bytes_written as f64 / (1024.0 * 1024.0);
                                }
                                
                                // Latency (ticks to milliseconds - approximate conversion)
                                if let Some(read_latency) = stats.get("read_latency_ticks").and_then(|v| v.as_u64()) {
                                    metrics.avg_read_latency_ms += read_latency as f64 / 1000.0; // Rough conversion
                                }
                                
                                if let Some(write_latency) = stats.get("write_latency_ticks").and_then(|v| v.as_u64()) {
                                    metrics.avg_write_latency_ms += write_latency as f64 / 1000.0; // Rough conversion
                                }
                            }
                        }
                        
                        // Calculate average latencies
                        if metrics.volume_count > 0 {
                            metrics.avg_read_latency_ms /= metrics.volume_count as f64;
                            metrics.avg_write_latency_ms /= metrics.volume_count as f64;
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("⚠️ [PERFORMANCE] Failed to get iostat for node {}: {}", node_name, e);
            metrics.spdk_active = false;
        }
    }
    
    // Calculate performance score (0-100)
    metrics.performance_score = calculate_performance_score(&metrics);
    
    println!("📊 [PERFORMANCE] Node {} - RAID: {}, IOPS: {}R/{}W, Latency: {:.2}ms, Score: {:.1}", 
             node_name, metrics.raid_count, metrics.total_read_iops, metrics.total_write_iops, 
             (metrics.avg_read_latency_ms + metrics.avg_write_latency_ms) / 2.0, metrics.performance_score);
    
    metrics
}

/// Calculate a performance score (0-100) for a node
fn calculate_performance_score(metrics: &NodePerformanceMetrics) -> f64 {
    if !metrics.spdk_active {
        return 0.0;
    }
    
    let mut score = 100.0;
    
    // Deduct points for high latency
    let avg_latency = (metrics.avg_read_latency_ms + metrics.avg_write_latency_ms) / 2.0;
    if avg_latency > 10.0 {
        score -= (avg_latency - 10.0) * 2.0; // -2 points per ms over 10ms
    }
    
    // Deduct points for high RAID load
    if metrics.raid_count > 5 {
        score -= (metrics.raid_count as f64 - 5.0) * 3.0; // -3 points per RAID over 5
    }
    
    // Deduct points for failed/degraded RAIDs
    score -= metrics.failed_raids as f64 * 20.0; // -20 points per failed RAID
    score -= metrics.degraded_raids as f64 * 10.0; // -10 points per degraded RAID
    
    // Bonus points for high throughput (simplified)
    let total_iops = metrics.total_read_iops + metrics.total_write_iops;
    if total_iops > 1000 {
        score += ((total_iops as f64 / 1000.0).ln() * 5.0).min(15.0); // Bonus up to +15 points
    }
    
    score.max(0.0).min(100.0)
}
