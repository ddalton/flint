// spdk_dashboard_backend_minimal.rs - Minimal State Dashboard Backend
// Replaces CRD queries with Node Agent API calls for better performance

use warp::{Filter, Reply};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use reqwest::Client as HttpClient;
use kube::{Client, Api, api::ListParams};
use chrono::{Utc, DateTime};
use std::env;
use k8s_openapi::api::core::v1::Pod;

use crate::minimal_models::DiskInfo;

// Dashboard data structures - kept compatible with frontend
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
    nvmeof_targets: Vec<NvmeofTargetInfo>,
    nvmeof_enabled: bool,
    transport_type: String,
    target_port: u16,
    raid_status: Option<DashboardRaidStatus>,
    ublk_device: Option<serde_json::Value>,
    spdk_validation_status: SpdkValidationStatus,
    pvc_info: Option<PvcInfo>,
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
    orphaned_spdk_volumes: Vec<OrphanedVolumeInfo>,
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
struct NvmeofTargetInfo {
    nqn: String,
    target_ip: String,
    target_port: u16,
    transport: String,
    node: String,
    bdev_name: String,
    active: bool,
    connection_count: u64,
}

#[derive(Serialize, Debug, Clone)]
struct DashboardRaidStatus {
    raid_level: u32,
    state: String,
    num_members: u32,
    operational_members: u32,
    discovered_members: u32,
    members: Vec<RaidMember>,
    rebuild_info: Option<RebuildInfo>,
    superblock_version: Option<u32>,
    auto_rebuild_enabled: bool,
}

#[derive(Serialize, Debug, Clone)]
struct RaidMember {
    slot: u32,
    name: String,
    state: String,
    uuid: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
struct RebuildInfo {
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
struct SpdkValidationStatus {
    has_spdk_backing: bool,
    validation_message: Option<String>,
    validation_severity: String, // "info", "warning", "error"
}

#[derive(Serialize, Debug, Clone)]
struct PvcInfo {
    name: String,
    namespace: String,
    storage_class: String,
    creation_timestamp: String,
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
struct OrphanedVolumeInfo {
    spdk_volume_name: String,
    spdk_volume_uuid: String,
    size_blocks: u64,
    size_gb: f64,
    orphaned_since: String,
}

#[derive(Serialize, Debug)]
struct DashboardData {
    volumes: Vec<DashboardVolume>,
    raw_volumes: Vec<serde_json::Value>,  // Unmanaged SPDK volumes
    disks: Vec<DashboardDisk>,
    nodes: Vec<String>,
}

#[derive(Clone)]
pub struct AppState {
    kube_client: Client,
    node_agents: Arc<RwLock<HashMap<String, String>>>, // node -> agent_url
    cache: Arc<RwLock<Option<DashboardData>>>,
    last_update: Arc<RwLock<DateTime<Utc>>>,
    target_namespace: String,
}


/// Get the current pod's namespace from the service account token
async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(namespace) = env::var("FLINT_NAMESPACE") {
        return Ok(namespace);
    }
    
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
    
    println!("⚠️ [NAMESPACE] Using fallback namespace: flint-system");
    Ok("flint-system".to_string())
}

/// Discover node agents by finding node agent pods
async fn discover_node_agents(kube_client: &Client, namespace: &str) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let pods_api: Api<Pod> = Api::namespaced(kube_client.clone(), namespace);
    let list_params = ListParams::default().labels("app=flint-csi-node");
    
    let pods = pods_api.list(&list_params).await?;
    let mut node_agents = HashMap::new();
    
    for pod in pods.items {
        if let (Some(_pod_name), Some(status)) = (pod.metadata.name, pod.status) {
            if let (Some(node_name), Some(pod_ip)) = (
                pod.spec.and_then(|s| s.node_name),
                status.pod_ip
            ) {
                let agent_url = format!("http://{}:8081", pod_ip);
                println!("🔍 [NODE_DISCOVERY] Found node agent for {}: {}", node_name, agent_url);
                node_agents.insert(node_name, agent_url);
            }
        }
    }
    
