// Updated controller.rs with vhost support
use kube::{
    Client, Api, ResourceExt, runtime::{Controller, watcher},
    api::{PatchParams, Patch, ListParams, PostParams},
};
use tokio::time::{Duration, interval, timeout};
use reqwest::Client as HttpClient;
use serde_json::json;
use chrono::{Utc, Duration as ChronoDuration};
use std::env;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, Mutex};
use uuid::Uuid;

use spdk_csi_driver::{SpdkVolume, SpdkDisk, Replica, SpdkVolumeStatus};

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
        pub vhost_socket: Option<String>, // Path to vhost socket
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
        pub vhost_socket: Option<String>, // For vhost-based access
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
        pub vhost_device: Option<String>, // Path to vhost block device
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

struct Context {
    client: Client,
    spdk_rpc_url: String,
    health_interval: u64,
    rebuild_enabled: bool,
    max_retries: u32,
    snapshot_retention: String,
    write_barrier_timeout: u64,
    active_rebuilds: Arc<RwLock<HashMap<String, ReplicationState>>>,
    write_sequence_counter: Arc<Mutex<u64>>,
    vhost_socket_base_path: String,
}

#[derive(Debug, Clone)]
struct NvmeDevice {
    controller_id: String,
    pcie_addr: String,
    capacity: i64,
    model: String,
    serial: String,
    firmware_version: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(client.clone(), "default");
    
    let vhost_socket_base_path = env::var("VHOST_SOCKET_PATH")
        .unwrap_or("/var/lib/spdk-csi/sockets".to_string());
    
    // Ensure vhost socket directory exists
    tokio::fs::create_dir_all(&vhost_socket_base_path).await?;
    
    let ctx = Arc::new(Context {
        client: client.clone(),
        spdk_rpc_url: env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        health_interval: env::var("HEALTH_CHECK_INTERVAL").unwrap_or("30".to_string()).parse().unwrap_or(30),
        rebuild_enabled: env::var("REBUILD_ENABLED").unwrap_or("true".to_string()).parse().unwrap_or(true),
        max_retries: env::var("REBUILD_MAX_RETRIES").unwrap_or("3".to_string()).parse().unwrap_or(3),
        snapshot_retention: env::var("SNAPSHOT_RETENTION").unwrap_or("1h".to_string()),
        write_barrier_timeout: env::var("WRITE_BARRIER_TIMEOUT").unwrap_or("30".to_string()).parse().unwrap_or(30),
        active_rebuilds: Arc::new(RwLock::new(HashMap::new())),
        write_sequence_counter: Arc::new(Mutex::new(0)),
        vhost_socket_base_path,
    });

    // Start health monitoring task
    let health_ctx = ctx.clone();
    tokio::spawn(async move {
        health_monitor_task(health_ctx).await;
    });

    // Start vhost cleanup task
    let cleanup_ctx = ctx.clone();
    tokio::spawn(async move {
        vhost_cleanup_task(cleanup_ctx).await;
    });

