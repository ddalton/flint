// csi_snapshotter.rs
use crate::csi_driver::csi::csi::v1::{
    CreateSnapshotRequest, CreateSnapshotResponse, DeleteSnapshotRequest, DeleteSnapshotResponse,
    ListSnapshotsRequest, ListSnapshotsResponse, Snapshot,
    list_snapshots_response
};
use spdk_csi_driver::*;
use kube::api::{Api, PostParams, Patch, PatchParams};
use reqwest::Client as HttpClient;
use serde_json::json;
use tonic::{Request, Response, Status};
use chrono::Utc;
use crate::SpdkCsiDriver;

/// An RAII guard to ensure the RAID device is resumed.
struct RaiiRaidResume<'a> {
    client: &'a HttpClient,
    url: &'a str,
    name: String,
    enabled: bool,
}

impl<'a> Drop for RaiiRaidResume<'a> {
    fn drop(&mut self) {
        if self.enabled {
            let client = self.client.clone();
            let url = self.url.to_string();
            let name = self.name.clone();
            // Fire-and-forget resume operation in a new task.
            tokio::spawn(async move {
                println!("RAII guard resuming I/O for RAID device: {}", name);
                if let Err(e) = client.post(&url).json(&json!({
                    "method": "bdev_raid_resume",
                    "params": { "name": &name }
                })).send().await {
                    eprintln!("Error in RAII resume guard for {}: {}", name, e);
                }
            });
        }
    }
}