    println!("✅ [NODE_DISCOVERY] Discovered {} node agents", node_agents.len());
    Ok(node_agents)
}

/// Fetch all disks from all node agents
async fn fetch_all_disks_from_node_agents(state: &AppState) -> Result<Vec<DashboardDisk>, Box<dyn std::error::Error>> {
    let node_agents = state.node_agents.read().await;
    let http_client = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let mut all_disks = Vec::new();
    
    for (node_name, agent_url) in node_agents.iter() {
        println!("🔍 [DISK_FETCH] Fetching disks from node: {} (fast mode)", node_name);
        
        // Use POST endpoint which calls discover_local_disks_fast() - skips expensive auto-recovery
        match http_client
            .post(&format!("{}/api/disks", agent_url))
            .json(&json!({}))  // Empty body for POST request
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    match response.json::<serde_json::Value>().await {
                        Ok(data) => {
                            if let Some(disks_array) = data["disks"].as_array() {
                                for disk_json in disks_array {
                                    if let Ok(disk_info) = serde_json::from_value::<DiskInfo>(disk_json.clone()) {
                                        let dashboard_disk = convert_disk_info_to_dashboard(&disk_info);
                                        all_disks.push(dashboard_disk);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("⚠️ [DISK_FETCH] Failed to parse response from {}: {}", node_name, e);
                        }
                    }
                } else {
                    println!("⚠️ [DISK_FETCH] HTTP error from {}: {}", node_name, response.status());
                }
            }
            Err(e) => {
                println!("⚠️ [DISK_FETCH] Failed to connect to {} (timeout or connection error): {}", node_name, e);
                // Continue with other nodes instead of failing completely
            }
        }
    }
    
    println!("✅ [DISK_FETCH] Collected {} disks from {} node agents", all_disks.len(), node_agents.len());
    Ok(all_disks)
}

/// Convert minimal DiskInfo to dashboard format
fn convert_disk_info_to_dashboard(disk_info: &DiskInfo) -> DashboardDisk {
    DashboardDisk {
        id: format!("{}_{}", disk_info.node_name, disk_info.pci_address.replace(":", "-")),
        node: disk_info.node_name.clone(),
        pci_addr: disk_info.pci_address.clone(),
        capacity: disk_info.size_bytes as i64,
        capacity_gb: (disk_info.size_bytes / (1024 * 1024 * 1024)) as i64,
        allocated_space: (disk_info.size_bytes - disk_info.free_space) as i64,
        free_space: disk_info.free_space as i64,
        free_space_display: format!("{}GB", disk_info.free_space / (1024 * 1024 * 1024)),
        healthy: disk_info.healthy,
        blobstore_initialized: disk_info.blobstore_initialized,
        lvol_count: disk_info.lvol_count,
        model: disk_info.model.clone(),
        read_iops: 0,     // TODO: Get from SPDK metrics
        write_iops: 0,    // TODO: Get from SPDK metrics
        read_latency: 0,  // TODO: Get from SPDK metrics
        write_latency: 0, // TODO: Get from SPDK metrics
        brought_online: Utc::now().to_rfc3339(),
        provisioned_volumes: Vec::new(), // TODO: Get from volume discovery
        orphaned_spdk_volumes: Vec::new(), // TODO: Get from SPDK queries
    }
}

/// Get all active PV lvol UUIDs from Kubernetes
async fn get_active_pv_lvol_uuids(kube_client: &Client) -> Result<std::collections::HashSet<String>, Box<dyn std::error::Error>> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    
    let pvs_api: Api<PersistentVolume> = Api::all(kube_client.clone());
    let pvs = pvs_api.list(&ListParams::default()).await?;
    
    let mut active_uuids = std::collections::HashSet::new();
    
    for pv in pvs.items {
        if let Some(csi) = &pv.spec.and_then(|s| s.csi) {
            if csi.driver == "flint.csi.storage.io" {
                if let Some(attrs) = &csi.volume_attributes {
                    if let Some(lvol_uuid) = attrs.get("flint.csi.storage.io/lvol-uuid") {
                        active_uuids.insert(lvol_uuid.clone());
                    }
                }
            }
        }
    }
    
