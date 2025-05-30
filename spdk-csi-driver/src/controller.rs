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
        pub blob_id: Option<String>, // SPDK blobstore blob ID
        pub rebuild_in_progress: Option<ReplicationState>,
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
        pub blob_id: Option<String>, // Individual replica blob ID
        pub health_status: ReplicaHealth,
        pub last_io_timestamp: Option<String>,
        pub write_sequence: u64, // For write ordering
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct ReplicationState {
        pub target_replica_index: usize,
        pub source_replica_index: usize,
        pub snapshot_id: String,
        pub copy_progress: f64,
        pub phase: String, // "snapshot", "copy", "sync", "finalize"
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
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Default)]
    pub struct SpdkDiskStatus {
        pub free_space: i64,
        pub healthy: bool,
        pub last_checked: String,
        pub blob_count: u32,
        pub blobstore_initialized: bool,
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
    disk_discovery_enabled: bool,
    disk_discovery_interval: u64,
    active_rebuilds: Arc<RwLock<HashMap<String, ReplicationState>>>,
    write_sequence_counter: Arc<Mutex<u64>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(client.clone(), "default");
    
    let ctx = Arc::new(Context {
        client: client.clone(),
        spdk_rpc_url: env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        health_interval: env::var("HEALTH_CHECK_INTERVAL").unwrap_or("30".to_string()).parse().unwrap_or(30),
        rebuild_enabled: env::var("REBUILD_ENABLED").unwrap_or("true".to_string()).parse().unwrap_or(true),
        max_retries: env::var("REBUILD_MAX_RETRIES").unwrap_or("3".to_string()).parse().unwrap_or(3),
        snapshot_retention: env::var("SNAPSHOT_RETENTION").unwrap_or("1h".to_string()),
        write_barrier_timeout: env::var("WRITE_BARRIER_TIMEOUT").unwrap_or("30".to_string()).parse().unwrap_or(30),
        disk_discovery_enabled: env::var("DISK_DISCOVERY_ENABLED").unwrap_or("true".to_string()).parse().unwrap_or(true),
        disk_discovery_interval: env::var("DISK_DISCOVERY_INTERVAL").unwrap_or("300".to_string()).parse().unwrap_or(300),
        active_rebuilds: Arc::new(RwLock::new(HashMap::new())),
        write_sequence_counter: Arc::new(Mutex::new(0)),
    });

    // Start health monitoring task
    let health_ctx = ctx.clone();
    tokio::spawn(async move {
        health_monitor_task(health_ctx).await;
    });

    // Note: Disk discovery is now handled by the node DaemonSet
    // Each node agent discovers local disks and creates/updates SpdkDisk resources
    // The controller only manages volume operations and rebuilds

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

async fn check_replica_health(
    spdk_volume: &SpdkVolume,
    ctx: &Context,
) -> Result<Vec<ReplicaHealth>, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let mut health_results = Vec::new();

    for (i, replica) in spdk_volume.spec.replicas.iter().enumerate() {
        // Check blobstore health via SPDK RPC
        let blob_id = replica.blob_id.as_ref()
            .ok_or("Missing blob ID for replica")?;
        
        let response = http_client
            .post(&ctx.spdk_rpc_url)
            .json(&json!({
                "method": "blob_get_info",
                "params": {
                    "blobstore_name": format!("bs_{}", replica.disk_ref),
                    "blob_id": blob_id
                }
            }))
            .send()
            .await;

        let health = match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    // Additional I/O health check
                    if perform_io_health_check(&http_client, &ctx.spdk_rpc_url, blob_id).await? {
                        ReplicaHealth::Healthy
                    } else {
                        ReplicaHealth::Degraded
                    }
                } else {
                    ReplicaHealth::Failed
                }
            }
            Err(_) => ReplicaHealth::Failed,
        };

        health_results.push(health);
    }

    Ok(health_results)
}

