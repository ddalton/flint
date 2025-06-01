use warp::Filter;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use reqwest::Client as HttpClient;
use kube::{Client, Api, api::ListParams};
use chrono::{Utc, DateTime};
use std::env;

// Import your existing CRD types
use spdk_csi_driver::{SpdkVolume, SpdkDisk, SpdkVolumeStatus, SpdkDiskStatus, Replica, ReplicaHealth};

mod spdk_csi_driver {
    use kube::CustomResource;
    use serde::{Deserialize, Serialize};

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
        pub rebuild_in_progress: Option<ReplicationState>,
        pub write_ordering_enabled: bool,
        pub vhost_socket: Option<String>,
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
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct ReplicationState {
        pub target_replica_index: usize,
        pub source_replica_index: usize,
        pub snapshot_id: String,
        pub copy_progress: f64,
        pub phase: String,
        pub started_at: String,
        pub catch_write_log: Vec<WriteOperation>,
        pub write_barrier_active: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct WriteOperation {
        pub offset: u64,
        pub length: u64,
        pub sequence: u64,
        pub timestamp: String,
        pub checksum: String,
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

// Dashboard API response types (matching your frontend expectations)
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
    // VHost-NVMe related fields
    vhost_socket: Option<String>,
    vhost_device: Option<String>,
    vhost_enabled: bool,
    vhost_type: String, // "nvme" instead of "blk"
    nvme_namespaces: Vec<VhostNvmeNamespace>,
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
    spdk_nodes: Arc<RwLock<HashMap<String, String>>>, // node -> spdk_rpc_url
    cache: Arc<RwLock<Option<DashboardData>>>,
    last_update: Arc<RwLock<DateTime<Utc>>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize Kubernetes client
    let kube_client = Client::try_default().await?;
    
    // Initialize SPDK node mapping from environment or discovery
    let mut spdk_nodes = HashMap::new();
    
    // Read SPDK RPC URLs from environment or use defaults
    if let Ok(node_urls) = env::var("SPDK_NODE_URLS") {
        // Format: "node-a=http://node-a:5260,node-b=http://node-b:5260"
        for pair in node_urls.split(',') {
            if let Some((node, url)) = pair.split_once('=') {
                spdk_nodes.insert(node.to_string(), url.to_string());
            }
        }
    } else {
        // Default fallback - discover nodes from SpdkDisk CRDs
        spdk_nodes = discover_spdk_nodes(&kube_client).await?;
    }
    
    let app_state = AppState {
        kube_client,
        spdk_nodes: Arc::new(RwLock::new(spdk_nodes)),
        cache: Arc::new(RwLock::new(None)),
        last_update: Arc::new(RwLock::new(Utc::now())),
    };
    
    // Start background refresh task
    let refresh_state = app_state.clone();
    tokio::spawn(async move {
        refresh_loop(refresh_state).await;
    });
    
    // Define API routes
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);
    
    let state_filter = warp::any().map(move || app_state.clone());
    
    let api = warp::path("api").and(
        // Get dashboard data
        warp::path("dashboard")
            .and(warp::get())
            .and(state_filter.clone())
            .and_then(get_dashboard_data)
        .or(
            // Get individual volume details
            warp::path("volumes")
                .and(warp::path::param::<String>())
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_volume_details)
        )
        .or(
            // Get vhost details for a specific volume
            warp::path("volumes")
                .and(warp::path::param::<String>())
                .and(warp::path("vhost"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_vhost_details)
        )
        .or(
            // Trigger manual refresh
            warp::path("refresh")
                .and(warp::post())
                .and(state_filter.clone())
                .and_then(trigger_refresh)
        )
        .or(
            // Get SPDK metrics from specific node
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("metrics"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_node_metrics)
        )
        .or(
            // Get vhost controller status
            warp::path("nodes")
                .and(warp::path::param::<String>())
                .and(warp::path("vhost"))
                .and(warp::get())
                .and(state_filter.clone())
                .and_then(get_node_vhost_status)
        )
    );
    
    let routes = api.with(cors);
    
    println!("SPDK Dashboard API server starting on http://0.0.0.0:8080");
    warp::serve(routes)
        .run(([0, 0, 0, 0], 8080))
        .await;
    
    Ok(())
}

async fn discover_spdk_nodes(client: &Client) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let disks: Api<SpdkDisk> = Api::namespaced(client.clone(), "default");
    let disk_list = disks.list(&ListParams::default()).await?;
    