    Controller::new(spdk_volumes, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .await;

    Ok(())
}

async fn reconcile(spdk_volume: SpdkVolume, ctx: Arc<Context>) -> Result<(), kube::Error> {
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let volume_id = spdk_volume.spec.volume_id.clone();
    let mut status = spdk_volume.status.unwrap_or_default();
    let mut update_needed = false;

    // Check if rebuild is in progress
    if let Some(rebuild_state) = &spdk_volume.spec.rebuild_in_progress {
        return handle_ongoing_rebuild(&spdk_volume, &ctx, rebuild_state).await;
    }

    // Health check all replicas
    let health_results = check_replica_health(&spdk_volume, &ctx).await?;
    let mut failed_replicas = Vec::new();
    let mut active_replicas = Vec::new();

    for (i, health) in health_results.iter().enumerate() {
        match health {
            ReplicaHealth::Healthy => active_replicas.push(i),
            ReplicaHealth::Failed => failed_replicas.push(i),
            _ => {}
        }
    }

    status.active_replicas = active_replicas.clone();
    status.failed_replicas = failed_replicas.clone();
    
    // Determine volume state
    if failed_replicas.is_empty() {
        status.state = "Healthy".to_string();
        status.degraded = false;
    } else if active_replicas.len() >= 1 {
        status.state = "Degraded".to_string();
        status.degraded = true;
    } else {
        status.state = "Failed".to_string();
        status.degraded = true;
    }

    status.last_checked = Utc::now().to_rfc3339();
    update_needed = true;

    // Check and manage vhost controllers
    manage_vhost_controllers(&spdk_volume, &ctx).await?;

    // Initiate rebuild if needed and enabled
    if ctx.rebuild_enabled && !failed_replicas.is_empty() && !active_replicas.is_empty() {
        for &failed_index in &failed_replicas {
            if let Err(e) = initiate_replica_rebuild(
                &spdk_volume,
                &ctx,
                failed_index,
                &active_replicas,
                &spdk_disks,
            ).await {
                eprintln!("Failed to initiate rebuild for replica {}: {}", failed_index, e);
                status.state = "Failed".to_string();
            }
        }
    }

    if update_needed {
        spdk_volumes
            .patch_status(&volume_id, &PatchParams::default(), &Patch::Merge(json!({
                "status": status
            })))
            .await?;
    }

    Ok(())
}

async fn manage_vhost_controllers(
    spdk_volume: &SpdkVolume,
    ctx: &Context,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    
    // Check if vhost controller exists for this volume
    let controller_exists = check_vhost_controller_exists(ctx, volume_id).await?;
    let socket_path = get_vhost_socket_path(ctx, volume_id);
    
    // If volume has local replicas that are being accessed by local pods, ensure vhost controller exists
    let has_local_replicas_with_pods = spdk_volume.spec.replicas.iter()
        .any(|r| r.replica_type == "lvol" && r.local_pod_scheduled);
    
    if has_local_replicas_with_pods && !controller_exists {
        create_vhost_controller_for_volume(ctx, volume_id, &socket_path).await?;
        
        // Update volume spec with socket path
        let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
        spdk_volumes
            .patch(volume_id, &PatchParams::default(), &Patch::Merge(json!({
                "spec": {
                    "vhost_socket": socket_path
                }
            })))
            .await?;
    } else if !has_local_replicas_with_pods && controller_exists {
        // Clean up unused vhost controller
        delete_vhost_controller(ctx, volume_id).await?;
    }
    
    Ok(())
}

async fn check_vhost_controller_exists(
    ctx: &Context,
    volume_id: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let controller_name = format!("vhost_{}", volume_id);
    
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_get_controllers"
        }))
        .send()
        .await?;
    
    if response.status().is_success() {
        let controllers: serde_json::Value = response.json().await?;
        if let Some(controller_list) = controllers["result"].as_array() {
            return Ok(controller_list.iter().any(|c| {
                c["ctrlr"].as_str() == Some(&controller_name)
            }));
        }
    }
    
    Ok(false)
}

async fn create_vhost_controller_for_volume(
    ctx: &Context,
    volume_id: &str,
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let controller_name = format!("vhost_{}", volume_id);
    
    // Ensure socket directory exists
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    
    // Create vhost-blk controller
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_create_blk_controller",
            "params": {
                "ctrlr": controller_name,
                "dev_name": volume_id, // Use RAID bdev as the underlying device
                "cpumask": "0x1",
                "readonly": false,
                "packed_ring": false
            }
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
        let error_text = response.text().await?;
        return Err(format!("Failed to create vhost controller: {}", error_text).into());
    }
    
    // Start the vhost controller with socket path
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_start_controller",
            "params": {
                "ctrlr": controller_name,
                "socket": socket_path
            }
        }))
        .send()
        .await?;
    
    println!("Created vhost controller for volume: {}", volume_id);
    Ok(())
}

