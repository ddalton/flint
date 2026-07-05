// spdk_dashboard_backend_minimal.rs - Minimal State Dashboard Backend
// Replaces CRD queries with Node Agent API calls for better performance

use warp::{Filter, Reply};
use serde::{Serialize, Deserialize};
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

// Query parameters for backend filtering
#[derive(Debug, Deserialize, Clone, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct DashboardQuery {
    // Volume filters
    volume_filter: Option<String>, // "all", "healthy", "degraded", "failed", "rebuilding", "local-nvme", "orphaned"
    volume_node: Option<String>,   // Filter volumes by node
    
    // Disk filters
    disk_node: Option<String>,     // Filter disks by node
    disk_initialized: Option<bool>, // Filter by blobstore initialization status
    
    // Node filters
    nodes_with_disks_only: Option<bool>, // Show only nodes that have disks (default: false)
    
    // Global filters
    node: Option<String>,          // Apply to both volumes and disks
}

// Dashboard data structures - kept compatible with frontend
#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
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
    #[schema(value_type = Option<Object>)]
    ublk_device: Option<serde_json::Value>,
    spdk_validation_status: SpdkValidationStatus,
    pvc_info: Option<PvcInfo>,
    current_epoch: Option<String>,
    consumer_raids: Vec<ConsumerRaid>,
}