    let mut nodes = HashMap::new();
    for disk in disk_list.items {
        let node = &disk.spec.node;
        if !nodes.contains_key(node) {
            // Default SPDK RPC URL pattern
            let url = format!("http://{}:5260", node);
            nodes.insert(node.clone(), url);
        }
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
    // Fetch volumes from Kubernetes
    let volumes_api: Api<SpdkVolume> = Api::namespaced(state.kube_client.clone(), "default");
    let volumes_list = volumes_api.list(&ListParams::default()).await?;
    
    // Fetch disks from Kubernetes
    let disks_api: Api<SpdkDisk> = Api::namespaced(state.kube_client.clone(), "default");
    let disks_list = disks_api.list(&ListParams::default()).await?;
    
    // Convert to dashboard format
    let mut dashboard_volumes = Vec::new();
    let mut dashboard_disks = Vec::new();
    let mut nodes = std::collections::HashSet::new();
    
    // Process volumes
    for volume in volumes_list.items {
        let dashboard_volume = convert_volume_to_dashboard(&volume);
        for replica in &dashboard_volume.replica_statuses {
            nodes.insert(replica.node.clone());
        }
        dashboard_volumes.push(dashboard_volume);
    }
    
    // Process disks
    for disk in disks_list.items {
        nodes.insert(disk.spec.node.clone());
        let dashboard_disk = convert_disk_to_dashboard(&disk, &dashboard_volumes);
        dashboard_disks.push(dashboard_disk);
    }
    
    // Enhance with real-time SPDK metrics
    enhance_with_spdk_metrics(&mut dashboard_volumes, &mut dashboard_disks, state).await?;
    
    let dashboard_data = DashboardData {
        volumes: dashboard_volumes,
        disks: dashboard_disks,
        nodes: nodes.into_iter().collect(),
    };
    
    // Update cache
    *state.cache.write().await = Some(dashboard_data);
    *state.last_update.write().await = Utc::now();
    
    Ok(())
}

fn convert_volume_to_dashboard(volume: &SpdkVolume) -> DashboardVolume {
    let status = volume.status.as_ref().unwrap_or(&SpdkVolumeStatus::default());
    let spec = &volume.spec;
    
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
        
        // Determine replica storage location and access method
        let access_method = match replica.replica_type.as_str() {
            "lvol" => "local-nvme".to_string(),    // Local replica: stored on local NVMe
            "nvmf" => "remote-nvmf".to_string(),   // Remote replica: accessed via NVMe-oF
            _ => "unknown".to_string(),
        };
        
        DashboardReplicaStatus {
            node: replica.node.clone(),
            status: format!("{:?}", replica.health_status).to_lowercase(),
            is_local: replica.replica_type == "lvol",
            last_io_timestamp: replica.last_io_timestamp.clone(),
            rebuild_progress: None, // Will be populated from rebuild state
            rebuild_target: None,
            is_new_replica: None,
            nvmf_target,
            access_method,
        }
    }).collect();
    
    let size_gb = spec.size_bytes / (1024 * 1024 * 1024);
    let has_local_nvme = spec.replicas.iter().any(|r| r.replica_type == "lvol");
    
    // Volume is always accessed locally via vhost-nvme
    let volume_access_method = "vhost-nvme".to_string();
    
    // Generate vhost socket path and device path for NVMe
    let volume_name = volume.metadata.name.clone().unwrap_or(spec.volume_id.clone());
    let vhost_socket = spec.vhost_socket.clone().or_else(|| 
        Some(format!("/var/lib/spdk/vhost/vhost_{}.sock", volume_name))
    );
    let vhost_device = status.vhost_device.clone().or_else(|| 
        Some(format!("/dev/nvme-vhost-{}", volume_name))
    );
    let vhost_enabled = vhost_socket.is_some();
    
    // Create NVMe namespace information
    let nvme_namespaces = vec![
        VhostNvmeNamespace {
            nsid: 1,
            size: spec.size_bytes as u64,
            uuid: spec.primary_lvol_uuid.clone().unwrap_or_default(),
            bdev_name: spec.volume_id.clone(),
        }
    ];
    
    DashboardVolume {
        id: spec.volume_id.clone(),
        name: volume_name,
        size: format!("{}GB", size_gb),
        state: status.state.clone(),
        replicas: spec.num_replicas,
        active_replicas: status.active_replicas.len() as i32,
        local_nvme: has_local_nvme,
        access_method: volume_access_method,
        rebuild_progress: spec.rebuild_in_progress.as_ref().map(|r| r.copy_progress),
        nodes: spec.replicas.iter().map(|r| r.node.clone()).collect(),
        replica_statuses,
        vhost_socket,
        vhost_device,
        vhost_enabled,
        vhost_type: "nvme".to_string(), // Changed from "blk" to "nvme"
        nvme_namespaces,
    }
}

