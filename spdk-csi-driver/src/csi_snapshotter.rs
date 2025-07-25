// csi_snapshotter.rs - Snapshot implementation functions
use std::sync::Arc;
use crate::driver::SpdkCsiDriver;
use spdk_csi_driver::csi::{
    CreateSnapshotRequest, CreateSnapshotResponse, DeleteSnapshotRequest, DeleteSnapshotResponse,
    ListSnapshotsRequest, ListSnapshotsResponse, Snapshot,
    list_snapshots_response
};
use prost_types;
use spdk_csi_driver::models::*;
use kube::api::{Api, PostParams, Patch, PatchParams, ListParams};
use reqwest::Client as HttpClient;
use serde_json::json;
use tonic::{Request, Response, Status};
use std::time::SystemTime;
use chrono::{DateTime, Utc};

// Helper function to convert DateTime<Utc> to prost_types::Timestamp
fn datetime_to_timestamp(dt: DateTime<Utc>) -> prost_types::Timestamp {
    let system_time: SystemTime = dt.into();
    system_time.into()
}

/// RAII guard to ensure the RAID device is resumed even on error
struct RaidPauseGuard<'a> {
    client: &'a HttpClient,
    url: &'a str,
    raid_name: String,
    paused: bool,
}

impl<'a> RaidPauseGuard<'a> {
    async fn new(client: &'a HttpClient, url: &'a str, raid_name: String) -> Result<Self, Status> {
        println!("Pausing I/O for RAID device: {}", raid_name);
        
        let response = client
            .post(url)
            .json(&json!({
                "method": "bdev_raid_pause",
                "params": { "name": &raid_name }
            }))
            .send()
            .await
            .map_err(|e| Status::internal(format!("Failed to pause RAID device I/O: {}", e)))?;

        if !response.status().is_success() {
            return Err(Status::internal("Failed to pause RAID device"));
        }

        Ok(Self {
            client,
            url,
            raid_name,
            paused: true,
        })
    }
}

impl<'a> Drop for RaidPauseGuard<'a> {
    fn drop(&mut self) {
        if self.paused {
            let client = self.client.clone();
            let url = self.url.to_string();
            let name = self.raid_name.clone();
            
            // Fire-and-forget resume operation
            tokio::spawn(async move {
                println!("Resuming I/O for RAID device: {}", name);
                if let Err(e) = client.post(&url).json(&json!({
                    "method": "bdev_raid_resume",
                    "params": { "name": &name }
                })).send().await {
                    eprintln!("Error resuming RAID device {}: {}", name, e);
                }
            });
        }
    }
}

async fn create_replica_snapshot(
    driver: &SpdkCsiDriver,
    http_client: &HttpClient,
    replica: &Replica,
    snapshot_id: &str,
    index: usize,
    volume_spec: &SpdkVolumeSpec,
) -> Result<Option<ReplicaSnapshot>, String> {
    // Only snapshot healthy, online replicas
    if replica.health_status != ReplicaHealth::Healthy || replica.raid_member_state != RaidMemberState::Online {
        return Ok(None);
    }

    let replica_rpc_url = driver.get_rpc_url_for_node(&replica.node).await
        .map_err(|e| format!("Failed to get RPC URL for node {}: {}", replica.node, e))?;

    let lvs_name = format!("lvs_{}", replica.disk_ref);
    let lvol_uuid = replica.lvol_uuid.as_ref()
        .ok_or("Replica is missing lvol_uuid")?;
    let source_lvol_bdev_name = format!("{}/{}", lvs_name, lvol_uuid);
    let snapshot_lvol_name = format!("snap_{}_{}", snapshot_id, index);

    println!("Creating snapshot '{}' from lvol '{}' on node '{}'", 
             snapshot_lvol_name, source_lvol_bdev_name, replica.node);

    let snapshot_response = http_client
        .post(&replica_rpc_url)
        .json(&json!({
            "method": "bdev_lvol_snapshot",
            "params": {
                "lvol_name": source_lvol_bdev_name,
                "snapshot_name": snapshot_lvol_name
            }
        }))
        .send()
        .await
        .map_err(|e| format!("SPDK lvol_snapshot RPC failed for replica {}: {}", index, e))?;

    if !snapshot_response.status().is_success() {
        let err = snapshot_response.text().await.unwrap_or_default();
        return Err(format!("Failed to create snapshot for replica {} on node {}: {}", index, replica.node, err));
    }

    let result: serde_json::Value = snapshot_response.json().await
        .map_err(|e| format!("Failed to parse snapshot response: {}", e))?;
    
    let created_snapshot_bdev_name = result.as_str().unwrap_or_default().to_string();

    // Create NVMe-oF export for snapshot if enabled
    let nvmeof_export = if volume_spec.nvmeof_transport.is_some() {
        create_snapshot_nvmeof_export(driver, &created_snapshot_bdev_name, snapshot_id, index, replica).await
    } else {
        None
    };

    Ok(Some(ReplicaSnapshot {
        node_name: replica.node.clone(),
        spdk_snapshot_lvol: created_snapshot_bdev_name,
        source_lvol_bdev: source_lvol_bdev_name,
        disk_ref: replica.disk_ref.clone(),
        nvmeof_export,
    }))
}

