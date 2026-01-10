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
#[derive(Debug, Deserialize, Clone)]
struct DashboardQuery {
    // Volume filters
    volume_filter: Option<String>, // "all", "healthy", "degraded", "failed", "rebuilding", "local-nvme", "orphaned"
    volume_node: Option<String>,   // Filter volumes by node
    
    // Disk filters
    disk_node: Option<String>,     // Filter disks by node
    disk_initialized: Option<bool>, // Filter by blobstore initialization status
    
    // Global filters
    node: Option<String>,          // Apply to both volumes and disks
}

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
    device_type: String, // "NVMe", "SCSI/SATA", "VirtIO", "IDE", "Unknown"
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
    node_info: HashMap<String, NodeInfo>,  // node_name -> node info with memory details
}

#[derive(Serialize, Debug, Clone)]
struct NodeInfo {
    name: String,
    memory_total_mb: u64,
    memory_available_mb: u64,
    memory_used_mb: u64,
    memory_utilization_pct: f64,
}

#[derive(Clone)]
pub struct AppState {
    kube_client: Client,
    node_agents: Arc<RwLock<HashMap<String, String>>>, // node -> agent_url
    target_namespace: String,
    // IOPS calculation: store previous iostat snapshots
    iostat_history: Arc<RwLock<HashMap<String, (DateTime<Utc>, serde_json::Value)>>>, // bdev_name -> (timestamp, iostat)
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
                let agent_url = format!("http://{}:8081", pod_ip);
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

    match HttpClient::new().get(&url).send().await {
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
        capacity_gb: (disk_info.size_bytes / (1024 * 1024 * 1024)) as i64,
        allocated_space: (disk_info.size_bytes - disk_info.free_space) as i64,
        free_space: disk_info.free_space as i64,
        free_space_display: format!("{}GB", disk_info.free_space / (1024 * 1024 * 1024)),
        healthy: disk_info.healthy,
        blobstore_initialized: disk_info.blobstore_initialized,
        lvol_count: disk_info.lvol_count,
        model: disk_info.model.clone(),
        read_iops: stats.read_iops,
        write_iops: stats.write_iops,
        read_latency: stats.read_latency_us,
        write_latency: stats.write_latency_us,
        brought_online: Utc::now().to_rfc3339(),
        provisioned_volumes: Vec::new(), // TODO: Get from volume discovery
        orphaned_spdk_volumes: Vec::new(), // Populated later
        device_type: disk_info.device_type.clone(),
    }
}

/// Get all active PV lvol UUIDs from Kubernetes
async fn get_active_pv_lvol_uuids(kube_client: &Client) -> Result<std::collections::HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
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
async fn detect_orphaned_lvols(state: &AppState) -> Result<HashMap<String, Vec<OrphanedVolumeInfo>>, Box<dyn std::error::Error + Send + Sync>> {
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
                    r.is_new_replica == Some(true)
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
async fn fetch_dashboard_data_minimal(state: &AppState, query: Option<DashboardQuery>) -> Result<DashboardData, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [MINIMAL_DASHBOARD_FETCH] Fetching fresh dashboard data...");
    if let Some(ref q) = query {
        println!("🔍 [FILTER] Query params: {:?}", q);
    }
    
    // Discover node agents (updates state for future queries)
    let node_agents = discover_node_agents(&state.kube_client, &state.target_namespace).await?;
    *state.node_agents.write().await = node_agents;
    
    // Fetch disks from all node agents in parallel
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
    
    // Apply filters if provided
    let (filtered_volumes, filtered_disks) = if let Some(query) = query {
        let vols = filter_volumes(dashboard_volumes, &query);
        let disks = filter_disks(dashboard_disks, &query);
        (vols, disks)
    } else {
        (dashboard_volumes, dashboard_disks)
    };
    
    // Get unique node names from filtered disks
    let nodes: Vec<String> = filtered_disks.iter()
        .map(|d| d.node.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    // Fetch memory info for each node in parallel
    let node_agents = state.node_agents.read().await;
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
        .filter_map(|r| r)
        .collect();

    println!("📊 [MEMORY_INFO] Collected memory info for {} nodes", node_info.len());
    for (node_name, info) in &node_info {
        println!("   Node {}: total={}MB, available={}MB, used={}MB",
            node_name, info.memory_total_mb, info.memory_available_mb, info.memory_used_mb);
    }

    let dashboard_data = DashboardData {
        volumes: filtered_volumes,
        raw_volumes,
        disks: filtered_disks,
        nodes,
        node_info,
    };
    
    println!("✅ [MINIMAL_DASHBOARD_FETCH] Fetch completed: {} volumes, {} disks, {} nodes", 
        dashboard_data.volumes.len(), dashboard_data.disks.len(), dashboard_data.nodes.len());
    
    Ok(dashboard_data)
}

/// Handle GET /api/dashboard - Main dashboard endpoint (always fetches fresh data)
/// Supports query parameters for backend filtering:
/// - volume_filter: "all", "healthy", "degraded", "failed", "rebuilding", "local-nvme"
/// - volume_node: filter volumes by node name (partial match)
/// - disk_node: filter disks by node name (partial match)
/// - disk_initialized: filter disks by blobstore initialization (true/false)
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
                warp::reply::json(&json!({"error": format!("Failed to fetch dashboard data: {}", e)})),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR
            ))
        }
    }
}