async fn perform_io_health_check(
    http_client: &HttpClient,
    spdk_rpc_url: &str,
    blob_id: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Perform a small test read to verify I/O functionality
    let test_data = vec![0u8; 4096]; // 4KB test
    
    let response = http_client
        .post(spdk_rpc_url)
        .json(&json!({
            "method": "blob_io_read",
            "params": {
                "blob_id": blob_id,
                "offset": 0,
                "length": 4096
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

    // Create blobstore snapshot
    let snapshot_id = create_blobstore_snapshot(
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

async fn create_blobstore_snapshot(
    ctx: &Context,
    source_replica: &Replica,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let snapshot_name = format!("snap_{}_{}", 
        source_replica.disk_ref, 
        Utc::now().timestamp()
    );
    
    // Get the source bdev name for the replica
    let source_bdev_name = if let Some(blob_id) = &source_replica.blob_id {
        format!("lvol_{}", blob_id)
    } else if let Some(pcie_addr) = &source_replica.pcie_addr {
        format!("Nvme_{}n1", pcie_addr.replace(":", "_"))
    } else {
        return Err("No valid bdev identifier found for source replica".into());
    };

    // Create snapshot using SPDK lvol snapshot functionality
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_snapshot",
            "params": {
                "lvol_name": source_bdev_name,
                "snapshot_name": snapshot_name
            }
        }))
        .send()
        .await?;

    Ok(snapshot_name)
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
    
    // Phase 2: Create snapshot of healthy replica bdev
    update_rebuild_phase(&ctx, volume_id, "snapshot").await?;
    let source_bdev_name = get_replica_bdev_name(&spdk_volume, rebuild_state.source_replica_index)?;
    let snapshot_name = create_bdev_snapshot(&ctx, &source_bdev_name, volume_id).await?;
    
    // Phase 3: Initialize target lvol store and create thin provisioned lvol
    update_rebuild_phase(&ctx, volume_id, "provision").await?;
    let target_lvs_name = initialize_target_lvol_store(&ctx, &replacement_disk).await?;
    let target_lvol_name = create_thin_provisioned_lvol(&ctx, &target_lvs_name, &snapshot_name, volume_id).await?;
    
    // Phase 4: Add new lvol bdev to RAID-1 configuration (while writes still paused)
    update_rebuild_phase(&ctx, volume_id, "integrate").await?;
    add_bdev_to_raid(&ctx, volume_id, &target_lvol_name, rebuild_state.target_replica_index).await?;
    
    // Phase 5: Unpause writes - RAID will handle synchronization automatically
    update_rebuild_phase(&ctx, volume_id, "unpause").await?;
    unpause_raid_writes(&ctx, volume_id).await?;
    
    // Phase 6: Inflate the thin provisioned lvol (make it independent) - background operation
    update_rebuild_phase(&ctx, volume_id, "inflate").await?;
    inflate_thin_provisioned_lvol(&ctx, &target_lvol_name).await?;
    
    // Phase 7: Finalize and cleanup
    update_rebuild_phase(&ctx, volume_id, "finalize").await?;
    finalize_rebuild_with_lvol(&ctx, &spdk_volume, rebuild_state, replacement_disk, target_lvol_name, snapshot_name).await?;

    Ok(())
}

async fn pause_raid_writes(
    ctx: &Context,
    volume_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // Pause writes on the RAID bdev to ensure consistency during snapshot
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

    // Wait a brief moment for in-flight I/Os to complete
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    Ok(())
}

async fn unpause_raid_writes(
    ctx: &Context,
    volume_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // Resume writes on the RAID bdev
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

fn get_replica_bdev_name(
    spdk_volume: &SpdkVolume,
    replica_index: usize,
) -> Result<String, Box<dyn std::error::Error>> {
    let replica = spdk_volume.spec.replicas.get(replica_index)
        .ok_or("Invalid replica index")?;
    
    // Construct bdev name based on replica type
    let bdev_name = if let Some(blob_id) = &replica.blob_id {
        format!("lvol_{}", blob_id)
    } else if let Some(pcie_addr) = &replica.pcie_addr {
        format!("Nvme_{}n1", pcie_addr.replace(":", "_"))
    } else {
        return Err("No valid bdev identifier found for replica".into());
    };
    
    Ok(bdev_name)
}

async fn create_bdev_snapshot(
    ctx: &Context,
    source_bdev_name: &str,
    volume_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let snapshot_name = format!("snap_{}_{}", volume_id, Utc::now().timestamp());
    
    // Create a snapshot of the source bdev
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_snapshot",
            "params": {
                "lvol_name": source_bdev_name,
                "snapshot_name": snapshot_name
            }
        }))
        .send()
        .await?;

    Ok(snapshot_name)
}

async fn initialize_target_lvol_store(
    ctx: &Context,
    replacement_disk: &SpdkDisk,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let lvs_name = format!("lvs_{}", replacement_disk.metadata.name.as_ref().unwrap());
    
    // Check if lvol store already exists
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
        // Create new lvol store on the replacement disk
        http_client
            .post(&ctx.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_lvol_create_lvstore",
                "params": {
                    "bdev_name": replacement_disk.spec.pcie_addr,
                    "lvs_name": lvs_name,
                    "cluster_sz": 65536 // 64KB clusters for good performance
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
    let lvol_name = format!("lvol_{}_{}", volume_id, Utc::now().timestamp());
    
    // Create thin provisioned logical volume using snapshot as base
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size": 0, // Size will be inherited from snapshot
                "thin_provision": true,
                "clone_snapshot_name": snapshot_name
            }
        }))
        .send()
        .await?;

    // The full bdev name includes the lvs prefix
    Ok(format!("{}/{}", lvs_name, lvol_name))
}