async fn delete_vhost_controller(
    ctx: &Context,
    volume_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let controller_name = format!("vhost_{}", volume_id);
    let socket_path = get_vhost_socket_path(ctx, volume_id);
    
    // Stop vhost controller
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_stop_controller",
            "params": { "ctrlr": controller_name }
        }))
        .send()
        .await
        .ok();
    
    // Delete vhost controller
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_delete_controller",
            "params": { "ctrlr": controller_name }
        }))
        .send()
        .await
        .ok();
    
    // Clean up socket file
    tokio::fs::remove_file(&socket_path).await.ok();
    
    println!("Deleted vhost controller for volume: {}", volume_id);
    Ok(())
}

fn get_vhost_socket_path(ctx: &Context, volume_id: &str) -> String {
    format!("{}/vhost_{}.sock", ctx.vhost_socket_base_path, volume_id)
}

async fn check_replica_health(
    spdk_volume: &SpdkVolume,
    ctx: &Context,
) -> Result<Vec<ReplicaHealth>, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let mut health_results = Vec::new();

    for replica in spdk_volume.spec.replicas.iter() {
        let health = match replica.replica_type.as_str() {
            "lvol" => {
                // For lvol replicas (both local and remote), check the underlying lvol health
                check_lvol_health(&http_client, ctx, replica).await?
            }
            "nvmf" => {
                // For NVMe-oF connected replicas, check remote connectivity and health
                check_nvmf_replica_health(&http_client, ctx, replica).await?
            }
            _ => {
                // Unknown replica type
                ReplicaHealth::Failed
            }
        };

        health_results.push(health);
    }

    Ok(health_results)
}

async fn check_lvol_health(
    http_client: &HttpClient,
    ctx: &Context,
    replica: &Replica,
) -> Result<ReplicaHealth, Box<dyn std::error::Error>> {
    let lvol_uuid = replica.lvol_uuid.as_ref()
        .ok_or("Missing lvol UUID for replica")?;
    
    let lvs_name = format!("lvs_{}", replica.disk_ref);
    let lvol_bdev_name = format!("{}/{}", lvs_name, lvol_uuid);
    
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": lvol_bdev_name
            }
        }))
        .send()
        .await;

    match response {
        Ok(resp) => {
            if resp.status().is_success() {
                // Additional I/O health check
                if perform_io_health_check(http_client, &ctx.spdk_rpc_url, &lvol_bdev_name).await? {
                    Ok(ReplicaHealth::Healthy)
                } else {
                    Ok(ReplicaHealth::Degraded)
                }
            } else {
                Ok(ReplicaHealth::Failed)
            }
        }
        Err(_) => Ok(ReplicaHealth::Failed),
    }
}

async fn check_nvmf_replica_health(
    _http_client: &HttpClient,
    _ctx: &Context,
    replica: &Replica,
) -> Result<ReplicaHealth, Box<dyn std::error::Error>> {
    // For NVMe-oF replicas, we can check connectivity
    if let (Some(ip), Some(port), Some(nqn)) = (&replica.ip, &replica.port, &replica.nqn) {
        // Simple connectivity test - in production, you might want more sophisticated checks
        let target_addr = format!("{}:{}", ip, port);
        match tokio::net::TcpStream::connect(&target_addr).await {
            Ok(_) => Ok(ReplicaHealth::Healthy),
            Err(_) => Ok(ReplicaHealth::Failed),
        }
    } else {
        Ok(ReplicaHealth::Failed)
    }
}

async fn perform_io_health_check(
    http_client: &HttpClient,
    spdk_rpc_url: &str,
    lvol_bdev_name: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Perform a small test read to verify I/O functionality
    let response = http_client
        .post(spdk_rpc_url)
        .json(&json!({
            "method": "bdev_get_iostat",
            "params": {
                "name": lvol_bdev_name
            }
        }))
        .send()
        .await?;

    Ok(response.status().is_success())
}