/// Handle POST /api/refresh - Rediscover node agents (no cache to refresh)
async fn handle_refresh(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🔄 [REFRESH_API] Rediscovering node agents");
    
    // No cache to refresh - just rediscover node agents for backwards compatibility
    match discover_node_agents(&state.kube_client, &state.target_namespace).await {
        Ok(node_agents) => {
            *state.node_agents.write().await = node_agents;
            Ok::<_, warp::Rejection>(warp::reply::json(&json!({
                "status": "success",
                "message": "Node agents rediscovered (no cache in use)"
            })))
        }
        Err(e) => Ok(warp::reply::json(&json!({
            "status": "error", 
            "error": e.to_string()
        }))),
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
                    Ok(warp::reply::with_status(
                        warp::reply::json(&json!({"success": false, "error": format!("Node agent returned: {}", status)})),
                        warp::http::StatusCode::BAD_GATEWAY
                    ))
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
    
    // Main dashboard endpoint with optional query parameters for backend filtering
    let dashboard_route = warp::path("api")
        .and(warp::path("dashboard"))
        .and(warp::get())
        .and(warp::query::<DashboardQuery>().map(Some).or(warp::any().map(|| None)).unify())
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

    let proxy_create_memory_disk = warp::path("api")
        .and(warp::path("nodes"))
        .and(warp::path::param::<String>())
        .and(warp::path("memory_disks"))
        .and(warp::path("create"))
        .and(warp::post())
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
        .and(warp::body::json())
        .and(state_filter.clone())
        .and_then(|node: String, body: serde_json::Value, state: AppState| {
            println!("🎯 [PROXY_ROUTE] Memory disk delete route matched! Node: {}, Body: {:?}", node, body);
            proxy_node_agent_endpoint(node, "/api/memory_disks/delete".to_string(), "POST".to_string(), Some(body), state)
        });

    let refresh_route = warp::path("api")
        .and(warp::path("refresh"))
        .and(warp::post())
        .and(state_filter.clone())
        .and_then(handle_refresh);
    
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
        .or(proxy_create_memory_disk)
        .or(proxy_delete_memory_disk)
        .or(refresh_route)
        .or(snapshots_list)
        .or(snapshots_tree)
        .or(snapshot_delete)
        .or(orphan_delete)
        .with(cors)
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
    };
    
    println!("🎯 [MINIMAL_DASHBOARD] Using namespace: {}", app_state.target_namespace);
    
    // Initial discovery of node agents
    let node_agents = discover_node_agents(&app_state.kube_client, &app_state.target_namespace).await?;
    let node_count = node_agents.len();
    *app_state.node_agents.write().await = node_agents;
    
    println!("🔍 [MINIMAL_DASHBOARD] Discovered {} node agents", node_count);
    
    let routes = setup_minimal_dashboard_routes(app_state);
    
    println!("✅ [MINIMAL_DASHBOARD] Dashboard backend ready - serving on 0.0.0.0:{}", port);
    println!("   Real-time mode: All queries fetch fresh data with parallel node requests");
    warp::serve(routes).run(([0, 0, 0, 0], port)).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    // Helper function to create test disks
    fn create_test_disk(id: &str, node: &str, initialized: bool) -> DashboardDisk {
        DashboardDisk {
            id: id.to_string(),
            node: node.to_string(),
            pci_addr: "0000:3b:00.0".to_string(),
            capacity: 1000000000000,
            capacity_gb: 1000,
            allocated_space: 500,
            free_space: 500,
            free_space_display: "500GB".to_string(),
            healthy: true,
            blobstore_initialized: initialized,
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
            node: None,
        };

        let filtered = filter_disks(disks, &query);
        assert_eq!(filtered.len(), 1);
    }
}