    println!("📋 [DASHBOARD] Found {} active PV lvol UUIDs", active_uuids.len());
    Ok(active_uuids)
}

/// Fetch all lvols from a specific node
async fn fetch_lvols_from_node(node_url: &str, node_name: &str) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let client = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    
    let url = format!("{}/api/spdk/rpc", node_url);
    let payload = json!({
        "method": "bdev_get_bdevs"
    });
    
    let response = client.post(&url)
        .json(&payload)
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Err(format!("Failed to get bdevs from {}: {}", node_name, response.status()).into());
    }
    
    let data: serde_json::Value = response.json().await?;
    
    if let Some(bdevs) = data["result"].as_array() {
        // Filter for logical volumes only
        let lvols: Vec<_> = bdevs.iter()
            .filter(|b| b["product_name"].as_str() == Some("Logical Volume"))
            .cloned()
            .collect();
        
        println!("   Found {} lvols on {}", lvols.len(), node_name);
        Ok(lvols)
    } else {
        Ok(Vec::new())
    }
}

/// Detect orphaned lvols across all nodes
async fn detect_orphaned_lvols(state: &AppState) -> Result<HashMap<String, Vec<OrphanedVolumeInfo>>, Box<dyn std::error::Error>> {
    println!("🔍 [DASHBOARD] Detecting orphaned lvols...");
    
    // Get active PV lvol UUIDs from Kubernetes
    let active_uuids = get_active_pv_lvol_uuids(&state.kube_client).await?;
    
    let node_agents = state.node_agents.read().await;
    let mut orphaned_by_node: HashMap<String, Vec<OrphanedVolumeInfo>> = HashMap::new();
    
    for (node_name, node_url) in node_agents.iter() {
        println!("   Checking node: {}", node_name);
        
        match fetch_lvols_from_node(node_url, node_name).await {
            Ok(lvols) => {
                let mut orphans = Vec::new();
                
                for lvol in lvols {
                    let uuid = lvol["uuid"].as_str().unwrap_or("");
                    let name = lvol.get("aliases")
                        .and_then(|a| a.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|a| a.as_str())
                        .unwrap_or(uuid);
                    
                    // Check if this lvol is referenced by any active PV
                    if !active_uuids.contains(uuid) {
                        let size_blocks = lvol["num_blocks"].as_u64().unwrap_or(0);
                        let block_size = lvol["block_size"].as_u64().unwrap_or(512);
                        let size_bytes = size_blocks * block_size;
                        let size_gb = size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                        
                        orphans.push(OrphanedVolumeInfo {
                            spdk_volume_name: name.to_string(),
                            spdk_volume_uuid: uuid.to_string(),
                            size_blocks,
                            size_gb,
                            orphaned_since: Utc::now().to_rfc3339(),
                        });
                    }
                }
                
                if !orphans.is_empty() {
                    println!("   ⚠️ Found {} orphaned lvols on {}", orphans.len(), node_name);
                    orphaned_by_node.insert(node_name.clone(), orphans);
                } else {
                    println!("   ✓ No orphaned lvols on {}", node_name);
                }
            }
            Err(e) => {
                println!("   ⚠️ Failed to fetch lvols from {}: {}", node_name, e);
            }
        }
    }
    
    let total_orphans: usize = orphaned_by_node.values().map(|v| v.len()).sum();
    println!("✅ [DASHBOARD] Orphan detection complete: {} orphaned lvols total", total_orphans);
    
    Ok(orphaned_by_node)
}

