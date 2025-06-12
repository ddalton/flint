// Updated controller.rs leveraging SPDK's native RAID1 rebuild capabilities
use kube::{
    Client, Api, runtime::{Controller, watcher, controller::Action},
    api::{PatchParams, Patch, ListParams},
    error::ErrorResponse,
};
use k8s_openapi::api::core::v1::Pod;
use tokio::time::{Duration, interval};
use reqwest::Client as HttpClient;
use serde_json::json;
use chrono::Utc;
use std::env;
use std::sync::Arc;
use spdk_csi_driver::models::*;

use spdk_csi_driver::{SpdkVolume, SpdkDisk, Replica};

struct Context {
    client: Client,
    spdk_rpc_url: String,
    health_interval: u64,
    rebuild_enabled: bool,
    vhost_socket_base_path: String,
    // Removed rebuild tracking - SPDK handles this
}

// Custom error type that is Send + Sync
#[derive(Debug, Clone)]
pub struct ControllerError {
    pub message: String,
}

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ControllerError {}

impl From<Box<dyn std::error::Error>> for ControllerError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        ControllerError {
            message: err.to_string(),
        }
    }
}

impl From<reqwest::Error> for ControllerError {
    fn from(err: reqwest::Error) -> Self {
        ControllerError {
            message: err.to_string(),
        }
    }
}