/// Tier-2 data plane (2b): each consumer node that stages the volume
/// assembles `raid_<pv>` locally from the replica legs — one row per
/// consumer. Presence of the raid bdev on a node IS the consumer set;
/// no VolumeAttachment inference.
#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct ConsumerRaid {
    node: String,
    raid_name: String,
    /// SPDK raid state (online/configuring/offline). "online" with
    /// operational < total is the degraded-n/m case.
    state: String,
    num_base_bdevs: u32,
    num_base_bdevs_operational: u32,
    base_bdevs: Vec<ConsumerRaidMember>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct ConsumerRaidMember {
    /// SPDK nulls name+uuid when a leg fails on an online raid
    /// (raid_bdev_free_base_bdev_resource) — a null member IS a failed slot.
    name: Option<String>,
    uuid: Option<String>,
    is_configured: bool,
    /// Replica (by node) this base backs, resolved against the sync record;
    /// None when unmatchable (failed slot, no record).
    replica_node: Option<String>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct DashboardDisk {
    id: String,
    node: String,
    pci_addr: String,
    capacity: i64,
    capacity_gb: f64,  // Changed to f64 for decimal precision
    allocated_space: f64,  // Changed to f64 for decimal precision (shows metadata overhead)
    free_space: f64,  // Changed to f64 for decimal precision
    free_space_display: String,
    healthy: bool,
    blobstore_initialized: bool,
    // Root/boot disks are never init candidates; the frontend needs this to
    // keep the uninitialized-disk badge from counting disks that can never
    // be initialized.
    is_system_disk: bool,
    lvol_count: u32,
    model: String,
    read_iops: u64,
    write_iops: u64,
    read_latency: u64,
    write_latency: u64,
    brought_online: String,
    provisioned_volumes: Vec<ProvisionedVolume>,
    orphaned_spdk_volumes: Vec<OrphanedVolumeInfo>,
    device_type: String, // "NVMe", "SCSI/SATA", "VirtIO", "IDE", "Unknown"
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
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
    sync: Option<ReplicaSyncInfo>,
}

/// Per-replica sync state projected from the PV `replica-sync-state`
/// annotation — the Tier-2 engine's live signal. `rebuild_progress` above
/// tracks the pre-Tier-2 blind-rebuild model and stays for raid-level
/// rebuild_info only; replica health is judged from here.
#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct ReplicaSyncInfo {
    sync_state: String,
    last_epoch: Option<String>,
    epoch_lag: Option<u64>,
    since: Option<String>,
    reason: Option<String>,
    hot_rejoin: Option<String>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct NvmfTarget {
    nqn: String,
    target_ip: String,
    target_port: String,
    transport_type: String,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
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

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
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

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct RaidMember {
    slot: u32,
    name: String,
    state: String,
    uuid: Option<String>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
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

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct SpdkValidationStatus {
    has_spdk_backing: bool,
    validation_message: Option<String>,
    validation_severity: String, // "info", "warning", "error"
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct PvcInfo {
    name: String,
    namespace: String,
    storage_class: String,
    creation_timestamp: String,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct ProvisionedVolume {
    volume_name: String,
    volume_id: String,
    size: i64,
    provisioned_at: String,
    replica_type: String,
    status: String,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct OrphanedVolumeInfo {
    spdk_volume_name: String,
    spdk_volume_uuid: String,
    size_blocks: u64,
    size_gb: f64,
    orphaned_since: String,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct DashboardData {
    volumes: Vec<DashboardVolume>,
    #[schema(value_type = Vec<Object>)]
    raw_volumes: Vec<serde_json::Value>,  // Unmanaged SPDK volumes
    disks: Vec<DashboardDisk>,
    nodes: Vec<String>,
    node_info: HashMap<String, NodeInfo>,  // node_name -> node info with memory details
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct NodeInfo {
    name: String,
    memory_total_mb: u64,
    memory_available_mb: u64,
    memory_used_mb: u64,
    memory_utilization_pct: f64,
}

// --- Typed endpoint responses ---
// Handlers serialize these structs rather than ad-hoc json! maps so the
// generated OpenAPI spec (src/bin/dashboard_openapi.rs) cannot drift from
// what the API actually sends.

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct VolumesResponse {
    volumes: Vec<DashboardVolume>,
    #[schema(value_type = Vec<Object>)]
    raw_volumes: Vec<serde_json::Value>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct DisksResponse {
    disks: Vec<DashboardDisk>,
    nodes: Vec<String>,
    node_info: HashMap<String, NodeInfo>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct EventsResponse {
    events: Vec<DashboardEvent>,
    windows: Vec<HotRejoinWindow>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct DashboardOverview {
    total_volumes: usize,
    healthy_volumes: usize,
    degraded_volumes: usize,
    failed_volumes: usize,
    faulted_volumes: usize,
    local_nvme_volumes: usize,
    total_disks: usize,
    healthy_disks: usize,
    initialized_disks: usize,
    total_nodes: usize,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct RefreshResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct ApiError {
    error: String,
}

#[derive(Clone)]
pub struct AppState {
    kube_client: Client,
    node_agents: Arc<RwLock<HashMap<String, String>>>, // node -> agent_url
    target_namespace: String,
    // IOPS calculation: store previous iostat snapshots
    iostat_history: Arc<RwLock<HashMap<String, (DateTime<Utc>, serde_json::Value)>>>, // bdev_name -> (timestamp, iostat)
    // Short-TTL cache of the UNFILTERED aggregate. Concurrent viewers (and the
    // per-tab endpoints) share one node fan-out per TTL instead of each
    // triggering their own — the write lock also single-flights the refresh so
    // a burst of requests collapses to one rebuild. Filters are applied per
    // request on the cached clone.
    dashboard_cache: Arc<RwLock<Option<(std::time::Instant, DashboardData)>>>,
}

/// Aggregate cache freshness window. Env `DASHBOARD_CACHE_TTL_MS` (default
/// 3000ms); 0 disables caching (every request rebuilds, pre-cache behavior).
fn dashboard_cache_ttl() -> std::time::Duration {
    std::env::var("DASHBOARD_CACHE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or_else(|| std::time::Duration::from_millis(3000))
}


/// Get the current pod's namespace from the service account token
async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
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
async fn discover_node_agents(kube_client: &Client, namespace: &str) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
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
                let node_agent_port = std::env::var("NODE_AGENT_PORT").unwrap_or("9081".to_string());
                let agent_url = format!("http://{}:{}", pod_ip, node_agent_port);
                println!("🔍 [NODE_DISCOVERY] Found node agent for {}: {}", node_name, agent_url);
                node_agents.insert(node_name, agent_url);
            }
        }
    }
    
    println!("✅ [NODE_DISCOVERY] Discovered {} node agents", node_agents.len());
    Ok(node_agents)
}

/// Read memory information from /proc/meminfo
async fn read_node_memory_info(node_url: &str, node_name: &str) -> Result<NodeInfo, Box<dyn std::error::Error>> {
    // Try to fetch memory info from node agent
    let url = format!("{}/api/system/memory", node_url);
    println!("🔍 [MEMORY_INFO] Fetching memory info for node {} from: {}", node_name, url);

    // Bounded like every other node-agent call: reqwest's default has NO
    // total timeout, so an unreachable node (dead spot instance) parks
    // this future in kernel SYN retries for ~2 minutes — join_all then
    // holds /api/dashboard past the probe deadline.
    let client = HttpClient::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => {
            // If node agent provides memory endpoint, use it
            let mem_data: serde_json::Value = response.json().await?;
            println!("   Raw memory data from node agent: {:?}", mem_data);

            let mem_total_mb = mem_data["total_mb"].as_u64().unwrap_or(0);
            let mem_available_mb = mem_data["available_mb"].as_u64().unwrap_or(0);
            let mem_used_mb = mem_total_mb.saturating_sub(mem_available_mb);
            let mem_utilization_pct = if mem_total_mb > 0 {
                (mem_used_mb as f64 / mem_total_mb as f64) * 100.0
            } else {
                0.0
            };

            println!("   Parsed: total={}MB, available={}MB, used={}MB, util={:.1}%",
                mem_total_mb, mem_available_mb, mem_used_mb, mem_utilization_pct);

            let node_info = NodeInfo {
                name: node_name.to_string(),
                memory_total_mb: mem_total_mb,
                memory_available_mb: mem_available_mb,
                memory_used_mb: mem_used_mb,
                memory_utilization_pct: mem_utilization_pct,
            };
            println!("   ✅ Returning NodeInfo: {:?}", node_info);
            Ok(node_info)
        }
        _ => {
            // Fallback: If running locally or node agent doesn't have memory endpoint,
            // try to read /proc/meminfo directly (only works if dashboard runs on same host)
            match tokio::fs::read_to_string("/proc/meminfo").await {
                Ok(meminfo) => {
                    let mut mem_total_kb = 0u64;
                    let mut mem_available_kb = 0u64;

                    for line in meminfo.lines() {
                        if line.starts_with("MemTotal:") {
                            mem_total_kb = line.split_whitespace()
                                .nth(1)
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                        } else if line.starts_with("MemAvailable:") {
                            mem_available_kb = line.split_whitespace()
                                .nth(1)
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                        }
                    }

                    let mem_total_mb = mem_total_kb / 1024;
                    let mem_available_mb = mem_available_kb / 1024;
                    let mem_used_mb = mem_total_mb.saturating_sub(mem_available_mb);
                    let mem_utilization_pct = if mem_total_mb > 0 {
                        (mem_used_mb as f64 / mem_total_mb as f64) * 100.0
                    } else {
                        0.0
                    };

                    Ok(NodeInfo {
                        name: node_name.to_string(),
                        memory_total_mb: mem_total_mb,
                        memory_available_mb: mem_available_mb,
                        memory_used_mb: mem_used_mb,
                        memory_utilization_pct: mem_utilization_pct,
                    })
                }
                Err(_) => {
                    // If we can't read memory info, return placeholder with 0 values
                    // This prevents the dashboard from breaking
                    Ok(NodeInfo {
                        name: node_name.to_string(),
                        memory_total_mb: 0,
                        memory_available_mb: 0,
                        memory_used_mb: 0,
                        memory_utilization_pct: 0.0,
                    })
                }
            }
        }
    }
}

/// Fetch all disks from all node agents in parallel
async fn fetch_all_disks_from_node_agents(state: &AppState) -> Result<Vec<DashboardDisk>, Box<dyn std::error::Error + Send + Sync>> {
    let node_agents = state.node_agents.read().await;
    let node_count = node_agents.len();
    
    println!("🔍 [DISK_FETCH] Fetching disks from {} nodes in parallel...", node_count);
    
    // Create parallel tasks for each node
    let mut tasks = Vec::new();
    for (node_name, agent_url) in node_agents.iter() {
        let node_name = node_name.clone();
        let agent_url = agent_url.clone();
        let state_clone = state.clone();
        
        tasks.push(tokio::spawn(async move {
            println!("🔍 [DISK_FETCH] Querying node: {}", node_name);
            
            let http_client = match HttpClient::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
            {
                Ok(client) => client,
                Err(e) => {
                    println!("⚠️ [DISK_FETCH] Failed to create HTTP client for {}: {}", node_name, e);
                    return Vec::new();
                }
            };
            
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
                                    let mut node_disks = Vec::new();
                                    for disk_json in disks_array {
                                        if let Ok(disk_info) = serde_json::from_value::<DiskInfo>(disk_json.clone()) {
                                            let dashboard_disk = convert_disk_info_to_dashboard(&disk_info, &agent_url, &state_clone).await;
                                            node_disks.push(dashboard_disk);
                                        }
                                    }
                                    println!("✅ [DISK_FETCH] Node {} returned {} disks", node_name, node_disks.len());
                                    return node_disks;
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
                }
            }
            Vec::new()
        }));
    }
    
    // Wait for all tasks to complete and collect results
    let mut all_disks = Vec::new();
    for task in tasks {
        match task.await {
            Ok(node_disks) => {
                all_disks.extend(node_disks);
            }
            Err(e) => {
                println!("⚠️ [DISK_FETCH] Task join error: {}", e);
            }
        }
    }
    
    println!("✅ [DISK_FETCH] Collected {} disks from {} node agents (parallel)", all_disks.len(), node_count);
    Ok(all_disks)
}

/// Block device statistics from SPDK iostat
#[derive(Debug, Clone, Default)]
struct BdevStats {
    read_iops: u64,
    write_iops: u64,
    read_latency_us: u64,  // Average read latency in microseconds
    write_latency_us: u64, // Average write latency in microseconds
}

/// Fetch IOPS and latency statistics for a bdev
async fn fetch_bdev_stats(
    node_url: &str,
    bdev_name: &str,
    state: &AppState
) -> BdevStats {
    let client = match HttpClient::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build() {
        Ok(c) => c,
        Err(_) => return BdevStats::default()
    };

    let url = format!("{}/api/spdk/rpc", node_url);
    let payload = json!({
        "method": "bdev_get_iostat",
        "params": {
            "name": bdev_name
        }
    });

    let response = match client.post(&url).json(&payload).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return BdevStats::default()
    };

    let data: serde_json::Value = match response.json().await {
        Ok(d) => d,
        Err(_) => return BdevStats::default()
    };
    
    // Extract current stats
    if let Some(bdevs) = data["result"]["bdevs"].as_array() {
        if let Some(bdev) = bdevs.first() {
            let num_read_ops = bdev["num_read_ops"].as_u64().unwrap_or(0);
            let num_write_ops = bdev["num_write_ops"].as_u64().unwrap_or(0);
            let read_latency_ticks = bdev["read_latency_ticks"].as_u64().unwrap_or(0);
            let write_latency_ticks = bdev["write_latency_ticks"].as_u64().unwrap_or(0);
            let tick_rate = bdev["tick_rate"].as_u64().unwrap_or(1); // Default to 1 to avoid division by zero

            // Calculate average latency in microseconds
            // Formula: (latency_ticks / num_ops) / tick_rate * 1,000,000
            let read_latency_us = if num_read_ops > 0 && tick_rate > 0 {
                ((read_latency_ticks as f64 / num_read_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
            } else {
                0
            };

            let write_latency_us = if num_write_ops > 0 && tick_rate > 0 {
                ((write_latency_ticks as f64 / num_write_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
            } else {
                0
            };

            // Calculate IOPS by comparing with previous snapshot
            let mut history = state.iostat_history.write().await;
            let key = format!("{}_{}", node_url, bdev_name);

            if let Some((prev_time, prev_data)) = history.get(&key) {
                let time_diff = Utc::now().signed_duration_since(*prev_time).num_seconds() as f64;

                if time_diff > 0.0 {
                    let prev_read_ops = prev_data["num_read_ops"].as_u64().unwrap_or(0);
                    let prev_write_ops = prev_data["num_write_ops"].as_u64().unwrap_or(0);

                    let read_iops = ((num_read_ops - prev_read_ops) as f64 / time_diff) as u64;
                    let write_iops = ((num_write_ops - prev_write_ops) as f64 / time_diff) as u64;

                    // Update history
                    let current_data = json!({
                        "num_read_ops": num_read_ops,
                        "num_write_ops": num_write_ops
                    });
                    history.insert(key, (Utc::now(), current_data));

                    return BdevStats {
                        read_iops,
                        write_iops,
                        read_latency_us,
                        write_latency_us,
                    };
                }
            }

            // First measurement - store and return stats with 0 IOPS but valid latency
            let current_data = json!({
                "num_read_ops": num_read_ops,
                "num_write_ops": num_write_ops
            });
            history.insert(key, (Utc::now(), current_data));

            return BdevStats {
                read_iops: 0,
                write_iops: 0,
                read_latency_us,
                write_latency_us,
            };
        }
    }

    BdevStats::default()
}

/// Convert minimal DiskInfo to dashboard format (async to fetch stats)
async fn convert_disk_info_to_dashboard(disk_info: &DiskInfo, node_url: &str, state: &AppState) -> DashboardDisk {
    // Get IOPS and latency stats for the base bdev
    let stats = fetch_bdev_stats(node_url, &disk_info.bdev_name, state).await;

    DashboardDisk {
        id: format!("{}_{}", disk_info.node_name, disk_info.pci_address.replace(":", "-")),
        node: disk_info.node_name.clone(),
        pci_addr: disk_info.pci_address.clone(),
        capacity: disk_info.size_bytes as i64,
        // Use f64 for decimal precision (shows actual capacity like 3.98GB instead of 3GB)
        capacity_gb: (disk_info.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0) * 100.0).round() / 100.0,
        free_space: (disk_info.free_space as f64 / (1024.0 * 1024.0 * 1024.0) * 100.0).round() / 100.0,
        allocated_space: {
            let cap_gb = (disk_info.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0) * 100.0).round() / 100.0;
            let free_gb = (disk_info.free_space as f64 / (1024.0 * 1024.0 * 1024.0) * 100.0).round() / 100.0;
            ((cap_gb - free_gb) * 100.0).round() / 100.0  // Shows metadata overhead like 0.02GB
        },
        free_space_display: format!("{:.2}GB", disk_info.free_space as f64 / (1024.0 * 1024.0 * 1024.0)),
        healthy: disk_info.healthy,
        blobstore_initialized: disk_info.blobstore_initialized,
        is_system_disk: disk_info.is_system_disk,
        lvol_count: disk_info.lvol_count,
        model: disk_info.model.clone(),
        read_iops: stats.read_iops,
        write_iops: stats.write_iops,
        read_latency: stats.read_latency_us,
        write_latency: stats.write_latency_us,
        brought_online: Utc::now().to_rfc3339(),
        provisioned_volumes: Vec::new(), // Filled by populate_disk_lvols
        orphaned_spdk_volumes: Vec::new(), // Filled by populate_disk_lvols
        device_type: disk_info.device_type.clone(),
    }
}

/// One lvol classified against the live volume set: provisioned storage
/// (its name parses to a live PV under the identity contract) or an
/// orphan. Tagged with the lvstore that hosts it so attribution lands on
/// the owning DISK.
enum ClassifiedLvol {
    Provisioned(ProvisionedVolume),
    Orphan(OrphanedVolumeInfo),
}

/// Classify a node's lvols by parsing each name's owner via
/// identity::lvol_owner. Names are the contract (identity.rs + CI lint)
/// and survive `_hr` recovery renames that change UUIDs — which is why
/// this does NOT key on the provision-time UUIDs in PV attributes. The
/// predecessor allowlist (`flint.csi.storage.io/lvol-uuid`) only existed
/// on legacy single-replica PVs, so on replica-set clusters every live
/// replica, user snapshot, and epoch snapshot was reported as an orphaned
/// "cleanup candidate".
fn classify_node_lvols(
    node_name: &str,
    lvols: &[serde_json::Value],
    volumes_by_id: &HashMap<&str, &DashboardVolume>,
) -> Vec<(String, ClassifiedLvol)> {
    let mut out = Vec::new();
    for lvol in lvols {
        let uuid = lvol["uuid"].as_str().unwrap_or("");
        let alias = lvol
            .get("aliases")
            .and_then(|a| a.as_array())
            .and_then(|arr| arr.first())
            .and_then(|a| a.as_str())
            .unwrap_or(uuid);
        // Alias shape is `<lvs>/<name>`; without it the lvol can neither be
        // parsed nor attributed to a disk — skip loudly rather than guess.
        let Some((lvs, short_name)) = alias.split_once('/') else {
            println!(
                "⚠️ [DASHBOARD] {}: lvol {} has no lvs-qualified alias; cannot attribute to a disk",
                node_name, alias
            );
            continue;
        };

        let size_blocks = lvol["num_blocks"].as_u64().unwrap_or(0);
        let block_size = lvol["block_size"].as_u64().unwrap_or(512);
        let size_bytes = size_blocks * block_size;

        let owner = crate::identity::lvol_owner(short_name)
            .and_then(|(vol_id, kind)| volumes_by_id.get(vol_id).map(|v| (*v, kind)));

        let entry = match owner {
            Some((volume, kind)) => {
                use crate::identity::LvolKind;
                let is_snapshot = lvol["driver_specific"]["lvol"]["snapshot"]
                    .as_bool()
                    .unwrap_or(false);
                // Heads carry the live per-node replica status; snapshot
                // lvols report what SPDK says they are.
                let status = match kind {
                    LvolKind::Primary | LvolKind::Replica => volume
                        .replica_statuses
                        .iter()
                        .find(|r| r.node == node_name)
                        .map(|r| r.status.clone())
                        .unwrap_or_else(|| "present".to_string()),
                    _ if is_snapshot => "read-only".to_string(),
                    _ => "present".to_string(),
                };
                ClassifiedLvol::Provisioned(ProvisionedVolume {
                    volume_name: volume.name.clone(),
                    volume_id: volume.id.clone(),
                    size: size_bytes as i64,
                    provisioned_at: volume
                        .pvc_info
                        .as_ref()
                        .map(|p| p.creation_timestamp.clone())
                        .unwrap_or_default(),
                    replica_type: match kind {
                        LvolKind::Primary => "primary",
                        LvolKind::Replica => "replica",
                        LvolKind::EpochSnapshot => "epoch-snapshot",
                        LvolKind::UserSnapshot => "user-snapshot",
                        LvolKind::CloneSource => "clone-source",
                    }
                    .to_string(),
                    status,
                })
            }
            None => ClassifiedLvol::Orphan(OrphanedVolumeInfo {
                spdk_volume_name: alias.to_string(),
                spdk_volume_uuid: uuid.to_string(),
                size_blocks,
                size_gb: size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                orphaned_since: Utc::now().to_rfc3339(),
            }),
        };
        out.push((lvs.to_string(), entry));
    }
    out
}

/// One lvol sweep per node fills every disk's provisioned_volumes and
/// orphaned_spdk_volumes. The lvol alias's lvs prefix identifies the disk
/// (lvs names are minted from node+PCI — identity::lvs_name — so they are
/// globally unique); the predecessor cloned the node's whole orphan list
/// onto every disk of that node.
async fn populate_disk_lvols(
    state: &AppState,
    disks: &mut [DashboardDisk],
    volumes: &[DashboardVolume],
) {
    let volumes_by_id: HashMap<&str, &DashboardVolume> =
        volumes.iter().map(|v| (v.id.as_str(), v)).collect();
    let disk_by_lvs: HashMap<String, usize> = disks
        .iter()
        .enumerate()
        .map(|(i, d)| (crate::identity::lvs_name(&d.node, &d.pci_addr), i))
        .collect();

    let node_agents = state.node_agents.read().await.clone();
    let (mut provisioned, mut orphaned) = (0usize, 0usize);
    for (node_name, node_url) in node_agents.iter() {
        let lvols = match fetch_lvols_from_node(node_url, node_name).await {
            Ok(l) => l,
            Err(e) => {
                println!("⚠️ [DASHBOARD] Failed to fetch lvols from {}: {}", node_name, e);
                continue;
            }
        };
        for (lvs, entry) in classify_node_lvols(node_name, &lvols, &volumes_by_id) {
            let Some(&idx) = disk_by_lvs.get(&lvs) else {
                println!(
                    "⚠️ [DASHBOARD] {}: lvstore {} matches no discovered disk",
                    node_name, lvs
                );
                continue;
            };
            match entry {
                ClassifiedLvol::Provisioned(p) => {
                    disks[idx].provisioned_volumes.push(p);
                    provisioned += 1;
                }
                ClassifiedLvol::Orphan(o) => {
                    disks[idx].orphaned_spdk_volumes.push(o);
                    orphaned += 1;
                }
            }
        }
    }
    println!(
        "✅ [DASHBOARD] Lvol classification: {} provisioned, {} orphaned across {} disks",
        provisioned,
        orphaned,
        disks.len()
    );
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

/// A flint engine event projected for the dashboard timeline (2c).
#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct DashboardEvent {
    timestamp: Option<String>,
    event_type: String,
    reason: String,
    volume: String,
    message: String,
    category: String,
    reporting_instance: String,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct WindowStep {
    name: String,
    ms: u64,
}

/// A completed hot-rejoin window parsed from a HotRejoinSucceeded event —
/// the after-the-fact record of the sub-2s window the 2a indicator cannot
/// catch live.
#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct HotRejoinWindow {
    timestamp: Option<String>,
    volume: String,
    node: String,
    raid: String,
    epoch: String,
    window_ms: u64,
    steps: Vec<WindowStep>,
    path: String,
    estimator_bytes: Option<u64>,
}

fn categorize_event_reason(reason: &str) -> &'static str {
    if reason.starts_with("HotRejoin") {
        "hot_rejoin"
    } else if reason.starts_with("VolumeDataPath") {
        "data_path"
    } else if reason.starts_with("Cutover") {
        "cutover"
    } else if reason.starts_with("Epoch") {
        "epoch"
    } else if reason.starts_with("Replica") || reason == "StandbyAdmissionDeferred" {
        "catchup"
    } else if reason == "VolumeDegraded" {
        "health"
    } else {
        "other"
    }
}

/// Parse a HotRejoinSucceeded message (hot_rejoin.rs emit format):
/// "Replica on {node} hot-rejoined raid {raid} at {ef} in {N}ms
///  ({step}={n}ms {step}={n}ms ...); <inline tail with estimator | esnap tail>"
/// Step names contain spaces ("cut E_f", "fenced final delta") but never '=',
/// so segments split on "ms " and names rsplit on '='.
fn parse_hot_rejoin_window(
    volume: &str,
    timestamp: Option<String>,
    message: &str,
) -> Option<HotRejoinWindow> {
    let rest = message.strip_prefix("Replica on ")?;
    let (node, rest) = rest.split_once(" hot-rejoined raid ")?;
    let (raid, rest) = rest.split_once(" at ")?;
    let (epoch, rest) = rest.split_once(" in ")?;
    let (window_ms, rest) = rest.split_once("ms (")?;
    let window_ms: u64 = window_ms.trim().parse().ok()?;
    let (detail, tail) = rest.split_once(");")?;
    let steps: Vec<WindowStep> = detail
        .split("ms ")
        .filter_map(|seg| {
            let (name, ms) = seg.trim().trim_end_matches("ms").rsplit_once('=')?;
            Some(WindowStep {
                name: name.trim().to_string(),
                ms: ms.trim().parse().ok()?,
            })
        })
        .collect();
    let (path, estimator_bytes) = if tail.contains("inline") {
        let est = tail
            .split_once(" bytes est.")
            .and_then(|(before, _)| before.rsplit_once('('))
            .and_then(|(_, n)| n.trim().parse().ok());
        ("inline", est)
    } else {
        ("esnap", None)
    };
    Some(HotRejoinWindow {
        timestamp,
        volume: volume.to_string(),
        node: node.to_string(),
        raid: raid.to_string(),
        epoch: epoch.to_string(),
        window_ms,
        steps,
        path: path.to_string(),
        estimator_bytes,
    })
}

/// Epochs are named strings ("epoch-<vol>-<n>"), not counters: lag is the
/// positional distance from `last_epoch` to `current_epoch` in the recorded
/// epoch history, falling back to the trailing counter in the names when the
/// history has been trimmed past `last_epoch`.
fn epoch_lag(record: &crate::replica_sync::VolumeSyncRecord, last_epoch: Option<&str>) -> Option<u64> {
    let current = record.current_epoch.as_deref()?;
    let last = last_epoch?;
    if last == current {
        return Some(0);
    }
    let pos = |name: &str| record.epochs.iter().position(|e| e.name == name);
    if let (Some(l), Some(c)) = (pos(last), pos(current)) {
        if c >= l {
            return Some((c - l) as u64);
        }
    }
    let counter = |name: &str| name.rsplit('-').next().and_then(|n| n.parse::<u64>().ok());
    match (counter(last), counter(current)) {
        (Some(l), Some(c)) if c >= l => Some(c - l),
        _ => None,
    }
}

/// Project a `replica-sync-state` record into per-replica dashboard rows:
/// (replica_statuses, nodes, active_replicas = in_sync count, current_epoch).
fn project_sync_record(
    rec: &crate::replica_sync::VolumeSyncRecord,
    primary_node: &str,
) -> (Vec<DashboardReplicaStatus>, Vec<String>, i32, Option<String>) {
    use crate::replica_sync::SyncState;
    let statuses: Vec<DashboardReplicaStatus> = rec.replicas.iter().map(|r| {
        DashboardReplicaStatus {
            node: r.node_name.clone(),
            status: match r.sync_state {
                SyncState::InSync => "healthy",
                SyncState::Stale => "stale",
                SyncState::Standby => "standby",
            }.to_string(),
            is_local: r.node_name == primary_node,
            last_io_timestamp: None,
            rebuild_progress: None,
            rebuild_target: None,
            is_new_replica: Some(false),
            nvmf_target: None,
            access_method: "Direct".to_string(),
            raid_member_slot: None,
            raid_member_state: "none".to_string(),
            sync: Some(ReplicaSyncInfo {
                sync_state: r.sync_state.as_str().to_string(),
                last_epoch: r.last_epoch.clone(),
                epoch_lag: if r.sync_state == SyncState::InSync {
                    Some(0)
                } else {
                    epoch_lag(rec, r.last_epoch.as_deref())
                },
                since: r.since.clone(),
                reason: r.reason.clone(),
                hot_rejoin: r.hot_rejoin.clone(),
            }),
        }
    }).collect();
    let in_sync = rec.replicas.iter()
        .filter(|r| r.sync_state == SyncState::InSync)
        .count() as i32;
    let nodes = statuses.iter().map(|s| s.node.clone()).collect();
    (statuses, nodes, in_sync, rec.current_epoch.clone())
}

/// Project one node's `bdev_raid_get_bdevs` result into consumer-raid rows.
/// Only flint volume raids (`raid_<pv>`) are kept.
fn project_node_raids(node: &str, result: &serde_json::Value) -> Vec<ConsumerRaid> {
    let Some(raids) = result.as_array() else { return Vec::new() };
    raids
        .iter()
        .filter_map(|raid| {
            let name = raid.get("name").and_then(|n| n.as_str())?;
            if !name.starts_with("raid_") {
                return None;
            }
            let count = |key: &str| raid.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let base_bdevs = raid
                .get("base_bdevs_list")
                .and_then(|b| b.as_array())
                .map(|bases| {
                    bases
                        .iter()
                        .map(|b| ConsumerRaidMember {
                            name: b.get("name").and_then(|v| v.as_str()).map(str::to_string),
                            uuid: b.get("uuid").and_then(|v| v.as_str()).map(str::to_string),
                            is_configured: b
                                .get("is_configured")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            replica_node: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(ConsumerRaid {
                node: node.to_string(),
                raid_name: name.to_string(),
                state: raid
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                num_base_bdevs: count("num_base_bdevs"),
                num_base_bdevs_operational: count("num_base_bdevs_operational"),
                base_bdevs,
            })
        })
        .collect()
}

/// Label each configured base with the replica it backs, using the same
/// matching rules as `replica_sync::replicas_missing_from_raid`: identity or
/// live uuid, name equal to the lvol uuid (local base), or the deterministic
/// remote bdev name.
fn label_consumer_raid_members(
    raids: &mut [ConsumerRaid],
    volume_id: &str,
    record: &crate::replica_sync::VolumeSyncRecord,
) {
    for raid in raids.iter_mut() {
        for member in raid.base_bdevs.iter_mut() {
            if !member.is_configured {
                continue;
            }
            let uuid = member.uuid.as_deref().unwrap_or("");
            let name = member.name.as_deref().unwrap_or("");
            member.replica_node = record
                .replicas
                .iter()
                .enumerate()
                .find(|(index, rec)| {
                    let live = rec.live_lvol_uuid();
                    let remote =
                        crate::replica_sync::expected_remote_base_bdev(volume_id, *index);
                    uuid == rec.lvol_uuid
                        || uuid == live
                        || name == rec.lvol_uuid
                        || name == live
                        || name == remote
                })
                .map(|(_, rec)| rec.node_name.clone());
        }
    }
}

/// One `bdev_raid_get_bdevs` per node agent (parallel, 5s timeout), keyed by
/// raid name. Failures degrade to "no rows from that node", not an error —
/// a volume with no rows renders as "no consumer" in the UI.
async fn fetch_consumer_raids(state: &AppState) -> HashMap<String, Vec<ConsumerRaid>> {
    let node_agents = state.node_agents.read().await;
    let futures: Vec<_> = node_agents
        .iter()
        .map(|(node, url)| {
            let node = node.clone();
            let url = url.clone();
            async move {
                match query_node_raids(&url).await {
                    Ok(result) => project_node_raids(&node, &result),
                    Err(e) => {
                        println!("   ⚠️ Failed to fetch raid bdevs from {}: {}", node, e);
                        Vec::new()
                    }
                }
            }
        })
        .collect();
    let mut by_raid: HashMap<String, Vec<ConsumerRaid>> = HashMap::new();
    for raids in futures::future::join_all(futures).await {
        for raid in raids {
            by_raid.entry(raid.raid_name.clone()).or_default().push(raid);
        }
    }
    let consumers: usize = by_raid.values().map(|v| v.len()).sum();
    println!("✅ [DASHBOARD] Found {} assembled volume raids across consumers", consumers);
    by_raid
}

async fn query_node_raids(
    node_url: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let client = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let response = client
        .post(format!("{}/api/spdk/rpc", node_url))
        .json(&json!({"method": "bdev_raid_get_bdevs", "params": {"category": "all"}}))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()).into());
    }
    let data: serde_json::Value = response.json().await?;
    Ok(data["result"].clone())
}

/// Fetch managed volumes from Kubernetes PVs
async fn fetch_volumes_from_pvs(
    state: &AppState,
    consumer_raids_by_name: &HashMap<String, Vec<ConsumerRaid>>,
) -> Result<Vec<DashboardVolume>, Box<dyn std::error::Error + Send + Sync>> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    
    println!("🔍 [DASHBOARD] Fetching managed volumes from PVs...");
    
    let pvs_api: Api<PersistentVolume> = Api::all(state.kube_client.clone());
    let pvs = pvs_api.list(&ListParams::default()).await?;
    
    let mut volumes = Vec::new();
    let node_agents = state.node_agents.read().await;
    
    for pv in pvs.items {
        if let Some(spec) = &pv.spec {
            if let Some(csi) = &spec.csi {
                if csi.driver == "flint.csi.storage.io" {
                    let pv_name = pv.metadata.name.as_deref().unwrap_or("unknown");
                    
                    if let Some(attrs) = &csi.volume_attributes {
                        let lvol_uuid = attrs.get("flint.csi.storage.io/lvol-uuid")
                            .map(|s| s.as_str()).unwrap_or("");
                        let node_name = attrs.get("flint.csi.storage.io/node-name")
                            .map(|s| s.as_str()).unwrap_or("");
                        let replica_count = attrs.get("flint.csi.storage.io/replica-count")
                            .and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
                        let size_str = attrs.get("size")
                            .map(|s| s.as_str()).unwrap_or("0");
                        
                        // Get PVC info from claimRef
                        let pvc_info = spec.claim_ref.as_ref()
                            .map(|claim| PvcInfo {
                                name: claim.name.clone().unwrap_or_default(),
                                namespace: claim.namespace.clone().unwrap_or_default(),
                                storage_class: spec.storage_class_name.clone().unwrap_or_default(),
                                creation_timestamp: pv.metadata.creation_timestamp.as_ref()
                                    .map(|t| t.0.to_string()).unwrap_or_default(),
                            });
                        
                        // Check if the lvol still exists on the node
                        let (state_str, validation_status) = if node_name.is_empty() {
                            ("Unknown".to_string(), SpdkValidationStatus {
                                has_spdk_backing: false,
                                validation_message: Some("No node information".to_string()),
                                validation_severity: "error".to_string(),
                            })
                        } else if let Some(node_url) = node_agents.get(node_name) {
                            match verify_lvol_exists(node_url, lvol_uuid).await {
                                Ok(true) => ("Healthy".to_string(), SpdkValidationStatus {
                                    has_spdk_backing: true,
                                    validation_message: None,
                                    validation_severity: "info".to_string(),
                                }),
                                Ok(false) => ("Failed".to_string(), SpdkValidationStatus {
                                    has_spdk_backing: false,
                                    validation_message: Some("SPDK lvol not found on node".to_string()),
                                    validation_severity: "error".to_string(),
                                }),
                                Err(e) => ("Unknown".to_string(), SpdkValidationStatus {
                                    has_spdk_backing: false,
                                    validation_message: Some(format!("Cannot verify: {}", e)),
                                    validation_severity: "warning".to_string(),
                                }),
                            }
                        } else {
                            ("Unknown".to_string(), SpdkValidationStatus {
                                has_spdk_backing: false,
                                validation_message: Some("Node agent not available".to_string()),
                                validation_severity: "warning".to_string(),
                            })
                        };
                        
                        // Multi-replica volumes carry the controller-maintained
                        // replica-sync-state annotation; project it into real
                        // per-replica rows. Single-replica volumes (no
                        // annotation) keep the synthetic primary-only row.
                        let sync_record = pv.metadata.annotations.as_ref()
                            .and_then(|a| a.get(crate::replica_sync::SYNC_STATE_ANNOTATION))
                            .and_then(|s| match crate::replica_sync::VolumeSyncRecord::from_annotation(s) {
                                Ok(rec) => Some(rec),
                                Err(e) => {
                                    println!("⚠️ [DASHBOARD] {}: unparseable replica-sync-state annotation: {}", pv_name, e);
                                    None
                                }
                            });

                        let primary_status = DashboardReplicaStatus {
                            node: node_name.to_string(),
                            status: if validation_status.has_spdk_backing { "active" } else { "failed" }.to_string(),
                            is_local: true,
                            last_io_timestamp: None,
                            rebuild_progress: None,
                            rebuild_target: None,
                            is_new_replica: Some(false),
                            nvmf_target: None,
                            access_method: "Direct".to_string(),
                            raid_member_slot: None,
                            raid_member_state: "none".to_string(),
                            sync: None,
                        };

                        let (replica_statuses, nodes, active_replicas, current_epoch) =
                            match &sync_record {
                                Some(rec) if !rec.replicas.is_empty() => {
                                    project_sync_record(rec, node_name)
                                }
                                _ => (
                                    vec![primary_status],
                                    vec![node_name.to_string()],
                                    if validation_status.has_spdk_backing { replica_count } else { 0 },
                                    None,
                                ),
                            };

                        // Replica-set volumes carry no single node-name
                        // attribute, so the legacy primary-lvol verify reads
                        // them as Unknown. The sync record is the controller-
                        // maintained truth: derive state from it — fully
                        // in_sync → Healthy, partially → Degraded (recovery
                        // in progress), none → Failed.
                        let (state_str, validation_status) = match &sync_record {
                            Some(rec) if !rec.replicas.is_empty() => {
                                let total = rec.replicas.len() as i32;
                                let state = if active_replicas == total {
                                    "Healthy"
                                } else if active_replicas > 0 {
                                    "Degraded"
                                } else {
                                    "Failed"
                                };
                                (state.to_string(), SpdkValidationStatus {
                                    has_spdk_backing: active_replicas > 0,
                                    validation_message: if active_replicas == total {
                                        None
                                    } else {
                                        Some(format!("{}/{} replicas in sync", active_replicas, total))
                                    },
                                    validation_severity: if active_replicas == total { "info" } else { "warning" }.to_string(),
                                })
                            }
                            _ if state_str == "Healthy" && active_replicas < replica_count => {
                                ("Degraded".to_string(), validation_status)
                            }
                            _ => (state_str, validation_status),
                        };

                        // Consumer raids observed on the nodes; labeled
                        // against the sync record when the volume has one.
                        let consumer_raids = {
                            let mut raids = consumer_raids_by_name
                                .get(&crate::identity::raid_name(pv_name))
                                .cloned()
                                .unwrap_or_default();
                            if let Some(rec) = &sync_record {
                                label_consumer_raid_members(&mut raids, pv_name, rec);
                            }
                            raids
                        };

                        volumes.push(DashboardVolume {
                            id: pv_name.to_string(),
                            name: pvc_info.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| pv_name.to_string()),
                            size: size_str.to_string(),
                            state: state_str.clone(),
                            replicas: replica_count,
                            active_replicas,
                            local_nvme: true, // Flint volumes are always local
                            access_method: "Direct".to_string(),
                            rebuild_progress: None,
                            nodes,
                            replica_statuses,
                            nvmeof_targets: Vec::new(),
                            nvmeof_enabled: false,
                            transport_type: "Local".to_string(),
                            target_port: 0,
                            raid_status: None,
                            ublk_device: None,
                            spdk_validation_status: validation_status,
                            pvc_info,
                            current_epoch,
                            consumer_raids,
                        });

                        println!("   Found volume: {} ({})", pv_name, state_str);
                    }
                }
            }
        }
    }
    
    println!("✅ [DASHBOARD] Found {} managed volumes", volumes.len());
    Ok(volumes)
}

/// Verify if an lvol exists on a node by its UUID
async fn verify_lvol_exists(node_url: &str, lvol_uuid: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let client = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(5))
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
        return Err(format!("Failed to query SPDK: {}", response.status()).into());
    }
    
    let data: serde_json::Value = response.json().await?;
    
    if let Some(bdevs) = data["result"].as_array() {
        for bdev in bdevs {
            if bdev["uuid"].as_str() == Some(lvol_uuid) {
                return Ok(true);
            }
        }
    }
    
    Ok(false)
}

/// Detect orphaned lvols across all nodes
/// Filter volumes based on query parameters
fn filter_volumes(volumes: Vec<DashboardVolume>, query: &DashboardQuery) -> Vec<DashboardVolume> {
    let mut filtered = volumes;
    
    // Apply node filter (global or volume-specific)
    if let Some(node) = query.node.as_ref().or(query.volume_node.as_ref()) {
        let node = node.to_lowercase();
        filtered.retain(|v| v.nodes.iter().any(|n| n.to_lowercase().contains(&node)));
        println!("🔍 [FILTER] Volume node filter: {} volumes match '{}'", filtered.len(), node);
    }
    
    // Apply volume state filter
    if let Some(filter) = &query.volume_filter {
        let original_count = filtered.len();
        filtered = match filter.as_str() {
            "healthy" => filtered.into_iter().filter(|v| v.state == "Healthy").collect(),
            "degraded" => filtered.into_iter().filter(|v| v.state == "Degraded").collect(),
            "failed" => filtered.into_iter().filter(|v| v.state == "Failed").collect(),
            "faulted" => filtered.into_iter().filter(|v| v.state == "Degraded" || v.state == "Failed").collect(),
            "rebuilding" => filtered.into_iter().filter(|v| {
                v.replica_statuses.iter().any(|r|
                    r.status == "rebuilding" ||
                    r.rebuild_progress.is_some() ||
                    r.is_new_replica == Some(true) ||
                    // Tier-2 recovery in progress: replica not in_sync
                    r.sync.as_ref().map(|s| s.sync_state != "in_sync").unwrap_or(false)
                ) || v.raid_status.as_ref().map(|rs| rs.rebuild_info.is_some()).unwrap_or(false)
            }).collect(),
            "local-nvme" => filtered.into_iter().filter(|v| v.local_nvme).collect(),
            "orphaned" => Vec::new(), // Orphaned volumes are in raw_volumes, not main volumes
            "all" | _ => filtered,
        };
        println!("🔍 [FILTER] Volume state filter '{}': {} -> {} volumes", filter, original_count, filtered.len());
    }
    
    filtered
}

/// Filter disks based on query parameters
fn filter_disks(disks: Vec<DashboardDisk>, query: &DashboardQuery) -> Vec<DashboardDisk> {
    let mut filtered = disks;
    
    // Apply node filter (global or disk-specific)
    if let Some(node) = query.node.as_ref().or(query.disk_node.as_ref()) {
        let node = node.to_lowercase();
        filtered.retain(|d| d.node.to_lowercase().contains(&node));
        println!("🔍 [FILTER] Disk node filter: {} disks match '{}'", filtered.len(), node);
    }
    
    // Apply initialization status filter
    if let Some(initialized) = query.disk_initialized {
        let original_count = filtered.len();
        filtered.retain(|d| d.blobstore_initialized == initialized);
        println!("🔍 [FILTER] Disk initialized={}: {} -> {} disks", initialized, original_count, filtered.len());
    }
    
    filtered
}

/// Fetch fresh dashboard data using minimal state approach (no caching)
/// Build the UNFILTERED aggregate: the expensive node-agent fan-out (disks,
/// orphan detection, PV volumes, per-node memory for ALL nodes). No query
/// filtering — that is applied per request in `fetch_dashboard_data_minimal`
/// so the cached result serves every filter combination.
async fn build_dashboard_aggregate(state: &AppState) -> Result<DashboardData, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [MINIMAL_DASHBOARD_FETCH] Building fresh dashboard aggregate (node fan-out)...");

    // Discover node agents (updates state for future queries)
    let node_agents = discover_node_agents(&state.kube_client, &state.target_namespace).await?;
    *state.node_agents.write().await = node_agents;

    // Fetch disks from all node agents in parallel
    let mut dashboard_disks = fetch_all_disks_from_node_agents(state).await?;

    // Consumer raid assembly state (2b): one cheap RPC per node agent.
    let consumer_raids_by_name = fetch_consumer_raids(state).await;

    // Fetch managed volumes from Kubernetes PVs
    let dashboard_volumes = fetch_volumes_from_pvs(state, &consumer_raids_by_name).await?;

    // One lvol sweep per node classifies every lvol against the live PV set:
    // provisioned entries and true orphans land on the disk whose lvstore
    // hosts them (volumes must be fetched first — they are the live set).
    populate_disk_lvols(state, &mut dashboard_disks, &dashboard_volumes).await;

    // Per-node memory for ALL discovered nodes (filtering happens on read).
    let node_agents = state.node_agents.read().await;
    let nodes: Vec<String> = node_agents.keys().cloned().collect();
    let memory_futures: Vec<_> = nodes.iter()
        .filter_map(|node_name| {
            node_agents.get(node_name).map(|node_url| {
                let node_name = node_name.clone();
                let node_url = node_url.clone();
                async move {
                    match read_node_memory_info(&node_url, &node_name).await {
                        Ok(info) => Some((node_name.clone(), info)),
                        Err(e) => {
                            eprintln!("⚠️ [MEMORY_INFO] Failed to read memory for node {}: {}", node_name, e);
                            None
                        }
                    }
                }
            })
        })
        .collect();

    let memory_results = futures::future::join_all(memory_futures).await;
    let node_info: HashMap<String, NodeInfo> = memory_results.into_iter()
        .flatten()
        .collect();

    println!("📊 [MEMORY_INFO] Collected memory info for {} nodes", node_info.len());

    let dashboard_data = DashboardData {
        volumes: dashboard_volumes,
        raw_volumes: Vec::new(), // raw_volumes surface as orphans on disks, not separately
        disks: dashboard_disks,
        nodes,
        node_info,
    };

    println!("✅ [MINIMAL_DASHBOARD_FETCH] Aggregate built: {} volumes, {} disks, {} nodes",
        dashboard_data.volumes.len(), dashboard_data.disks.len(), dashboard_data.nodes.len());

    Ok(dashboard_data)
}

/// Return the aggregate, served from the short-TTL cache when fresh. The write
/// lock single-flights the rebuild: concurrent requests during a refresh queue
/// on it, then the re-check serves them the just-built result — so a burst of
/// viewers produces one fan-out, not one per viewer.
async fn get_cached_aggregate(state: &AppState) -> Result<DashboardData, Box<dyn std::error::Error + Send + Sync>> {
    let ttl = dashboard_cache_ttl();
    if !ttl.is_zero() {
        let guard = state.dashboard_cache.read().await;
        if let Some((stamped, data)) = guard.as_ref() {
            if stamped.elapsed() < ttl {
                return Ok(data.clone());
            }
        }
    }

    let mut guard = state.dashboard_cache.write().await;
    // Re-check under the write lock: a concurrent refresher may have just
    // populated it while we waited.
    if !ttl.is_zero() {
        if let Some((stamped, data)) = guard.as_ref() {
            if stamped.elapsed() < ttl {
                return Ok(data.clone());
            }
        }
    }
    let fresh = build_dashboard_aggregate(state).await?;
    *guard = Some((std::time::Instant::now(), fresh.clone()));
    Ok(fresh)
}

async fn fetch_dashboard_data_minimal(state: &AppState, query: Option<DashboardQuery>) -> Result<DashboardData, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(ref q) = query {
        println!("🔍 [FILTER] Query params: {:?}", q);
    }

    let aggregate = get_cached_aggregate(state).await?;

    // Apply filters to the cached clone.
    let (filtered_volumes, filtered_disks) = if let Some(ref q) = query {
        (filter_volumes(aggregate.volumes, q), filter_disks(aggregate.disks, q))
    } else {
        (aggregate.volumes, aggregate.disks)
    };

    // Node list: all discovered nodes, or only those with disks after filtering.
    let nodes: Vec<String> = if query.as_ref().and_then(|q| q.nodes_with_disks_only) == Some(true) {
        filtered_disks.iter()
            .map(|d| d.node.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    } else {
        aggregate.nodes.clone()
    };

    // Narrow node_info to the surfaced nodes.
    let node_info: HashMap<String, NodeInfo> = aggregate.node_info
        .into_iter()
        .filter(|(name, _)| nodes.contains(name))
        .collect();

    Ok(DashboardData {
        volumes: filtered_volumes,
        raw_volumes: Vec::new(),
        disks: filtered_disks,
        nodes,
        node_info,
    })
}

/// Handle GET /api/dashboard - Main dashboard endpoint (always fetches fresh data)
/// Supports query parameters for backend filtering:
/// - volume_filter: "all", "healthy", "degraded", "failed", "rebuilding", "local-nvme"
/// - volume_node: filter volumes by node name (partial match)
/// - disk_node: filter disks by node name (partial match)
/// - disk_initialized: filter disks by blobstore initialization (true/false)
/// - nodes_with_disks_only: show only nodes that have disks (true/false, default: false)
/// - node: global filter for both volumes and disks
async fn get_dashboard_data_minimal(
    query: Option<DashboardQuery>,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [DASHBOARD_API] Handling /api/dashboard request (no cache, fresh data)");
    
    // Always fetch fresh data - parallel queries make this fast even with 100+ nodes
    // Backend filtering reduces network transfer significantly
    match fetch_dashboard_data_minimal(&state, query).await {
        Ok(data) => {
            println!("✅ [DASHBOARD_API] Returning fresh dashboard data: {} volumes, {} disks, {} nodes", 
                data.volumes.len(), data.disks.len(), data.nodes.len());
            Ok(warp::reply::with_status(
                warp::reply::json(&data),
                warp::http::StatusCode::OK
            ))
        }
        Err(e) => {
            println!("❌ [DASHBOARD_API] Failed to fetch dashboard data: {}", e);
            Ok(warp::reply::with_status(
                warp::reply::json(&ApiError { error: format!("Failed to fetch dashboard data: {}", e) }),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ))
        }
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct EventsQuery {
    volume: Option<String>,
    limit: Option<usize>,
}

/// Handle GET /api/events - the flint engine event timeline plus parsed
/// hot-rejoin windows. One namespaced Event list per request (the engine
/// emits all PV events into "default"); no node fan-out, so uncached.
async fn get_events_minimal(
    query: EventsQuery,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    use k8s_openapi::api::core::v1::Event;

    let events_api: Api<Event> = Api::namespaced(state.kube_client.clone(), "default");
    let lp = ListParams::default().fields("involvedObject.kind=PersistentVolume");
    match events_api.list(&lp).await {
        Ok(list) => {
            let mut events: Vec<DashboardEvent> = list
                .items
                .iter()
                .filter_map(|e| {
                    let flint_emitter = e
                        .reporting_component
                        .as_deref()
                        .map(|c| c.starts_with("flint.csi.storage.io"))
                        .unwrap_or(false);
                    if !flint_emitter {
                        return None;
                    }
                    let volume = e.involved_object.name.clone().unwrap_or_default();
                    if let Some(want) = &query.volume {
                        if &volume != want {
                            return None;
                        }
                    }
                    let reason = e.reason.clone().unwrap_or_default();
                    let timestamp = e
                        .event_time
                        .as_ref()
                        .map(|t| t.0.to_string())
                        .or_else(|| e.last_timestamp.as_ref().map(|t| t.0.to_string()))
                        .or_else(|| {
                            e.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string())
                        });
                    Some(DashboardEvent {
                        timestamp,
                        event_type: e.type_.clone().unwrap_or_default(),
                        category: categorize_event_reason(&reason).to_string(),
                        reason,
                        volume,
                        message: e.message.clone().unwrap_or_default(),
                        reporting_instance: e.reporting_instance.clone().unwrap_or_default(),
                    })
                })
                .collect();
            // Newest first (RFC3339 UTC strings sort chronologically).
            events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

            // Windows come from the full filtered set, before the event-list
            // cap, so a recent window is never dropped by timeline volume.
            let windows: Vec<HotRejoinWindow> = events
                .iter()
                .filter(|e| e.reason == "HotRejoinSucceeded")
                .filter_map(|e| parse_hot_rejoin_window(&e.volume, e.timestamp.clone(), &e.message))
                .take(50)
                .collect();

            let limit = query.limit.unwrap_or(200).min(1000);
            events.truncate(limit);

            Ok(warp::reply::with_status(
                warp::reply::json(&EventsResponse { events, windows }),
                warp::http::StatusCode::OK,
            ))
        }
        Err(e) => {
            println!("❌ [EVENTS_API] Failed to list events: {}", e);
            Ok(warp::reply::with_status(
                warp::reply::json(&ApiError { error: format!("Failed to list events: {}", e) }),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

/// Handle POST /api/refresh - invalidate the aggregate cache so the next
/// dashboard/per-tab request rebuilds, and rediscover node agents.
async fn handle_refresh(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🔄 [REFRESH_API] Invalidating cache + rediscovering node agents");

    // Drop the cached aggregate so the next read is fresh (explicit refresh).
    *state.dashboard_cache.write().await = None;

    match discover_node_agents(&state.kube_client, &state.target_namespace).await {
        Ok(node_agents) => {
            *state.node_agents.write().await = node_agents;
            Ok::<_, warp::Rejection>(warp::reply::json(&RefreshResponse {
                status: "success".to_string(),
                message: Some("Cache invalidated; node agents rediscovered".to_string()),
                error: None,
            }))
        }
        Err(e) => Ok(warp::reply::json(&RefreshResponse {
            status: "error".to_string(),
            message: None,
            error: Some(e.to_string()),
        })),
    }
}

/// Handle GET /api/volumes - volumes slice of the cached aggregate.
async fn get_volumes_minimal(
    query: Option<DashboardQuery>,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    match fetch_dashboard_data_minimal(&state, query).await {
        Ok(data) => Ok(warp::reply::with_status(
            warp::reply::json(&VolumesResponse { volumes: data.volumes, raw_volumes: data.raw_volumes }),
            warp::http::StatusCode::OK,
        )),
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&ApiError { error: format!("Failed to fetch volumes: {}", e) }),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle GET /api/disks - disks slice of the cached aggregate.
async fn get_disks_minimal(
    query: Option<DashboardQuery>,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    match fetch_dashboard_data_minimal(&state, query).await {
        Ok(data) => Ok(warp::reply::with_status(
            warp::reply::json(&DisksResponse { disks: data.disks, nodes: data.nodes, node_info: data.node_info }),
            warp::http::StatusCode::OK,
        )),
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&ApiError { error: format!("Failed to fetch disks: {}", e) }),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle GET /api/overview - summary counts only (smallest payload; the
/// Overview tab's 30s tick no longer ships the full volume/disk/snapshot world).
async fn get_overview_minimal(
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    match fetch_dashboard_data_minimal(&state, None).await {
        Ok(data) => Ok(warp::reply::with_status(
            warp::reply::json(&dashboard_overview(&data)),
            warp::http::StatusCode::OK,
        )),
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&ApiError { error: format!("Failed to fetch overview: {}", e) }),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Summary counts derived from the aggregate (mirrors the frontend's stats).
fn dashboard_overview(data: &DashboardData) -> DashboardOverview {
    let degraded = data.volumes.iter().filter(|v| v.state == "Degraded").count();
    let failed = data.volumes.iter().filter(|v| v.state == "Failed").count();
    DashboardOverview {
        total_volumes: data.volumes.len(),
        healthy_volumes: data.volumes.iter().filter(|v| v.state == "Healthy").count(),
        degraded_volumes: degraded,
        failed_volumes: failed,
        faulted_volumes: degraded + failed,
        local_nvme_volumes: data.volumes.iter().filter(|v| v.local_nvme).count(),
        total_disks: data.disks.len(),
        healthy_disks: data.disks.iter().filter(|d| d.healthy).count(),
        initialized_disks: data.disks.iter().filter(|d| d.blobstore_initialized).count(),
        total_nodes: data.nodes.len(),
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
    println!("   Request body: {:?}", body);

    let node_agents = state.node_agents.read().await;
    if let Some(agent_url) = node_agents.get(&node) {
        let http_client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| warp::reject::reject())?;
        let full_url = format!("{}{}", agent_url, endpoint);
        println!("   Full URL: {}", full_url);
        
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
                let status = response.status();
                println!("   Response status: {}", status);
                if status.is_success() {
                    match response.json::<serde_json::Value>().await {
                        Ok(data) => {
                            println!("   Response data: {:?}", data);
                            Ok(warp::reply::with_status(
                                warp::reply::json(&data),
                                warp::http::StatusCode::OK
                            ))
                        }
                        Err(e) => {
                            println!("   ❌ Failed to parse response: {}", e);
                            Ok(warp::reply::with_status(
                                warp::reply::json(&json!({"success": false, "error": "Failed to parse response"})),
                                warp::http::StatusCode::BAD_GATEWAY
                            ))
                        }
                    }
                } else {
                    println!("   ❌ Node agent returned error status: {}", status);
                    // Pass the agent's verdict through, status and body alike —
                    // a refusal (e.g. 409 "lvols exist" from /disks/delete) is
                    // the operator-facing reason, not a gateway fault.
                    Ok(agent_error_passthrough(status, response).await)
                }
            }
            Err(e) => {
                println!("   ❌ Failed to connect to node agent: {}", e);
                Ok(warp::reply::with_status(
                    warp::reply::json(&json!({"success": false, "error": format!("Failed to connect: {}", e)})),
                    warp::http::StatusCode::SERVICE_UNAVAILABLE
                ))
            }
        }
    } else {
        println!("   ❌ Node agent not found: {}", node);
        Ok(warp::reply::with_status(
            warp::reply::json(&json!({"success": false, "error": "Node agent not found"})),
            warp::http::StatusCode::NOT_FOUND
        ))
    }
}

/// Relay a node agent's non-success response verbatim: same status code,
/// same JSON body (non-JSON bodies are wrapped). Anything that can't map
/// onto an HTTP status degrades to 502.
async fn agent_error_passthrough(
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> warp::reply::WithStatus<warp::reply::Json> {
    let passthrough_status = warp::http::StatusCode::from_u16(status.as_u16())
        .unwrap_or(warp::http::StatusCode::BAD_GATEWAY);
    let bytes = response.bytes().await.unwrap_or_default();
    let body = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_else(|_| {
        json!({
            "success": false,
            "error": if bytes.is_empty() {
                format!("Node agent returned: {}", status)
            } else {
                String::from_utf8_lossy(&bytes).to_string()
            }
        })
    });
    warp::reply::with_status(warp::reply::json(&body), passthrough_status)
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
                let status = response.status();
                if status.is_success() {
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
                    // Same passthrough as the short proxy: the agent's 409
                    // refusal reason must reach the operator.
                    Ok(agent_error_passthrough(status, response).await)
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

/// Query SPDK data from a single node (snapshots + bdevs + lvol stores)
async fn query_node_spdk_data(
    node_name: String,
    node_url: String,
) -> Result<(String, serde_json::Value), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::time::timeout;
    use std::time::Duration;
    
    // Timeout for entire node query: 5 seconds
    let result = timeout(Duration::from_secs(5), async {
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(3))
            .build()?;
        
        // Query 1: Get snapshots list
        let snapshots_url = format!("{}/api/snapshots/list", node_url);
        let snapshots_resp = client.get(&snapshots_url).send().await?;
        let snapshots_data: serde_json::Value = snapshots_resp.json().await?;
        let snapshots = snapshots_data["snapshots"].as_array()
            .map(|arr| arr.clone())
            .unwrap_or_default();
        
        // Query 2: Get lvol stores (for cluster size)
        let rpc_url = format!("{}/api/spdk/rpc", node_url);
        let lvs_payload = json!({"method": "bdev_lvol_get_lvstores"});
        let lvs_resp = client.post(&rpc_url)
            .json(&lvs_payload)
            .send()
            .await?;
        let lvs_data: serde_json::Value = lvs_resp.json().await?;
        let lvol_stores = lvs_data.get("result")
            .and_then(|r| r.as_array())
            .map(|arr| arr.clone())
            .unwrap_or_default();
        
        // Query 3: Get bdevs (for allocated clusters)
        let bdevs_payload = json!({"method": "bdev_get_bdevs"});
        let bdevs_resp = client.post(&rpc_url)
            .json(&bdevs_payload)
            .send()
            .await?;
        let bdevs_data: serde_json::Value = bdevs_resp.json().await?;
        let bdevs = bdevs_data.get("result")
            .and_then(|r| r.as_array())
            .map(|arr| arr.clone())
            .unwrap_or_default();
        
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(json!({
            "node": node_name,
            "snapshots": snapshots,
            "lvol_stores": lvol_stores,
            "bdevs": bdevs
        }))
    }).await;
    
    match result {
        Ok(Ok(data)) => Ok((node_name, data)),
        Ok(Err(e)) => {
            println!("   ⚠️ Failed to query {}: {}", node_name, e);
            Err(e)
        }
        Err(_) => {
            println!("   ⚠️ Timeout querying {} (5s)", node_name);
            Err("Timeout".into())
        }
    }
}

/// Build snapshot tree/hierarchy from all nodes with accurate SPDK storage analytics
async fn get_snapshots_tree(state: AppState) -> Result<impl Reply, warp::Rejection> {
    println!("🌳 [DASHBOARD] Building snapshot tree with SPDK data from all nodes (parallel)");
    
    let node_agents = state.node_agents.read().await;
    
    // Launch parallel queries to all nodes
    let query_tasks: Vec<_> = node_agents.iter()
        .map(|(node_name, node_url)| {
            let name = node_name.clone();
            let url = node_url.clone();
            tokio::spawn(async move {
                query_node_spdk_data(name, url).await
            })
        })
        .collect();
    
    // Wait for all queries to complete
    let results = futures::future::join_all(query_tasks).await;
    
    // Process results
    use std::collections::HashMap;
    let mut all_snapshots = Vec::new();
    let mut cluster_size_map: HashMap<String, u64> = HashMap::new();
    let mut bdev_consumption_map: HashMap<String, u64> = HashMap::new();
    let mut active_volume_consumption_map: HashMap<String, u64> = HashMap::new();
    
    for task_result in results {
        if let Ok(Ok((node_name, data))) = task_result {
            println!("   ✓ Got data from node: {}", node_name);
            
            // Extract snapshots
            if let Some(snapshots) = data["snapshots"].as_array() {
                for snapshot in snapshots {
                    let mut s = snapshot.clone();
                    s["node"] = json!(node_name);
                    all_snapshots.push(s);
                }
            }
            
            // Build cluster size map from lvol stores
            if let Some(lvol_stores) = data["lvol_stores"].as_array() {
                for lvs in lvol_stores {
                    if let (Some(uuid), Some(cluster_size)) = (
                        lvs["uuid"].as_str(),
                        lvs["cluster_size"].as_u64()
                    ) {
                        cluster_size_map.insert(uuid.to_string(), cluster_size);
                    }
                }
            }
            
            // Build bdev consumption maps (both snapshots and active volumes)
            if let Some(bdevs) = data["bdevs"].as_array() {
                for bdev in bdevs {
                    let uuid = bdev["uuid"].as_str().unwrap_or("");
                    let lvol_info = &bdev["driver_specific"]["lvol"];
                    let is_snapshot = lvol_info["snapshot"].as_bool().unwrap_or(false);
                    
                    if let (Some(lvs_uuid), Some(allocated_clusters)) = (
                        lvol_info["lvol_store_uuid"].as_str(),
                        lvol_info["num_allocated_clusters"].as_u64()
                    ) {
                        if let Some(&cluster_size) = cluster_size_map.get(lvs_uuid) {
                            let consumed_bytes = allocated_clusters * cluster_size;
                            
                            if is_snapshot {
                                // This is a snapshot bdev
                                bdev_consumption_map.insert(uuid.to_string(), consumed_bytes);
                            } else {
                                // This is an active volume - extract volume_id from alias
                                if let Some(aliases) = bdev["aliases"].as_array() {
                                    if let Some(alias) = aliases.first().and_then(|a| a.as_str()) {
                                        // Alias format: "lvs_node_disk/vol_pvc-xxxxx"
                                        // Extract "pvc-xxxxx" from "vol_pvc-xxxxx"
                                        if let Some(vol_part) = alias.split('/').last() {
                                            if let Some(volume_id) = vol_part.strip_prefix("vol_") {
                                                active_volume_consumption_map.insert(
                                                    volume_id.to_string(), 
                                                    consumed_bytes
                                                );
                                                println!("   → Active volume {}: {} bytes", volume_id, consumed_bytes);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    println!("   Found {} total snapshots across all nodes", all_snapshots.len());
    println!("   Got cluster size info for {} lvol stores", cluster_size_map.len());
    println!("   Got consumption data for {} snapshot bdevs", bdev_consumption_map.len());
    println!("   Got consumption data for {} active volumes", active_volume_consumption_map.len());
    
    // Group snapshots by source_volume_id
    let mut volumes: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    
    for snapshot in &all_snapshots {
        // Older agents report source_volume_id="unknown" for epoch snapshots
        // (their lister predates the epoch-<pv>-<seq> convention); re-derive
        // from the snapshot name with the shared parser so chains group under
        // their real volume instead of one "unknown" bucket.
        let mut volume_id = snapshot["source_volume_id"].as_str().unwrap_or("unknown").to_string();
        if volume_id == "unknown" {
            volume_id = crate::snapshot::snapshot_models::SnapshotInfo::volume_id_from_snapshot_name(
                snapshot["snapshot_name"].as_str().unwrap_or(""),
            );
        }
        volumes.entry(volume_id).or_insert_with(Vec::new).push(snapshot.clone());
    }
    
    // Build tree structure with accurate storage analytics for each volume
    let mut tree_map: HashMap<String, serde_json::Value> = HashMap::new();
    
    for (volume_id, volume_snapshots) in volumes {
        // Get volume size from first snapshot
        let volume_size = volume_snapshots.first()
            .and_then(|s| s["size_bytes"].as_u64())
            .unwrap_or(0);
        
        // Calculate ACTUAL storage consumption from SPDK
        let mut total_snapshot_consumed: u64 = 0;
        let mut snapshot_count_with_data = 0;
        
        for snapshot in &volume_snapshots {
            if let Some(snapshot_uuid) = snapshot["snapshot_uuid"].as_str() {
                if let Some(&consumed_bytes) = bdev_consumption_map.get(snapshot_uuid) {
                    total_snapshot_consumed += consumed_bytes;
                    snapshot_count_with_data += 1;
                } else {
                    // Fallback: use logical size if SPDK data not available
                    total_snapshot_consumed += snapshot["size_bytes"].as_u64().unwrap_or(0);
                }
            }
        }
        
        // Get ACTUAL data size from active volume bdev (not estimated!)
        let actual_data_size = active_volume_consumption_map.get(&volume_id)
            .copied()
            .unwrap_or(volume_size); // Fallback to volume_size if active volume not found
        
        let snapshot_efficiency_ratio = if volume_size > 0 {
            total_snapshot_consumed as f64 / volume_size as f64
        } else {
            0.0
        };
        
        // Build recommendations based on actual consumption
        let mut recommendations = Vec::new();
        
        // Check if we have actual volume data
        let has_actual_volume_data = active_volume_consumption_map.contains_key(&volume_id);
        if !has_actual_volume_data {
            recommendations.push("Note: Using estimated volume size (actual consumption unavailable)".to_string());
        }
        
        if snapshot_count_with_data < volume_snapshots.len() {
            recommendations.push(format!(
                "Warning: Only {}/{} snapshots have SPDK data",
                snapshot_count_with_data,
                volume_snapshots.len()
            ));
        }
        
        if snapshot_efficiency_ratio > 0.5 {
            recommendations.push("HIGH PRIORITY: >50% snapshot overhead detected".to_string());
            recommendations.push("Consider deleting old snapshots".to_string());
        } else if snapshot_efficiency_ratio > 0.3 {
            recommendations.push("Moderate snapshot overhead detected".to_string());
            recommendations.push("Review snapshot retention policy".to_string());
        } else {
            recommendations.push("Storage efficiency is good".to_string());
        }
        
        let storage_analytics = json!({
            "total_volume_size": volume_size,
            "actual_data_size": actual_data_size,
            "total_snapshot_overhead": total_snapshot_consumed,
            "snapshot_efficiency_ratio": snapshot_efficiency_ratio,
            "storage_breakdown": {
                "active_volume_consumption": actual_data_size,
                "snapshot_consumption": total_snapshot_consumed,
                "metadata_overhead": 0,
                "free_space_in_volume": 0
            },
            "recommendations": recommendations
        });
        
        // Transform snapshots into UI-compatible format with storage_info
        let formatted_snapshots: Vec<serde_json::Value> = volume_snapshots.iter().enumerate().map(|(idx, snap)| {
            let snapshot_uuid = snap["snapshot_uuid"].as_str().unwrap_or("");
            let consumed = bdev_consumption_map.get(snapshot_uuid).copied().unwrap_or(0);
            
            // Get lvs_name for output
            let lvs_name = snap["lvs_name"].as_str().unwrap_or("");
            // TODO: Try to get cluster size from lvs_name (e.g., "lvs_flnt-4-46-m1_memory-m1")
            // For now, use default 4MB cluster size
            let cluster_size = 4194304u64;
            
            let allocated_clusters = if cluster_size > 0 { consumed / cluster_size } else { 0 };
            
            json!({
                "bdev_name": snap["snapshot_name"].as_str().unwrap_or(""),
                "snapshot_id": snapshot_uuid,
                "snapshot_uuid": snapshot_uuid,
                "creation_time": snap["creation_time"],
                "size_bytes": snap["size_bytes"],
                "node": snap["node"],
                "lvs_name": lvs_name,
                "source_volume_id": snap["source_volume_id"],
                "ready_to_use": snap["ready_to_use"],
                "snapshot_type": "Bdev",
                "details": snap,
                "children": [],
                "creation_order": idx,
                "is_active_volume": false,
                "storage_info": {
                    "consumed_bytes": consumed,
                    "cluster_size": cluster_size,
                    "allocated_clusters": allocated_clusters
                },
                "replica_bdev_details": [{
                    "node": snap["node"].as_str().unwrap_or("unknown"),
                    "name": snap["snapshot_name"].as_str().unwrap_or(""),
                    "aliases": [snap["snapshot_name"].as_str().unwrap_or("")],
                    "driver": "lvol",
                    "snapshot_source_bdev": crate::identity::lvol_name(snap["source_volume_id"].as_str().unwrap_or("")),
                    "storage_info": {
                        "consumed_bytes": consumed,
                        "cluster_size": cluster_size,
                        "allocated_clusters": allocated_clusters
                    }
                }]
            })
        }).collect();
        
        // Build snapshot chain
        let snapshot_chain = json!({
            "active_lvol": crate::identity::lvol_name(&volume_id),
            "chain_depth": volume_snapshots.len(),
            "snapshots": formatted_snapshots,
            "error": null
        });
        
        tree_map.insert(volume_id.clone(), json!({
            // The id IS the PV name (pvc-…); prefixing "volume-" just made
            // labels read worse ("volume-unknown").
            "volume_name": volume_id.clone(),
            "volume_id": volume_id,
            "volume_size": volume_size,
            "snapshot_chain": snapshot_chain,
            "storage_analytics": storage_analytics
        }));
    }
    
    println!("✅ [DASHBOARD] Built snapshot tree for {} volumes with SPDK data", tree_map.len());
    Ok(warp::reply::json(&tree_map))
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

// --- Snapshot timeline: user VolumeSnapshots + engine epochs, real times ---
//
// The flat /api/snapshots endpoint reports SPDK's view only, and SPDK lvols
// carry no creation time (the node agent stamps "now" on every list). Real
// times live in Kubernetes: VolumeSnapshotContent.status.creationTime for
// user snapshots (status.snapshotHandle IS the SPDK lvol name — the join
// key), and the PV replica-sync-state annotation's EpochEntry.recorded_at
// for engine epochs. This endpoint merges the three sources per volume.

/// The CSI driver name VolumeSnapshotContents are matched against — only
/// flint-owned snapshot objects appear in the timeline or may be deleted.
const TIMELINE_CSI_DRIVER: &str = "flint.csi.storage.io";

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct TimelineQuery {
    volume: String,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct SnapshotTimelineEvent {
    /// Stable identity: VolumeSnapshotContent name for user snapshots,
    /// epoch snapshot name for epochs, SPDK lvol name for orphans.
    id: String,
    /// "user" (VolumeSnapshot-backed) or "epoch" (engine-cut).
    kind: String,
    /// Display name: the VolumeSnapshot name, or the epoch/lvol name.
    name: String,
    /// SPDK lvol snapshot name (the CSI snapshot handle), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    spdk_name: Option<String>,
    /// RFC3339. None only for orphans (no CR, and SPDK stores no time).
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    ready: bool,
    /// Nodes whose SPDK currently holds a copy of this snapshot.
    nodes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vs_namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vs_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vsc_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch_seq: Option<u64>,
    /// SPDK-side user snapshot with no VolumeSnapshot CR behind it
    /// (Retain-policy leftovers). Not deletable through the CR path.
    orphan: bool,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct TimelineReplica {
    node: String,
    sync_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_epoch: Option<String>,
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct SnapshotTimelineResponse {
    volume_id: String,
    /// Server time at response build — the frontend's "now" anchor.
    now: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_epoch: Option<String>,
    replicas: Vec<TimelineReplica>,
    /// Chronological (unknown-time orphans last).
    events: Vec<SnapshotTimelineEvent>,
    /// SPDK epoch snapshots not (or no longer) in the PV's retained-epoch
    /// record — mid-rotation stragglers. Counted, never plotted at a
    /// fabricated position.
    untracked_epochs: u64,
}

/// One VolumeSnapshotContent projected to what the timeline needs.
#[derive(Debug, Clone, PartialEq)]
struct VscEntry {
    vsc_name: String,
    vs_namespace: Option<String>,
    vs_name: Option<String>,
    /// status.snapshotHandle == the SPDK lvol snapshot name.
    handle: String,
    created_at: Option<String>,
    ready: bool,
    size_bytes: Option<u64>,
}

/// Per-lvol-name aggregate of the SPDK fan-out.
#[derive(Debug, Clone, Default, PartialEq)]
struct SpdkSnapAgg {
    nodes: Vec<String>,
    size_bytes: Option<u64>,
}

fn nanos_to_rfc3339(nanos: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(
        nanos.div_euclid(1_000_000_000),
        nanos.rem_euclid(1_000_000_000) as u32,
    )
    .map(|t| t.to_rfc3339())
}

/// Pure merge of the three sources (unit-tested; the handler only gathers).
fn build_snapshot_timeline(
    volume_id: &str,
    record: Option<&crate::replica_sync::VolumeSyncRecord>,
    vsc_entries: Vec<VscEntry>,
    spdk: &HashMap<String, SpdkSnapAgg>,
) -> SnapshotTimelineResponse {
    let mut events: Vec<SnapshotTimelineEvent> = Vec::new();
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

    // User snapshots: the CR is authoritative (name, real creation time,
    // readiness); SPDK contributes which nodes hold copies.
    for e in vsc_entries {
        let spdk_entry = spdk.get(&e.handle);
        claimed.insert(e.handle.clone());
        events.push(SnapshotTimelineEvent {
            id: e.vsc_name.clone(),
            kind: "user".to_string(),
            name: e.vs_name.clone().unwrap_or_else(|| e.handle.clone()),
            spdk_name: Some(e.handle),
            created_at: e.created_at,
            // The driver reports restoreSize 0 on multi-replica snapshots —
            // a zero CR size is "unknown", not an answer; SPDK's is real.
            size_bytes: e
                .size_bytes
                .filter(|&s| s > 0)
                .or(spdk_entry.and_then(|s| s.size_bytes)),
            ready: e.ready,
            nodes: spdk_entry.map(|s| s.nodes.clone()).unwrap_or_default(),
            vs_namespace: e.vs_namespace,
            vs_name: e.vs_name,
            vsc_name: Some(e.vsc_name),
            epoch_seq: None,
            orphan: false,
        });
    }

    // Epochs: the PV annotation is authoritative — recorded_at is the real
    // cut time and the retained window is the truth of what still exists.
    let mut annotated: std::collections::HashSet<&str> = std::collections::HashSet::new();
    if let Some(rec) = record {
        for entry in &rec.epochs {
            annotated.insert(entry.name.as_str());
            let spdk_entry = spdk.get(&entry.name);
            events.push(SnapshotTimelineEvent {
                id: entry.name.clone(),
                kind: "epoch".to_string(),
                name: entry.name.clone(),
                spdk_name: Some(entry.name.clone()),
                created_at: Some(entry.recorded_at.clone()),
                size_bytes: spdk_entry.and_then(|s| s.size_bytes),
                ready: true,
                nodes: spdk_entry.map(|s| s.nodes.clone()).unwrap_or_default(),
                vs_namespace: None,
                vs_name: None,
                vsc_name: None,
                epoch_seq: crate::identity::epoch_seq(volume_id, &entry.name),
                orphan: false,
            });
        }
    }

    // SPDK leftovers: user-shaped names with no CR are orphans (shown,
    // time unknown, not CR-deletable); epoch-shaped names outside the
    // annotation are counted but never plotted at a made-up time.
    let mut untracked_epochs = 0u64;
    for (name, agg) in spdk {
        if claimed.contains(name) {
            continue;
        }
        match crate::identity::snapshot_owner(name).as_deref() {
            Some(owner) if owner == volume_id => {}
            _ => continue,
        }
        if crate::identity::epoch_seq(volume_id, name).is_some() {
            if !annotated.contains(name.as_str()) {
                untracked_epochs += 1;
            }
        } else {
            events.push(SnapshotTimelineEvent {
                id: name.clone(),
                kind: "user".to_string(),
                name: name.clone(),
                spdk_name: Some(name.clone()),
                created_at: None,
                size_bytes: agg.size_bytes,
                ready: true,
                nodes: agg.nodes.clone(),
                vs_namespace: None,
                vs_name: None,
                vsc_name: None,
                epoch_seq: None,
                orphan: true,
            });
        }
    }

    // Chronological; unknown-time orphans sort last.
    events.sort_by(|a, b| match (&a.created_at, &b.created_at) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.id.cmp(&b.id),
    });

    let replicas = record
        .map(|rec| {
            rec.replicas
                .iter()
                .map(|r| TimelineReplica {
                    node: r.node_name.clone(),
                    sync_state: r.sync_state.as_str().to_string(),
                    last_epoch: r.last_epoch.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    SnapshotTimelineResponse {
        volume_id: volume_id.to_string(),
        now: Utc::now().to_rfc3339(),
        current_epoch: record.and_then(|r| r.current_epoch.clone()),
        replicas,
        events,
        untracked_epochs,
    }
}

/// List flint-owned VolumeSnapshotContents whose handle belongs to `volume_id`.
async fn list_flint_snapshot_contents(
    kube_client: &Client,
    volume_id: &str,
) -> Result<Vec<VscEntry>, kube::Error> {
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind};
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "snapshot.storage.k8s.io",
        "v1",
        "VolumeSnapshotContent",
    ));
    let api: Api<DynamicObject> = Api::all_with(kube_client.clone(), &ar);
    let list = api.list(&ListParams::default()).await?;
    Ok(list
        .items
        .into_iter()
        .filter_map(|obj| {
            let data = &obj.data;
            if data["spec"]["driver"].as_str() != Some(TIMELINE_CSI_DRIVER) {
                return None;
            }
            let handle = data["status"]["snapshotHandle"].as_str()?.to_string();
            if crate::identity::snapshot_owner(&handle).as_deref() != Some(volume_id) {
                return None;
            }
            // v1 VolumeSnapshotContent creationTime is int64 nanos; tolerate
            // a string form too, then fall back to the object's own stamp
            // (later than the cut, but real).
            let created_at = data["status"]["creationTime"]
                .as_i64()
                .and_then(nanos_to_rfc3339)
                .or_else(|| data["status"]["creationTime"].as_str().map(String::from))
                .or_else(|| {
                    // jiff::Timestamp's Display is RFC3339.
                    obj.metadata
                        .creation_timestamp
                        .as_ref()
                        .map(|t| t.0.to_string())
                });
            Some(VscEntry {
                vsc_name: obj.metadata.name.clone().unwrap_or_default(),
                vs_namespace: data["spec"]["volumeSnapshotRef"]["namespace"]
                    .as_str()
                    .map(String::from),
                vs_name: data["spec"]["volumeSnapshotRef"]["name"]
                    .as_str()
                    .map(String::from),
                handle,
                created_at,
                ready: data["status"]["readyToUse"].as_bool().unwrap_or(false),
                size_bytes: data["status"]["restoreSize"].as_u64(),
            })
        })
        .collect())
}

/// Fan out to node agents and aggregate this volume's SPDK snapshots by name.
async fn collect_spdk_snapshots(state: &AppState, volume_id: &str) -> HashMap<String, SpdkSnapAgg> {
    let agents: Vec<(String, String)> = state
        .node_agents
        .read()
        .await
        .iter()
        .map(|(n, u)| (n.clone(), u.clone()))
        .collect();

    let fetches = agents.into_iter().map(|(node, url)| async move {
        let client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let resp = client
            .get(format!("{}/api/snapshots/list", url))
            .send()
            .await
            .ok()?;
        let data: serde_json::Value = resp.json().await.ok()?;
        Some((node, data["snapshots"].as_array().cloned().unwrap_or_default()))
    });

    let mut agg: HashMap<String, SpdkSnapAgg> = HashMap::new();
    for fetched in futures::future::join_all(fetches).await.into_iter().flatten() {
        let (node, snaps) = fetched;
        for snap in snaps {
            let Some(name) = snap["snapshot_name"].as_str() else {
                continue;
            };
            if crate::identity::snapshot_owner(name).as_deref() != Some(volume_id) {
                continue;
            }
            let entry = agg.entry(name.to_string()).or_default();
            entry.nodes.push(node.clone());
            if entry.size_bytes.is_none() {
                entry.size_bytes = snap["size_bytes"].as_u64();
            }
        }
    }
    for entry in agg.values_mut() {
        entry.nodes.sort();
    }
    agg
}

/// Handle GET /api/snapshots/timeline?volume= — the merged snapshot timeline.
async fn get_snapshot_timeline(
    query: TimelineQuery,
    state: AppState,
) -> Result<impl Reply, warp::Rejection> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    let volume_id = query.volume;
    println!("🕒 [DASHBOARD] Building snapshot timeline for {}", volume_id);

    let pvs_api: Api<PersistentVolume> = Api::all(state.kube_client.clone());
    let record = match pvs_api.get_opt(&volume_id).await {
        Ok(Some(pv)) => pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crate::replica_sync::SYNC_STATE_ANNOTATION))
            .and_then(
                |s| match crate::replica_sync::VolumeSyncRecord::from_annotation(s) {
                    Ok(rec) => Some(rec),
                    Err(e) => {
                        println!(
                            "⚠️ [TIMELINE] {}: unparseable replica-sync-state annotation: {}",
                            volume_id, e
                        );
                        None
                    }
                },
            ),
        Ok(None) => None,
        Err(e) => {
            println!("⚠️ [TIMELINE] PV {} read failed: {}", volume_id, e);
            None
        }
    };

    let vsc_entries = match list_flint_snapshot_contents(&state.kube_client, &volume_id).await {
        Ok(entries) => entries,
        Err(e) => {
            // Snapshot CRDs may simply not be installed; the epoch lane and
            // SPDK view still stand on their own.
            println!("⚠️ [TIMELINE] VolumeSnapshotContent list failed: {}", e);
            Vec::new()
        }
    };

    let spdk = collect_spdk_snapshots(&state, &volume_id).await;
    let response = build_snapshot_timeline(&volume_id, record.as_ref(), vsc_entries, &spdk);
    println!(
        "✅ [TIMELINE] {}: {} events ({} untracked epochs)",
        volume_id,
        response.events.len(),
        response.untracked_epochs
    );
    Ok(warp::reply::json(&response))
}

#[derive(Serialize, Debug, Clone, utoipa::ToSchema)]
struct DeleteVolumeSnapshotResponse {
    success: bool,
    namespace: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Handle DELETE /api/volumesnapshots/{namespace}/{name} — delete a USER
/// snapshot by deleting its VolumeSnapshot CR (the snapshot-controller and
/// csi-snapshotter then retire the content + SPDK lvol per deletionPolicy).
/// Deleting the lvol directly (the legacy /api/snapshots/{id} route) would
/// orphan the CR; this is the correct path for CR-backed snapshots.
async fn delete_volume_snapshot(
    namespace: String,
    name: String,
    state: AppState,
) -> Result<impl Reply, warp::Rejection> {
    use kube::api::{ApiResource, DeleteParams, DynamicObject, GroupVersionKind};
    println!(
        "🗑️ [DASHBOARD] Deleting VolumeSnapshot {}/{}",
        namespace, name
    );

    let vs_ar = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "snapshot.storage.k8s.io",
        "v1",
        "VolumeSnapshot",
    ));
    let api: Api<DynamicObject> =
        Api::namespaced_with(state.kube_client.clone(), &namespace, &vs_ar);

    let reply = |status: warp::http::StatusCode, body: DeleteVolumeSnapshotResponse| {
        Ok::<_, warp::Rejection>(warp::reply::with_status(warp::reply::json(&body), status))
    };
    let failure = |error: String| DeleteVolumeSnapshotResponse {
        success: false,
        namespace: namespace.clone(),
        name: name.clone(),
        content: None,
        error: Some(error),
    };

    let vs = match api.get_opt(&name).await {
        Ok(Some(vs)) => vs,
        Ok(None) => {
            return reply(
                warp::http::StatusCode::NOT_FOUND,
                failure("VolumeSnapshot not found".to_string()),
            )
        }
        Err(e) => {
            return reply(
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                failure(format!("VolumeSnapshot get failed: {}", e)),
            )
        }
    };

    // Driver guard: never delete another CSI driver's snapshot. Bound
    // snapshots resolve through their content; unbound ones through the
    // class. Indeterminate → refuse.
    let content_name = vs.data["status"]["boundVolumeSnapshotContentName"]
        .as_str()
        .map(String::from);
    let driver = if let Some(ref content) = content_name {
        let vsc_ar = ApiResource::from_gvk(&GroupVersionKind::gvk(
            "snapshot.storage.k8s.io",
            "v1",
            "VolumeSnapshotContent",
        ));
        let vsc_api: Api<DynamicObject> = Api::all_with(state.kube_client.clone(), &vsc_ar);
        vsc_api
            .get_opt(content)
            .await
            .ok()
            .flatten()
            .and_then(|vsc| vsc.data["spec"]["driver"].as_str().map(String::from))
    } else {
        match vs.data["spec"]["volumeSnapshotClassName"].as_str() {
            Some(class) => {
                let class_ar = ApiResource::from_gvk(&GroupVersionKind::gvk(
                    "snapshot.storage.k8s.io",
                    "v1",
                    "VolumeSnapshotClass",
                ));
                let class_api: Api<DynamicObject> =
                    Api::all_with(state.kube_client.clone(), &class_ar);
                class_api
                    .get_opt(class)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|cl| cl.data["driver"].as_str().map(String::from))
            }
            None => None,
        }
    };
    if driver.as_deref() != Some(TIMELINE_CSI_DRIVER) {
        return reply(
            warp::http::StatusCode::CONFLICT,
            failure(format!(
                "refused: snapshot driver is {:?}, not {}",
                driver, TIMELINE_CSI_DRIVER
            )),
        );
    }

    match api.delete(&name, &DeleteParams::default()).await {
        Ok(_) => {
            println!("✅ [DASHBOARD] VolumeSnapshot {}/{} deleted", namespace, name);
            reply(
                warp::http::StatusCode::OK,
                DeleteVolumeSnapshotResponse {
                    success: true,
                    namespace,
                    name,
                    content: content_name,
                    error: None,
                },
            )
        }
        Err(e) => reply(
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            failure(format!("delete failed: {}", e)),
        ),
    }
}

/// Setup all HTTP routes for the minimal dashboard backend.
///
/// Every /api route except /api/login requires a bearer token; destructive
/// routes additionally require the admin role. No CORS layer: the frontend
/// reaches the API same-origin (nginx in-pod proxy in production, the vite
/// dev-server proxy in development).
pub fn setup_minimal_dashboard_routes(
    app_state: AppState,
    auth: Arc<crate::dashboard_auth::AuthState>,
) -> impl Filter<Extract = impl Reply, Error = warp::Rejection> + Clone {
    use crate::dashboard_auth::{login_route, require, Role};

    let viewer = require(auth.clone(), Role::Viewer);
    let admin = require(auth.clone(), Role::Admin);
    let login = login_route(auth);

    let state_filter = warp::any().map(move || app_state.clone());

    // Liveness/readiness target: answers from the server loop alone, no
    // remote calls. Probing /api/dashboard instead couples pod health to
    // every node agent — one unreachable node stalls the aggregate past
    // the probe deadline and the kubelet kills a perfectly healthy
    // backend (the 2026-06-12 dashboard outage).
    let healthz_route = warp::path("healthz")
        .and(warp::get())
        .map(|| warp::reply::json(&json!({"status": "ok"})));

    // Main dashboard endpoint with optional query parameters for backend filtering
    let dashboard_route = warp::path("api")
        .and(warp::path("dashboard"))
        .and(warp::get())
        .and(viewer.clone())
        .and(warp::query::<DashboardQuery>().map(Some).or(warp::any().map(|| None)).unify())
        .and(state_filter.clone())
        .and_then(get_dashboard_data_minimal);

    // Per-tab endpoints: thin projections over the same cached aggregate as
    // /api/dashboard, so the Overview tick and each tab fetch only the slice
    // they render instead of the full world.
    let overview_route = warp::path("api")
        .and(warp::path("overview"))
        .and(warp::path::end())
        .and(warp::get())
        .and(viewer.clone())
        .and(state_filter.clone())
        .and_then(get_overview_minimal);

    let volumes_route = warp::path("api")
        .and(warp::path("volumes"))
        .and(warp::path::end())
        .and(warp::get())
        .and(viewer.clone())
        .and(warp::query::<DashboardQuery>().map(Some).or(warp::any().map(|| None)).unify())
        .and(state_filter.clone())
        .and_then(get_volumes_minimal);

    let events_route = warp::path("api")
        .and(warp::path("events"))
        .and(warp::path::end())
        .and(warp::get())
        .and(viewer.clone())
        .and(warp::query::<EventsQuery>())
        .and(state_filter.clone())
        .and_then(get_events_minimal);

    let disks_route = warp::path("api")
        .and(warp::path("disks"))
        .and(warp::path::end())
        .and(warp::get())
        .and(viewer.clone())
        .and(warp::query::<DashboardQuery>().map(Some).or(warp::any().map(|| None)).unify())
        .and(state_filter.clone())
        .and_then(get_disks_minimal);

    // Proxy routes for node agents
    let proxy_uninitialized = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("disks"))
        .and(warp::path("uninitialized"))
        .and(warp::get())
        .and(viewer.clone())
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
        .and(admin.clone())
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
        .and(admin.clone())
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
        .and(viewer.clone())
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
        .and(admin.clone())
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
        .and(admin.clone())
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            // Delete operations can take longer - use extended timeout
            proxy_node_agent_endpoint_long(node, "/api/disks/delete".to_string(), "POST".to_string(), Some(body), state)
        });

    let proxy_create_memory_disk = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("memory_disks"))
        .and(warp::path("create"))
        .and(warp::post())
        .and(admin.clone())
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            println!("🎯 [PROXY_ROUTE] Memory disk create route matched! Node: {}, Body: {:?}", node, body);
            proxy_node_agent_endpoint(node, "/api/memory_disks/create".to_string(), "POST".to_string(), Some(body), state)
        });

    let proxy_delete_memory_disk = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("memory_disks"))
        .and(warp::path("delete"))
        .and(warp::post())
        .and(admin.clone())
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            println!("🎯 [PROXY_ROUTE] Memory disk delete route matched! Node: {}, Body: {:?}", node, body);
            proxy_node_agent_endpoint(node, "/api/memory_disks/delete".to_string(), "POST".to_string(), Some(body), state)
        });

    let refresh_route = warp::path("api")
        .and(warp::path("refresh"))
        .and(warp::post())
        .and(viewer.clone())
        .and(state_filter.clone())
        .and_then(handle_refresh);
    
    // Snapshot aggregation routes
    let snapshots_list = warp::path("api")
        .and(warp::path("snapshots"))
        .and(warp::path::end())
        .and(warp::get())
        .and(viewer.clone())
        .and(state_filter.clone())
        .and_then(get_all_snapshots);

    let snapshots_tree = warp::path("api")
        .and(warp::path("snapshots"))
        .and(warp::path("tree"))
        .and(warp::get())
        .and(viewer.clone())
        .and(state_filter.clone())
        .and_then(get_snapshots_tree);

    let snapshot_delete = warp::path("api")
        .and(warp::path("snapshots"))
        .and(warp::path::param::<String>())
        .and(warp::delete())
        .and(admin.clone())
        .and(state_filter.clone())
        .and_then(delete_snapshot_by_id);

    let snapshots_timeline = warp::path("api")
        .and(warp::path("snapshots"))
        .and(warp::path("timeline"))
        .and(warp::path::end())
        .and(warp::get())
        .and(viewer.clone())
        .and(warp::query::<TimelineQuery>())
        .and(state_filter.clone())
        .and_then(get_snapshot_timeline);

    // User-snapshot deletion goes through the VolumeSnapshot CR — the only
    // path that keeps Kubernetes and SPDK in agreement.
    let volumesnapshot_delete = warp::path("api")
        .and(warp::path("volumesnapshots"))
        .and(warp::path::param::<String>())
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::delete())
        .and(admin.clone())
        .and(state_filter.clone())
        .and_then(delete_volume_snapshot);

    // Orphaned volume deletion route
    let orphan_delete = warp::path("api")
        .and(warp::path("orphans"))
        .and(warp::path::param::<String>())
        .and(warp::delete())
        .and(admin.clone())
        .and(state_filter.clone())
        .and_then(delete_orphaned_lvol);

    healthz_route
        .or(login)
        .or(dashboard_route)
        .or(overview_route)
        .or(volumes_route)
        .or(disks_route)
        .or(events_route)
        .or(proxy_uninitialized)
        .or(proxy_setup)
        .or(proxy_initialize)
        .or(proxy_status)
        .or(proxy_reset)
        .or(proxy_delete)
        .or(proxy_create_memory_disk)
        .or(proxy_delete_memory_disk)
        .or(refresh_route)
        .or(snapshots_list)
        .or(snapshots_tree)
        .or(snapshots_timeline)
        .or(snapshot_delete)
        .or(volumesnapshot_delete)
        .or(orphan_delete)
        .recover(crate::dashboard_auth::handle_rejection)
}

/// Initialize and start the minimal dashboard backend
pub async fn start_minimal_dashboard_backend(port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🚀 [MINIMAL_DASHBOARD] Starting minimal state dashboard backend on port {}", port);
    println!("   Architecture: Cache-free, parallel node queries for real-time data");
    
    let kube_client = Client::try_default().await?;
    let target_namespace = get_current_namespace().await?;
    
    let app_state = AppState {
        kube_client,
        node_agents: Arc::new(RwLock::new(HashMap::new())),
        target_namespace,
        iostat_history: Arc::new(RwLock::new(HashMap::new())),
        dashboard_cache: Arc::new(RwLock::new(None)),
    };
    
    println!("🎯 [MINIMAL_DASHBOARD] Using namespace: {}", app_state.target_namespace);
    
    // Initial discovery of node agents
    let node_agents = discover_node_agents(&app_state.kube_client, &app_state.target_namespace).await?;
    let node_count = node_agents.len();
    *app_state.node_agents.write().await = node_agents;
    
    println!("🔍 [MINIMAL_DASHBOARD] Discovered {} node agents", node_count);

    let auth = crate::dashboard_auth::AuthState::from_env();
    let routes = setup_minimal_dashboard_routes(app_state, auth);
    
    println!("✅ [MINIMAL_DASHBOARD] Dashboard backend ready - serving on 0.0.0.0:{}", port);
    println!("   Real-time mode: All queries fetch fresh data with parallel node requests");
    warp::serve(routes).run(([0, 0, 0, 0], port)).await;

    Ok(())
}

// --- OpenAPI document (spdk-dashboard/api/openapi.json source) ---
// The SPA's TypeScript API types are generated from this document
// (`npm run gen:api` in spdk-dashboard/), so the schemas below and the
// typed response structs they reference ARE the frontend contract.
// Regenerate with: cargo run --bin dashboard-openapi
mod api_doc {
    #![allow(dead_code)] // path fns are annotation carriers, never called

    use utoipa::OpenApi;

    use super::*;
    use crate::dashboard_auth::{LoginRequest, LoginResponse};
    use crate::node_agent::{
        DeleteDiskRequest, DiskDeleteResponse, DiskSetupRequest, DiskSetupResponse,
        NodeAgentError, NodeDiskListing, NodeDiskStatus, NodeDisksStatusResponse,
        UninitializedDisksResponse,
    };

    // The real handlers are warp filter chains the #[utoipa::path] macro
    // cannot attach to; these stubs carry the path annotations instead.

    #[utoipa::path(get, path = "/healthz", tag = "system",
        responses((status = 200, description = "Liveness probe", body = Object)))]
    fn healthz() {}

    #[utoipa::path(post, path = "/api/login", tag = "auth",
        request_body = LoginRequest,
        responses(
            (status = 200, description = "Session token", body = LoginResponse),
            (status = 401, description = "Invalid credentials", body = ApiError)))]
    fn login() {}

    #[utoipa::path(get, path = "/api/dashboard", tag = "aggregate",
        params(DashboardQuery),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Full aggregate (uncached legacy endpoint)", body = DashboardData),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 500, description = "Aggregate build failed", body = ApiError)))]
    fn dashboard() {}

    #[utoipa::path(get, path = "/api/overview", tag = "aggregate",
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Summary counts", body = DashboardOverview),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 500, description = "Aggregate build failed", body = ApiError)))]
    fn overview() {}

    #[utoipa::path(get, path = "/api/volumes", tag = "aggregate",
        params(DashboardQuery),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Volumes slice of the cached aggregate", body = VolumesResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 500, description = "Aggregate build failed", body = ApiError)))]
    fn volumes() {}

    #[utoipa::path(get, path = "/api/disks", tag = "aggregate",
        params(DashboardQuery),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Disks slice of the cached aggregate", body = DisksResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 500, description = "Aggregate build failed", body = ApiError)))]
    fn disks() {}

    #[utoipa::path(get, path = "/api/events", tag = "events",
        params(EventsQuery),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Engine event timeline + parsed hot-rejoin windows", body = EventsResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 500, description = "Event list failed", body = ApiError)))]
    fn events() {}

    #[utoipa::path(post, path = "/api/refresh", tag = "aggregate",
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Cache invalidated; agents rediscovered", body = RefreshResponse),
            (status = 401, description = "Missing/expired token", body = ApiError)))]
    fn refresh() {}

    #[utoipa::path(get, path = "/api/snapshots", tag = "snapshots",
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Raw SPDK snapshot objects (untyped passthrough)", body = Vec<Object>),
            (status = 401, description = "Missing/expired token", body = ApiError)))]
    fn snapshots() {}

    #[utoipa::path(get, path = "/api/snapshots/timeline", tag = "snapshots",
        params(TimelineQuery),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Per-volume snapshot timeline: user VolumeSnapshots (real CR creation times) merged with engine epochs (PV-annotation recorded_at) and SPDK per-node presence", body = SnapshotTimelineResponse),
            (status = 401, description = "Missing/expired token", body = ApiError)))]
    fn snapshots_timeline() {}

    #[utoipa::path(delete, path = "/api/volumesnapshots/{namespace}/{name}", tag = "snapshots",
        params(
            ("namespace" = String, Path, description = "VolumeSnapshot namespace"),
            ("name" = String, Path, description = "VolumeSnapshot name")),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "VolumeSnapshot CR deleted; snapshot-controller retires the content/SPDK data per deletionPolicy", body = DeleteVolumeSnapshotResponse),
            (status = 404, description = "No such VolumeSnapshot", body = DeleteVolumeSnapshotResponse),
            (status = 409, description = "Refused: snapshot belongs to a different CSI driver", body = DeleteVolumeSnapshotResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 403, description = "Viewer token on a destructive route", body = ApiError),
            (status = 500, description = "Kubernetes API failure", body = DeleteVolumeSnapshotResponse)))]
    fn volumesnapshot_delete() {}

    #[utoipa::path(get, path = "/api/nodes/{node}/disks/status", tag = "node-disks",
        params(("node" = String, Path, description = "Kubernetes node name")),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "All healthy disks on the node (Disk Setup tab source)", body = NodeDisksStatusResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 500, description = "Agent unreachable or discovery failed", body = NodeAgentError)))]
    fn node_disk_status() {}

    #[utoipa::path(post, path = "/api/nodes/{node}/disks/uninitialized", tag = "node-disks",
        params(("node" = String, Path, description = "Kubernetes node name")),
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "All healthy disks (field name is historical)", body = UninitializedDisksResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 500, description = "Agent unreachable or discovery failed", body = NodeAgentError)))]
    fn node_disks_uninitialized() {}

    #[utoipa::path(post, path = "/api/nodes/{node}/disks/setup", tag = "node-disks",
        params(("node" = String, Path, description = "Kubernetes node name")),
        request_body = DiskSetupRequest,
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Per-disk outcomes; idempotent on initialized disks", body = DiskSetupResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 403, description = "Viewer token on a destructive route", body = ApiError),
            (status = 500, description = "Agent unreachable", body = NodeAgentError)))]
    fn node_disks_setup() {}

    #[utoipa::path(post, path = "/api/nodes/{node}/disks/initialize", tag = "node-disks",
        params(("node" = String, Path, description = "Kubernetes node name")),
        request_body = DiskSetupRequest,
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "Alias of /disks/setup", body = DiskSetupResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 403, description = "Viewer token on a destructive route", body = ApiError),
            (status = 500, description = "Agent unreachable", body = NodeAgentError)))]
    fn node_disks_initialize() {}

    #[utoipa::path(post, path = "/api/nodes/{node}/disks/reset", tag = "node-disks",
        params(("node" = String, Path, description = "Kubernetes node name")),
        request_body = DiskSetupRequest,
        security(("bearerAuth" = [])),
        responses(
            (status = 501, description = "Reset not implemented in minimal state", body = DiskSetupResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 403, description = "Viewer token on a destructive route", body = ApiError)))]
    fn node_disks_reset() {}

    #[utoipa::path(post, path = "/api/nodes/{node}/disks/delete", tag = "node-disks",
        params(("node" = String, Path, description = "Kubernetes node name")),
        request_body = DeleteDiskRequest,
        security(("bearerAuth" = [])),
        responses(
            (status = 200, description = "LVS deleted (or no-op on an uninitialized disk)", body = DiskDeleteResponse),
            (status = 409, description = "Refused: logical volumes still exist on the LVS", body = DiskDeleteResponse),
            (status = 401, description = "Missing/expired token", body = ApiError),
            (status = 403, description = "Viewer token on a destructive route", body = ApiError),
            (status = 500, description = "Agent unreachable or SPDK error", body = DiskDeleteResponse)))]
    fn node_disks_delete() {}

    struct SecurityAddon;

    impl utoipa::Modify for SecurityAddon {
        fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
            use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
            let components = openapi.components.get_or_insert_with(Default::default);
            components.add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
        }
    }

    #[derive(OpenApi)]
    #[openapi(
        info(
            title = "Flint Dashboard API",
            description = "Dashboard backend served by the flint CSI driver (ENABLE_DASHBOARD=true). \
                           Sessions are in-memory: tokens invalidate on backend restart.",
        ),
        modifiers(&SecurityAddon),
        paths(
            healthz, login, dashboard, overview, volumes, disks, events, refresh,
            snapshots, snapshots_timeline, volumesnapshot_delete,
            node_disk_status, node_disks_uninitialized, node_disks_setup,
            node_disks_initialize, node_disks_reset, node_disks_delete,
        ),
        components(schemas(
            LoginRequest, LoginResponse, crate::dashboard_auth::Role,
            DashboardData, DashboardVolume, DashboardDisk, DashboardReplicaStatus,
            ReplicaSyncInfo, NvmfTarget, NvmeofTargetInfo, DashboardRaidStatus,
            RaidMember, RebuildInfo, SpdkValidationStatus, PvcInfo, ProvisionedVolume,
            OrphanedVolumeInfo, ConsumerRaid, ConsumerRaidMember, NodeInfo,
            DashboardOverview, VolumesResponse, DisksResponse, EventsResponse,
            RefreshResponse, ApiError, DashboardEvent, WindowStep, HotRejoinWindow,
            SnapshotTimelineResponse, SnapshotTimelineEvent, TimelineReplica,
            DeleteVolumeSnapshotResponse,
            DiskSetupRequest, DiskSetupResponse, DeleteDiskRequest, DiskDeleteResponse,
            NodeDiskStatus, NodeDiskListing,
            NodeDisksStatusResponse, UninitializedDisksResponse, NodeAgentError,
        )),
    )]
    struct ApiDoc;

    pub fn openapi_json() -> String {
        ApiDoc::openapi()
            .to_pretty_json()
            .expect("OpenAPI document serializes")
    }
}