/// Refresh dashboard data using minimal state approach
async fn refresh_dashboard_data_minimal(state: &AppState) -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 [MINIMAL_DASHBOARD_REFRESH] Starting minimal state dashboard refresh...");
    
    // Discover node agents
    let node_agents = discover_node_agents(&state.kube_client, &state.target_namespace).await?;
    *state.node_agents.write().await = node_agents;
    
    // Fetch disks from all node agents
    let mut dashboard_disks = fetch_all_disks_from_node_agents(state).await?;
    
    // Detect orphaned lvols and populate disk orphaned_spdk_volumes
    let orphaned_by_node = detect_orphaned_lvols(state).await?;
    
    // Add orphaned volumes to their respective disks
    for disk in dashboard_disks.iter_mut() {
        if let Some(orphans) = orphaned_by_node.get(&disk.node) {
            disk.orphaned_spdk_volumes = orphans.clone();
            println!("   Added {} orphaned lvols to disk on {}", orphans.len(), disk.node);
        }
    }
    
    // TODO: Fetch volumes from node agents (when volume management is added)
    let dashboard_volumes = Vec::new();
    let raw_volumes = Vec::new();
    
    // Get unique node names
    let nodes: Vec<String> = dashboard_disks.iter()
        .map(|d| d.node.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    
    let dashboard_data = DashboardData {
        volumes: dashboard_volumes,
        raw_volumes,
        disks: dashboard_disks,
        nodes,
    };
    
    *state.cache.write().await = Some(dashboard_data);
    *state.last_update.write().await = Utc::now();
    
    println!("✅ [MINIMAL_DASHBOARD_REFRESH] Refresh completed successfully");
    Ok(())
}

/// Handle GET /api/dashboard - Main dashboard endpoint
async fn get_dashboard_data_minimal(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [DASHBOARD_API] Handling /api/dashboard request");
    
    // Check if we need to refresh (every 5 minutes or if no cache)
    let should_refresh = {
        let last_update = state.last_update.read().await;
        let cache = state.cache.read().await;
        cache.is_none() || Utc::now().signed_duration_since(*last_update).num_seconds() > 300
    };
    
    if should_refresh {
        if let Err(e) = refresh_dashboard_data_minimal(&state).await {
            println!("❌ [DASHBOARD_API] Failed to refresh data: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&json!({"error": "Failed to refresh dashboard data"})),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ));
        }
    }
    
    let cache = state.cache.read().await;
    match cache.as_ref() {
        Some(data) => {
            println!("✅ [DASHBOARD_API] Returning dashboard data: {} volumes, {} disks, {} nodes", 
                data.volumes.len(), data.disks.len(), data.nodes.len());
            Ok(warp::reply::with_status(
                warp::reply::json(data),
                warp::http::StatusCode::OK
            ))
        }
        None => {
            println!("❌ [DASHBOARD_API] No cached data available");
            Ok(warp::reply::with_status(
                warp::reply::json(&json!({"error": "No data available"})),
                warp::http::StatusCode::SERVICE_UNAVAILABLE
            ))
        }
    }
}

/// Proxy node agent endpoints for dashboard compatibility
async fn proxy_node_agent_endpoint(
    node: String,
    endpoint: String,
    method: String,
    body: Option<serde_json::Value>,
    state: AppState
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [PROXY] Proxying {} {} for node: {}", method, endpoint, node);
    
    let node_agents = state.node_agents.read().await;
    if let Some(agent_url) = node_agents.get(&node) {
        let http_client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| warp::reject::reject())?;
        let full_url = format!("{}{}", agent_url, endpoint);
        
        let result = match method.as_str() {
            "GET" => {
                http_client.get(&full_url)
                    .timeout(std::time::Duration::from_secs(8))
                    .send().await
            }
            "POST" => {
                let mut request = http_client.post(&full_url);
                if let Some(json_body) = body {
                    request = request.json(&json_body);
                }
                request.timeout(std::time::Duration::from_secs(8)).send().await
            }
            _ => {
                return Ok(warp::reply::with_status(
                    warp::reply::json(&json!({"error": "Method not supported"})),
                    warp::http::StatusCode::METHOD_NOT_ALLOWED
                ));
            }
        };
        
        match result {
            Ok(response) => {
                if response.status().is_success() {
                    match response.json::<serde_json::Value>().await {
                        Ok(data) => Ok(warp::reply::with_status(
                            warp::reply::json(&data),
                            warp::http::StatusCode::OK
                        )),
                        Err(_) => Ok(warp::reply::with_status(
                            warp::reply::json(&json!({"success": false, "error": "Failed to parse response"})),
                            warp::http::StatusCode::BAD_GATEWAY
                        ))
                    }
                } else {
                    Ok(warp::reply::with_status(
                        warp::reply::json(&json!({"success": false, "error": format!("Node agent returned: {}", response.status())})),
                        warp::http::StatusCode::BAD_GATEWAY
                    ))
                }
            }
            Err(e) => {
                Ok(warp::reply::with_status(
                    warp::reply::json(&json!({"success": false, "error": format!("Failed to connect: {}", e)})),
                    warp::http::StatusCode::SERVICE_UNAVAILABLE
                ))
            }
        }
    } else {
        Ok(warp::reply::with_status(
            warp::reply::json(&json!({"success": false, "error": "Node agent not found"})),
            warp::http::StatusCode::NOT_FOUND
        ))
    }
}

