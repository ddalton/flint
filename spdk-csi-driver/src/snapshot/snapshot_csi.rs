//! CSI Controller snapshot RPC implementations
//!
//! Implements the CSI snapshot RPCs completely separately from main CSI controller.
//! These methods are delegated from main.rs to keep existing CSI operations isolated.

use tonic::{Request, Response, Status};
use std::sync::Arc;
use std::collections::BTreeMap;
use crate::csi::*;
use crate::driver::SpdkCsiDriver;

/// Detect whether a PV's volumeAttributes describe a pNFS volume.
/// pNFS volumes are tagged at create time with pnfs.flint.io/* keys
/// (see pnfs_csi::create_volume). Detection is by namespace prefix so
/// future pnfs.flint.io/* attributes are caught automatically.
///
/// Exposed `pub` so the binary crate (main.rs) can reuse it for the
/// CreateVolume-from-snapshot and CreateVolume-from-PVC guards.
pub fn is_pnfs_volume_attrs(volume_attrs: &BTreeMap<String, String>) -> bool {
    volume_attrs.keys().any(|k| k.starts_with("pnfs.flint.io/"))
}

/// Reject CreateSnapshot for source volumes whose deployment mode does
/// not support snapshots. Today the only such case is pNFS volumes:
/// snapshots require either SPDK blobstore COW (single-server volumes)
/// or a distributed-snapshot protocol coordinating MDS+DSes (not built).
/// Without this guard, the request falls through to the SPDK metadata
/// lookup, which fails with NOT_FOUND because pNFS volumes don't carry
/// SPDK volumeAttributes — and `external-snapshotter` retries NOT_FOUND
/// indefinitely, producing the visible "snapshot controller crash loop."
///
/// Returning FAILED_PRECONDITION marks the rejection as final:
/// external-snapshotter records `VolumeSnapshot.status.error` and stops
/// retrying. The user sees the message via `kubectl describe vs ...`.
pub(crate) fn validate_snapshot_source(
    volume_attrs: &BTreeMap<String, String>,
) -> Result<(), Status> {
    if is_pnfs_volume_attrs(volume_attrs) {
        return Err(Status::failed_precondition(
            "snapshots are not supported for pNFS volumes; \
             use a StorageClass without `layout: pnfs` for snapshotted volumes, \
             or enable SPDK on this cluster",
        ));
    }
    Ok(())
}

/// Read the PV's `volume_attributes` map for a given volume_id without
/// requiring SPDK metadata keys to be present (unlike the existing
/// `get_volume_info_from_pv`, which enforces SPDK shape and was the
/// pre-fix source of NOT_FOUND for pNFS volumes).
async fn lookup_volume_attributes(
    driver: &SpdkCsiDriver,
    volume_id: &str,
) -> Result<BTreeMap<String, String>, Status> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    use kube::Api;

    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let list = pvs
        .list(&Default::default())
        .await
        .map_err(|e| Status::internal(format!("list PVs: {}", e)))?;

    for pv in list.items {
        if let Some(spec) = &pv.spec {
            if let Some(csi) = &spec.csi {
                if csi.volume_handle == volume_id {
                    return Ok(csi.volume_attributes.clone().unwrap_or_default());
                }
            }
        }
    }
    Err(Status::not_found(format!(
        "PV for volume {} not found",
        volume_id
    )))
}

/// Snapshot-specific CSI controller
/// Implements only the snapshot-related RPCs - CreateSnapshot, DeleteSnapshot, ListSnapshots
pub struct SnapshotController {
    driver: Arc<SpdkCsiDriver>,
}