async fn add_bdev_to_raid(
    ctx: &Context,
    volume_id: &str,
    target_lvol_name: &str,
    failed_replica_index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    
    // First, remove the failed bdev from RAID if it's still there
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
        .ok(); // Ignore errors in case it's already removed

    // Add the new lvol bdev to the RAID configuration
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

    // Wait for RAID to stabilize
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    // Verify RAID health
    let status_response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_get_bdevs",
            "params": {
                "category": "all"
            }
        }))
        .send()
        .await?;

    let raid_status: serde_json::Value = status_response.json().await?;
    let raid_bdev = raid_status["result"]
        .as_array()
        .and_then(|arr| arr.iter().find(|bdev| bdev["name"] == volume_id))
        .ok_or("RAID bdev not found after rebuild")?;

    let state = raid_bdev["state"].as_str().unwrap_or("unknown");
    if state != "online" {
        return Err(format!("RAID bdev is in unexpected state: {}", state).into());
    }

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
    
    // Delete the snapshot bdev
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

async fn finalize_rebuild_with_lvol(
    ctx: &Context,
    spdk_volume: &SpdkVolume,
    rebuild_state: &ReplicationState,
    replacement_disk: SpdkDisk,
    target_lvol_name: String,
    snapshot_name: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    
    // Extract the lvol UUID for CRD update
    let lvol_uuid = get_lvol_uuid(&ctx, &target_lvol_name).await?;
    
    // Update volume spec - replace failed replica
    let mut new_spec = spdk_volume.spec.clone();
    new_spec.replicas[rebuild_state.target_replica_index] = Replica {
        node: replacement_disk.spec.node.clone(),
        replica_type: "lvol".to_string(),
        pcie_addr: Some(replacement_disk.spec.pcie_addr.clone()),
        disk_ref: replacement_disk.metadata.name.clone().unwrap_or_default(),
        blob_id: Some(lvol_uuid),
        nqn: Some(format!("nqn.2025-05.io.spdk:lvol-{}", target_lvol_name.replace('/', "-"))),
        health_status: ReplicaHealth::Healthy,
        last_io_timestamp: Some(Utc::now().to_rfc3339()),
        write_sequence: 0, // Reset write sequence for new replica
        local_pod_scheduled: false,
        ..Default::default()
    };
    new_spec.rebuild_in_progress = None;
    
    // Update Kubernetes resources
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
    
    // Update SpdkDisk status to reflect actual usage after inflation
    let disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let disk_name = replacement_disk.metadata.name.clone().unwrap_or_default();
    let mut disk_status = replacement_disk.status.unwrap_or_default();
    
    // After inflation, the lvol now uses the full space
    disk_status.free_space -= spdk_volume.spec.size_bytes;
    disk_status.blob_count += 1;
    
    disks
        .patch_status(&disk_name, &PatchParams::default(), &Patch::Merge(json!({
            "status": disk_status
        })))
        .await?;
    
    // Cleanup snapshot after successful rebuild
    cleanup_snapshot(&ctx, &snapshot_name).await?;
    
    // Remove from active rebuilds tracking
    ctx.active_rebuilds.write().await.remove(volume_id);
    
    println!("Successfully completed rebuild for volume {} with inflated lvol replica", volume_id);
    
    Ok(())
}

