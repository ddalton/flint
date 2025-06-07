// csi_snapshotter.rs
use crate::csi_driver::csi::{
    CreateSnapshotRequest, CreateSnapshotResponse, DeleteSnapshotRequest, DeleteSnapshotResponse,
    ListSnapshotsRequest, ListSnapshotsResponse, Snapshot,
};
use crate::{SpdkCsiDriver, SpdkSnapshot, SpdkSnapshotSpec, SpdkVolume};
use crate::snapshot::SnapshotType; // Import the new enum
use kube::{
    api::{Api, PostParams, Patch, PatchParams},
    Client,
};
use reqwest::Client as HttpClient;
use serde_json::json;
use tonic::{Request, Response, Status};
use chrono::Utc;

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

        // 1. Get the source SpdkVolume CR to find its size.
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), "default");
        let source_volume = volumes_api.get(&source_volume_id).await
            .map_err(|e| Status::not_found(format!("Source volume {} not found: {}", source_volume_id, e)))?;

        // For a RAID1 volume, we snapshot the RAID bdev itself.
        let source_bdev_name = &source_volume_id;
        let snapshot_bdev_name = format!("snap_{}", snapshot_id);

        // 2. Call SPDK RPC to create the snapshot bdev.
        let http_client = HttpClient::new();
        let snapshot_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_snapshot",
                "params": {
                    "bdev_name": source_bdev_name,
                    "snapshot_name": snapshot_bdev_name
                }
            }))
            .send()
            .await
            .map_err(|e| Status::internal(format!("SPDK RPC call failed: {}", e)))?;

        if !snapshot_response.status().is_success() {
            let error_text = snapshot_response.text().await.unwrap_or_default();
            return Err(Status::internal(format!("Failed to create SPDK snapshot: {}", error_text)));
        }

        // 3. Create the SpdkSnapshot CRD to persist the snapshot info.
        // The spdk_snapshot_lvol field will store the name of the generic snapshot bdev.
        let snapshot_crd = SpdkSnapshot::new(
            &snapshot_id,
            SpdkSnapshotSpec {
                source_volume_id: source_volume_id.clone(),
                snapshot_id: snapshot_id.clone(),
                spdk_snapshot_lvol: snapshot_bdev_name.clone(),
                // --- Start of Change ---
                snapshot_type: SnapshotType::Bdev, // Set the snapshot type on creation
                clone_source_snapshot_id: None,    // This is a direct snapshot, not a clone
                // --- End of Change ---
            },
        );

        let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), "default");
        snapshots_api.create(&PostParams::default(), &snapshot_crd).await
            .map_err(|e| Status::internal(format!("Failed to create SpdkSnapshot CRD: {}", e)))?;

        // 4. Update the status of the new CRD.
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


        // 5. Return the CSI response.
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

        // 1. Get the SpdkSnapshot CRD. If not found, assume deleted.
        let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), "default");
        let snapshot_crd = match snapshots_api.get(&snapshot_id).await {
            Ok(crd) => crd,
            Err(_) => return Ok(Response::new(DeleteSnapshotResponse {})),
        };

        // 2. Call SPDK RPC to delete the snapshot bdev.
        // --- Start of Fix ---
        // Corrected to use bdev_delete for a generic bdev snapshot.
        let http_client = HttpClient::new();
        let delete_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_delete",
                "params": {
                    "name": snapshot_crd.spec.spdk_snapshot_lvol,
                }
            }))
            .send()
            .await
            .map_err(|e| Status::internal(format!("SPDK RPC call failed: {}", e)))?;
        // --- End of Fix ---

        if !delete_response.status().is_success() {
            let error_text = delete_response.text().await.unwrap_or_default();
            // Ignore "not found" errors, as the snapshot might already be gone.
            if !error_text.contains("No such device") {
                 return Err(Status::internal(format!("Failed to delete SPDK snapshot: {}", error_text)));
            }
        }

        // 3. Delete the SpdkSnapshot CRD.
        snapshots_api.delete(&snapshot_id, &Default::default()).await
            .map_err(|e| Status::internal(format!("Failed to delete SpdkSnapshot CRD: {}", e)))?;

        Ok(Response::new(DeleteSnapshotResponse {}))
    }

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
                crate::csi_driver::csi::list_snapshots_response::Entry {
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