async fn initiate_replica_rebuild(
    spdk_volume: &SpdkVolume,
    ctx: &Context,
    failed_replica_index: usize,
    active_replicas: &[usize],
    spdk_disks: &Api<SpdkDisk>,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    
    // Select best source replica (prefer local, then by performance)
    let source_replica_index = select_best_source_replica(spdk_volume, active_replicas).await?;
    
    // Find suitable replacement disk
    let replacement_disk = find_replacement_disk(
        spdk_volume,
        failed_replica_index,
        spdk_disks,
    ).await?;

    // Create lvol snapshot for rebuild
    let snapshot_id = create_lvol_snapshot(
        ctx,
        &spdk_volume.spec.replicas[source_replica_index],
    ).await?;

    // Initialize rebuild state
    let rebuild_state = ReplicationState {
        target_replica_index: failed_replica_index,
        source_replica_index,
        snapshot_id: snapshot_id.clone(),
        copy_progress: 0.0,
        phase: "snapshot".to_string(),
        started_at: Utc::now().to_rfc3339(),
        catch_write_log: Vec::new(),
        write_barrier_active: false,
    };

    // Store rebuild state
    ctx.active_rebuilds.write().await.insert(volume_id.clone(), rebuild_state.clone());

    // Update volume spec with rebuild state
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    spdk_volumes
        .patch(&volume_id, &PatchParams::default(), &Patch::Merge(json!({
            "spec": {
                "rebuild_in_progress": rebuild_state
            }
        })))
        .await?;

    // Start async rebuild process
    let rebuild_ctx = ctx.clone();
    let rebuild_volume = spdk_volume.clone();
    tokio::spawn(async move {
        if let Err(e) = execute_replica_rebuild(rebuild_volume, rebuild_ctx, replacement_disk).await {
            eprintln!("Rebuild failed: {}", e);
        }
    });

    Ok(())
}

async fn execute_replica_rebuild(
    spdk_volume: SpdkVolume,
    ctx: Arc<Context>,
    replacement_disk: SpdkDisk,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    let rebuild_state = spdk_volume.spec.rebuild_in_progress.as_ref()
        .ok_or("No rebuild state found")?;

    // Phase 1: Pause writes to ensure consistency
    update_rebuild_phase(&ctx, volume_id, "pause").await?;
    pause_raid_writes(&ctx, volume_id).await?;
    
    // Phase 2: Create snapshot of healthy replica lvol
    update_rebuild_phase(&ctx, volume_id, "snapshot").await?;
    let source_replica = &spdk_volume.spec.replicas[rebuild_state.source_replica_index];
    let snapshot_name = create_lvol_snapshot(&ctx, source_replica).await?;
    
    // Phase 3: Initialize target lvol store and create thin provisioned lvol
    update_rebuild_phase(&ctx, volume_id, "provision").await?;
    let target_lvs_name = initialize_target_lvol_store(&ctx, &replacement_disk).await?;
    let target_lvol_name = create_thin_provisioned_lvol(&ctx, &target_lvs_name, &snapshot_name, volume_id).await?;
    
    // Phase 4: Add new lvol bdev to RAID-1 configuration
    update_rebuild_phase(&ctx, volume_id, "integrate").await?;
    add_lvol_to_raid(&ctx, volume_id, &target_lvol_name, rebuild_state.target_replica_index).await?;
    
    // Phase 5: Unpause writes - RAID will handle synchronization automatically
    update_rebuild_phase(&ctx, volume_id, "unpause").await?;
    unpause_raid_writes(&ctx, volume_id).await?;
    
    // Phase 6: Inflate the thin provisioned lvol (make it independent)
    update_rebuild_phase(&ctx, volume_id, "inflate").await?;
    inflate_thin_provisioned_lvol(&ctx, &target_lvol_name).await?;
    
    // Phase 7: Recreate vhost controller if needed
    update_rebuild_phase(&ctx, volume_id, "vhost_update").await?;
    recreate_vhost_controller_after_rebuild(&ctx, &spdk_volume).await?;
    
    // Phase 8: Finalize and cleanup
    update_rebuild_phase(&ctx, volume_id, "finalize").await?;
    finalize_rebuild_with_lvol(&ctx, &spdk_volume, rebuild_state, replacement_disk, target_lvol_name, snapshot_name).await?;

    Ok(())
}