// Helper functions
async fn select_best_source_replica(
    spdk_volume: &SpdkVolume,
    active_replicas: &[usize],
) -> Result<usize, Box<dyn std::error::Error>> {
    // Prefer local replicas, then by last I/O timestamp
    for &index in active_replicas {
        if spdk_volume.spec.replicas[index].local_pod_scheduled {
            return Ok(index);
        }
    }
    Ok(active_replicas[0]) // Fallback to first active
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
        .filter(|d| d.status.as_ref().map(|s| s.healthy && s.free_space >= required_capacity).unwrap_or(false))
        .filter(|d| !used_nodes.contains(&d.spec.node))
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
    // This would check the current phase and progress
    Ok(())
}

async fn health_monitor_task(ctx: Arc<Context>) {
    let mut interval = interval(Duration::from_secs(ctx.health_interval));
    
    loop {
        interval.tick().await;
        
        // Perform periodic health checks on all volumes
        if let Err(e) = perform_periodic_health_check(&ctx).await {
            eprintln!("Health check failed: {}", e);
        }
    }
}

async fn disk_discovery_task(ctx: Arc<Context>) {
    let mut interval = interval(Duration::from_secs(ctx.disk_discovery_interval));
    
    loop {
        interval.tick().await;
        
        if let Err(e) = discover_and_update_disks(&ctx).await {
            eprintln!("Disk discovery failed: {}", e);
        }
    }
}

async fn discover_and_update_disks(ctx: &Context) -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting automatic disk discovery...");
    
    // Get all Kubernetes nodes
    let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(ctx.client.clone());
    let node_list = nodes.list(&ListParams::default()).await?;
    
    for node in node_list.items {
        let node_name = node.metadata.name.as_ref()
            .ok_or("Node missing name")?;
        
        // Skip if node is not ready
        if !is_node_ready(&node) {
            continue;
        }
        
        // Discover NVMe devices on this node
        if let Err(e) = discover_node_nvme_devices(ctx, node_name).await {
            eprintln!("Failed to discover devices on node {}: {}", node_name, e);
        }
    }
    
    // Clean up stale disk resources
    cleanup_stale_disks(ctx).await?;
    
    println!("Disk discovery completed");
    Ok(())
}

fn is_node_ready(node: &k8s_openapi::api::core::v1::Node) -> bool {
    if let Some(status) = &node.status {
        if let Some(conditions) = &status.conditions {
            return conditions.iter().any(|condition| {
                condition.type_ == "Ready" && condition.status == "True"
            });
        }
    }
    false
}

async fn discover_node_nvme_devices(ctx: &Context, node_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Query the node for NVMe devices through a DaemonSet or node agent
    let discovered_devices = query_node_nvme_devices(ctx, node_name).await?;
    
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    
    for device in discovered_devices {
        let disk_name = format!("{}-{}", node_name, device.controller_id);
        
        // Check if SpdkDisk already exists
        match spdk_disks.get(&disk_name).await {
            Ok(mut existing_disk) => {
                // Update existing disk if needed
                if should_update_disk(&existing_disk, &device) {
                    update_existing_disk(ctx, &mut existing_disk, &device).await?;
                }
            }
            Err(_) => {
                // Create new SpdkDisk resource
                create_new_disk_resource(ctx, node_name, &device).await?;
            }
        }
    }
    
    Ok(())
}