/// Proxy node agent endpoints with longer timeout for disk operations
async fn proxy_node_agent_endpoint_long(
    node: String,
    endpoint: String,
    method: String,
    body: Option<serde_json::Value>,
    state: AppState
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [PROXY_LONG] Proxying {} {} for node: {} (extended timeout)", method, endpoint, node);
    
    let node_agents = state.node_agents.read().await;
    if let Some(agent_url) = node_agents.get(&node) {
        let http_client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|_| warp::reject::reject())?;
        let full_url = format!("{}{}", agent_url, endpoint);
        
        let result = match method.as_str() {
            "GET" => {
                http_client.get(&full_url)
                    .timeout(std::time::Duration::from_secs(45))
                    .send().await
            }
            "POST" => {
                let mut request = http_client.post(&full_url);
                if let Some(json_body) = body {
                    request = request.json(&json_body);
                }
                request.timeout(std::time::Duration::from_secs(45)).send().await
            }
            _ => {
                return Ok(warp::reply::with_status(
                    warp::reply::json(&json!({"error": "Method not supported"})),
                    warp::http::StatusCode::METHOD_NOT_ALLOWED
                ));
            }
        };
        
        match result {
            Ok(response) => {
                if response.status().is_success() {
                    match response.json::<serde_json::Value>().await {
                        Ok(data) => Ok(warp::reply::with_status(
                            warp::reply::json(&data),
                            warp::http::StatusCode::OK
                        )),
                        Err(_) => Ok(warp::reply::with_status(
                            warp::reply::json(&json!({"success": false, "error": "Failed to parse response"})),
                            warp::http::StatusCode::BAD_GATEWAY
                        ))
                    }
                } else {
                    Ok(warp::reply::with_status(
                        warp::reply::json(&json!({"success": false, "error": format!("Node agent returned: {}", response.status())})),
                        warp::http::StatusCode::BAD_GATEWAY
                    ))
                }
            }
            Err(e) => {
                Ok(warp::reply::with_status(
                    warp::reply::json(&json!({"success": false, "error": format!("Failed to connect: {}", e)})),
                    warp::http::StatusCode::SERVICE_UNAVAILABLE
                ))
            }
        }
    } else {
        Ok(warp::reply::with_status(
            warp::reply::json(&json!({"success": false, "error": "Node agent not found"})),
            warp::http::StatusCode::NOT_FOUND
        ))
    }
}

/// Get all snapshots from all nodes
async fn get_all_snapshots(state: AppState) -> Result<impl Reply, warp::Rejection> {
    println!("📸 [DASHBOARD] Fetching snapshots from all nodes");
    
    let node_agents = state.node_agents.read().await;
    let mut all_snapshots = Vec::new();
    
    for (node_name, node_url) in node_agents.iter() {
        println!("   Querying node: {}", node_name);
        
        let client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| warp::reject())?;
        
        let url = format!("{}/api/snapshots/list", node_url);
        
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                if let Ok(data) = response.json::<serde_json::Value>().await {
                    if let Some(snapshots) = data["snapshots"].as_array() {
                        for snapshot in snapshots {
                            let mut snapshot_with_node = snapshot.clone();
                            snapshot_with_node["node"] = json!(node_name);
                            all_snapshots.push(snapshot_with_node);
                        }
                        println!("   ✓ Found {} snapshots on {}", snapshots.len(), node_name);
                    }
                }
            }
            Ok(_) => {
                println!("   ⚠️ Non-success response from {}", node_name);
            }
            Err(e) => {
                println!("   ⚠️ Failed to query {}: {}", node_name, e);
            }
        }
    }
    
    println!("✅ [DASHBOARD] Total snapshots found: {}", all_snapshots.len());
    Ok(warp::reply::json(&all_snapshots))
}