impl SnapshotController {
    /// Create new snapshot controller
    pub fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }

    /// Create snapshot - CSI RPC implementation
    /// 
    /// Called by Kubernetes snapshot-controller when user creates a VolumeSnapshot.
    /// 
    /// # Flow
    /// 1. Find which node has the source volume
    /// 2. Generate unique snapshot name
    /// 3. Call node agent to create SPDK snapshot
    /// 4. Return CSI Snapshot response
    pub async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        let req = request.into_inner();

        tracing::info!("📸 [SNAPSHOT_CSI] CreateSnapshot: volume={}, name={}",
                 req.source_volume_id, req.name);

        // Guard: reject snapshot requests for volume types that don't
        // support snapshots (pNFS today). Returns FAILED_PRECONDITION so
        // external-snapshotter records the error and stops retrying,
        // instead of looping on NOT_FOUND from the SPDK-shaped lookup.
        let volume_attrs = lookup_volume_attributes(&self.driver, &req.source_volume_id).await?;
        validate_snapshot_source(&volume_attrs)?;

        // Step 1: Find source volume
        let volume_info = self.driver
            .get_volume_info(&req.source_volume_id)
            .await
            .map_err(|e| Status::not_found(format!("Source volume not found: {}", e)))?;

        tracing::info!("✅ [SNAPSHOT_CSI] Found source volume on node: {}", volume_info.node_name);

        // Step 2: Generate unique snapshot name
        // Format: snap_{volume_id}_{timestamp}
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let snapshot_name = format!("snap_{}_{}", req.source_volume_id, timestamp);

        tracing::info!("📸 [SNAPSHOT_CSI] Generated snapshot name: {}", snapshot_name);

        // Step 3: Call node agent to create snapshot
        let payload = serde_json::json!({
            "lvol_name": volume_info.lvol_uuid,
            "snapshot_name": snapshot_name
        });

        let response = self.driver
            .call_node_agent(&volume_info.node_name, "/api/snapshots/create", &payload)
            .await
            .map_err(|e| Status::internal(format!("Failed to create snapshot: {}", e)))?;

        let snapshot_uuid = response["snapshot_uuid"]
            .as_str()
            .ok_or_else(|| Status::internal("No snapshot UUID in response"))?
            .to_string();

        let size_bytes = response["size_bytes"].as_i64().unwrap_or(0);

        tracing::info!("✅ [SNAPSHOT_CSI] Snapshot created: {}", snapshot_uuid);

        // Step 4: Return CSI response
        let snapshot = Snapshot {
            snapshot_id: snapshot_uuid.clone(),
            source_volume_id: req.source_volume_id.clone(),
            creation_time: Some(prost_types::Timestamp {
                seconds: timestamp as i64,
                nanos: 0,
            }),
            ready_to_use: true, // SPDK snapshots are instantly ready (copy-on-write)
            size_bytes,
            group_snapshot_id: String::new(), // Not using group snapshots
        };

        tracing::info!("🎉 [SNAPSHOT_CSI] CreateSnapshot succeeded: {}", snapshot_uuid);

        Ok(Response::new(CreateSnapshotResponse {
            snapshot: Some(snapshot),
        }))
    }

    /// Delete snapshot - CSI RPC implementation
    /// 
    /// Called by Kubernetes snapshot-controller when user deletes a VolumeSnapshot.
    /// 
    /// # Flow
    /// 1. Query all nodes to find the snapshot
    /// 2. Call node agent to delete SPDK snapshot
    /// 3. Return success (idempotent - OK if not found)
    pub async fn delete_snapshot(
        &self,
        request: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        let req = request.into_inner();
        
        tracing::info!("🗑️ [SNAPSHOT_CSI] DeleteSnapshot: {}", req.snapshot_id);

        // Query all nodes to find the snapshot
        let nodes = self.driver.get_all_nodes().await
            .map_err(|e| Status::internal(format!("Failed to list nodes: {}", e)))?;

        tracing::info!("🔍 [SNAPSHOT_CSI] Searching for snapshot across {} nodes", nodes.len());

        let mut snapshot_found = false;

        for node in nodes {
            let payload = serde_json::json!({
                "snapshot_uuid": req.snapshot_id
            });

            match self.driver
                .call_node_agent(&node, "/api/snapshots/delete", &payload)
                .await {
                Ok(_) => {
                    tracing::info!("✅ [SNAPSHOT_CSI] Deleted snapshot from node: {}", node);
                    snapshot_found = true;
                    break;
                }
                Err(e) => {
                    // Snapshot might not be on this node, continue searching
                    tracing::info!("ℹ️ [SNAPSHOT_CSI] Snapshot not on node {}: {}", node, e);
                }
            }
        }

        if !snapshot_found {
            tracing::info!("⚠️ [SNAPSHOT_CSI] Snapshot not found (may already be deleted)");
            // This is OK - delete is idempotent
        }

        tracing::info!("🎉 [SNAPSHOT_CSI] DeleteSnapshot succeeded");

        Ok(Response::new(DeleteSnapshotResponse {}))
    }

    /// List snapshots - CSI RPC implementation
    /// 
    /// Called by Kubernetes snapshot-controller or kubectl to list snapshots.
    /// 
    /// # Flow
    /// 1. Query all nodes for their snapshots
    /// 2. Aggregate results
    /// 3. Apply filters (source_volume_id, snapshot_id)
    /// 4. Return list
    pub async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        let req = request.into_inner();
        
        tracing::info!("📋 [SNAPSHOT_CSI] ListSnapshots");
        if !req.source_volume_id.is_empty() {
            tracing::info!("📋 [SNAPSHOT_CSI] Filter: source_volume_id={}", req.source_volume_id);
        }
        if !req.snapshot_id.is_empty() {
            tracing::info!("📋 [SNAPSHOT_CSI] Filter: snapshot_id={}", req.snapshot_id);
        }

        // Query all nodes for snapshots
        let nodes = self.driver.get_all_nodes().await
            .map_err(|e| Status::internal(format!("Failed to list nodes: {}", e)))?;

        tracing::info!("🔍 [SNAPSHOT_CSI] Querying {} nodes for snapshots", nodes.len());

        let mut all_snapshots = Vec::new();

        for node in nodes {
            match self.driver
                .call_node_agent(&node, "/api/snapshots/list", &serde_json::json!({}))
                .await {
                Ok(response) => {
                    if let Some(snapshots) = response["snapshots"].as_array() {
                        tracing::info!("✅ [SNAPSHOT_CSI] Node {} has {} snapshots", node, snapshots.len());
                        
                        for snap in snapshots {
                            let snapshot = Snapshot {
                                snapshot_id: snap["snapshot_uuid"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                source_volume_id: snap["source_volume_id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                creation_time: Some(prost_types::Timestamp {
                                    seconds: snap["creation_time"]
                                        .as_str()
                                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                        .map(|dt| dt.timestamp())
                                        .unwrap_or(0),
                                    nanos: 0,
                                }),
                                ready_to_use: snap["ready_to_use"].as_bool().unwrap_or(true),
                                size_bytes: snap["size_bytes"].as_i64().unwrap_or(0),
                                group_snapshot_id: String::new(), // Not using group snapshots
                            };
                            
                            // Apply filters if specified
                            if !req.source_volume_id.is_empty() 
                               && snapshot.source_volume_id != req.source_volume_id {
                                continue;
                            }
                            
                            if !req.snapshot_id.is_empty() 
                               && snapshot.snapshot_id != req.snapshot_id {
                                continue;
                            }
                            
                            all_snapshots.push(snapshot);
                        }
                    }
                }
                Err(e) => {
                    tracing::info!("⚠️ [SNAPSHOT_CSI] Failed to list snapshots on node {}: {}", node, e);
                    // Continue with other nodes
                }
            }
        }

        tracing::info!("✅ [SNAPSHOT_CSI] Found {} total snapshots (after filtering)", all_snapshots.len());

        // TODO: Implement pagination if needed (max_entries, starting_token)
        // For now, return all snapshots
        
        // Convert Snapshot to Entry
        use crate::csi::list_snapshots_response::Entry;
        let entries: Vec<Entry> = all_snapshots.into_iter()
            .map(|snapshot| Entry {
                snapshot: Some(snapshot),
            })
            .collect();

        Ok(Response::new(ListSnapshotsResponse {
            entries,
            next_token: String::new(), // No pagination yet
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pnfs_volume_attrs() -> BTreeMap<String, String> {
        let mut a = BTreeMap::new();
        a.insert("pnfs.flint.io/mds-ip".to_string(), "10.0.0.1".to_string());
        a.insert("pnfs.flint.io/mds-port".to_string(), "20049".to_string());
        a.insert("pnfs.flint.io/volume-id".to_string(), "pvc-pnfs-1".to_string());
        a
    }

    fn spdk_volume_attrs() -> BTreeMap<String, String> {
        let mut a = BTreeMap::new();
        a.insert(
            "flint.csi.storage.io/node-name".to_string(),
            "worker-1".to_string(),
        );
        a.insert(
            "flint.csi.storage.io/lvol-uuid".to_string(),
            "00000000-0000-0000-0000-000000000001".to_string(),
        );
        a.insert(
            "flint.csi.storage.io/lvs-name".to_string(),
            "lvs0".to_string(),
        );
        a
    }

    /// Mirrors the pre-fix code path that produced the crash loop: the
    /// existing `get_volume_info_from_pv` requires `flint.csi.storage.io/
    /// node-name` to be present in the PV's volumeAttributes, and
    /// `create_snapshot` mapped its missing-metadata error to
    /// `Status::not_found(...)`. NOT_FOUND is retryable per CSI, so
    /// external-snapshotter loops indefinitely. This simulation pins
    /// that broken behavior so the regression test is unambiguous.
    fn pre_fix_lookup_simulation(attrs: &BTreeMap<String, String>) -> Result<(), Status> {
        if attrs.contains_key("flint.csi.storage.io/node-name") {
            Ok(())
        } else {
            Err(Status::not_found(
                "Source volume not found: PV missing flint metadata in volumeAttributes",
            ))
        }
    }

    // ----- pre-fix reproduction tests --------------------------------

    #[test]
    fn pre_fix_pnfs_attrs_lack_spdk_metadata_keys() {
        let attrs = pnfs_volume_attrs();
        assert!(
            !attrs.contains_key("flint.csi.storage.io/node-name"),
            "pNFS volumes do not carry SPDK node-name attribute"
        );
        assert!(
            !attrs.contains_key("flint.csi.storage.io/lvol-uuid"),
            "pNFS volumes do not carry SPDK lvol-uuid attribute"
        );
    }

    #[test]
    fn pre_fix_path_returns_retryable_not_found_for_pnfs() {
        let attrs = pnfs_volume_attrs();
        let result = pre_fix_lookup_simulation(&attrs);
        assert!(result.is_err(), "pre-fix lookup must fail for pNFS attrs");
        let status = result.unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::NotFound,
            "pre-fix returned NOT_FOUND, which external-snapshotter retries forever"
        );
    }

    // ----- post-fix tests --------------------------------------------

    #[test]
    fn post_fix_validate_rejects_pnfs_with_failed_precondition() {
        let attrs = pnfs_volume_attrs();
        let result = validate_snapshot_source(&attrs);
        assert!(result.is_err(), "pNFS source must be rejected");
        let status = result.unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::FailedPrecondition,
            "must be FAILED_PRECONDITION (final, non-retryable) so the loop stops"
        );
        let msg = status.message();
        assert!(msg.contains("pNFS"), "message must name pNFS: got {:?}", msg);
        assert!(
            msg.to_lowercase().contains("snapshot"),
            "message must explain it's about snapshots: got {:?}",
            msg
        );
    }

    #[test]
    fn post_fix_validate_allows_spdk_volumes() {
        let attrs = spdk_volume_attrs();
        assert!(
            validate_snapshot_source(&attrs).is_ok(),
            "SPDK volume must pass validation and reach the existing snapshot path"
        );
    }

    #[test]
    fn post_fix_validate_rejects_any_pnfs_namespace_key() {
        let mut attrs = BTreeMap::new();
        attrs.insert(
            "pnfs.flint.io/some-future-key".to_string(),
            "x".to_string(),
        );
        let result = validate_snapshot_source(&attrs);
        assert!(
            result.is_err(),
            "detection is by pnfs.flint.io/* prefix, not a single key"
        );
        assert_eq!(
            result.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
    }

    #[test]
    fn post_fix_validate_passes_through_empty_or_legacy_attrs() {
        // Volumes with no flint-managed attributes fall through to the
        // existing diagnosis path; the guard does not invent rejections
        // beyond its scope.
        let attrs = BTreeMap::new();
        assert!(validate_snapshot_source(&attrs).is_ok());
    }

    // ----- is_pnfs_volume_attrs (used by main.rs CreateVolume guards) ----

    #[test]
    fn is_pnfs_volume_attrs_detects_pnfs() {
        assert!(is_pnfs_volume_attrs(&pnfs_volume_attrs()));
    }

    #[test]
    fn is_pnfs_volume_attrs_returns_false_for_spdk() {
        assert!(!is_pnfs_volume_attrs(&spdk_volume_attrs()));
    }

    #[test]
    fn is_pnfs_volume_attrs_returns_false_for_empty() {
        assert!(!is_pnfs_volume_attrs(&BTreeMap::new()));
    }

    #[test]
    fn is_pnfs_volume_attrs_matches_namespace_prefix_only() {
        // A key that contains "pnfs" but isn't in the pnfs.flint.io/
        // namespace must NOT trigger detection (e.g. user labels).
        let mut attrs = BTreeMap::new();
        attrs.insert("user.example.com/pnfs-tag".to_string(), "x".to_string());
        attrs.insert("flint.csi.storage.io/node-name".to_string(), "n1".to_string());
        assert!(!is_pnfs_volume_attrs(&attrs));
    }

    // ----- preserved existing test -----------------------------------

    #[test]
    fn test_snapshot_name_generation() {
        let volume_id = "pvc-abc123";
        let timestamp = 1234567890;
        let snapshot_name = format!("snap_{}_{}", volume_id, timestamp);
        assert!(snapshot_name.starts_with("snap_"));
        assert!(snapshot_name.contains(volume_id));
    }
}