async fn query_node_nvme_devices(ctx: &Context, node_name: &str) -> Result<Vec<NvmeDevice>, Box<dyn std::error::Error>> {
    // In a real implementation, this would query the SPDK daemon running on the node
    // For now, simulate discovery via SPDK RPC calls to the node's SPDK instance
    
    let node_spdk_url = format!("http://{}:5260", get_node_ip_internal(node_name).await?);
    let http_client = HttpClient::new();
    
    // Get all NVMe controllers
    let response = http_client
        .post(&node_spdk_url)
        .json(&json!({
            "method": "bdev_nvme_get_controllers"
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Ok(Vec::new()); // Node might not have SPDK running yet
    }
    
    let controllers: serde_json::Value = response.json().await?;
    let mut devices = Vec::new();
    
    if let Some(controller_list) = controllers["result"].as_array() {
        for controller in controller_list {
            if let Some(device) = parse_nvme_controller(controller) {
                devices.push(device);
            }
        }
    }
    
    Ok(devices)
}

fn parse_nvme_controller(controller: &serde_json::Value) -> Option<NvmeDevice> {
    let name = controller["name"].as_str()?;
    let pcie_addr = controller["trid"]["traddr"].as_str()?;
    
    // Get capacity from the first namespace
    let namespaces = controller["namespaces"].as_array()?;
    let capacity = if let Some(ns) = namespaces.first() {
        ns["size"].as_u64().unwrap_or(0) as i64
    } else {
        0
    };
    
    Some(NvmeDevice {
        controller_id: name.to_string(),
        pcie_addr: pcie_addr.to_string(),
        capacity,
        model: controller["model"].as_str().unwrap_or("Unknown").to_string(),
        serial: controller["serial"].as_str().unwrap_or("Unknown").to_string(),
        firmware_version: controller["fw_rev"].as_str().unwrap_or("Unknown").to_string(),
    })
}

async fn create_new_disk_resource(ctx: &Context, node_name: &str, device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error>> {
    let disk_name = format!("{}-{}", node_name, device.controller_id);
    
    let spdk_disk = SpdkDisk::new(&disk_name, SpdkDiskSpec {
        node: node_name.to_string(),
        pcie_addr: device.pcie_addr.clone(),
        capacity: device.capacity,
        blobstore_uuid: None,
        nvme_controller_id: Some(device.controller_id.clone()),
    });
    
    // Initialize status
    let mut spdk_disk_with_status = spdk_disk;
    spdk_disk_with_status.status = Some(SpdkDiskStatus {
        free_space: device.capacity,
        healthy: true,
        last_checked: Utc::now().to_rfc3339(),
        blob_count: 0,
        blobstore_initialized: false,
        io_stats: IoStatistics::default(),
    });
    
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    spdk_disks.create(&PostParams::default(), &spdk_disk_with_status).await?;
    
    println!("Created SpdkDisk resource: {} for device {} on node {}", 
             disk_name, device.pcie_addr, node_name);
    
    // Initialize blobstore on the device
    initialize_blobstore_on_device(ctx, &spdk_disk_with_status).await?;
    
    Ok(())
}

async fn initialize_blobstore_on_device(ctx: &Context, disk: &SpdkDisk) -> Result<(), Box<dyn std::error::Error>> {
    let node_spdk_url = format!("http://{}:5260", get_node_ip_internal(&disk.spec.node).await?);
    let http_client = HttpClient::new();
    
    let blobstore_name = format!("bs_{}", disk.metadata.name.as_ref().unwrap());
    
    // Create blobstore
    let response = http_client
        .post(&node_spdk_url)
        .json(&json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": disk.spec.pcie_addr,
                "lvs_name": blobstore_name,
                "cluster_sz": 65536
            }
        }))
        .send()
        .await;
    
    match response {
        Ok(resp) => {
            if resp.status().is_success() {
                // Update disk status to mark blobstore as initialized
                update_disk_blobstore_status(ctx, disk, true).await?;
                println!("Initialized blobstore on disk: {}", disk.metadata.name.as_ref().unwrap());
            }
        }
        Err(e) => {
            eprintln!("Failed to initialize blobstore on {}: {}", disk.spec.pcie_addr, e);
        }
    }
    
    Ok(())
}