async fn recreate_vhost_controller_after_rebuild(
    ctx: &Context,
    spdk_volume: &SpdkVolume,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    
    // Check if volume has local replicas that need vhost access
    let has_local_replicas_with_pods = spdk_volume.spec.replicas.iter()
        .any(|r| r.replica_type == "lvol" && r.local_pod_scheduled);
    
    if has_local_replicas_with_pods {
        // Delete existing vhost controller
        delete_vhost_controller(ctx, volume_id).await.ok();
        
        // Wait a moment for cleanup
        tokio::time::sleep(Duration::from_secs(1)).await;
        
        // Create new vhost controller
        let socket_path = get_vhost_socket_path(ctx, volume_id);
        create_vhost_controller_for_volume(ctx, volume_id, &socket_path).await?;
    }
    
    Ok(())
}

async fn vhost_cleanup_task(ctx: Arc<Context>) {
    let mut interval = interval(Duration::from_secs(300)); // Run every 5 minutes
    
    loop {
        interval.tick().await;
        
        if let Err(e) = cleanup_orphaned_vhost_controllers(&ctx).await {
            eprintln!("Vhost cleanup failed: {}", e);
        }
    }
}

async fn cleanup_orphaned_vhost_controllers(
    ctx: &Context,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // Get all vhost controllers
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_get_controllers"
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Ok(()); // Skip if command not available
    }
    
    let controllers: serde_json::Value = response.json().await?;
    if let Some(controller_list) = controllers["result"].as_array() {
        let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
        let volumes = spdk_volumes.list(&ListParams::default()).await?;
        
        for controller in controller_list {
            if let Some(controller_name) = controller["ctrlr"].as_str() {
                if controller_name.starts_with("vhost_") {
                    let volume_id = controller_name.strip_prefix("vhost_").unwrap();
                    
                    // Check if corresponding volume exists
                    let volume_exists = volumes.items.iter()
                        .any(|v| v.spec.volume_id == volume_id);
                    
                    if !volume_exists {
                        println!("Cleaning up orphaned vhost controller: {}", controller_name);
                        delete_vhost_controller(ctx, volume_id).await.ok();
                    }
                }
            }
        }
    }
    
    Ok(())
}

// Include all the existing helper functions from the original controller.rs
// (create_lvol_snapshot, pause_raid_writes, etc.) with minimal modifications

async fn create_lvol_snapshot(
    ctx: &Context,
    source_replica: &Replica,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let snapshot_name = format!("snap_{}_{}", 
        source_replica.disk_ref, 
        Utc::now().timestamp()
    );
    
    let lvol_uuid = source_replica.lvol_uuid.as_ref()
        .ok_or("Missing lvol UUID for source replica")?;
    let lvs_name = format!("lvs_{}", source_replica.disk_ref);
    let source_lvol_name = format!("{}/{}", lvs_name, lvol_uuid);

    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_snapshot",
            "params": {
                "lvol_name": source_lvol_name,
                "snapshot_name": snapshot_name
            }
        }))
        .send()
        .await?;

    Ok(snapshot_name)
}

async fn pause_raid_writes(
    ctx: &Context,
    volume_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_pause_io",
            "params": {
                "name": volume_id,
                "io_type": "write"
            }
        }))
        .send()
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

async fn unpause_raid_writes(
    ctx: &Context,
    volume_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_resume_io",
            "params": {
                "name": volume_id,
                "io_type": "write"
            }
        }))
        .send()
        .await?;
    
    Ok(())
}