/// Build snapshot tree/hierarchy from all nodes
async fn get_snapshots_tree(state: AppState) -> Result<impl Reply, warp::Rejection> {
    println!("🌳 [DASHBOARD] Building snapshot tree from all nodes");
    
    let node_agents = state.node_agents.read().await;
    let mut all_snapshots = Vec::new();
    
    // Collect all snapshots
    for (node_name, node_url) in node_agents.iter() {
        let client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| warp::reject())?;
        
        let url = format!("{}/api/snapshots/list", node_url);
        
        if let Ok(response) = client.get(&url).send().await {
            if let Ok(data) = response.json::<serde_json::Value>().await {
                if let Some(snapshots) = data["snapshots"].as_array() {
                    for snapshot in snapshots {
                        let mut s = snapshot.clone();
                        s["node"] = json!(node_name);
                        all_snapshots.push(s);
                    }
                }
            }
        }
    }
    
    // Build tree structure
    // Group snapshots by parent-child relationships
    let mut tree_nodes = Vec::new();
    let mut root_snapshots = Vec::new();
    
    for snapshot in &all_snapshots {
        let snapshot_id = snapshot["snapshot_uuid"].as_str().unwrap_or("");
        let parent_id = snapshot["parent_id"].as_str();
        
        if parent_id.is_none() || parent_id.unwrap().is_empty() {
            // Root level snapshot (no parent)
            root_snapshots.push(snapshot.clone());
        } else {
            // Child snapshot
            tree_nodes.push(snapshot.clone());
        }
    }
    
    // Build hierarchical tree
    fn build_children(parent_id: &str, snapshots: &[serde_json::Value]) -> Vec<serde_json::Value> {
        snapshots.iter()
            .filter(|s| s["parent_id"].as_str().unwrap_or("") == parent_id)
            .map(|s| {
                let mut node = s.clone();
                let children = build_children(
                    s["snapshot_uuid"].as_str().unwrap_or(""),
                    snapshots
                );
                if !children.is_empty() {
                    node["children"] = json!(children);
                }
                node
            })
            .collect()
    }
    
    let tree: Vec<_> = root_snapshots.iter()
        .map(|root| {
            let mut node = root.clone();
            let children = build_children(
                root["snapshot_uuid"].as_str().unwrap_or(""),
                &tree_nodes
            );
            if !children.is_empty() {
                node["children"] = json!(children);
            }
            node
        })
        .collect();
    
    println!("✅ [DASHBOARD] Built snapshot tree with {} roots", tree.len());
    Ok(warp::reply::json(&tree))
}

/// Delete an orphaned lvol by UUID
async fn delete_orphaned_lvol(
    lvol_uuid: String,
    state: AppState
) -> Result<impl Reply, warp::Rejection> {
    println!("🗑️ [DASHBOARD] Deleting orphaned lvol: {}", lvol_uuid);
    
    let node_agents = state.node_agents.read().await;
    
    // Find which node has this lvol and delete it
    for (node_name, node_url) in node_agents.iter() {
        let client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| warp::reject())?;
        
        // Try to delete lvol on this node using bdev_lvol_delete
        let url = format!("{}/api/spdk/rpc", node_url);
        let payload = json!({
            "method": "bdev_lvol_delete",
            "params": {
                "name": lvol_uuid
            }
        });
        
        match client.post(&url).json(&payload).send().await {
            Ok(response) if response.status().is_success() => {
                println!("✅ [DASHBOARD] Orphaned lvol {} deleted from node {}", lvol_uuid, node_name);
                
                // Invalidate cache to force refresh on next dashboard request
                *state.cache.write().await = None;
                println!("🔄 [DASHBOARD] Cache invalidated - next request will refresh");
                
                return Ok(warp::reply::json(&json!({
                    "success": true,
                    "lvol_uuid": lvol_uuid,
                    "node": node_name
                })));
            }
            Ok(response) => {
                // Check if it's a "not found" error
                if let Ok(body) = response.text().await {
                    if body.contains("No such device") || body.contains("not found") {
                        // Lvol not on this node, try next
                        continue;
                    }
                    println!("   ⚠️ Error from {}: {}", node_name, body);
                }
                continue;
            }
            Err(e) => {
                println!("   ⚠️ Failed to query {}: {}", node_name, e);
                continue;
            }
        }
    }
    
    // Lvol not found on any node
    println!("❌ [DASHBOARD] Orphaned lvol {} not found on any node", lvol_uuid);
    Ok(warp::reply::json(&json!({
        "success": false,
        "error": format!("Orphaned lvol {} not found", lvol_uuid)
    })))
}