async fn update_disk_blobstore_status(ctx: &Context, disk: &SpdkDisk, initialized: bool) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let disk_name = disk.metadata.name.as_ref().unwrap();
    
    let mut status = disk.status.clone().unwrap_or_default();
    status.blobstore_initialized = initialized;
    status.last_checked = Utc::now().to_rfc3339();
    
    spdk_disks
        .patch_status(disk_name, &PatchParams::default(), &Patch::Merge(json!({
            "status": status
        })))
        .await?;
    
    Ok(())
}

fn should_update_disk(existing: &SpdkDisk, discovered: &NvmeDevice) -> bool {
    // Update if capacity changed or if blobstore not initialized
    existing.spec.capacity != discovered.capacity ||
    existing.status.as_ref().map(|s| !s.blobstore_initialized).unwrap_or(true)
}

async fn update_existing_disk(ctx: &Context, disk: &mut SpdkDisk, device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let disk_name = disk.metadata.name.as_ref().unwrap();
    
    // Update spec if needed
    if disk.spec.capacity != device.capacity {
        disk.spec.capacity = device.capacity;
        
        spdk_disks
            .patch(disk_name, &PatchParams::default(), &Patch::Merge(json!({
                "spec": disk.spec
            })))
            .await?;
    }
    
    // Initialize blobstore if not done
    if disk.status.as_ref().map(|s| !s.blobstore_initialized).unwrap_or(true) {
        initialize_blobstore_on_device(ctx, disk).await?;
    }
    
    Ok(())
}

async fn cleanup_stale_disks(ctx: &Context) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let disk_list = spdk_disks.list(&ListParams::default()).await?;
    
    for disk in disk_list.items {
        // Check if the node still exists and is ready
        let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(ctx.client.clone());
        match nodes.get(&disk.spec.node).await {
            Ok(node) => {
                if !is_node_ready(&node) {
                    // Node exists but not ready - update disk status
                    update_disk_health_status(ctx, &disk, false).await?;
                }
            }
            Err(_) => {
                // Node no longer exists - mark disk as unhealthy
                update_disk_health_status(ctx, &disk, false).await?;
                println!("Marked disk {} as unhealthy due to missing node {}", 
                        disk.metadata.name.as_ref().unwrap_or("unknown"), disk.spec.node);
            }
        }
    }
    
    Ok(())
}

async fn update_disk_health_status(ctx: &Context, disk: &SpdkDisk, healthy: bool) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let disk_name = disk.metadata.name.as_ref().unwrap();
    
    let mut status = disk.status.clone().unwrap_or_default();
    status.healthy = healthy;
    status.last_checked = Utc::now().to_rfc3339();
    
    spdk_disks
        .patch_status(disk_name, &PatchParams::default(), &Patch::Merge(json!({
            "status": status
        })))
        .await?;
    
    Ok(())
}

async fn perform_periodic_health_check(ctx: &Context) -> Result<(), Box<dyn std::error::Error>> {
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    let volumes = spdk_volumes.list(&ListParams::default()).await?;
    
    for volume in volumes {
        // Trigger reconciliation for each volume
        // This will be handled by the controller loop
    }
    
    Ok(())
}

fn error_policy(_error: &kube::Error, _ctx: Arc<Context>) -> watcher::Action {
    watcher::Action::Requeue(Duration::from_secs(60))
}