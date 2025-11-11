// controller_operator.rs - SPDK Volume Controller Operator
use kube::{
    Client, Api, runtime::{Controller, watcher, controller::Action},
    api::{PatchParams, Patch, ListParams},
};
use k8s_openapi::api::core::v1::Pod;
use tokio::time::Duration;
use reqwest::Client as HttpClient;
use serde_json::json;
use chrono::Utc;
use std::env;
use std::sync::Arc;
use spdk_csi_driver::minimal_models::*;
use warp::Filter;


struct Context {
    client: Client,
    spdk_rpc_url: String,
    health_interval: u64,
    rebuild_enabled: bool,
    target_namespace: String,
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

impl From<Box<dyn std::error::Error + Send + Sync>> for ControllerError {
    fn from(err: Box<dyn std::error::Error + Send + Sync>) -> Self {
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

impl From<Box<dyn std::error::Error>> for ControllerError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        ControllerError {
            message: err.to_string(),
        }
    }
}

/// Get the current pod's namespace from the service account token
async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error>> {
    // Try environment variable first (allows override)
    if let Ok(namespace) = env::var("FLINT_NAMESPACE") {
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
    let client = Client::try_default().await?;
    let target_namespace = get_current_namespace().await?;
    println!("🎯 [OPERATOR] Using namespace for custom resources: {}", target_namespace);
    
    let ctx = Arc::new(Context {
        client: client.clone(),
        spdk_rpc_url: env::var("SPDK_RPC_URL").unwrap_or("http://localhost:5260".to_string()),
        health_interval: env::var("HEALTH_CHECK_INTERVAL")
            .unwrap_or("30".to_string())
            .parse()
            .unwrap_or(30),
        rebuild_enabled: env::var("REBUILD_ENABLED")
            .unwrap_or("true".to_string())
            .parse()
            .unwrap_or(true),
        target_namespace,
    });
    
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(client.clone(), &ctx.target_namespace);

    // Start health server for Kubernetes liveness probes
    tokio::spawn(async move {
        start_health_server().await;
    });

    // Start health monitoring task
    let health_ctx = ctx.clone();
    tokio::spawn(async move {
        health_monitor_task(health_ctx).await;
    });

    // Start the controller
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
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), &ctx.target_namespace);
    let spdk_disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), &ctx.target_namespace);
    let volume_id = &spdk_volume.spec.volume_id;
    let mut status = spdk_volume.status.clone().unwrap_or_default();
    let mut status_update_needed = false;

    // Health check logic based on replica count
    if spdk_volume.spec.num_replicas > 1 {
        // Multi-replica (RAID1) volume health check
        match get_raid_status(&ctx, volume_id).await {
            Ok(Some(raid_info)) => {
                status.raid_status = Some(raid_info.clone());
                status.state = match raid_info.state.as_str() {
                    "online" => "ready".to_string(), // Use CRD-compliant state
                    "degraded" => "degraded".to_string(), // Use CRD-compliant state
                    "failed" | "broken" => "failed".to_string(), // Use CRD-compliant state
                    _ => "degraded".to_string(), // Default to degraded for unknown states
                };
                status.degraded = raid_info.state == "degraded";
                
                let failed_replicas: Vec<usize> = raid_info.base_bdevs_list.iter()
                    .filter(|m| m.state == "failed")
                    .map(|m| m.slot as usize)
                    .collect();
                
                if !failed_replicas.is_empty() {
                    status.failed_replicas = failed_replicas.clone();
                    
                    // Handle failed replicas if rebuild is enabled
                    if ctx.rebuild_enabled {
                        for &failed_index in &failed_replicas {
                            if let Err(e) = handle_failed_replica(&spdk_volume, &ctx, failed_index, &spdk_disks).await {
                                eprintln!("Failed to handle failed replica {} for volume {}: {}", failed_index, volume_id, e);
                                status.state = "failed".to_string();
                            }
                        }
                    }
                }

                status_update_needed = true;
            }
            Ok(None) => {
                // RAID not found, might be single replica or error
                            if status.state != "failed" {
                status.state = "failed".to_string();
                    status_update_needed = true;
                }
            }
            Err(e) => {
                eprintln!("Error getting RAID status for volume {}: {}", volume_id, e);
            }
        }
    } else {
        // Single-replica volume health check
        if let Some(replica) = spdk_volume.spec.replicas.first() {
            if let Some(lvol_uuid) = &replica.lvol_uuid {
                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let bdev_name = format!("{}/{}", lvs_name, lvol_uuid);

                match get_lvol_status(&ctx, &bdev_name).await {
                    Ok(lvol_status) => {
                        let should_be_healthy = lvol_status.is_healthy;
                        let currently_healthy = status.state == "ready";
                        
                        if should_be_healthy != currently_healthy {
                            status.state = if should_be_healthy { "ready" } else { "failed" }.to_string();
                            status.degraded = !should_be_healthy;
                            
                            if !should_be_healthy {
                                eprintln!("Lvol {} is unhealthy: {:?}", bdev_name, lvol_status.error_reason);
                            }
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

    // Update NVMe-oF targets status
    update_nvmeof_targets_status(&spdk_volume, &ctx, &mut status).await?;

    // Update timestamp and patch status if needed
    let current_time = Utc::now().to_rfc3339();
    if status.last_checked != current_time {
        status.last_checked = current_time;
        status_update_needed = true;
    }

    if status_update_needed {
        let patch = json!({ "status": status });
        spdk_volumes.patch_status(volume_id, &PatchParams::default(), &Patch::Merge(patch)).await?;
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

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

async fn handle_failed_replica(
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let http_client = HttpClient::new();

    // Get current raid status to find the failed base bdev name
    let raid_status = get_raid_status(ctx, volume_id).await?
        .ok_or("RAID status not found")?;
    
    // Find the failed base bdev name by slot
    let failed_base_bdev_name = raid_status.base_bdevs_list
        .iter()
        .find(|member| member.slot == failed_member_slot as u32)
        .map(|member| &member.name)
        .ok_or(format!("Failed member at slot {} not found", failed_member_slot))?;

    // Remove failed member with correct base bdev name
    http_client
        .post(&ctx.spdk_rpc_url)
        .json(&json!({
            "method": "bdev_raid_remove_base_bdev",
            "params": {
                "name": failed_base_bdev_name  // Fixed: Use actual base bdev name
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
                "raid_bdev": volume_id,    // Fixed: Use "raid_bdev" per documentation
                "base_bdev": new_bdev_name
                // Removed "slot" - not in SPDK documentation
            }
        }))
        .send()
        .await?;

    // SPDK automatically starts rebuild when a replacement member is added
    // No manual rebuild initiation needed

    Ok(())
}

async fn get_rpc_url_for_node(
    client: &Client,
    node_name: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let pods_api: Api<Pod> = Api::all(client.clone());
    let lp = ListParams::default().labels("app=flint-csi-node");

    for pod in pods_api.list(&lp).await? {
        if pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(node_name) {
            if let Some(pod_ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref()) {
                return Ok(format!("http://{}:8081/api/spdk/rpc", pod_ip));
            }
        }
    }

    Err(format!("Could not find flint-csi-node pod on node '{}'", node_name).into())
}

async fn create_replacement_lvol(
    ctx: &Context,
    replacement_disk: &SpdkDisk,
    size_bytes: i64,
    volume_id: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let http_client = HttpClient::new();
    let lvs_name = format!("lvs_{}", replacement_disk.metadata.name.as_ref().unwrap());
    let lvol_name = format!("vol_{}_{}", volume_id, Utc::now().timestamp());

    let target_node_name = &replacement_disk.spec.node_id;
    let rpc_url = get_rpc_url_for_node(&ctx.client, target_node_name).await?;
    
    println!("Creating replacement lvol on node '{}' via URL '{}'", target_node_name, rpc_url);

    // Convert bytes to MiB as required by SPDK bdev_lvol_create RPC
    let size_in_mib = (size_bytes + 1048575) / 1048576; // Round up to nearest MiB
    
    let res = http_client
        .post(&rpc_url)
        .json(&json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size_in_mib": size_in_mib,
                "thin_provision": false,
                "clear_method": "unmap"
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let volume_id = &spdk_volume.spec.volume_id;
    let lvol_uuid = get_lvol_uuid(&ctx, &new_lvol_bdev).await?;
    
    let mut new_spec = spdk_volume.spec.clone();
    new_spec.replicas[failed_replica_index] = Replica {
        node: replacement_disk.spec.node_id.clone(),
        replica_type: if failed_replica_index == 0 { "primary".to_string() } else { "secondary".to_string() },
        pcie_addr: Some(replacement_disk.spec.pcie_addr.clone()),
        disk_ref: replacement_disk.metadata.name.clone().unwrap_or_default(),
        lvol_uuid: Some(lvol_uuid),
        nqn: Some(format!("nqn.2025-05.io.spdk:lvol-{}", new_lvol_bdev.replace('/', "-"))),
        health_status: ReplicaHealth::Rebuilding,
        last_io_timestamp: Some(Utc::now().to_rfc3339()),
        write_sequence: 0,
        local_pod_scheduled: false,
        raid_member_index: failed_replica_index,
        raid_member_state: RaidMemberState::Rebuilding,
        ..Default::default()
    };
    
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), &ctx.target_namespace);
    spdk_volumes
        .patch(volume_id, &PatchParams::default(), &Patch::Merge(json!({
            "spec": new_spec
        })))
        .await?;

    // Update disk status
    let disks: Api<SpdkDisk> = Api::namespaced(ctx.client.clone(), &ctx.target_namespace);
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
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
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

async fn find_replacement_disk(
    spdk_volume: &SpdkVolume,
    failed_replica_index: usize,
    spdk_disks: &Api<SpdkDisk>,
) -> Result<SpdkDisk, Box<dyn std::error::Error + Send + Sync>> {
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
                    && !used_nodes.contains(&d.spec.node_id)
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

async fn update_nvmeof_targets_status(
    spdk_volume: &SpdkVolume,
    ctx: &Context,
    status: &mut SpdkVolumeStatus,
) -> Result<(), kube::Error> {
    let mut nvmeof_targets = Vec::new();
    
    for replica in &spdk_volume.spec.replicas {
        if let (Some(nqn), Some(ip), Some(port)) = (&replica.nqn, &replica.ip, &replica.port) {
            // Check if NVMe-oF target is active
            let is_active = check_nvmeof_target_active(ctx, &replica.node, nqn).await.unwrap_or(false);
            
            let bdev_name = if spdk_volume.spec.num_replicas > 1 {
                spdk_volume.spec.volume_id.clone() // RAID bdev name
            } else {
                format!("lvs_{}/{}", replica.disk_ref, replica.lvol_uuid.as_ref().unwrap_or(&"unknown".to_string()))
            };
            
            nvmeof_targets.push(NvmeofTarget {
                nqn: nqn.clone(),
                transport: spdk_volume.spec.nvmeof_transport.clone().unwrap_or("tcp".to_string()),
                target_addr: ip.clone(),
                target_port: port.parse().unwrap_or(4420),
                node: replica.node.clone(),
                bdev_name,
                active: is_active,
            });
        }
    }
    
    status.nvmeof_targets = nvmeof_targets;
    Ok(())
}

async fn check_nvmeof_target_active(
    ctx: &Context,
    node_name: &str,
    nqn: &str,
) -> Result<bool, ControllerError> {
    let rpc_url = get_rpc_url_for_node(&ctx.client, node_name).await
        .map_err(|e| ControllerError { message: e.to_string() })?;
    
    let http_client = HttpClient::new();
    
    let response = http_client
        .post(&rpc_url)
        .json(&json!({
            "method": "nvmf_get_subsystems"
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(false);
    }

    let subsystems: serde_json::Value = response.json().await?;
    
    if let Some(subsystem_list) = subsystems["result"].as_array() {
        for subsystem in subsystem_list {
            if subsystem["nqn"].as_str() == Some(nqn) {
                return Ok(subsystem["state"].as_str() == Some("active"));
            }
        }
    }
    
    Ok(false)
}

async fn health_monitor_task(ctx: Arc<Context>) {
    let mut interval = tokio::time::interval(Duration::from_secs(ctx.health_interval));
    
    loop {
        interval.tick().await;
        
        if let Err(e) = perform_periodic_health_check(&ctx).await {
            eprintln!("Health check failed: {}", e);
        }
    }
}

async fn perform_periodic_health_check(ctx: &Context) -> Result<(), ControllerError> {
    let spdk_volumes: Api<SpdkVolume> = Api::namespaced(ctx.client.clone(), &ctx.target_namespace);
    let volumes = spdk_volumes.list(&ListParams::default()).await
        .map_err(|e| ControllerError { message: format!("Failed to list volumes: {}", e) })?;
    
    for volume in volumes {
        // Get updated RAID status from SPDK
        if volume.spec.num_replicas > 1 {
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
                "online" => "ready".to_string(),
                "degraded" => "degraded".to_string(),
                "failed" | "broken" => "failed".to_string(),
                _ => "degraded".to_string(),
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
        let is_healthy = bdev["supported_io_types"]["read"].as_bool().unwrap_or(false) &&
                         bdev["supported_io_types"]["write"].as_bool().unwrap_or(false);

        Ok(LvolStatus {
            name: bdev_name.to_string(),
            is_healthy,
            error_reason: if is_healthy { None } else { Some("Bdev does not support read/write I/O".to_string()) },
        })
    } else {
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

/// Simple health check endpoint for Kubernetes liveness probes
async fn start_health_server() {
    let health = warp::path("healthz")
        .and(warp::get())
        .map(|| {
            // Simple health check - always return OK for liveness probe
            // The fact that the container is running means it's healthy
            warp::reply::with_status("OK", warp::http::StatusCode::OK)
        });

    let health_port = std::env::var("HEALTH_PORT")
        .unwrap_or("9809".to_string())
        .parse()
        .unwrap_or(9809);
    
    println!("Starting health server on port {}", health_port);
    warp::serve(health)
        .run(([0, 0, 0, 0], health_port))
        .await;
}


