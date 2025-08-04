use warp::{Filter, Reply, Rejection};
use serde::{Serialize, Deserialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use reqwest::Client as HttpClient;
use kube::{Client, Api, api::ListParams};
use chrono::{Utc, DateTime};
use std::env;
use spdk_csi_driver::*;
use k8s_openapi::api::core::v1::Pod;  

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

// Volume validation result structures
#[derive(Debug, Clone)]
struct SpdkVolumeInfo {
    name: String,
    node: String,
    lvs_name: String,
    num_blocks: u64,
    uuid: String,
}

#[derive(Debug, Clone)]
struct PhantomVolumeInfo {
    volume_id: String,
    expected_node: String,
    expected_lvol_uuid: String,
}

#[derive(Debug, Clone)]
struct VolumeValidationResult {
    orphaned_spdk_volumes: Vec<SpdkVolumeInfo>,
    phantom_k8s_volumes: Vec<PhantomVolumeInfo>,
}

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
    #[serde(rename = "warning")] 
    Warning,
    #[serde(rename = "error")]
    Error,
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
    nodes: Vec<String>,
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
    reason: Option<String>, // Optional reason for deletion
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
        .or(
            // Disk setup APIs - proxy to node-agents
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("disks"))
                .and(warp::path("uninitialized"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_node_uninitialized_disks)
        )
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
        .or(
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("disks"))
                .and(warp::path("status"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_node_disk_status)
        )
        .or(
            // Get all nodes disk info for setup page
            warp::path("disks")
                .and(warp::path("setup"))
                .and(warp::path("nodes"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_all_nodes_disk_setup)
        )
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
            // DELETE /api/volumes/orphaned - Delete orphaned SPDK logical volumes (legacy endpoint)
            warp::path("volumes")
                .and(warp::path("orphaned"))
                .and(warp::delete())
                .and(warp::body::json())
                .and(state_filter.clone())
                .and_then(delete_orphaned_spdk_volume)
        )
    );

    let routes = api.with(cors);
    
    println!("SPDK Dashboard API server starting on http://0.0.0.0:8080");
    warp::serve(routes)
        .run(([0, 0, 0, 0], 8080))
        .await;
    
    Ok(())
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
    
    let disks_api: Api<SpdkDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
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
    
    println!("🔧 [DASHBOARD_REFRESH] Converting {} disks to dashboard format...", disks_list.items.len());
    for (i, disk) in disks_list.items.iter().enumerate() {
        // Track the node for this disk
        nodes.insert(disk.spec.node_id.clone());
        
        // Convert disk to dashboard format
        let dashboard_disk = convert_disk_to_dashboard(disk, &dashboard_volumes);
        println!("✅ [DASHBOARD_REFRESH] Disk {}/{}: {} converted successfully", 
            i + 1, disks_list.items.len(), disk.metadata.name.as_ref().unwrap_or(&"unnamed".to_string()));
        dashboard_disks.push(dashboard_disk);
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
        nodes: nodes.into_iter().collect(),
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

fn convert_disk_to_dashboard(disk: &SpdkDisk, volumes: &[DashboardVolume]) -> DashboardDisk {
    let default_status = SpdkDiskStatus::default();
    let status = disk.status.as_ref().unwrap_or(&default_status);
    let spec = &disk.spec;
    
    let provisioned_volumes: Vec<ProvisionedVolume> = volumes.iter()
        .filter_map(|vol| {
            for replica in &vol.replica_statuses {
                if replica.node == spec.node_id {
                    return Some(ProvisionedVolume {
                        volume_name: vol.name.clone(),
                        volume_id: vol.id.clone(),
                        size: vol.size.trim_end_matches("GB").parse().unwrap_or(0),
                        provisioned_at: Utc::now().to_rfc3339(),
                        replica_type: format!("{} replica ({})", 
                            if replica.is_local { "Local" } else { "Remote" },
                            "NVMe-oF"),
                        status: replica.status.clone(),
                    });
                }
            }
            None
        })
        .collect();
    
    DashboardDisk {
        id: disk.metadata.name.clone().unwrap_or_default(),
        node: spec.node_id.clone(),
        pci_addr: spec.pcie_addr.clone(),
        capacity: status.total_capacity,
        capacity_gb: status.total_capacity / (1024 * 1024 * 1024),
        allocated_space: status.used_space,
        free_space: status.free_space,
        free_space_display: format!("{}GB", status.free_space / (1024 * 1024 * 1024)),
        healthy: status.healthy,
        blobstore_initialized: status.blobstore_initialized,
        lvol_count: status.lvol_count,
        model: format!("NVMe Disk"),
        read_iops: status.io_stats.read_iops,
        write_iops: status.io_stats.write_iops,
        read_latency: status.io_stats.read_latency_us,
        write_latency: status.io_stats.write_latency_us,
        brought_online: status.last_checked.clone(),
        provisioned_volumes,
        // Default empty - will be populated by validation process
        orphaned_spdk_volumes: Vec::new(),
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

/// Apply validation results to dashboard volumes and disks for frontend display
async fn apply_validation_results_to_dashboard(
    volumes: &mut [DashboardVolume],
    disks: &mut [DashboardDisk],
    validation_result: &VolumeValidationResult,
) {
    println!("🔄 [SPDK_VALIDATION] Applying validation results to dashboard data");
    
    // Update phantom volumes (K8s exists, SPDK missing)
    for phantom in &validation_result.phantom_k8s_volumes {
        for volume in volumes.iter_mut() {
            if volume.id == phantom.volume_id {
                volume.spdk_validation_status = SpdkValidationStatus {
                    has_spdk_backing: false,
                    validation_message: Some(format!(
                        "⚠️ Volume exists in Kubernetes but no SPDK backing found on node '{}'", 
                        phantom.expected_node
                    )),
                    validation_severity: ValidationSeverity::Error,
                };
                println!("🔴 [SPDK_VALIDATION] Marked volume '{}' as phantom (no SPDK backing)", volume.id);
                break;
            }
        }
    }
    
    // Update disk orphaned volumes (SPDK exists, K8s missing)
    for disk in disks.iter_mut() {
        let orphans_on_disk: Vec<OrphanedVolumeInfo> = validation_result.orphaned_spdk_volumes
            .iter()
            .filter(|orphan| orphan.node == disk.node)
            .map(|orphan| OrphanedVolumeInfo {
                spdk_volume_name: orphan.name.clone(),
                spdk_volume_uuid: orphan.uuid.clone(),
                size_blocks: orphan.num_blocks,
                size_gb: (orphan.num_blocks * 512) as f64 / (1024.0 * 1024.0 * 1024.0), // Assuming 512-byte blocks
                orphaned_since: chrono::Utc::now().to_rfc3339(),
            })
            .collect();
        
        if !orphans_on_disk.is_empty() {
            disk.orphaned_spdk_volumes = orphans_on_disk;
            println!("🟡 [SPDK_VALIDATION] Added {} orphaned volumes to disk '{}' on node '{}'", 
                disk.orphaned_spdk_volumes.len(), disk.id, disk.node);
        }
    }
    
    println!("✅ [SPDK_VALIDATION] Applied validation results: {} phantom volumes, {} orphaned volumes", 
        validation_result.phantom_k8s_volumes.len(), 
        validation_result.orphaned_spdk_volumes.len());
}

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
    println!("🔍 [VOLUME_SPDK] Getting SPDK details for volume: {}", volume_id);
    
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    
    // Get the target node from query parameter or fallback to first available
    let target_node = query.node.unwrap_or_else(|| {
        spdk_nodes.keys().next().unwrap_or(&"unknown".to_string()).clone()
    });
    
    println!("🎯 [VOLUME_SPDK] Querying node '{}' for volume '{}'", target_node, volume_id);
    
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
        
    // Find the volume by checking if volume_id is contained in the volume name
    for lvol in lvols_list {
        if let Some(vol_name) = lvol["name"].as_str() {
            if vol_name.contains(&volume_id) {
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
    println!("🗑️ [DELETE_RAW] Received request to delete raw SPDK volume '{}'", volume_uuid);
    
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
    
    // Delete the volume using bdev_lvol_delete with the UUID as bdev_name
    println!("🗑️ [DELETE_RAW] Attempting to delete volume with bdev_name (UUID): '{}'", target_volume.uuid);
    
    let http_client = HttpClient::new();
    let delete_response = http_client
        .post(rpc_url)
        .json(&json!({
            "method": "bdev_lvol_delete",
            "params": [target_volume.uuid],  // Positional argument: bdev_name (UUID)
            "id": 1
        }))
        .send()
        .await;
        
    match delete_response {
        Ok(response) => {
            if response.status().is_success() {
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
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                println!("❌ [DELETE_RAW] SPDK deletion failed: {}", error_text);
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
            println!("❌ [DELETE_RAW] Network error during deletion: {}", e);
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
async fn get_node_uninitialized_disks(node: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(node_agent_url) = get_node_agent_url(&spdk_nodes, &node) {
        let http_client = HttpClient::new();
        
        match http_client
            .get(&format!("{}/api/disks/uninitialized", node_agent_url))
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    match response.json::<serde_json::Value>().await {
                        Ok(data) => Ok(warp::reply::json(&data)),
                        Err(_) => Ok(warp::reply::json(&json!({
                            "success": false,
                            "error": "Failed to parse node-agent response",
                            "node": node
                        })))
                    }
                } else {
                    Ok(warp::reply::json(&json!({
                        "success": false,
                        "error": format!("Node-agent returned status: {}", response.status()),
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

async fn get_node_disk_status(node: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(node_agent_url) = get_node_agent_url(&spdk_nodes, &node) {
        let http_client = HttpClient::new();
        
        match http_client
            .get(&format!("{}/api/disks/status", node_agent_url))
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    match response.json::<serde_json::Value>().await {
                        Ok(data) => Ok(warp::reply::json(&data)),
                        Err(_) => Ok(warp::reply::json(&json!({
                            "success": false,
                            "error": "Failed to parse node-agent response",
                            "node": node
                        })))
                    }
                } else {
                    Ok(warp::reply::json(&json!({
                        "success": false,
                        "error": format!("Node-agent returned status: {}", response.status()),
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

async fn get_all_nodes_disk_setup(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    let mut all_nodes_data = json!({});
    
    for (node_name, _node_agent_url) in spdk_nodes.iter() {
        let node_agent_base = get_node_agent_url(&spdk_nodes, node_name).unwrap_or_default();
        
        // Get uninitialized disks for this node
        match http_client
            .get(&format!("{}/api/disks/uninitialized", node_agent_base))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                match response.json::<serde_json::Value>().await {
                    Ok(data) => {
                        all_nodes_data[node_name] = data;
                    }
                    Err(_) => {
                        all_nodes_data[node_name] = json!({
                            "success": false,
                            "error": "Failed to parse response",
                            "node": node_name,
                            "disks": []
                        });
                    }
                }
            }
            _ => {
                all_nodes_data[node_name] = json!({
                    "success": false,
                    "error": "Failed to connect to node-agent",
                    "node": node_name,
                    "disks": []
                });
            }
        }
    }
    
    Ok(warp::reply::json(&json!({
        "success": true,
        "nodes": all_nodes_data
    })))
}

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