impl From<serde_json::Error> for ControllerError {
    fn from(err: serde_json::Error) -> Self {
        ControllerError {
            message: err.to_string(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(client.clone(), "default");
    
    let vhost_socket_base_path = env::var("VHOST_SOCKET_PATH")
        .unwrap_or("/var/lib/spdk-csi/sockets".to_string());
    
    tokio::fs::create_dir_all(&vhost_socket_base_path).await?;
    
    let ctx = Arc::new(Context {
        client: client.clone(),
        spdk_rpc_url: env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        health_interval: env::var("HEALTH_CHECK_INTERVAL").unwrap_or("30".to_string()).parse().unwrap_or(30),
        rebuild_enabled: env::var("REBUILD_ENABLED").unwrap_or("true".to_string()).parse().unwrap_or(true),
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

    // Fix: Use for_each instead of await, and fix reconcile function signature
    let controller = Controller::new(spdk_volumes, watcher::Config::default())
        .run(reconcile, error_policy, ctx);
    
    // Run the controller stream
    use futures::stream::StreamExt;
    controller.for_each(|res| async move {
        match res {
            Ok((obj_ref, action)) => {
                println!("Reconciled {}: {:?}", obj_ref.name, action);
            }
            Err(e) => {
                eprintln!("Controller error: {}", e);
            }
        }
    }).await;

    Ok(())
}

async fn reconcile(spdk_volume: Arc<SpdkVolume>, ctx: Arc<Context>) -> Result<Action, kube::Error> {
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), "default");
    let volume_id = spdk_volume.spec.volume_id.clone();
    let mut status = spdk_volume.status.clone().unwrap_or_default();
    let _spec_update_needed = false;
    let mut status_update_needed = false;

    // --- Health Check Logic ---
    if spdk_volume.spec.num_replicas > 1 {
        // --- Multi-Replica (RAID1) Volume Health Check ---
        let raid_status = get_raid_status(&ctx, &volume_id).await
            .map_err(|e| kube::Error::Api(ErrorResponse {
                status: "Failure".to_string(),
                message: format!("Failed to get RAID status: {}", e),
                reason: "SPDKError".to_string(),
                code: 500,
            }))?;

        if let Some(ref raid_info) = raid_status {
            status.raid_status = Some(raid_info.clone());
            status.state = match raid_info.state.as_str() {
                "online" => "Healthy".to_string(),
                "degraded" => "Degraded".to_string(),
                "failed" | "broken" => "Failed".to_string(),
                _ => raid_info.state.clone(),
            };
            status.degraded = raid_info.state == "degraded";
            
            let failed_replicas: Vec<usize> = raid_info.base_bdevs_list.iter()
                .filter(|m| m.state == "failed")
                .map(|m| m.slot as usize)
                .collect();
            
            if !failed_replicas.is_empty() {
                 status.failed_replicas = failed_replicas;
            }

            status_update_needed = true;
        }

        if ctx.rebuild_enabled && !status.failed_replicas.is_empty() {
            for &failed_index in &status.failed_replicas {
                if let Err(e) = handle_failed_replica_with_spdk(&spdk_volume, &ctx, failed_index, &spdk_disks).await {
                    eprintln!("Failed to handle failed replica {} for volume {}: {}", failed_index, volume_id, e);
                    status.state = "RebuildFailed".to_string();
                }
            }
        }

    } else {
        // --- Single-Replica (Lvol) Volume Health Check ---
        if let Some(replica) = spdk_volume.spec.replicas.first() {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let bdev_name = format!("{}/{}", lvs_name, lvol_uuid);

                match get_lvol_status(&ctx, &bdev_name).await {
                    Ok(lvol_status) => {
                        let current_state_is_healthy = status.state == "Healthy";
                        if lvol_status.is_healthy && !current_state_is_healthy {
                            status.state = "Healthy".to_string();
                            status.degraded = false;
                            status_update_needed = true;
                        } else if !lvol_status.is_healthy && current_state_is_healthy {
                            status.state = "Failed".to_string();
                            status.degraded = true;
                            eprintln!("Lvol {} is unhealthy: {:?}", bdev_name, lvol_status.error_reason);
                            status_update_needed = true;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error checking lvol status for {}: {}", bdev_name, e);
                    }
                }
            }
        }
    }

    // --- Manage Vhost Controllers ---
    manage_vhost_controllers(&spdk_volume, &ctx).await
        .map_err(|e| kube::Error::Api(ErrorResponse {
            status: "Failure".to_string(),
            message: format!("Vhost management error: {}", e),
            reason: "VHostError".to_string(),
            code: 500,
        }))?;
    
    // --- Update Status if Changed ---
    let current_time = Utc::now().to_rfc3339();
    if status.last_checked != current_time {
        status.last_checked = current_time;
        status_update_needed = true;
    }

    if status_update_needed {
        let patch = json!({ "status": status });
        spdk_volumes.patch_status(&volume_id, &PatchParams::default(), &Patch::Merge(patch)).await?;
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

// New function to get RAID status directly from SPDK
async fn get_raid_status(
    ctx: &Context,
    volume_id: &str,
) -> Result<Option<RaidStatus>, ControllerError> {
    let http_client = HttpClient::new();
    
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_get_bdevs",
            "params": {
                "category": "all"
            }
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(None);
    }

    let raid_info: serde_json::Value = response.json().await?;
    
    if let Some(raid_bdevs) = raid_info["result"].as_array() {
        for raid_bdev in raid_bdevs {
            if let Some(name) = raid_bdev["name"].as_str() {
                if name == volume_id {
                    return Ok(Some(RaidStatus::from_spdk_response(raid_bdev)?));
                }
            }
        }
    }
    
    Ok(None)
}

// Updated function to handle failed replicas using SPDK's native RAID capabilities
async fn handle_failed_replica_with_spdk(
    spdk_volume: &SpdkVolume,
    ctx: &Context,
    failed_replica_index: usize,
    spdk_disks: &Api<SpdkDisk>,
) -> Result<(), ControllerError> {
    let volume_id = &spdk_volume.spec.volume_id;
    
    // Find a suitable replacement disk
    let replacement_disk = find_replacement_disk(
        spdk_volume,
        failed_replica_index,
        spdk_disks,
    ).await?;

    // Create new lvol on replacement disk
    let new_lvol_bdev = create_replacement_lvol(
        ctx,
        &replacement_disk,
        spdk_volume.spec.size_bytes,
        volume_id,
    ).await?;

    // Use SPDK's native RAID member replacement
    replace_raid_member_with_spdk(
        ctx,
        volume_id,
        failed_replica_index,
        &new_lvol_bdev,
    ).await?;

    // Update the SpdkVolume CRD with new replica information
    update_replica_after_replacement(
        ctx,
        spdk_volume,
        failed_replica_index,
        replacement_disk,
        new_lvol_bdev,
    ).await?;

    println!("Successfully initiated SPDK native rebuild for volume {} replica {}", 
             volume_id, failed_replica_index);

    Ok(())
}

async fn replace_raid_member_with_spdk(
    ctx: &Context,
    volume_id: &str,
    failed_member_slot: usize,
    new_bdev_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();

    // Remove failed member
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_remove_base_bdev",
            "params": {
                "name": volume_id,
                "slot": failed_member_slot
            }
        }))
        .send()
        .await?;

    // Add replacement member - SPDK will automatically start rebuild
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_add_base_bdev",
            "params": {
                "name": volume_id,
                "base_bdev": new_bdev_name,
                "slot": failed_member_slot
            }
        }))
        .send()
        .await?;

    // Enable automatic rebuild if not already enabled
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_start_rebuild",
            "params": {
                "name": volume_id,
                "slot": failed_member_slot
            }
        }))
        .send()
        .await
        .ok(); // This might fail if rebuild auto-starts, which is fine

    Ok(())
}