async fn create_snapshot_nvmeof_export(
    driver: &SpdkCsiDriver,
    snapshot_bdev_name: &str,
    snapshot_id: &str,
    index: usize,
    replica: &Replica,
) -> Option<NvmeofExportInfo> {
    let snapshot_nqn = format!("nqn.2025-05.io.spdk:snapshot-{}-{}", snapshot_id, index);
    
    match driver.get_node_ip(&replica.node).await {
        Ok(ip) => {
            match driver.create_nvmeof_target(snapshot_bdev_name, &snapshot_nqn).await {
                Ok(_) => Some(NvmeofExportInfo {
                    nqn: snapshot_nqn,
                    target_ip: ip,
                    target_port: driver.nvmeof_target_port,
                    transport: driver.nvmeof_transport.clone(),
                    exported: true,
                    export_time: Some(Utc::now().to_rfc3339()),
                }),
                Err(e) => {
                    eprintln!("Failed to export snapshot as NVMe-oF: {}", e);
                    None
                }
            }
        }
        Err(_) => None,
    }
}

pub async fn create_snapshot_impl(
    driver: &Arc<SpdkCsiDriver>,
    request: Request<CreateSnapshotRequest>,
) -> Result<Response<CreateSnapshotResponse>, Status> {
    let req = request.into_inner();
    let snapshot_id = req.name;
    let source_volume_id = req.source_volume_id;

    if snapshot_id.is_empty() || source_volume_id.is_empty() {
        return Err(Status::invalid_argument("Snapshot ID and Source Volume ID are required"));
    }

    // Get the source SpdkVolume CR
    let volumes_api: Api<SpdkVolume> = Api::namespaced(driver.kube_client.clone(), &driver.target_namespace);
    let source_volume = volumes_api.get(&source_volume_id).await
        .map_err(|e| Status::not_found(format!("Source volume {} not found: {}", source_volume_id, e)))?;

    let http_client = HttpClient::new();
    
    // For multi-replica volumes, pause RAID I/O for consistency
    let _pause_guard = if source_volume.spec.num_replicas > 1 {
        Some(RaidPauseGuard::new(&http_client, &driver.spdk_rpc_url, source_volume.spec.volume_id.clone()).await?)
    } else {
        None
    };

    // Create snapshots on healthy replicas
    let mut replica_snapshots = Vec::new();
    let mut errors = Vec::new();

    for (index, replica) in source_volume.spec.replicas.iter().enumerate() {
        match create_replica_snapshot(driver, &http_client, replica, &snapshot_id, index, &source_volume.spec).await {
            Ok(Some(snapshot)) => replica_snapshots.push(snapshot),
            Ok(None) => continue, // Replica not healthy, skip
            Err(e) => errors.push(e),
        }
    }

    if replica_snapshots.is_empty() {
        let error_msg = if errors.is_empty() {
            "No healthy replicas available for snapshot".to_string()
        } else {
            format!("Failed to create snapshot on any replica: {}", errors.join(", "))
        };
        return Err(Status::internal(error_msg));
    }

    // Create SpdkSnapshot CRD
    let snapshot_crd = SpdkSnapshot::new_with_metadata(
        &snapshot_id,
        SpdkSnapshotSpec {
            source_volume_id: source_volume_id.clone(),
            snapshot_id: snapshot_id.clone(),
            replica_snapshots,
            snapshot_type: SnapshotType::Bdev,
            clone_source_snapshot_id: None,
            nvmeof_access_enabled: source_volume.spec.nvmeof_transport.is_some(),
            nvmeof_transport: source_volume.spec.nvmeof_transport.clone(),
        },
        &driver.target_namespace,
    );

    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(driver.kube_client.clone(), &driver.target_namespace);
    snapshots_api.create(&PostParams::default(), &snapshot_crd).await
        .map_err(|e| Status::internal(format!("Failed to create SpdkSnapshot CRD: {}", e)))?;

    // Update snapshot status
    let creation_time = Utc::now();
    let nvmeof_targets: Vec<NvmeofExportInfo> = snapshot_crd.spec.replica_snapshots.iter()
        .filter_map(|rs| rs.nvmeof_export.clone())
        .collect();
    
    let status_patch = json!({
        "status": {
            "creation_time": creation_time,
            "ready_to_use": true,
            "size_bytes": source_volume.spec.size_bytes,
            "error": null,
            "nvmeof_targets": nvmeof_targets,
            "accessible_nodes": snapshot_crd.spec.replica_snapshots.iter()
                .map(|rs| rs.node_name.clone())
                .collect::<Vec<String>>()
        }
    });
    
    snapshots_api.patch_status(&snapshot_id, &PatchParams::default(), &Patch::Merge(status_patch)).await
        .map_err(|e| Status::internal(format!("Failed to update snapshot status: {}", e)))?;

    Ok(Response::new(CreateSnapshotResponse {
        snapshot: Some(Snapshot {
            snapshot_id,
            source_volume_id,
            creation_time: Some(datetime_to_timestamp(creation_time)),
            ready_to_use: true,
            size_bytes: source_volume.spec.size_bytes,
            ..Default::default()
        }),
    }))
}