/// Delete a snapshot by ID (find node and delete)
async fn delete_snapshot_by_id(
    snapshot_id: String,
    state: AppState
) -> Result<impl Reply, warp::Rejection> {
    println!("🗑️ [DASHBOARD] Deleting snapshot: {}", snapshot_id);
    
    let node_agents = state.node_agents.read().await;
    
    // Find which node has this snapshot
    for (node_name, node_url) in node_agents.iter() {
        let client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| warp::reject())?;
        
        // Try to delete on this node
        let url = format!("{}/api/snapshots/delete", node_url);
        let payload = json!({
            "snapshot_uuid": snapshot_id
        });
        
        match client.post(&url).json(&payload).send().await {
            Ok(response) if response.status().is_success() => {
                println!("✅ [DASHBOARD] Snapshot {} deleted from node {}", snapshot_id, node_name);
                return Ok(warp::reply::json(&json!({
                    "success": true,
                    "snapshot_id": snapshot_id,
                    "node": node_name
                })));
            }
            Ok(response) if response.status() == warp::http::StatusCode::NOT_FOUND => {
                // Snapshot not on this node, try next
                continue;
            }
            Ok(_) => {
                println!("   ⚠️ Error response from {}", node_name);
                continue;
            }
            Err(e) => {
                println!("   ⚠️ Failed to query {}: {}", node_name, e);
                continue;
            }
        }
    }
    
    // Snapshot not found on any node
    println!("❌ [DASHBOARD] Snapshot {} not found on any node", snapshot_id);
    Ok(warp::reply::json(&json!({
        "success": false,
        "error": format!("Snapshot {} not found", snapshot_id)
    })))
}

