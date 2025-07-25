use warp::Filter;
use serde::Serialize;
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

#[derive(Serialize, Debug)]
struct DashboardData {
    volumes: Vec<DashboardVolume>,
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
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let volumes_list = volumes_api.list(&ListParams::default()).await?;
    
    let disks_api: Api<SpdkDisk> = Api::namespaced(state.kube_client.clone(), &state.target_namespace);
    let disks_list = disks_api.list(&ListParams::default()).await?;
    
    let mut dashboard_volumes = Vec::new();
    let mut dashboard_disks = Vec::new();
    let mut nodes = std::collections::HashSet::new();
    
    for volume in volumes_list.items {
        let dashboard_volume = convert_volume_to_dashboard(&volume);
        for replica in &dashboard_volume.replica_statuses {
            nodes.insert(replica.node.clone());
        }
        dashboard_volumes.push(dashboard_volume);
    }
    
    for disk in disks_list.items {
        nodes.insert(disk.spec.node_id.clone());
        let dashboard_disk = convert_disk_to_dashboard(&disk, &dashboard_volumes);
        dashboard_disks.push(dashboard_disk);
    }
    
    enhance_with_spdk_metrics(&mut dashboard_volumes, &mut dashboard_disks, state).await?;
    
    // Include all discovered nodes, not just those with volumes/disks
    let spdk_nodes = state.spdk_nodes.read().await;
    for discovered_node in spdk_nodes.keys() {
        nodes.insert(discovered_node.clone());
    }
    
    let dashboard_data = DashboardData {
        volumes: dashboard_volumes,
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
    }
}

async fn enhance_with_spdk_metrics(
    volumes: &mut [DashboardVolume],
    disks: &mut [DashboardDisk],
    state: &AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    
    for (_node, rpc_url) in spdk_nodes.iter() {
        // Get real-time RAID status and update volumes
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
                            for volume in volumes.iter_mut() {
                                if volume.id == raid_name || raid_name.contains(&volume.id) {
                                    // Update volume with real-time RAID status from SPDK
                                    update_volume_with_live_raid_status(volume, raid_bdev);
                                }
                            }
                        }
                    }
                }
            }
        }
        
        // Get NVMe-oF subsystem status instead of vhost controllers
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
                            for volume in volumes.iter_mut() {
                                // Find volumes that match this NQN
                                if nqn.contains(&volume.id) || volume.nvmeof_targets.iter().any(|t| t.nqn == nqn) {
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
                        }
                    }
                }
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