async fn delete_replica_snapshot(
    driver: &SpdkCsiDriver,
    http_client: &HttpClient,
    replica_snap: &ReplicaSnapshot,
) -> Result<(), String> {
    // Delete NVMe-oF target if exists
    if let Some(nvmeof_export) = &replica_snap.nvmeof_export {
        if nvmeof_export.exported {
            let rpc_url = driver.get_rpc_url_for_node(&replica_snap.node_name).await
                .map_err(|e| format!("Failed to get RPC URL: {}", e))?;
            
            http_client
                .post(&rpc_url)
                .json(&json!({
                    "method": "nvmf_delete_subsystem",
                    "params": {
                        "nqn": nvmeof_export.nqn
                    }
                }))
                .send()
                .await
                .ok(); // Best effort
        }
    }

    // Delete SPDK snapshot bdev
    let replica_rpc_url = driver.get_rpc_url_for_node(&replica_snap.node_name).await
        .map_err(|e| format!("Failed to get RPC URL: {}", e))?;
    
    println!("Deleting SPDK snapshot bdev '{}' on node '{}'", 
             replica_snap.spdk_snapshot_lvol, replica_snap.node_name);
    
    let delete_response = http_client
        .post(&replica_rpc_url)
        .json(&json!({
            "method": "bdev_delete",
            "params": {
                "name": &replica_snap.spdk_snapshot_lvol,
            }
        }))
        .send()
        .await
        .map_err(|e| format!("SPDK RPC call to delete snapshot failed: {}", e))?;

    if !delete_response.status().is_success() {
        let error_text = delete_response.text().await.unwrap_or_default();
        if !error_text.contains("No such device") {
            return Err(format!("Failed to delete SPDK snapshot '{}': {}", 
                              replica_snap.spdk_snapshot_lvol, error_text));
        }
    }

    Ok(())
}

pub async fn delete_snapshot_impl(
    driver: &Arc<SpdkCsiDriver>,
    request: Request<DeleteSnapshotRequest>,
) -> Result<Response<DeleteSnapshotResponse>, Status> {
    let snapshot_id = request.into_inner().snapshot_id;
    if snapshot_id.is_empty() {
        return Err(Status::invalid_argument("Snapshot ID is required"));
    }

    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(driver.kube_client.clone(), &driver.target_namespace);
    let snapshot_crd = match snapshots_api.get(&snapshot_id).await {
        Ok(crd) => crd,
        Err(_) => return Ok(Response::new(DeleteSnapshotResponse {})),
    };

    let http_client = HttpClient::new();
    let mut errors = Vec::new();

    // Delete all replica snapshots
    for replica_snap in &snapshot_crd.spec.replica_snapshots {
        if let Err(e) = delete_replica_snapshot(driver, &http_client, replica_snap).await {
            errors.push(e);
        }
    }

    // Delete the SpdkSnapshot CRD
    snapshots_api.delete(&snapshot_id, &Default::default()).await
        .map_err(|e| Status::internal(format!("Failed to delete SpdkSnapshot CRD: {}", e)))?;

    if !errors.is_empty() {
        eprintln!("Some snapshot replicas failed to delete: {}", errors.join(", "));
        // Continue anyway since the CRD is deleted
    }

    Ok(Response::new(DeleteSnapshotResponse {}))
}

pub async fn list_snapshots_impl(
    driver: &Arc<SpdkCsiDriver>,
    _request: Request<ListSnapshotsRequest>,
) -> Result<Response<ListSnapshotsResponse>, Status> {
    let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(driver.kube_client.clone(), &driver.target_namespace);
    let crd_list = snapshots_api.list(&ListParams::default()).await
        .map_err(|e| Status::internal(format!("Failed to list SpdkSnapshots: {}", e)))?;

    let entries = crd_list.items.into_iter().filter_map(|s| {
        s.status.as_ref().map(|status| {
            list_snapshots_response::Entry {
                snapshot: Some(Snapshot {
                    snapshot_id: s.spec.snapshot_id,
                    source_volume_id: s.spec.source_volume_id,
                    creation_time: status.creation_time.map(datetime_to_timestamp),
                    ready_to_use: status.ready_to_use,
                    size_bytes: status.size_bytes,
                    ..Default::default()
                }),
            }
        })
    }).collect();

    Ok(Response::new(ListSnapshotsResponse {
        entries,
        next_token: "".to_string(),
    }))
}