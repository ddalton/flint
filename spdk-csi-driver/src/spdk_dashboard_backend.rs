use warp::Filter;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use reqwest::Client as HttpClient;
use kube::{Client, Api, api::ListParams, CustomResource};
use chrono::{Utc, DateTime};
use std::env;

// Import your existing CRD types with updated RAID status
use spdk_csi_driver::{SpdkVolume, SpdkDisk, SpdkVolumeStatus, SpdkDiskStatus, Replica, ReplicaHealth, RaidStatus, RaidMember, RaidRebuildInfo, RaidMemberState, SpdkSnapshot, SnapshotType};

mod spdk_csi_driver {
    use kube::CustomResource;
    use serde::{Deserialize, Serialize};
    use chrono::{DateTime, Utc};

    // --- Start of New Snapshot Definitions ---

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub enum SnapshotType {
        #[default]
        Bdev,
        LvolClone,
        External,
    }

    #[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default)]
    #[kube(
        group = "csi.spdk.io",
        version = "v1",
        kind = "SpdkSnapshot",
        plural = "spdksnapshots"
    )]
    #[kube(namespaced)]
    #[kube(status = "SpdkSnapshotStatus")]
    pub struct SpdkSnapshotSpec {
        pub source_volume_id: String,
        pub snapshot_id: String,
        pub spdk_snapshot_lvol: String,
        #[serde(default)]
        pub snapshot_type: SnapshotType,
        pub clone_source_snapshot_id: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct SpdkSnapshotStatus {
        pub creation_time: Option<DateTime<Utc>>,
        pub ready_to_use: bool,
        pub size_bytes: i64,
        pub error: Option<String>,
    }

    // --- End of New Snapshot Definitions ---


    #[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default)]
    #[kube(group = "csi.spdk.io", version = "v1", kind = "SpdkVolume", plural = "spdkvolumes")]
    #[kube(namespaced)]
    #[kube(status = "SpdkVolumeStatus")]
    pub struct SpdkVolumeSpec {
        pub volume_id: String,
        pub size_bytes: i64,
        pub num_replicas: i32,
        pub replicas: Vec<Replica>,
        pub primary_lvol_uuid: Option<String>,
        pub write_ordering_enabled: bool,
        pub vhost_socket: Option<String>,
        pub raid_auto_rebuild: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct Replica {
        pub node: String,
        #[serde(rename = "type")]
        pub replica_type: String,
        pub pcie_addr: Option<String>,
        pub nqn: Option<String>,
        pub ip: Option<String>,
        pub port: Option<String>,
        pub local_pod_scheduled: bool,
        pub pod_name: Option<String>,
        pub disk_ref: String,
        pub lvol_uuid: Option<String>,
        pub health_status: ReplicaHealth,
        pub last_io_timestamp: Option<String>,
        pub write_sequence: u64,
        pub vhost_socket: Option<String>,
        pub raid_member_index: usize,
        pub raid_member_state: RaidMemberState,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub enum RaidMemberState {
        #[default]
        Online,
        Degraded,
        Failed,
        Rebuilding,
        Spare,
        Removing,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub enum ReplicaHealth {
        #[default]
        Healthy,
        Degraded,
        Failed,
        Rebuilding,
        Syncing,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct SpdkVolumeStatus {
        pub state: String,
        pub degraded: bool,
        pub last_checked: String,
        pub active_replicas: Vec<usize>,
        pub failed_replicas: Vec<usize>,
        pub write_sequence: u64,
        pub vhost_device: Option<String>,
        pub raid_status: Option<RaidStatus>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct RaidStatus {
        pub raid_level: u32,
        pub state: String,
        pub num_base_bdevs: u32,
        pub num_base_bdevs_discovered: u32,
        pub num_base_bdevs_operational: u32,
        pub base_bdevs_list: Vec<RaidMember>,
        pub rebuild_info: Option<RaidRebuildInfo>,
        pub superblock_version: Option<u32>,
        pub process_request_fn: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct RaidMember {
        pub name: String,
        pub state: String,
        pub slot: u32,
        pub uuid: Option<String>,
        pub is_configured: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct RaidRebuildInfo {
        pub state: String,
        pub target_slot: u32,
        pub source_slot: u32,
        pub blocks_remaining: u64,
        pub blocks_total: u64,
        pub progress_percentage: f64,
    }

    #[derive(CustomResource, Serialize, Deserialize, Debug, Clone, Default)]
    #[kube(group = "csi.spdk.io", version = "v1", kind = "SpdkDisk", plural = "spdkdisks")]
    #[kube(namespaced)]
    #[kube(status = "SpdkDiskStatus")]
    pub struct SpdkDiskSpec {
        pub node: String,
        pub pcie_addr: String,
        pub capacity: i64,
        pub blobstore_uuid: Option<String>,
        pub nvme_controller_id: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct SpdkDiskStatus {
        pub total_capacity: i64,
        pub free_space: i64,
        pub used_space: i64,
        pub healthy: bool,
        pub last_checked: String,
        pub lvol_count: u32,
        pub blobstore_initialized: bool,
        pub io_stats: IoStatistics,
        pub lvs_name: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct IoStatistics {
        pub read_iops: u64,
        pub write_iops: u64,
        pub read_latency_us: u64,
        pub write_latency_us: u64,
        pub error_count: u64,
    }
}

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


// Enhanced dashboard API response types with native RAID status
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
    vhost_socket: Option<String>,
    vhost_device: Option<String>,
    vhost_enabled: bool,
    vhost_type: String,
    nvme_namespaces: Vec<VhostNvmeNamespace>,
    // Enhanced RAID status from SPDK
    raid_status: Option<DashboardRaidStatus>,
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
struct VhostNvmeNamespace {
    nsid: u32,
    size: u64,
    uuid: String,
    bdev_name: String,
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kube_client = Client::try_default().await?;
    
    let mut spdk_nodes = HashMap::new();
    
    if let Ok(node_urls) = env::var("SPDK_NODE_URLS") {
        for pair in node_urls.split(',') {
            if let Some((node, url)) = pair.split_once('=') {
                spdk_nodes.insert(node.to_string(), url.to_string());
            }
        }
    } else {
        spdk_nodes = discover_spdk_nodes(&kube_client).await?;
    }
    
    let app_state = AppState {
        kube_client,
        spdk_nodes: Arc::new(RwLock::new(spdk_nodes)),
        cache: Arc::new(RwLock::new(None)),
        last_update: Arc::new(RwLock::new(Utc::now())),
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
        // --- End of New Endpoint ---
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
                .and(warp::path("vhost"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_vhost_details)
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
    println!("Discovering SPDK nodes by listing 'spdk-node-agent' pods...");
    let mut nodes = HashMap::new();
    // Assumes node_agent pods are labeled with 'app=spdk-node-agent'.
    let pods_api: Api<Pod> = Api::all(client.clone());
    let lp = ListParams::default().labels("app=spdk-node-agent");

    let pods = pods_api.list(&lp).await?;

    for pod in pods.items {
        let node_name = pod.spec.as_ref().and_then(|s| s.node_name.clone());
        let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());

        if let (Some(name), Some(ip)) = (node_name, pod_ip) {
            let url = format!("http://{}:5260", ip);
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
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), "default");
    let volumes_list = volumes_api.list(&ListParams::default()).await?;
    
    let disks_api: Api<SpdkDisk> = Api::namespaced(state.kube_client.clone(), "default");
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
        nodes.insert(disk.spec.node.clone());
        let dashboard_disk = convert_disk_to_dashboard(&disk, &dashboard_volumes);
        dashboard_disks.push(dashboard_disk);
    }
    
    enhance_with_spdk_metrics(&mut dashboard_volumes, &mut dashboard_disks, state).await?;
    
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
    let status = volume.status.as_ref().unwrap_or(&SpdkVolumeStatus::default());
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
        let nvmf_target = if replica.replica_type == "nvmf" {
            Some(NvmfTarget {
                nqn: replica.nqn.clone().unwrap_or_default(),
                target_ip: replica.ip.clone().unwrap_or_default(),
                target_port: replica.port.clone().unwrap_or("4420".to_string()),
                transport_type: "TCP".to_string(),
            })
        } else {
            None
        };
        
        let access_method = match replica.replica_type.as_str() {
            "lvol" => "local-nvme".to_string(),
            "nvmf" => "remote-nvmf".to_string(),
            _ => "unknown".to_string(),
        };
        
        // Get rebuild progress from RAID status if this replica is rebuilding
        let rebuild_progress = raid_status.as_ref()
            .and_then(|rs| rs.rebuild_info.as_ref())
            .filter(|ri| ri.target_slot == replica.raid_member_index as u32)
            .map(|ri| ri.progress_percentage);
        
        DashboardReplicaStatus {
            node: replica.node.clone(),
            status: format!("{:?}", replica.health_status).to_lowercase(),
            is_local: replica.replica_type == "lvol",
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
    let has_local_nvme = spec.replicas.iter().any(|r| r.replica_type == "lvol");
    
    let volume_access_method = "vhost-nvme".to_string();
    
    let volume_name = volume.metadata.name.clone().unwrap_or(spec.volume_id.clone());
    let vhost_socket = spec.vhost_socket.clone().or_else(|| 
        Some(format!("/var/lib/spdk/vhost/vhost_{}.sock", volume_name))
    );
    let vhost_device = status.vhost_device.clone().or_else(|| 
        Some(format!("/dev/nvme-vhost-{}", volume_name))
    );
    let vhost_enabled = vhost_socket.is_some();
    
    let nvme_namespaces = vec![
        VhostNvmeNamespace {
            nsid: 1,
            size: spec.size_bytes as u64,
            uuid: spec.primary_lvol_uuid.clone().unwrap_or_default(),
            bdev_name: spec.volume_id.clone(),
        }
    ];
    
    // Get rebuild progress from RAID status
    let rebuild_progress = raid_status.as_ref()
        .and_then(|rs| rs.rebuild_info.as_ref())
        .map(|ri| ri.progress_percentage);
    
    DashboardVolume {
        id: spec.volume_id.clone(),
        name: volume_name,
        size: format!("{}GB", size_gb),
        state: status.state.clone(),
        replicas: spec.num_replicas,
        active_replicas: status.active_replicas.len() as i32,
        local_nvme: has_local_nvme,
        access_method: volume_access_method,
        rebuild_progress,
        nodes: spec.replicas.iter().map(|r| r.node.clone()).collect(),
        replica_statuses,
        vhost_socket,
        vhost_device,
        vhost_enabled,
        vhost_type: "nvme".to_string(),
        nvme_namespaces,
        raid_status,
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
    let status = disk.status.as_ref().unwrap_or(&SpdkDiskStatus::default());
    let spec = &disk.spec;
    
    let provisioned_volumes: Vec<ProvisionedVolume> = volumes.iter()
        .filter_map(|vol| {
            for replica in &vol.replica_statuses {
                if replica.node == spec.node {
                    return Some(ProvisionedVolume {
                        volume_name: vol.name.clone(),
                        volume_id: vol.id.clone(),
                        size: vol.size.trim_end_matches("GB").parse().unwrap_or(0),
                        provisioned_at: Utc::now().to_rfc3339(),
                        replica_type: format!("{} replica ({})", 
                            if replica.is_local { "Local" } else { "Remote" },
                            if replica.is_local { "NVMe" } else { "NVMe-oF" }),
                        status: replica.status.clone(),
                    });
                }
            }
            None
        })
        .collect();
    
    DashboardDisk {
        id: disk.metadata.name.clone().unwrap_or_default(),
        node: spec.node.clone(),
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
    
    for (node, rpc_url) in spdk_nodes.iter() {
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
        
        // Get vhost-nvme controller status
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "vhost_get_controllers"
            }))
            .send()
            .await
        {
            if let Ok(vhost_info) = response.json::<serde_json::Value>().await {
                if let Some(controllers) = vhost_info["result"].as_array() {
                    for controller in controllers {
                        if let Some(ctrlr_name) = controller["ctrlr"].as_str() {
                            let is_nvme_controller = controller["backend_specific"]["type"]
                                .as_str() == Some("nvme");
                            
                            for volume in volumes.iter_mut() {
                                if ctrlr_name.contains(&volume.name) || 
                                   ctrlr_name.contains(&volume.id) {
                                    
                                    if let Some(socket_path) = controller["socket"].as_str() {
                                        volume.vhost_socket = Some(socket_path.to_string());
                                    }
                                    
                                    if let Some(active) = controller["active"].as_bool() {
                                        volume.vhost_enabled = active;
                                    }
                                    
                                    volume.vhost_type = if is_nvme_controller {
                                        "nvme".to_string()
                                    } else {
                                        "blk".to_string()
                                    };
                                    
                                    if is_nvme_controller {
                                        if let Some(namespaces) = controller["backend_specific"]["namespaces"].as_array() {
                                            volume.nvme_namespaces = namespaces.iter().map(|ns| {
                                                VhostNvmeNamespace {
                                                    nsid: ns["nsid"].as_u64().unwrap_or(1) as u32,
                                                    size: ns["size"].as_u64().unwrap_or(0),
                                                    uuid: ns["uuid"].as_str().unwrap_or("").to_string(),
                                                    bdev_name: ns["bdev_name"].as_str().unwrap_or("").to_string(),
                                                }
                                            }).collect();
                                        }
                                    }
                                    
                                    volume.access_method = if is_nvme_controller {
                                        "vhost-nvme".to_string()
                                    } else {
                                        "vhost-blk".to_string()
                                    };
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

async fn get_vhost_details(volume_id: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    let mut vhost_details = json!({
        "volume_id": volume_id,
        "vhost_controllers": [],
        "controller_type": "nvme"
    });
    
    for (node, rpc_url) in spdk_nodes.iter() {
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "vhost_get_controllers"
            }))
            .send()
            .await
        {
            if let Ok(vhost_info) = response.json::<serde_json::Value>().await {
                if let Some(controllers) = vhost_info["result"].as_array() {
                    for controller in controllers {
                        if let Some(ctrlr_name) = controller["ctrlr"].as_str() {
                            if ctrlr_name.contains(&volume_id) {
                                let mut controller_info = controller.clone();
                                controller_info["node"] = json!(node);
                                
                                if controller["backend_specific"]["type"].as_str() == Some("nvme") {
                                    controller_info["controller_type"] = json!("nvme");
                                    if let Some(namespaces) = controller["backend_specific"]["namespaces"].as_array() {
                                        controller_info["nvme_namespaces"] = json!(namespaces);
                                    }
                                } else {
                                    controller_info["controller_type"] = json!("blk");
                                }
                                
                                vhost_details["vhost_controllers"]
                                    .as_array_mut()
                                    .unwrap()
                                    .push(controller_info);
                            }
                        }
                    }
                }
            }
        }
    }
    
    Ok(warp::reply::json(&vhost_details))
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
        
        // Get vhost-nvme controllers
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "vhost_get_controllers"}))
            .send()
            .await
        {
            if let Ok(vhost_controllers) = response.json::<serde_json::Value>().await {
                metrics["vhost_controllers"] = vhost_controllers;
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
        
        // Get NVMe-oF subsystems
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
    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(state.kube_client.clone(), "default");
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
            replica_bdev_details: bdev_details_list, // Use the populated list.
        });
    }

    Ok(warp::reply::json(&detailed_results))
}

async fn get_snapshot_details(
    snapshot_id: String,
    state: AppState,
) -> Result<impl warp::Reply, warp::Rejection> {
    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(state.kube_client.clone(), "default");
    
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

async fn get_snapshots_tree(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(state.kube_client.clone(), "default");
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), "default");
    
    let all_snapshots = match snapshots_api.list(&ListParams::default()).await {
        Ok(list) => list,
        Err(e) => {
            return Ok(warp::reply::json(&json!({
                "error": format!("Failed to list snapshots: {}", e),
                "tree": {}
            })));
        }
    };

    let all_volumes = match volumes_api.list(&ListParams::default()).await {
        Ok(list) => list,
        Err(e) => {
            return Ok(warp::reply::json(&json!({
                "error": format!("Failed to list volumes: {}", e),
                "tree": {}
            })));
        }
    };

    // Build tree structure
    let mut tree = json!({});
    let mut volume_snapshots: std::collections::HashMap<String, Vec<_>> = std::collections::HashMap::new();

    // Group snapshots by volume
    for snapshot in all_snapshots.items {
        let volume_id = snapshot.spec.source_volume_id.clone();
        volume_snapshots.entry(volume_id).or_insert(Vec::new()).push(snapshot);
    }

    // Build tree for each volume
    for volume in all_volumes.items {
        let volume_id = volume.spec.volume_id.clone();
        let volume_name = volume.metadata.name.clone().unwrap_or(volume_id.clone());
        
        if let Some(snapshots) = volume_snapshots.get_mut(&volume_id) {
            // Sort snapshots by creation time
            snapshots.sort_by(|a, b| {
                let time_a = a.status.as_ref()
                    .and_then(|s| s.creation_time)
                    .unwrap_or_else(chrono::Utc::now);
                let time_b = b.status.as_ref()
                    .and_then(|s| s.creation_time)
                    .unwrap_or_else(chrono::Utc::now);
                time_a.cmp(&time_b)
            });

            // Build hierarchy
            let mut snapshot_tree = Vec::new();
            for snapshot in snapshots {
                let status = snapshot.status.clone().unwrap_or_default();

                // --- Start of Change ---
                // Create a JSON object for each replica snapshot with its details.
                let replica_details: Vec<_> = snapshot.spec.replica_snapshots.iter().map(|rs| {
                    json!({
                        "node": rs.node_name,
                        "bdev_name": rs.spdk_snapshot_lvol,
                        "source_bdev": rs.source_lvol_bdev,
                        "disk": rs.disk_ref
                    })
                }).collect();
                // --- End of Change ---

                let snapshot_info = json!({
                    "snapshot_id": snapshot.spec.snapshot_id,
                    "snapshot_type": snapshot.spec.snapshot_type,
                    "creation_time": status.creation_time,
                    "ready_to_use": status.ready_to_use,
                    "size_bytes": status.size_bytes,
                    "clone_source_snapshot_id": snapshot.spec.clone_source_snapshot_id,
                    // --- Start of Change ---
                    // Add the new detailed array to the response.
                    "replica_snapshots": replica_details,
                    // --- End of Change ---
                    "children": []
                });
                snapshot_tree.push(snapshot_info);
            }

            tree[&volume_id] = json!({
                "volume_name": volume_name,
                "volume_id": volume_id,
                "volume_size": volume.spec.size_bytes,
                "snapshots": snapshot_tree
            });
        }
    }

    Ok(warp::reply::json(&tree))
}