/// Setup all HTTP routes for the minimal dashboard backend  
pub fn setup_minimal_dashboard_routes(app_state: AppState) -> impl Filter<Extract = impl Reply, Error = warp::Rejection> + Clone {
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);
    
    let state_filter = warp::any().map(move || app_state.clone());
    
    // Main dashboard endpoint
    let dashboard_route = warp::path("api")
        .and(warp::path("dashboard"))
        .and(warp::get())
        .and(state_filter.clone())
        .and_then(get_dashboard_data_minimal);

    // Proxy routes for node agents
    let proxy_uninitialized = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("disks"))
        .and(warp::path("uninitialized"))
        .and(warp::get())
        .and(state_filter.clone())
        .and_then(|node: String, state: AppState| {
            // Node agent expects POST with empty body for uninitialized disks
            proxy_node_agent_endpoint(node, "/api/disks/uninitialized".to_string(), "POST".to_string(), Some(json!({})), state)
        });

    let proxy_setup = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("disks"))
        .and(warp::path("setup"))
        .and(warp::post())
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            // Setup operations can take longer - use extended timeout
            proxy_node_agent_endpoint_long(node, "/api/disks/setup".to_string(), "POST".to_string(), Some(body), state)
        });

    let proxy_initialize = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("disks"))
        .and(warp::path("initialize"))
        .and(warp::post())
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            // Initialize operations can take longer - use extended timeout
            proxy_node_agent_endpoint_long(node, "/api/disks/initialize".to_string(), "POST".to_string(), Some(body), state)
        });

    let proxy_status = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("disks"))
        .and(warp::path("status"))
        .and(warp::get())
        .and(state_filter.clone())
        .and_then(|node: String, state: AppState| {
            // Node agent expects POST with empty body for status
            proxy_node_agent_endpoint(node, "/api/disks/status".to_string(), "POST".to_string(), Some(json!({})), state)
        });

    let proxy_reset = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("disks"))
        .and(warp::path("reset"))
        .and(warp::post())
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            // Reset operations can take longer - use extended timeout
            proxy_node_agent_endpoint_long(node, "/api/disks/reset".to_string(), "POST".to_string(), Some(body), state)
        });

    let proxy_delete = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("disks"))
        .and(warp::path("delete"))
        .and(warp::post())
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            // Delete operations can take longer - use extended timeout
            proxy_node_agent_endpoint_long(node, "/api/disks/delete".to_string(), "POST".to_string(), Some(body), state)
        });

    let refresh_route = warp::path("api")
        .and(warp::path("refresh"))
        .and(warp::post())
        .and(state_filter.clone())
        .and_then(|state: AppState| async move {
            match refresh_dashboard_data_minimal(&state).await {
                Ok(_) => Ok::<_, warp::Rejection>(warp::reply::json(&json!({"status": "success"}))),
                Err(e) => Ok(warp::reply::json(&json!({"status": "error", "error": e.to_string()}))),
            }
        });
    
    // Snapshot aggregation routes
    let snapshots_list = warp::path("api")
        .and(warp::path("snapshots"))
        .and(warp::path::end())
        .and(warp::get())
        .and(state_filter.clone())
        .and_then(get_all_snapshots);
    
    let snapshots_tree = warp::path("api")
        .and(warp::path("snapshots"))
        .and(warp::path("tree"))
        .and(warp::get())
        .and(state_filter.clone())
        .and_then(get_snapshots_tree);
    
    let snapshot_delete = warp::path("api")
        .and(warp::path("snapshots"))
        .and(warp::path::param::<String>())
        .and(warp::delete())
        .and(state_filter.clone())
        .and_then(delete_snapshot_by_id);
    
    // Orphaned volume deletion route
    let orphan_delete = warp::path("api")
        .and(warp::path("orphans"))
        .and(warp::path::param::<String>())
        .and(warp::delete())
        .and(state_filter.clone())
        .and_then(delete_orphaned_lvol);
    
    dashboard_route
        .or(proxy_uninitialized)
        .or(proxy_setup)
        .or(proxy_initialize)
        .or(proxy_status)
        .or(proxy_reset)
        .or(proxy_delete)
        .or(refresh_route)
        .or(snapshots_list)
        .or(snapshots_tree)
        .or(snapshot_delete)
        .or(orphan_delete)
        .with(cors)
}

/// Initialize and start the minimal dashboard backend
pub async fn start_minimal_dashboard_backend(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 [MINIMAL_DASHBOARD] Starting minimal state dashboard backend on port {}", port);
    
    let kube_client = Client::try_default().await?;
    let target_namespace = get_current_namespace().await?;
    
    let app_state = AppState {
        kube_client,
        node_agents: Arc::new(RwLock::new(HashMap::new())),
        cache: Arc::new(RwLock::new(None)),
        last_update: Arc::new(RwLock::new(DateTime::from_timestamp(0, 0).unwrap_or_else(Utc::now))),
        target_namespace,
    };
    
    println!("🎯 [MINIMAL_DASHBOARD] Using namespace: {}", app_state.target_namespace);
    
    // Initial discovery
    let node_agents = discover_node_agents(&app_state.kube_client, &app_state.target_namespace).await?;
    *app_state.node_agents.write().await = node_agents;
    
    let routes = setup_minimal_dashboard_routes(app_state);
    
    println!("✅ [MINIMAL_DASHBOARD] Dashboard backend ready - serving on 0.0.0.0:{}", port);
    warp::serve(routes).run(([0, 0, 0, 0], port)).await;
    
    Ok(())
}