/// Emit the OpenAPI document (used by the dashboard-openapi bin).
pub fn openapi_json() -> String {
    api_doc::openapi_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- lvol classification: the disk↔volume join (live shapes: runk
    //     2026-07-04, the cluster where the predecessor flagged every live
    //     lvol as an orphaned "cleanup candidate") ---

    /// Real bdev_get_bdevs lvol shape.
    fn lvol_json(alias: &str, uuid: &str, snapshot: bool) -> serde_json::Value {
        json!({
            "name": uuid,
            "aliases": [alias],
            "uuid": uuid,
            "num_blocks": 4194304u64,
            "block_size": 512,
            "product_name": "Logical Volume",
            "driver_specific": { "lvol": { "snapshot": snapshot, "clone": false } }
        })
    }

    fn provisioned_of(classified: &[(String, ClassifiedLvol)]) -> Vec<&ProvisionedVolume> {
        classified
            .iter()
            .filter_map(|(_, c)| match c {
                ClassifiedLvol::Provisioned(p) => Some(p),
                ClassifiedLvol::Orphan(_) => None,
            })
            .collect()
    }

    fn orphans_of(classified: &[(String, ClassifiedLvol)]) -> Vec<&OrphanedVolumeInfo> {
        classified
            .iter()
            .filter_map(|(_, c)| match c {
                ClassifiedLvol::Orphan(o) => Some(o),
                ClassifiedLvol::Provisioned(_) => None,
            })
            .collect()
    }

    #[test]
    fn classify_joins_live_lvols_to_their_volume_and_lvstore() {
        let volumes = vec![create_test_volume(
            TL_VOL,
            "Healthy",
            vec!["runk-aws-1".to_string(), "runk-aws-2".to_string()],
            false,
            false,
        )];
        let by_id: HashMap<&str, &DashboardVolume> =
            volumes.iter().map(|v| (v.id.as_str(), v)).collect();

        let lvs = "lvs_runk-aws-1_0000-00-1f-0";
        let lvols = vec![
            lvol_json(&format!("{}/vol_{}_replica_0", lvs, TL_VOL), "c871cc84", false),
            lvol_json(&format!("{}/snap_{}_68366263527245013", lvs, TL_VOL), "058da0da", true),
            lvol_json(&format!("{}/epoch-{}-130", lvs, TL_VOL), "ee1a8818", true),
        ];

        let classified = classify_node_lvols("runk-aws-1", &lvols, &by_id);
        assert!(classified.iter().all(|(l, _)| l == lvs), "lvstore tag drives disk attribution");
        let provisioned = provisioned_of(&classified);
        assert_eq!(provisioned.len(), 3, "live-volume lvols must never be orphans");
        assert!(provisioned.iter().all(|p| p.volume_id == TL_VOL));
        assert_eq!(provisioned[0].replica_type, "replica");
        // Head lvols carry the live per-node replica status
        assert_eq!(provisioned[0].status, "healthy");
        assert_eq!(provisioned[1].replica_type, "user-snapshot");
        assert_eq!(provisioned[1].status, "read-only");
        assert_eq!(provisioned[2].replica_type, "epoch-snapshot");
        assert_eq!(provisioned[0].size, 4194304 * 512);
    }

    #[test]
    fn classify_orphans_only_ownerless_lvols() {
        let volumes = vec![create_test_volume(
            "pvc-live",
            "Healthy",
            vec!["n1".to_string()],
            false,
            false,
        )];
        let by_id: HashMap<&str, &DashboardVolume> =
            volumes.iter().map(|v| (v.id.as_str(), v)).collect();

        let lvols = vec![
            // Owner parses but the PV is gone → orphan
            lvol_json("lvs_n1_0000-00-1f-0/vol_pvc-deleted_replica_0", "dead-1", false),
            // Not a contract shape at all → orphan
            lvol_json("lvs_n1_0000-00-1f-0/leftover-scratch", "dead-2", false),
            // Legacy single-replica head of the live volume → provisioned
            lvol_json("lvs_n1_0000-00-1f-0/vol_pvc-live", "live-1", false),
        ];

        let classified = classify_node_lvols("n1", &lvols, &by_id);
        let orphans = orphans_of(&classified);
        assert_eq!(orphans.len(), 2);
        // Orphan names keep the lvs-qualified alias (what an operator sees in SPDK)
        assert!(orphans.iter().any(|o| o.spdk_volume_name.ends_with("vol_pvc-deleted_replica_0")));
        let provisioned = provisioned_of(&classified);
        assert_eq!(provisioned.len(), 1);
        assert_eq!(provisioned[0].replica_type, "primary");
    }

    #[test]
    fn classify_handles_hr_heads_and_skips_aliasless_lvols() {
        let volumes = vec![create_test_volume(
            "pvc-live",
            "Healthy",
            vec!["n1".to_string()],
            false,
            true, // rebuilding → per-node status "rebuilding"
        )];
        let by_id: HashMap<&str, &DashboardVolume> =
            volumes.iter().map(|v| (v.id.as_str(), v)).collect();

        let lvols = vec![
            // Hot-rejoin head: renamed lvol, possibly new uuid — the NAME
            // still parses to its volume (the whole point of name-keying)
            lvol_json("lvs_n1_0000-00-1f-0/vol_pvc-live_replica_1_hr", "new-uuid", false),
            // No alias → cannot attribute; skipped, never misfiled
            json!({ "uuid": "bare", "num_blocks": 1, "block_size": 512 }),
        ];

        let classified = classify_node_lvols("n1", &lvols, &by_id);
        assert_eq!(classified.len(), 1);
        let provisioned = provisioned_of(&classified);
        assert_eq!(provisioned[0].replica_type, "replica");
        assert_eq!(provisioned[0].status, "rebuilding");
    }

    /// Live pin: the attribution key minted from a disk's node+PCI equals
    /// the lvs prefix SPDK reports in lvol aliases (runk disk
    /// runk-aws-1_0000-00-1f.0 hosts lvs_runk-aws-1_0000-00-1f-0).
    #[test]
    fn lvs_attribution_key_matches_the_identity_mint() {
        assert_eq!(
            crate::identity::lvs_name("runk-aws-1", "0000:00:1f.0"),
            "lvs_runk-aws-1_0000-00-1f-0"
        );
    }

    // --- snapshot timeline merge (live-shape corpus: runk 2026-07-04) ---

    const TL_VOL: &str = "pvc-93edc114-bec7-43a0-8273-5812c2c52d13";

    /// Real replica-sync-state annotation shape from the runk fixture volume
    /// (trimmed to two epochs / two replicas, field-for-field faithful).
    fn tl_annotation() -> String {
        format!(
            r#"{{"current_epoch":"epoch-{v}-9",
                "epochs":[
                  {{"name":"epoch-{v}-8","recorded_at":"2026-07-04T21:23:51Z"}},
                  {{"name":"epoch-{v}-9","recorded_at":"2026-07-04T21:24:51Z"}}],
                "replicas":[
                  {{"node_name":"runk-aws-1","node_uid":"178db387","lvol_uuid":"c871cc84","sync_state":"in_sync","last_epoch":"epoch-{v}-9"}},
                  {{"node_name":"runk-aws-2","node_uid":"2c45d4eb","lvol_uuid":"daed5e18","sync_state":"stale","last_epoch":"epoch-{v}-8"}}]}}"#,
            v = TL_VOL
        )
    }

    fn tl_spdk() -> HashMap<String, SpdkSnapAgg> {
        let mut spdk = HashMap::new();
        // User snapshot present on both replicas (handle == VSC snapshotHandle).
        spdk.insert(
            format!("snap_{}_68366263527245013", TL_VOL),
            SpdkSnapAgg { nodes: vec!["runk-aws-1".into(), "runk-aws-2".into()], size_bytes: Some(2147483648) },
        );
        // Current epoch, present on both.
        spdk.insert(
            format!("epoch-{}-9", TL_VOL),
            SpdkSnapAgg { nodes: vec!["runk-aws-1".into(), "runk-aws-2".into()], size_bytes: Some(2147483648) },
        );
        // Mid-rotation epoch straggler: on one node only, NOT in the annotation.
        spdk.insert(
            format!("epoch-{}-3", TL_VOL),
            SpdkSnapAgg { nodes: vec!["runk-aws-2".into()], size_bytes: Some(2147483648) },
        );
        // CR-less user-shaped leftover (Retain-policy orphan).
        spdk.insert(
            format!("snap_{}_99999999999", TL_VOL),
            SpdkSnapAgg { nodes: vec!["runk-aws-1".into()], size_bytes: Some(2147483648) },
        );
        // Backs the zero-restoreSize CR (snap-demo-4).
        spdk.insert(
            format!("snap_{}_55555555555", TL_VOL),
            SpdkSnapAgg { nodes: vec!["runk-aws-1".into(), "runk-aws-2".into()], size_bytes: Some(2147483648) },
        );
        // Foreign volume's snapshot must never leak in.
        spdk.insert(
            "snap_pvc-ffffffff-0000-0000-0000-000000000000_1".to_string(),
            SpdkSnapAgg { nodes: vec!["runk-aws-3".into()], size_bytes: Some(1) },
        );
        spdk
    }

    #[test]
    fn timeline_merges_cr_annotation_and_spdk_sources() {
        let record = crate::replica_sync::VolumeSyncRecord::from_annotation(&tl_annotation())
            .expect("live-shape annotation parses");
        let vsc = vec![
            VscEntry {
                vsc_name: "snapcontent-62c2ee8e".into(),
                vs_namespace: Some("default".into()),
                vs_name: Some("snap-demo-1".into()),
                handle: format!("snap_{}_68366263527245013", TL_VOL),
                created_at: nanos_to_rfc3339(1783199824000000000),
                ready: true,
                size_bytes: Some(2147483648),
            },
            // Still-provisioning snapshot: no SPDK view yet, CR time only.
            VscEntry {
                vsc_name: "snapcontent-pending".into(),
                vs_namespace: Some("default".into()),
                vs_name: Some("snap-demo-2".into()),
                handle: format!("snap_{}_62060056664648443", TL_VOL),
                created_at: nanos_to_rfc3339(1783199912000000000),
                ready: false,
                size_bytes: None,
            },
            // Live 2r-volume quirk: the driver stamps restoreSize 0 — the
            // SPDK size must win over a zero CR size.
            VscEntry {
                vsc_name: "snapcontent-zerosize".into(),
                vs_namespace: Some("default".into()),
                vs_name: Some("snap-demo-4".into()),
                handle: format!("snap_{}_55555555555", TL_VOL),
                created_at: nanos_to_rfc3339(1783199990000000000),
                ready: true,
                size_bytes: Some(0),
            },
        ];

        let resp = build_snapshot_timeline(TL_VOL, Some(&record), vsc, &tl_spdk());

        // Replica + epoch header state comes straight from the annotation.
        assert_eq!(resp.current_epoch.as_deref(), Some(format!("epoch-{}-9", TL_VOL).as_str()));
        assert_eq!(resp.replicas.len(), 2);
        assert_eq!(resp.replicas[1].sync_state, "stale");

        // 3 user (CR) + 2 epochs (annotation) + 1 orphan; straggler epoch counted, not plotted.
        assert_eq!(resp.events.len(), 6);
        assert_eq!(resp.untracked_epochs, 1);

        let user: Vec<_> = resp.events.iter().filter(|e| e.kind == "user").collect();
        assert_eq!(user.len(), 4);
        // Zero CR restoreSize is "unknown" — SPDK's real size must win.
        let zerosize = user.iter().find(|e| e.name == "snap-demo-4").unwrap();
        assert_eq!(zerosize.size_bytes, Some(2147483648));
        let demo1 = user.iter().find(|e| e.name == "snap-demo-1").unwrap();
        // Real CR time (nanos), not a list-time stamp; both replicas hold it.
        assert_eq!(demo1.created_at.as_deref(), Some("2026-07-04T21:17:04+00:00"));
        assert_eq!(demo1.nodes, vec!["runk-aws-1", "runk-aws-2"]);
        assert!(!demo1.orphan);
        let pending = user.iter().find(|e| e.name == "snap-demo-2").unwrap();
        assert!(!pending.ready);
        assert!(pending.nodes.is_empty());
        let orphan = user.iter().find(|e| e.orphan).unwrap();
        assert_eq!(orphan.created_at, None);
        assert_eq!(orphan.name, format!("snap_{}_99999999999", TL_VOL));

        let epochs: Vec<_> = resp.events.iter().filter(|e| e.kind == "epoch").collect();
        assert_eq!(epochs.len(), 2);
        let e9 = epochs.iter().find(|e| e.epoch_seq == Some(9)).unwrap();
        assert_eq!(e9.created_at.as_deref(), Some("2026-07-04T21:24:51Z"));
        assert_eq!(e9.nodes.len(), 2);
        // Rotated-out-of-SPDK epoch still listed (annotation is the truth of
        // the retained window) with no holding nodes.
        let e8 = epochs.iter().find(|e| e.epoch_seq == Some(8)).unwrap();
        assert!(e8.nodes.is_empty());

        // Chronological, unknown-time orphan last.
        let times: Vec<_> = resp.events.iter().map(|e| e.created_at.clone()).collect();
        assert_eq!(times.last().unwrap(), &None);
        let known: Vec<_> = times.iter().flatten().cloned().collect();
        let mut sorted = known.clone();
        sorted.sort();
        assert_eq!(known, sorted);

        // The foreign volume's snapshot never appears.
        assert!(!resp.events.iter().any(|e| e.name.contains("ffffffff")));
    }

    #[test]
    fn timeline_stands_without_annotation_or_crs() {
        // Single-replica volume (no annotation) with CRD-less cluster: the
        // SPDK view alone still yields an honest (if time-less) answer.
        let resp = build_snapshot_timeline(TL_VOL, None, Vec::new(), &tl_spdk());
        assert!(resp.replicas.is_empty());
        assert_eq!(resp.current_epoch, None);
        // All three user-shaped snapshots are orphans; both epochs are untracked.
        assert_eq!(resp.events.len(), 3);
        assert!(resp.events.iter().all(|e| e.orphan && e.created_at.is_none()));
        assert_eq!(resp.untracked_epochs, 2);
    }

    #[test]
    fn nanos_round_trip_matches_live_vsc_stamp() {
        // Live VSC creationTime from runk: 1783199824000000000 == 21:17:04Z.
        assert_eq!(
            nanos_to_rfc3339(1783199824000000000).as_deref(),
            Some("2026-07-04T21:17:04+00:00")
        );
        assert_eq!(nanos_to_rfc3339(0).as_deref(), Some("1970-01-01T00:00:00+00:00"));
    }

    /// Test basic latency calculation with valid values
    #[test]
    fn test_latency_calculation_basic() {
        // Scenario: 1000 read ops with 10,000 ticks total latency
        // tick_rate = 1,000,000,000 ticks/second (1 GHz)
        // Expected: (10,000 / 1000) / 1,000,000,000 * 1,000,000 = 0.01 microseconds
        let read_latency_ticks: u64 = 10_000;
        let num_read_ops: u64 = 1_000;
        let tick_rate: u64 = 1_000_000_000;

        let read_latency_us = if num_read_ops > 0 && tick_rate > 0 {
            ((read_latency_ticks as f64 / num_read_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
        } else {
            0
        };

        assert_eq!(read_latency_us, 0); // Rounds down to 0 due to very low latency
    }

    /// Test latency calculation with realistic NVMe values
    #[test]
    fn test_latency_calculation_realistic_nvme() {
        // Scenario: NVMe with ~100 microsecond average latency
        // 10,000 read ops, tick_rate = 2.4 GHz (common TSC frequency)
        // 100 us = 100 * 2.4e9 / 1e6 = 240,000 ticks per operation
        // Total ticks = 10,000 ops * 240,000 ticks/op = 2,400,000,000 ticks
        let read_latency_ticks: u64 = 2_400_000_000;
        let num_read_ops: u64 = 10_000;
        let tick_rate: u64 = 2_400_000_000; // 2.4 GHz

        let read_latency_us = if num_read_ops > 0 && tick_rate > 0 {
            ((read_latency_ticks as f64 / num_read_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
        } else {
            0
        };

        // Expected: (2,400,000,000 / 10,000) / 2,400,000,000 * 1,000,000 = 100 microseconds
        assert_eq!(read_latency_us, 100);
    }

    /// Test latency calculation with realistic HDD values
    #[test]
    fn test_latency_calculation_realistic_hdd() {
        // Scenario: HDD with ~10 millisecond average latency
        // 1,000 read ops, tick_rate = 2.4 GHz
        // 10 ms = 10,000 us = 10,000 * 2.4e9 / 1e6 = 24,000,000 ticks per operation
        // Total ticks = 1,000 ops * 24,000,000 ticks/op = 24,000,000,000 ticks
        let read_latency_ticks: u64 = 24_000_000_000;
        let num_read_ops: u64 = 1_000;
        let tick_rate: u64 = 2_400_000_000; // 2.4 GHz

        let read_latency_us = if num_read_ops > 0 && tick_rate > 0 {
            ((read_latency_ticks as f64 / num_read_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
        } else {
            0
        };

        // Expected: (24,000,000,000 / 1,000) / 2,400,000,000 * 1,000,000 = 10,000 microseconds (10 ms)
        assert_eq!(read_latency_us, 10_000);
    }

    /// Test latency calculation with zero operations (avoid division by zero)
    #[test]
    fn test_latency_calculation_zero_ops() {
        let read_latency_ticks: u64 = 1_000_000;
        let num_read_ops: u64 = 0;
        let tick_rate: u64 = 1_000_000_000;

        let read_latency_us = if num_read_ops > 0 && tick_rate > 0 {
            ((read_latency_ticks as f64 / num_read_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
        } else {
            0
        };

        assert_eq!(read_latency_us, 0);
    }

    /// Test latency calculation with zero tick rate (avoid division by zero)
    #[test]
    fn test_latency_calculation_zero_tick_rate() {
        let read_latency_ticks: u64 = 1_000_000;
        let num_read_ops: u64 = 100;
        let tick_rate: u64 = 0;

        let read_latency_us = if num_read_ops > 0 && tick_rate > 0 {
            ((read_latency_ticks as f64 / num_read_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
        } else {
            0
        };

        assert_eq!(read_latency_us, 0);
    }

    /// Test latency calculation with both zero (edge case)
    #[test]
    fn test_latency_calculation_both_zero() {
        let read_latency_ticks: u64 = 0;
        let num_read_ops: u64 = 0;
        let tick_rate: u64 = 1_000_000_000;

        let read_latency_us = if num_read_ops > 0 && tick_rate > 0 {
            ((read_latency_ticks as f64 / num_read_ops as f64) / tick_rate as f64 * 1_000_000.0) as u64
        } else {
            0
        };

        assert_eq!(read_latency_us, 0);
    }

    /// Test BdevStats struct default values
    #[test]
    fn test_bdev_stats_default() {
        let stats = BdevStats::default();
        assert_eq!(stats.read_iops, 0);
        assert_eq!(stats.write_iops, 0);
        assert_eq!(stats.read_latency_us, 0);
        assert_eq!(stats.write_latency_us, 0);
    }

    /// Test BdevStats struct creation with values
    #[test]
    fn test_bdev_stats_with_values() {
        let stats = BdevStats {
            read_iops: 1000,
            write_iops: 500,
            read_latency_us: 100,
            write_latency_us: 150,
        };

        assert_eq!(stats.read_iops, 1000);
        assert_eq!(stats.write_iops, 500);
        assert_eq!(stats.read_latency_us, 100);
        assert_eq!(stats.write_latency_us, 150);
    }

    #[test]
    fn parse_hot_rejoin_window_inline_real_drill_message() {
        // Verbatim HotRejoinSucceeded payload from the 2026-07-02 2a drill.
        let msg = "Replica on runj-aws-2 hot-rejoined raid raid_pvc-c6896f1f-1ad2-4bbc-bbd1-11c14fc32aef at epoch-pvc-c6896f1f-1ad2-4bbc-bbd1-11c14fc32aef-1264 in 1729ms (quiesce=12ms cut E_f=10ms fenced final delta=1655ms lease renew=9ms add --skip-rebuild=31ms unquiesce=12ms); inline fenced final delta (27262976 bytes est.) — no esnap exposure, fully redundant immediately";
        let w = parse_hot_rejoin_window("pvc-c6896f1f", Some("t".into()), msg).unwrap();
        assert_eq!(w.node, "runj-aws-2");
        assert_eq!(w.raid, "raid_pvc-c6896f1f-1ad2-4bbc-bbd1-11c14fc32aef");
        assert_eq!(w.epoch, "epoch-pvc-c6896f1f-1ad2-4bbc-bbd1-11c14fc32aef-1264");
        assert_eq!(w.window_ms, 1729);
        assert_eq!(w.path, "inline");
        assert_eq!(w.estimator_bytes, Some(27262976));
        let steps: Vec<(&str, u64)> = w.steps.iter().map(|s| (s.name.as_str(), s.ms)).collect();
        assert_eq!(steps, vec![
            ("quiesce", 12),
            ("cut E_f", 10),
            ("fenced final delta", 1655),
            ("lease renew", 9),
            ("add --skip-rebuild", 31),
            ("unquiesce", 12),
        ]);
        assert_eq!(w.steps.iter().map(|s| s.ms).sum::<u64>(), 1729);
    }

    #[test]
    fn parse_hot_rejoin_window_esnap_variant() {
        let msg = "Replica on n1 hot-rejoined raid raid_v at epoch-v-3 in 148ms (quiesce=19ms cut E_f=10ms export+AER E_f=25ms esnap clone=11ms export+AER head=41ms renew=11ms add=19ms unquiesce=12ms); localizing the esnap chain";
        let w = parse_hot_rejoin_window("v", None, msg).unwrap();
        assert_eq!(w.window_ms, 148);
        assert_eq!(w.path, "esnap");
        assert_eq!(w.estimator_bytes, None);
        assert_eq!(w.steps.len(), 8);
        assert_eq!(w.steps[2].name, "export+AER E_f");
        assert_eq!(w.steps[2].ms, 25);
    }

    #[test]
    fn parse_hot_rejoin_window_rejects_other_messages() {
        assert!(parse_hot_rejoin_window("v", None, "Replica on n1 is a warm standby").is_none());
        assert!(parse_hot_rejoin_window("v", None, "").is_none());
    }

    #[test]
    fn categorize_event_reasons() {
        assert_eq!(categorize_event_reason("HotRejoinSucceeded"), "hot_rejoin");
        assert_eq!(categorize_event_reason("HotRejoinWindowSlow"), "hot_rejoin");
        assert_eq!(categorize_event_reason("VolumeDataPathLost"), "data_path");
        assert_eq!(categorize_event_reason("ReplicaCatchupStarted"), "catchup");
        assert_eq!(categorize_event_reason("ReplicaStale"), "catchup");
        assert_eq!(categorize_event_reason("StandbyAdmissionDeferred"), "catchup");
        assert_eq!(categorize_event_reason("CutoverSucceeded"), "cutover");
        assert_eq!(categorize_event_reason("EpochCutFailed"), "epoch");
        assert_eq!(categorize_event_reason("VolumeDegraded"), "health");
        assert_eq!(categorize_event_reason("SomethingElse"), "other");
    }

    #[test]
    fn epoch_lag_from_history_and_name_fallback() {
        use crate::replica_sync::VolumeSyncRecord;
        // Positional distance in the recorded history.
        let rec = VolumeSyncRecord::from_annotation(
            r#"{"current_epoch":"epoch-v-3",
                "epochs":[{"name":"epoch-v-1","recorded_at":"t"},
                          {"name":"epoch-v-2","recorded_at":"t"},
                          {"name":"epoch-v-3","recorded_at":"t"}],
                "replicas":[]}"#,
        ).unwrap();
        assert_eq!(epoch_lag(&rec, Some("epoch-v-1")), Some(2));
        assert_eq!(epoch_lag(&rec, Some("epoch-v-3")), Some(0));
        assert_eq!(epoch_lag(&rec, None), None);

        // History trimmed past last_epoch: falls back to the trailing counter.
        let trimmed = VolumeSyncRecord::from_annotation(
            r#"{"current_epoch":"epoch-v-7",
                "epochs":[{"name":"epoch-v-7","recorded_at":"t"}],
                "replicas":[]}"#,
        ).unwrap();
        assert_eq!(epoch_lag(&trimmed, Some("epoch-v-4")), Some(3));
        // Non-numeric epoch names with no history entry: honest unknown.
        assert_eq!(epoch_lag(&trimmed, Some("bootstrap")), None);
    }

    #[test]
    fn project_sync_record_maps_replicas_and_counts() {
        use crate::replica_sync::VolumeSyncRecord;
        let rec = VolumeSyncRecord::from_annotation(
            r#"{"current_epoch":"epoch-v-2",
                "epochs":[{"name":"epoch-v-1","recorded_at":"t"},
                          {"name":"epoch-v-2","recorded_at":"t"}],
                "replicas":[
                  {"node_name":"n1","node_uid":"u1","lvol_uuid":"x1",
                   "sync_state":"in_sync","last_epoch":"epoch-v-2"},
                  {"node_name":"n2","node_uid":"u2","lvol_uuid":"x2",
                   "sync_state":"stale","last_epoch":"epoch-v-1",
                   "reason":"leg failure","hot_rejoin":"epoch-v-2"}
                ]}"#,
        ).unwrap();

        let (statuses, nodes, active, current_epoch) = project_sync_record(&rec, "n1");
        assert_eq!(nodes, vec!["n1", "n2"]);
        assert_eq!(active, 1);
        assert_eq!(current_epoch.as_deref(), Some("epoch-v-2"));

        assert_eq!(statuses[0].status, "healthy");
        assert!(statuses[0].is_local);
        let s0 = statuses[0].sync.as_ref().unwrap();
        assert_eq!(s0.sync_state, "in_sync");
        assert_eq!(s0.epoch_lag, Some(0));

        assert_eq!(statuses[1].status, "stale");
        assert!(!statuses[1].is_local);
        let s1 = statuses[1].sync.as_ref().unwrap();
        assert_eq!(s1.sync_state, "stale");
        assert_eq!(s1.epoch_lag, Some(1));
        assert_eq!(s1.reason.as_deref(), Some("leg failure"));
        assert_eq!(s1.hot_rejoin.as_deref(), Some("epoch-v-2"));
    }

    #[test]
    fn project_node_raids_maps_states_and_filters_foreign_bdevs() {
        // Shape per SPDK bdev_raid_get_bdevs: a healthy 2/2 raid, a degraded
        // raid whose failed slot has name+uuid nulled
        // (raid_bdev_free_base_bdev_resource), and a non-flint raid.
        let result = json!([
            {
                "name": "raid_pvc-abc", "state": "online", "raid_level": "raid1",
                "num_base_bdevs": 2, "num_base_bdevs_discovered": 2,
                "num_base_bdevs_operational": 2,
                "base_bdevs_list": [
                    {"name": "11111111-aaaa", "uuid": "11111111-aaaa", "is_configured": true},
                    {"name": "nvme_remote_1n1", "uuid": "22222222-bbbb", "is_configured": true}
                ]
            },
            {
                "name": "raid_pvc-deg", "state": "online", "raid_level": "raid1",
                "num_base_bdevs": 2, "num_base_bdevs_operational": 1,
                "base_bdevs_list": [
                    {"name": null, "uuid": null, "is_configured": false},
                    {"name": "33333333-cccc", "uuid": "33333333-cccc", "is_configured": true}
                ]
            },
            {"name": "cache_bdev", "state": "online", "num_base_bdevs": 1}
        ]);

        let raids = project_node_raids("consumer-1", &result);
        assert_eq!(raids.len(), 2);

        assert_eq!(raids[0].node, "consumer-1");
        assert_eq!(raids[0].raid_name, "raid_pvc-abc");
        assert_eq!(raids[0].state, "online");
        assert_eq!(raids[0].num_base_bdevs, 2);
        assert_eq!(raids[0].num_base_bdevs_operational, 2);
        assert_eq!(raids[0].base_bdevs.len(), 2);
        assert!(raids[0].base_bdevs.iter().all(|m| m.is_configured));

        assert_eq!(raids[1].raid_name, "raid_pvc-deg");
        assert_eq!(raids[1].num_base_bdevs_operational, 1);
        let failed = &raids[1].base_bdevs[0];
        assert!(!failed.is_configured);
        assert!(failed.name.is_none() && failed.uuid.is_none());

        assert!(project_node_raids("n", &json!(null)).is_empty());
    }

    #[test]
    fn label_consumer_raid_members_resolves_replica_nodes() {
        use crate::replica_sync::{expected_remote_base_bdev, VolumeSyncRecord};
        let vol = "pvc-abc";
        // r0 has a catch-up-revert uuid override: the base admitted at
        // reassembly carries the LIVE uuid, not the identity uuid.
        let rec = VolumeSyncRecord::from_annotation(
            r#"{"replicas":[
                  {"node_name":"n1","node_uid":"u1","lvol_uuid":"aaa",
                   "sync_state":"in_sync","active_lvol_uuid":"aaa-live"},
                  {"node_name":"n2","node_uid":"u2","lvol_uuid":"bbb",
                   "sync_state":"in_sync"}
                ]}"#,
        )
        .unwrap();

        let mut raids = vec![ConsumerRaid {
            node: "n1".to_string(),
            raid_name: crate::identity::raid_name(vol),
            state: "online".to_string(),
            num_base_bdevs: 2,
            num_base_bdevs_operational: 2,
            base_bdevs: vec![
                // Local base: bdev named by the live lvol uuid.
                ConsumerRaidMember {
                    name: Some("aaa-live".to_string()),
                    uuid: Some("aaa-live".to_string()),
                    is_configured: true,
                    replica_node: None,
                },
                // Remote base: deterministic attach name, replica index 1.
                ConsumerRaidMember {
                    name: Some(expected_remote_base_bdev(vol, 1)),
                    uuid: Some("something-else".to_string()),
                    is_configured: true,
                    replica_node: None,
                },
                // Failed slot stays unlabeled.
                ConsumerRaidMember {
                    name: None,
                    uuid: None,
                    is_configured: false,
                    replica_node: None,
                },
            ],
        }];

        label_consumer_raid_members(&mut raids, vol, &rec);
        assert_eq!(raids[0].base_bdevs[0].replica_node.as_deref(), Some("n1"));
        assert_eq!(raids[0].base_bdevs[1].replica_node.as_deref(), Some("n2"));
        assert_eq!(raids[0].base_bdevs[2].replica_node, None);
    }

    // Helper function to create test volumes
    fn create_test_volume(id: &str, state: &str, nodes: Vec<String>, local_nvme: bool, rebuilding: bool) -> DashboardVolume {
        let replica_statuses = nodes.iter().map(|node| DashboardReplicaStatus {
            node: node.clone(),
            status: if rebuilding { "rebuilding".to_string() } else { "healthy".to_string() },
            is_local: node == &nodes[0],
            last_io_timestamp: Some("2025-01-06T00:00:00Z".to_string()),
            rebuild_progress: if rebuilding { Some(50.0) } else { None },
            rebuild_target: None,
            is_new_replica: Some(rebuilding),
            nvmf_target: None,
            access_method: "nvmeof".to_string(),
            raid_member_slot: None,
            raid_member_state: "online".to_string(),
            sync: None,
        }).collect();

        DashboardVolume {
            id: id.to_string(),
            name: id.to_string(),
            size: "100GB".to_string(),
            state: state.to_string(),
            replicas: nodes.len() as i32,
            active_replicas: nodes.len() as i32,
            local_nvme,
            access_method: "nvmeof".to_string(),
            rebuild_progress: if rebuilding { Some(50.0) } else { None },
            nodes,
            replica_statuses,
            nvmeof_targets: vec![],
            nvmeof_enabled: false,
            transport_type: "TCP".to_string(),
            target_port: 4420,
            raid_status: None,
            ublk_device: None,
            spdk_validation_status: SpdkValidationStatus {
                has_spdk_backing: true,
                validation_message: None,
                validation_severity: "info".to_string(),
            },
            pvc_info: None,
            current_epoch: None,
            consumer_raids: Vec::new(),
        }
    }

    // Helper function to create test disks
    fn create_test_disk(id: &str, node: &str, initialized: bool) -> DashboardDisk {
        DashboardDisk {
            id: id.to_string(),
            node: node.to_string(),
            pci_addr: "0000:3b:00.0".to_string(),
            capacity: 1000000000000,
            capacity_gb: 1000.0,
            allocated_space: 500.0,
            free_space: 500.0,
            free_space_display: "500GB".to_string(),
            healthy: true,
            blobstore_initialized: initialized,
            is_system_disk: false,
            lvol_count: 2,
            model: "Test NVMe SSD".to_string(),
            read_iops: 10000,
            write_iops: 8000,
            read_latency: 100,
            write_latency: 150,
            brought_online: "2025-01-06T00:00:00Z".to_string(),
            provisioned_volumes: vec![],
            orphaned_spdk_volumes: vec![],
            device_type: "NVMe".to_string(),
        }
    }

    #[test]
    fn dashboard_overview_counts_match_the_aggregate() {
        let data = DashboardData {
            volumes: vec![
                create_test_volume("v1", "Healthy", vec!["n1".to_string()], true, false),
                create_test_volume("v2", "Degraded", vec!["n1".to_string()], false, false),
                create_test_volume("v3", "Failed", vec!["n2".to_string()], false, false),
                create_test_volume("v4", "Healthy", vec!["n2".to_string()], true, false),
            ],
            raw_volumes: vec![],
            disks: vec![
                create_test_disk("d1", "n1", true),
                create_test_disk("d2", "n2", false),
            ],
            nodes: vec!["n1".to_string(), "n2".to_string()],
            node_info: HashMap::new(),
        };

        let ov = dashboard_overview(&data);
        assert_eq!(ov.total_volumes, 4);
        assert_eq!(ov.healthy_volumes, 2);
        assert_eq!(ov.degraded_volumes, 1);
        assert_eq!(ov.failed_volumes, 1);
        assert_eq!(ov.faulted_volumes, 2);
        assert_eq!(ov.local_nvme_volumes, 2);
        assert_eq!(ov.total_disks, 2);
        assert_eq!(ov.initialized_disks, 1);
        assert_eq!(ov.total_nodes, 2);

        // The serialized shape is the API contract (see dashboard_openapi):
        // field names must match what the SPA consumed before the struct
        // replaced the ad-hoc json! map.
        let json = serde_json::to_value(&ov).unwrap();
        for key in [
            "total_volumes", "healthy_volumes", "degraded_volumes",
            "failed_volumes", "faulted_volumes", "local_nvme_volumes",
            "total_disks", "healthy_disks", "initialized_disks", "total_nodes",
        ] {
            assert!(json.get(key).is_some(), "overview lost key {key}");
        }
    }

    #[test]
    fn cache_ttl_defaults_and_parses_env() {
        // Default when unset. (Env-mutation is avoided to keep tests
        // parallel-safe; the parse path is exercised by the explicit values.)
        assert_eq!(dashboard_cache_ttl(), std::time::Duration::from_millis(3000));
    }

    // Volume filtering tests

    #[test]
    fn test_filter_volumes_by_healthy_state() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
            create_test_volume("vol2", "Degraded", vec!["node1".to_string()], true, false),
            create_test_volume("vol3", "Healthy", vec!["node2".to_string()], true, false),
            create_test_volume("vol4", "Failed", vec!["node1".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("healthy".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].state, "Healthy");
        assert_eq!(filtered[1].state, "Healthy");
    }

    #[test]
    fn test_filter_volumes_by_degraded_state() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
            create_test_volume("vol2", "Degraded", vec!["node1".to_string()], true, false),
            create_test_volume("vol3", "Degraded", vec!["node2".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("degraded".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|v| v.state == "Degraded"));
    }

    #[test]
    fn test_filter_volumes_by_failed_state() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
            create_test_volume("vol2", "Failed", vec!["node1".to_string()], true, false),
            create_test_volume("vol3", "Degraded", vec!["node2".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("failed".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].state, "Failed");
    }

    #[test]
    fn test_filter_volumes_by_faulted_state() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
            create_test_volume("vol2", "Failed", vec!["node1".to_string()], true, false),
            create_test_volume("vol3", "Degraded", vec!["node2".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("faulted".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 2); // Both Failed and Degraded
        assert!(filtered.iter().any(|v| v.state == "Failed"));
        assert!(filtered.iter().any(|v| v.state == "Degraded"));
    }

    #[test]
    fn test_filter_volumes_by_rebuilding_state() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
            create_test_volume("vol2", "Degraded", vec!["node1".to_string()], true, true),
            create_test_volume("vol3", "Healthy", vec!["node2".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("rebuilding".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].rebuild_progress.is_some());
    }

    #[test]
    fn test_filter_volumes_by_local_nvme() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
            create_test_volume("vol2", "Healthy", vec!["node1".to_string()], false, false),
            create_test_volume("vol3", "Healthy", vec!["node2".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("local-nvme".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|v| v.local_nvme));
    }

    #[test]
    fn test_filter_volumes_by_node() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["worker-node-1".to_string()], true, false),
            create_test_volume("vol2", "Healthy", vec!["worker-node-2".to_string()], true, false),
            create_test_volume("vol3", "Healthy", vec!["worker-node-1".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: Some("node-1".to_string()), // Partial match
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|v| v.nodes[0].contains("node-1")));
    }

    #[test]
    fn test_filter_volumes_combined() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["worker-node-1".to_string()], true, false),
            create_test_volume("vol2", "Degraded", vec!["worker-node-1".to_string()], true, false),
            create_test_volume("vol3", "Healthy", vec!["worker-node-2".to_string()], true, false),
            create_test_volume("vol4", "Degraded", vec!["worker-node-2".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("degraded".to_string()),
            volume_node: Some("node-1".to_string()),
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].state, "Degraded");
        assert!(filtered[0].nodes[0].contains("node-1"));
    }

    #[test]
    fn test_filter_volumes_empty_result() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("failed".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes, &query);
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn test_filter_volumes_all_filter() {
        let volumes = vec![
            create_test_volume("vol1", "Healthy", vec!["node1".to_string()], true, false),
            create_test_volume("vol2", "Degraded", vec!["node1".to_string()], true, false),
        ];

        let query = DashboardQuery {
            volume_filter: Some("all".to_string()),
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_volumes(volumes.clone(), &query);
        assert_eq!(filtered.len(), volumes.len());
    }

    // Disk filtering tests

    #[test]
    fn test_filter_disks_by_node() {
        let disks = vec![
            create_test_disk("disk1", "worker-node-1", true),
            create_test_disk("disk2", "worker-node-2", true),
            create_test_disk("disk3", "worker-node-1", false),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: None,
            disk_node: Some("node-1".to_string()),
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|d| d.node.contains("node-1")));
    }

    #[test]
    fn test_filter_disks_by_initialized_true() {
        let disks = vec![
            create_test_disk("disk1", "node1", true),
            create_test_disk("disk2", "node1", false),
            create_test_disk("disk3", "node2", true),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: None,
            disk_node: None,
            disk_initialized: Some(true),
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|d| d.blobstore_initialized));
    }

    #[test]
    fn test_filter_disks_by_initialized_false() {
        let disks = vec![
            create_test_disk("disk1", "node1", true),
            create_test_disk("disk2", "node1", false),
            create_test_disk("disk3", "node2", false),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: None,
            disk_node: None,
            disk_initialized: Some(false),
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|d| !d.blobstore_initialized));
    }

    #[test]
    fn test_filter_disks_combined() {
        let disks = vec![
            create_test_disk("disk1", "worker-node-1", true),
            create_test_disk("disk2", "worker-node-1", false),
            create_test_disk("disk3", "worker-node-2", true),
            create_test_disk("disk4", "worker-node-2", false),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: None,
            disk_node: Some("node-1".to_string()),
            disk_initialized: Some(true),
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].node.contains("node-1"));
        assert!(filtered[0].blobstore_initialized);
    }

    #[test]
    fn test_filter_disks_global_node_filter() {
        let disks = vec![
            create_test_disk("disk1", "worker-node-1", true),
            create_test_disk("disk2", "worker-node-2", true),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: None,
            disk_node: None,
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: Some("node-2".to_string()), // Global node filter
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].node.contains("node-2"));
    }

    #[test]
    fn test_filter_disks_empty_result() {
        let disks = vec![
            create_test_disk("disk1", "node1", true),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: None,
            disk_node: Some("nonexistent".to_string()),
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn test_filter_disks_case_insensitive() {
        let disks = vec![
            create_test_disk("disk1", "Worker-Node-1", true),
            create_test_disk("disk2", "worker-node-2", true),
        ];

        let query = DashboardQuery {
            volume_filter: None,
            volume_node: None,
            disk_node: Some("WORKER-NODE-1".to_string()), // Different case
            disk_initialized: None,
            nodes_with_disks_only: None,
            node: None,
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 1);
    }
}