/// Finds the RPC URL for the node_agent pod on a given node.
/// This is a standalone function for use in the volume controller.
async fn get_rpc_url_for_node_in_controller(
    client: &Client,
    node_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    // Assumes node_agent pods are labeled with 'app=spdk-node-agent'.
    let pods_api: Api<Pod> = Api::all(client.clone());
    let lp = ListParams::default().labels("app=spdk-node-agent");

    for pod in pods_api.list(&lp).await? {
        if pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(node_name) {
            if let Some(pod_ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref()) {
                return Ok(format!("http://{}:5260", pod_ip));
            }
        }
    }

    Err(format!("Could not find spdk-node-agent pod on node '{}'", node_name).into())
}

/// Creates a replacement lvol on the specified disk by connecting to the correct node_agent pod.
async fn create_replacement_lvol(
    ctx: &Context,
    replacement_disk: &SpdkDisk,
    size_bytes: i64,
    volume_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let lvs_name = format!("lvs_{}", replacement_disk.metadata.name.as_ref().unwrap());
    let lvol_name = format!("vol_{}_{}", volume_id, Utc::now().timestamp());

    // Discover the RPC URL for the target node.
    let target_node_name = &replacement_disk.spec.node;
    let rpc_url = get_rpc_url_for_node_in_controller(&ctx.client, target_node_name).await?;
    println!("Creating replacement lvol on node '{}' via URL '{}'", target_node_name, rpc_url);

    // Create the new lvol by calling the correct node's SPDK instance.
    let res = http_client
        .post(&rpc_url) // Use the discovered URL
        .json(&json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size": size_bytes,
                "thin_provision": false,
                "clear_method": "write_zeroes"
            }
        }))
        .send()
        .await?;
    
    if !res.status().is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err(format!("Failed to create replacement lvol: {}", err_text).into());
    }

    Ok(format!("{}/{}", lvs_name, lvol_name))
}