async fn initialize_target_lvol_store(
    ctx: &Context,
    replacement_disk: &SpdkDisk,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let lvs_name = format!("lvs_{}", replacement_disk.metadata.name.as_ref().unwrap());
    
    let check_response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_get_lvstores"
        }))
        .send()
        .await?;

    let existing_stores: serde_json::Value = check_response.json().await?;
    let store_exists = existing_stores["result"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|store| store["name"].as_str() == Some(&lvs_name));

    if !store_exists {
        let bdev_name = format!("{}n1", replacement_disk.spec.nvme_controller_id.as_ref().unwrap_or(&"nvme0".to_string()));
        http_client
            .post(&ctx.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_lvol_create_lvstore",
                "params": {
                    "bdev_name": bdev_name,
                    "lvs_name": lvs_name,
                    "cluster_sz": 65536
                }
            }))
            .send()
            .await?;
    }

    Ok(lvs_name)
}

async fn create_thin_provisioned_lvol(
    ctx: &Context,
    lvs_name: &str,
    snapshot_name: &str,
    volume_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let lvol_name = format!("vol_{}_{}", volume_id, Utc::now().timestamp());
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size": 0,
                "thin_provision": true,
                "clone_snapshot_name": snapshot_name
            }
        }))
        .send()
        .await?;

    Ok(format!("{}/{}", lvs_name, lvol_name))
}

async fn add_lvol_to_raid(
    ctx: &Context,
    volume_id: &str,
    target_lvol_name: &str,
    failed_replica_index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_remove_base_bdev",
            "params": {
                "name": volume_id,
                "base_bdev_slot": failed_replica_index
            }
        }))
        .send()
        .await
        .ok();

    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_add_base_bdev",
            "params": {
                "name": volume_id,
                "base_bdev_name": target_lvol_name,
                "base_bdev_slot": failed_replica_index
            }
        }))
        .send()
        .await?;

}

async fn inflate_thin_provisioned_lvol(
    ctx: &Context,
    target_lvol_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_inflate",
            "params": {
                "name": target_lvol_name
            }
        }))
        .send()
        .await?;

    Ok(())
}

async fn finalize_rebuild_with_lvol(
    ctx: &Context,
    spdk_volume: &SpdkVolume,
    rebuild_state: &ReplicationState,
    replacement_disk: SpdkDisk,
    target_lvol_name: String,
    snapshot_name: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    
    let lvol_uuid = get_lvol_uuid(&ctx, &target_lvol_name).await?;
    
    let mut new_spec = spdk_volume.spec.clone();
    new_spec.replicas[rebuild_state.target_replica_index] = Replica {
        node: replacement_disk.spec.node.clone(),
        replica_type: "lvol".to_string(), // Still lvol - the replica type doesn't change
        pcie_addr: Some(replacement_disk.spec.pcie_addr.clone()),
        disk_ref: replacement_disk.metadata.name.clone().unwrap_or_default(),
        lvol_uuid: Some(lvol_uuid),
        nqn: Some(format!("nqn.2025-05.io.spdk:lvol-{}", target_lvol_name.replace('/', "-"))),
        health_status: ReplicaHealth::Healthy,
        last_io_timestamp: Some(Utc::now().to_rfc3339()),
        write_sequence: 0,
        local_pod_scheduled: false,
        vhost_socket: None, // vhost is for the RAID volume, not individual replicas
        ..Default::default()
    };
    new_spec.rebuild_in_progress = None;
    
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    spdk_volumes
        .patch(volume_id, &PatchParams::default(), &Patch::Merge(json!({
            "spec": new_spec,
            "status": {
                "state": "Healthy",
                "degraded": false,
                "last_checked": Utc::now().to_rfc3339(),
                "active_replicas": (0..spdk_volume.spec.num_replicas).collect::<Vec<_>>(),
                "failed_replicas": []
            }
        })))
        .await?;
    
    let disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let disk_name = replacement_disk.metadata.name.clone().unwrap_or_default();
    let mut disk_status = replacement_disk.status.unwrap_or_default();
    
    disk_status.free_space -= spdk_volume.spec.size_bytes;
    disk_status.used_space += spdk_volume.spec.size_bytes;
    disk_status.lvol_count += 1;
    
    disks
        .patch_status(&disk_name, &PatchParams::default(), &Patch::Merge(json!({
            "status": disk_status
        })))
        .await?;
    
    cleanup_snapshot(&ctx, &snapshot_name).await?;
    
    ctx.active_rebuilds.write().await.remove(volume_id);
    
    println!("Successfully completed rebuild for volume {} with vhost support", volume_id);
    
    Ok(())
}