impl SpdkCsiDriver {
    pub async fn create_snapshot_impl(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        let req = request.into_inner();
        let snapshot_id = req.name;
        let source_volume_id = req.source_volume_id;

        if snapshot_id.is_empty() || source_volume_id.is_empty() {
            return Err(Status::invalid_argument("Snapshot ID and Source Volume ID are required"));
        }

        // 1. Get the source SpdkVolume CR to find its replicas and size.
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let source_volume = volumes_api.get(&source_volume_id).await
            .map_err(|e| Status::not_found(format!("Source volume {} not found: {}", source_volume_id, e)))?;

        // The RAID bdev name is the volume_id.
        let source_raid_bdev = &source_volume.spec.volume_id;
        let http_client = HttpClient::new();

        // 2. Pause I/O on the RAID device for consistency.
        // The pause/resume RPC can be sent to any node in the cluster that sees the RAID bdev.
        println!("Pausing I/O for RAID device: {}", source_raid_bdev);
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_raid_pause",
                "params": { "name": source_raid_bdev }
            }))
            .send()
            .await
            .map_err(|e| Status::internal(format!("Failed to pause RAID device I/O: {}", e)))?;

        // RAII guard to ensure we resume I/O even on error.
        let mut resume_guard = RaiiRaidResume {
            client: &http_client,
            url: &self.spdk_rpc_url,
            name: source_raid_bdev.clone(),
            enabled: true,
        };

        // 3. Iterate over healthy replicas and create snapshots on their respective lvols.
        let mut replica_snapshots = Vec::new();
        for (index, replica) in source_volume.spec.replicas.iter().enumerate() {
            // Only snapshot online, healthy replicas.
            if replica.health_status == ReplicaHealth::Healthy && replica.raid_member_state == RaidMemberState::Online {
                let replica_rpc_url = self.get_rpc_url_for_node(&replica.node).await?;

                let lvs_name = format!("lvs_{}", replica.disk_ref);
                let lvol_uuid = replica.lvol_uuid.as_ref().ok_or_else(|| Status::internal("Replica is missing lvol_uuid"))?;
                let source_lvol_bdev_name = format!("{}/{}", lvs_name, lvol_uuid);
                let snapshot_lvol_name = format!("snap_{}_{}", snapshot_id, index);

                println!("Creating snapshot '{}' from lvol '{}' on node '{}'", snapshot_lvol_name, source_lvol_bdev_name, replica.node);

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
                    .map_err(|e| Status::internal(format!("SPDK lvol_snapshot RPC failed for replica {}: {}", index, e)))?;

                if !snapshot_response.status().is_success() {
                    let err = snapshot_response.text().await.unwrap_or_default();
                    eprintln!("Failed to create snapshot for replica {} on node {}: {}", index, replica.node, err);
                    continue; // Continue to next replica
                }

                let result: serde_json::Value = snapshot_response.json().await
                    .map_err(|e| Status::internal(format!("Failed to parse snapshot response: {}", e)))?;
                
                let created_snapshot_bdev_name = result.as_str().unwrap_or_default().to_string();

                replica_snapshots.push(ReplicaSnapshot {
                    node_name: replica.node.clone(),
                    spdk_snapshot_lvol: created_snapshot_bdev_name,
                    source_lvol_bdev: source_lvol_bdev_name,
                    disk_ref: replica.disk_ref.clone(),
                });
            }
        }

        // 4. Resume I/O by disabling and dropping the guard.
        println!("Resuming I/O for RAID device: {}", source_raid_bdev);
        resume_guard.enabled = false;
        drop(resume_guard);
        http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({ "method": "bdev_raid_resume", "params": { "name": source_raid_bdev } }))
            .send().await.ok(); // Best effort resume

        if replica_snapshots.is_empty() {
            return Err(Status::internal("Failed to create snapshot on any healthy replica."));
        }

        // 5. Create the SpdkSnapshot CRD to persist the snapshot info.
        let snapshot_crd = SpdkSnapshot::new(
            &snapshot_id,
            SpdkSnapshotSpec {
                source_volume_id: source_volume_id.clone(),
                snapshot_id: snapshot_id.clone(),
                replica_snapshots,
                snapshot_type: SnapshotType::Bdev,
                clone_source_snapshot_id: None,
            },
        );

        let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), "default");
        snapshots_api.create(&PostParams::default(), &snapshot_crd).await
            .map_err(|e| Status::internal(format!("Failed to create SpdkSnapshot CRD: {}", e)))?;

        // 6. Update the status of the new CRD.
        let creation_time = Utc::now();
        let status_patch = json!({
            "status": {
                "creation_time": creation_time,
                "ready_to_use": true,
                "size_bytes": source_volume.spec.size_bytes,
                "error": null
            }
        });
        snapshots_api.patch_status(&snapshot_id, &PatchParams::default(), &Patch::Merge(status_patch)).await
             .map_err(|e| Status::internal(format!("Failed to update snapshot status: {}", e)))?;

        // 7. Return the CSI response.
        Ok(Response::new(CreateSnapshotResponse {
            snapshot: Some(Snapshot {
                snapshot_id,
                source_volume_id,
                creation_time: Some(creation_time.into()),
                ready_to_use: true,
                size_bytes: source_volume.spec.size_bytes,
                ..Default::default()
            }),
        }))
    }

    pub async fn delete_snapshot_impl(
        &self,
        request: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        let snapshot_id = request.into_inner().snapshot_id;
        if snapshot_id.is_empty() {
            return Err(Status::invalid_argument("Snapshot ID is required"));
        }

        let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), "default");
        let snapshot_crd = match snapshots_api.get(&snapshot_id).await {
            Ok(crd) => crd,
            Err(_) => return Ok(Response::new(DeleteSnapshotResponse {})),
        };

        // 2. Iterate through each replica snapshot and delete the underlying SPDK bdev.
        let http_client = HttpClient::new();
        for replica_snap in &snapshot_crd.spec.replica_snapshots {
            let replica_rpc_url = self.get_rpc_url_for_node(&replica_snap.node_name).await?;
            println!("Deleting SPDK snapshot bdev '{}' on node '{}'", replica_snap.spdk_snapshot_lvol, replica_snap.node_name);
            
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
                .map_err(|e| Status::internal(format!("SPDK RPC call to delete snapshot failed: {}", e)))?;

            if !delete_response.status().is_success() {
                let error_text = delete_response.text().await.unwrap_or_default();
                if !error_text.contains("No such device") {
                     eprintln!("Failed to delete SPDK snapshot '{}': {}", replica_snap.spdk_snapshot_lvol, error_text);
                }
            }
        }
        
        // 3. Delete the SpdkSnapshot CRD.
        snapshots_api.delete(&snapshot_id, &Default::default()).await
            .map_err(|e| Status::internal(format!("Failed to delete SpdkSnapshot CRD: {}", e)))?;

        Ok(Response::new(DeleteSnapshotResponse {}))
    }
    
    // Unchanged...
    pub async fn list_snapshots_impl(
        &self,
        _request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        // This is a basic implementation. A production driver would handle pagination.
        let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), "default");
        let crd_list = snapshots_api.list(&Default::default()).await
            .map_err(|e| Status::internal(format!("Failed to list SpdkSnapshots: {}", e)))?;

        let entries = crd_list.items.into_iter().filter_map(|s| {
            s.status.as_ref().map(|status| {
                list_snapshots_response::Entry {
                    snapshot: Some(Snapshot {
                        snapshot_id: s.spec.snapshot_id,
                        source_volume_id: s.spec.source_volume_id,
                        creation_time: status.creation_time.map(|t| t.into()),
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
}