async fn update_replica_after_replacement(
    ctx: &Context,
    spdk_volume: &SpdkVolume,
    failed_replica_index: usize,
    replacement_disk: SpdkDisk,
    new_lvol_bdev: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    let lvol_uuid = get_lvol_uuid(&ctx, &new_lvol_bdev).await?;
    
    let mut new_spec = spdk_volume.spec.clone();
    new_spec.replicas[failed_replica_index] = Replica {
        node: replacement_disk.spec.node.clone(),
        replica_type: "lvol".to_string(),
        pcie_addr: Some(replacement_disk.spec.pcie_addr.clone()),
        disk_ref: replacement_disk.metadata.name.clone().unwrap_or_default(),
        lvol_uuid: Some(lvol_uuid),
        nqn: Some(format!("nqn.2025-05.io.spdk:lvol-{}", new_lvol_bdev.replace('/', "-"))),
        health_status: ReplicaHealth::Rebuilding, // Will be updated by SPDK status
        last_io_timestamp: Some(Utc::now().to_rfc3339()),
        write_sequence: 0,
        local_pod_scheduled: false,
        vhost_socket: None,
        raid_member_index: failed_replica_index,
        raid_member_state: RaidMemberState::Rebuilding,
        ..Default::default()
    };
    
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    spdk_volumes
        .patch(volume_id, &PatchParams::default(), &Patch::Merge(json!({
            "spec": new_spec
        })))
        .await?;

    // Update disk status
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

// Keep existing helper functions with minimal changes...
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

// Keep existing vhost management functions unchanged...
async fn manage_vhost_controllers(
    spdk_volume: &SpdkVolume,
    ctx: &Context,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume_id = &spdk_volume.spec.volume_id;
    
    let controller_exists = check_vhost_controller_exists(ctx, volume_id).await?;
    let socket_path = get_vhost_socket_path(ctx, volume_id);
    
    let has_local_replicas_with_pods = spdk_volume.spec.replicas.iter()
        .any(|r| r.replica_type == "lvol" && r.local_pod_scheduled);
    
    if has_local_replicas_with_pods && !controller_exists {
        create_vhost_controller_for_volume(ctx, volume_id, &socket_path).await?;
        
        let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
        spdk_volumes
            .patch(volume_id, &PatchParams::default(), &Patch::Merge(json!({
                "spec": {
                    "vhost_socket": socket_path
                }
            })))
            .await?;
    } else if !has_local_replicas_with_pods && controller_exists {
        delete_vhost_controller(ctx, volume_id).await?;
    }
    
    Ok(())
}

async fn create_vhost_controller_for_volume(
    ctx: &Context,
    volume_id: &str,
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let controller_name = format!("vhost_{}", volume_id);
    
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_create_nvme_controller",
            "params": {
                "ctrlr": controller_name,
                "io_queues": 4,
                "cpumask": "0x1",
                "max_namespaces": 32
            }
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
        let error_text = response.text().await?;
        return Err(format!("Failed to create vhost-nvme controller: {}", error_text).into());
    }
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_nvme_controller_add_ns",
            "params": {
                "ctrlr": controller_name,
                "bdev_name": volume_id
            }
        }))
        .send()
        .await?;
    
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
    
    println!("Created vhost-nvme controller for volume: {}", volume_id);
    Ok(())
}

async fn delete_vhost_controller(
    ctx: &Context,
    volume_id: &str,
) -> Result<(), ControllerError> {
    let http_client = HttpClient::new();
    let controller_name = format!("vhost_{}", volume_id);
    let socket_path = get_vhost_socket_path(ctx, volume_id);
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_nvme_controller_remove_ns",
            "params": { 
                "ctrlr": controller_name,
                "nsid": 1
            }
        }))
        .send()
        .await
        .ok();
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_stop_controller",
            "params": { "ctrlr": controller_name }
        }))
        .send()
        .await
        .ok();
    
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_delete_controller",
            "params": { "ctrlr": controller_name }
        }))
        .send()
        .await
        .ok();
    
    tokio::fs::remove_file(&socket_path).await.ok();
    
    println!("Deleted vhost-nvme controller for volume: {}", volume_id);
    Ok(())
}