async fn get_lvol_uuid(
    ctx: &Context,
    lvol_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": lvol_name
            }
        }))
        .send()
        .await?;

    let bdev_info: serde_json::Value = response.json().await?;
    let uuid = bdev_info["result"][0]["uuid"]
        .as_str()
        .ok_or("Failed to get lvol UUID")?;

    Ok(uuid.to_string())
}

async fn cleanup_snapshot(
    ctx: &Context,
    snapshot_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_delete",
            "params": {
                "name": snapshot_name
            }
        }))
        .send()
        .await?;
    
    println!("Cleaned up snapshot: {}", snapshot_name);
    Ok(())
}

async fn select_best_source_replica(
    spdk_volume: &SpdkVolume,
    active_replicas: &[usize],
) -> Result<usize, Box<dyn std::error::Error>> {
    for &index in active_replicas {
        if spdk_volume.spec.replicas[index].local_pod_scheduled {
            return Ok(index);
        }
    }
    Ok(active_replicas[0])
}

async fn find_replacement_disk(
    spdk_volume: &SpdkVolume,
    failed_replica_index: usize,
    spdk_disks: &Api<SpdkDisk>,
) -> Result<SpdkDisk, Box<dyn std::error::Error>> {
    let required_capacity = spdk_volume.spec.size_bytes;
    let used_nodes: Vec<String> = spdk_volume.spec.replicas
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != failed_replica_index)
        .map(|(_, r)| r.node.clone())
        .collect();
    
    let available_disks = spdk_disks.list(&ListParams::default()).await?
        .items
        .into_iter()
        .filter(|d| {
            if let Some(status) = &d.status {
                status.healthy 
                    && status.blobstore_initialized 
                    && status.free_space >= required_capacity 
                    && !used_nodes.contains(&d.spec.node)
            } else {
                false
            }
        })
        .collect::<Vec<_>>();
    
    available_disks
        .into_iter()
        .max_by_key(|d| d.status.as_ref().unwrap().free_space)
        .ok_or_else(|| "No suitable replacement disk found".into())
}

async fn update_rebuild_phase(
    ctx: &Context,
    volume_id: &str,
    phase: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    spdk_volumes
        .patch(volume_id, &PatchParams::default(), &Patch::Merge(json!({
            "spec": {
                "rebuild_in_progress": {
                    "phase": phase
                }
            }
        })))
        .await?;
    Ok(())
}

async fn handle_ongoing_rebuild(
    _spdk_volume: &SpdkVolume,
    _ctx: &Context,
    _rebuild_state: &ReplicationState,
) -> Result<(), kube::Error> {
    // Monitor ongoing rebuild progress
    Ok(())
}

async fn health_monitor_task(ctx: Arc<Context>) {
    let mut interval = interval(Duration::from_secs(ctx.health_interval));
    
    loop {
        interval.tick().await;
        
        if let Err(e) = perform_periodic_health_check(&ctx).await {
            eprintln!("Health check failed: {}", e);
        }
    }
}

async fn perform_periodic_health_check(ctx: &Context) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    let volumes = spdk_volumes.list(&ListParams::default()).await?;
    
    for volume in volumes {
        let patch = json!({
            "metadata": {
                "annotations": {
                    "spdk.io/last-health-check": Utc::now().to_rfc3339()
                }
            }
        });
        
        spdk_volumes
            .patch(&volume.spec.volume_id, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .ok();
    }
    
    Ok(())
}

fn error_policy(_error: &kube::Error, _ctx: Arc<Context>) -> watcher::Action {
    watcher::Action::Requeue(Duration::from_secs(60))
}