//! CSI Controller snapshot RPC implementations
//! 
//! Implements the CSI snapshot RPCs completely separately from main CSI controller.
//! These methods are delegated from main.rs to keep existing CSI operations isolated.

use tonic::{Request, Response, Status};
use std::sync::Arc;
use crate::csi::*;
use crate::driver::SpdkCsiDriver;

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
    

    #[test]
    fn test_snapshot_name_generation() {
        let volume_id = "pvc-abc123";
        let timestamp = 1234567890;
        let snapshot_name = format!("snap_{}_{}", volume_id, timestamp);
        
        assert!(snapshot_name.starts_with("snap_"));
        assert!(snapshot_name.contains(volume_id));
    }
}