fn convert_disk_to_dashboard(disk: &SpdkDisk, volumes: &[DashboardVolume]) -> DashboardDisk {
    let status = disk.status.as_ref().unwrap_or(&SpdkDiskStatus::default());
    let spec = &disk.spec;
    
    // Find volumes using this disk
    let provisioned_volumes: Vec<ProvisionedVolume> = volumes.iter()
        .filter_map(|vol| {
            for replica in &vol.replica_statuses {
                if replica.node == spec.node {
                    return Some(ProvisionedVolume {
                        volume_name: vol.name.clone(),
                        volume_id: vol.id.clone(),
                        size: vol.size.trim_end_matches("GB").parse().unwrap_or(0),
                        provisioned_at: Utc::now().to_rfc3339(), // You might want to track this
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
        model: format!("NVMe Disk"), // You might want to enhance this
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
        // Get vhost-nvme controller status for volumes using vhost-nvme
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "vhost_get_controllers"
            }))
            .send()
            .await
        {
            if let Ok(vhost_info) = response.json::<serde_json::Value>().await {
                // Process vhost controller information and update volume states
                if let Some(controllers) = vhost_info["result"].as_array() {
                    for controller in controllers {
                        if let Some(ctrlr_name) = controller["ctrlr"].as_str() {
                            // Check if this is a vhost-nvme controller
                            let is_nvme_controller = controller["backend_specific"]["type"]
                                .as_str() == Some("nvme");
                            
                            // Match vhost controller to volume
                            for volume in volumes.iter_mut() {
                                // Check if this vhost controller corresponds to this volume
                                if ctrlr_name.contains(&volume.name) || 
                                   ctrlr_name.contains(&volume.id) {
                                    
                                    // Update vhost information from live SPDK data
                                    if let Some(socket_path) = controller["socket"].as_str() {
                                        volume.vhost_socket = Some(socket_path.to_string());
                                    }
                                    
                                    // Check if controller is active
                                    if let Some(active) = controller["active"].as_bool() {
                                        volume.vhost_enabled = active;
                                    }
                                    
                                    // Update vhost type
                                    volume.vhost_type = if is_nvme_controller {
                                        "nvme".to_string()
                                    } else {
                                        "blk".to_string()
                                    };
                                    
                                    // Get NVMe namespace information for vhost-nvme controllers
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
                                    
                                    // Update access method confirmation
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
        
        // Get real-time bdev statistics and update volumes/disks
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({
                "method": "bdev_get_iostat"
            }))
            .send()
            .await
        {
            if let Ok(iostat) = response.json::<serde_json::Value>().await {
                // Process real-time I/O statistics and update dashboard data
                if let Some(bdevs) = iostat["result"].as_array() {
                    for bdev_stat in bdevs {
                        if let Some(bdev_name) = bdev_stat["name"].as_str() {
                            // Update disk statistics
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
        
        // Get RAID status and rebuild information
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
                // Update volume states based on RAID status
                if let Some(raid_bdevs) = raid_info["result"].as_array() {
                    for raid_bdev in raid_bdevs {
                        if let Some(raid_name) = raid_bdev["name"].as_str() {
                            for volume in volumes.iter_mut() {
                                if volume.id == raid_name || raid_name.contains(&volume.id) {
                                    // Update volume state based on RAID status
                                    if let Some(state) = raid_bdev["state"].as_str() {
                                        volume.state = match state {
                                            "online" => "Healthy".to_string(),
                                            "degraded" => "Degraded".to_string(),
                                            "broken" => "Failed".to_string(),
                                            _ => state.to_string(),
                                        };
                                    }
                                    
                                    // Check for rebuild operations
                                    if let Some(rebuild_info) = raid_bdev["rebuild_info"].as_object() {
                                        if let Some(progress) = rebuild_info["progress_percentage"].as_f64() {
                                            volume.rebuild_progress = Some(progress);
                                            
                                            // Update replica status for rebuilding replicas
                                            if let Some(target_idx) = rebuild_info["target_replica_index"].as_usize() {
                                                if let Some(replica) = volume.replica_statuses.get_mut(target_idx) {
                                                    replica.status = "rebuilding".to_string();
                                                    replica.rebuild_progress = Some(progress);
                                                }
                                            }
                                        }
                                    }
                                    
                                    // Update replica statuses based on RAID member states
                                    if let Some(base_bdevs) = raid_bdev["base_bdevs"].as_array() {
                                        for (idx, base_bdev) in base_bdevs.iter().enumerate() {
                                            if let Some(replica) = volume.replica_statuses.get_mut(idx) {
                                                if let Some(member_state) = base_bdev["state"].as_str() {
                                                    replica.status = match member_state {
                                                        "online" => "healthy".to_string(),
                                                        "degraded" => "degraded".to_string(),
                                                        "failed" => "failed".to_string(),
                                                        "rebuilding" => "rebuilding".to_string(),
                                                        _ => member_state.to_string(),
                                                    };
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
    }
    
    Ok(())
}

// API handlers
async fn get_dashboard_data(state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    // Check if cache is fresh (less than 60 seconds old)
    let last_update = *state.last_update.read().await;
    let cache_age = Utc::now().signed_duration_since(last_update);
    
    if cache_age.num_seconds() > 60 {
        // Refresh if cache is stale
        if let Err(e) = refresh_dashboard_data(&state).await {
            eprintln!("Failed to refresh data: {}", e);
        }
    }
    
    let cache = state.cache.read().await;
    if let Some(data) = cache.as_ref() {
        Ok(warp::reply::json(data))
    } else {
        // Return empty data if cache is not ready
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

async fn get_vhost_details(volume_id: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    let http_client = HttpClient::new();
    let mut vhost_details = json!({
        "volume_id": volume_id,
        "vhost_controllers": [],
        "controller_type": "nvme"
    });
    
    // Query all nodes for vhost-nvme controller information
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
                            // Check if this controller is for our volume
                            if ctrlr_name.contains(&volume_id) {
                                let mut controller_info = controller.clone();
                                controller_info["node"] = json!(node);
                                
                                // Add NVMe-specific information
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
        
        // Get various SPDK metrics for the node
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
        
        // Get RAID information
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "bdev_raid_get_bdevs"}))
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

async fn get_node_vhost_status(node: String, state: AppState) -> Result<impl warp::Reply, warp::Rejection> {
    let spdk_nodes = state.spdk_nodes.read().await;
    if let Some(rpc_url) = spdk_nodes.get(&node) {
        let http_client = HttpClient::new();
        
        // Get vhost controller status with enhanced NVMe information
        if let Ok(response) = http_client
            .post(rpc_url)
            .json(&json!({"method": "vhost_get_controllers"}))
            .send()
            .await
        {
            if let Ok(mut vhost_controllers) = response.json::<serde_json::Value>().await {
                // Enhance with additional NVMe controller information
                if let Some(controllers) = vhost_controllers["result"].as_array_mut() {
                    for controller in controllers {
                        // Add controller type information
                        let controller_type = controller["backend_specific"]["type"]
                            .as_str()
                            .unwrap_or("unknown");
                        controller["controller_type"] = json!(controller_type);
                        
                        // For NVMe controllers, add namespace count and details
                        if controller_type == "nvme" {
                            let namespace_count = controller["backend_specific"]["namespaces"]
                                .as_array()
                                .map(|ns| ns.len())
                                .unwrap_or(0);
                            controller["namespace_count"] = json!(namespace_count);
                            
                            // Add detailed namespace information
                            if let Some(namespaces) = controller["backend_specific"]["namespaces"].as_array() {
                                let mut namespace_details = Vec::new();
                                for ns in namespaces {
                                    namespace_details.push(json!({
                                        "nsid": ns["nsid"],
                                        "size": ns["size"],
                                        "uuid": ns["uuid"],
                                        "bdev_name": ns["bdev_name"],
                                        "block_size": ns["block_size"],
                                        "md_size": ns["md_size"]
                                    }));
                                }
                                controller["namespace_details"] = json!(namespace_details);
                            }
                        } else if controller_type == "blk" {
                            // For block controllers, add backend device information
                            if let Some(backend) = controller["backend_specific"]["backend"].as_str() {
                                controller["backend_device"] = json!(backend);
                            }
                        }
                        
                        // Add socket information
                        if let Some(socket) = controller["socket"].as_str() {
                            controller["socket_path"] = json!(socket);
                        }
                        
                        // Add connection status
                        let is_active = controller["active"].as_bool().unwrap_or(false);
                        controller["connection_status"] = json!(if is_active { "active" } else { "inactive" });
                        
                        // Add CPU mask information
                        if let Some(cpumask) = controller["cpumask"].as_str() {
                            controller["cpu_affinity"] = json!(cpumask);
                        }
                    }
                }
                return Ok(warp::reply::json(&vhost_controllers));
            }
        }
    }
    
    Ok(warp::reply::json(&json!({"error": "Failed to get vhost status"})))
}