async fn check_vhost_controller_exists(ctx: &Context, volume_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let http_client = HttpClient::new();
    let controller_name = format!("vhost_{}", volume_id);

    // Use get_vhost_controllers RPC
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "get_vhost_controllers"
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

fn get_vhost_socket_path(ctx: &Context, volume_id: &str) -> String {
    format!("{}/vhost_{}.sock", ctx.vhost_socket_base_path, volume_id)
}

async fn vhost_cleanup_task(ctx: Arc<Context>) {
    let mut interval = interval(Duration::from_secs(300));
    
    loop {
        interval.tick().await;
        
        if let Err(e) = cleanup_orphaned_vhost_controllers(&ctx).await {
            eprintln!("Vhost cleanup failed: {}", e);
        }
    }
}

async fn cleanup_orphaned_vhost_controllers(
    ctx: &Context,
) -> Result<(), ControllerError> {
    let http_client = HttpClient::new();
    
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "vhost_get_controllers"
        }))
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Ok(());
    }
    
    let controllers: serde_json::Value = response.json().await?;
    if let Some(controller_list) = controllers["result"].as_array() {
        let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
        let volumes = spdk_volumes.list(&ListParams::default()).await
            .map_err(|e| ControllerError { message: format!("Failed to list volumes: {}", e) })?;
        
        for controller in controller_list {
            if let Some(controller_name) = controller["ctrlr"].as_str() {
                if controller_name.starts_with("vhost_") {
                    let volume_id = controller_name.strip_prefix("vhost_").unwrap();
                    
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

async fn health_monitor_task(ctx: Arc<Context>) {
    let mut interval = interval(Duration::from_secs(ctx.health_interval));
    
    loop {
        interval.tick().await;
        
        if let Err(e) = perform_periodic_health_check(&ctx).await {
            eprintln!("Health check failed: {}", e);
        }
    }
}

async fn perform_periodic_health_check(ctx: &Context) -> Result<(), ControllerError> {
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), "default");
    let volumes = spdk_volumes.list(&ListParams::default()).await
        .map_err(|e| ControllerError { message: format!("Failed to list volumes: {}", e) })?;
    
    for volume in volumes {
        // Get updated RAID status from SPDK
        if let Ok(Some(raid_status)) = get_raid_status(ctx, &volume.spec.volume_id).await {
            let mut needs_update = false;
            let mut status = volume.status.unwrap_or_default();
            
            // Check if RAID status has changed
            let status_changed = match &status.raid_status {
                Some(existing) => {
                    existing.state != raid_status.state ||
                    existing.num_base_bdevs_operational != raid_status.num_base_bdevs_operational ||
                    existing.rebuild_info.is_some() != raid_status.rebuild_info.is_some()
                }
                None => true,
            };
            
            if status_changed {
                status.raid_status = Some(raid_status.clone());
                status.state = match raid_status.state.as_str() {
                    "online" => "Healthy".to_string(),
                    "degraded" => "Degraded".to_string(),
                    "failed" | "broken" => "Failed".to_string(),
                    _ => raid_status.state.clone(),
                };
                status.degraded = raid_status.state == "degraded";
                status.last_checked = Utc::now().to_rfc3339();
                needs_update = true;
            }
            
            if needs_update {
                spdk_volumes
                    .patch_status(&volume.spec.volume_id, &PatchParams::default(), &Patch::Merge(json!({
                        "status": status
                    })))
                    .await
                    .ok();
            }
        }
        
        // Update health check timestamp
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

/// Gets the real-time status of a single lvol bdev from the SPDK RPC server.
async fn get_lvol_status(
    ctx: &Context,
    bdev_name: &str,
) -> Result<LvolStatus, ControllerError> {
    let http_client = HttpClient::new();
    
    let response = http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_get_bdevs",
            "params": { "name": bdev_name }
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(LvolStatus {
            name: bdev_name.to_string(),
            is_healthy: false,
            error_reason: Some(format!("RPC failed with status: {}", response.status())),
        });
    }

    let bdev_info: serde_json::Value = response.json().await?;
    
    if let Some(bdev) = bdev_info.get("result").and_then(|r| r.as_array()).and_then(|a| a.get(0)) {
        // A bdev is considered healthy if it supports read and write I/O.
        let is_healthy = bdev["supported_io_types"]["read"].as_bool().unwrap_or(false) &&
                         bdev["supported_io_types"]["write"].as_bool().unwrap_or(false);

        Ok(LvolStatus {
            name: bdev_name.to_string(),
            is_healthy,
            error_reason: if is_healthy { None } else { Some("Bdev does not support read/write I/O".to_string()) },
        })
    } else {
        // The bdev was not found in the SPDK instance.
        Ok(LvolStatus {
            name: bdev_name.to_string(),
            is_healthy: false,
            error_reason: Some("Bdev not found in SPDK".to_string()),
        })
    }
}

fn error_policy(obj: Arc<SpdkVolume>, error: &kube::Error, _ctx: Arc<Context>) -> Action {
    match error {
        kube::Error::Api(api_error) if api_error.code == 404 => {
            Action::await_change()
        }
        _ => {
            eprintln!("Error reconciling volume {}: {}", 
                      obj.spec.volume_id, error);
            Action::requeue(Duration::from_secs(60))
        }
    }
}
