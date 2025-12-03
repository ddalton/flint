//! Core snapshot operations using SPDK RPC
//! 
//! This service wraps SPDK bdev_lvol_snapshot operations without touching
//! existing volume management code. All operations are isolated and independent.

use serde_json::{json, Value};
use crate::minimal_models::MinimalStateError;
use crate::spdk_native::SpdkNative;
use super::snapshot_models::*;

/// Isolated snapshot service - no dependencies on existing volume code
#[derive(Clone)]
pub struct SnapshotService {
    /// Node name where this service is running
    pub node_name: String,
    /// SPDK RPC socket path (e.g., "unix:///var/tmp/spdk.sock")
    pub spdk_socket_path: String,
}

impl SnapshotService {
    /// Create new snapshot service instance
    pub fn new(node_name: String, spdk_socket_path: String) -> Self {
        Self {
            node_name,
            spdk_socket_path,
        }
    }

    /// Create SPDK snapshot - completely independent operation
    /// 
    /// Uses `bdev_lvol_snapshot` RPC to create a read-only point-in-time snapshot.
    /// Snapshots use copy-on-write, so creation is instant with minimal space usage.
    /// 
    /// # Arguments
    /// * `lvol_name` - Source lvol UUID or name (e.g., "vol_pvc-abc123")
    /// * `snapshot_name` - Unique name for snapshot (e.g., "snap_pvc-abc123_1234567890")
    /// 
    /// # Returns
    /// SPDK snapshot UUID on success
    pub async fn create_snapshot(
        &self,
        lvol_name: &str,
        snapshot_name: &str,
    ) -> Result<CreateSnapshotResponse, MinimalStateError> {
        println!("📸 [SNAPSHOT_SERVICE] Creating snapshot: {} from lvol: {}", 
                 snapshot_name, lvol_name);

        // Clean socket path (remove unix:// prefix if present)
        let socket_path = self.spdk_socket_path.trim_start_matches("unix://");

        // Connect to SPDK
        let spdk = SpdkNative::new(Some(socket_path.to_string())).await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to connect to SPDK: {}", e),
            })?;

        // Call bdev_lvol_snapshot
        let params = json!({
            "lvol_name": lvol_name,
            "snapshot_name": snapshot_name
        });

        let response = spdk.call_method("bdev_lvol_snapshot", Some(params))
            .await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("SPDK snapshot creation failed: {}", e),
            })?;

        let snapshot_uuid = response.as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError {
                message: "No UUID in SPDK snapshot response".to_string(),
            })?
            .to_string();

        println!("✅ [SNAPSHOT_SERVICE] Snapshot created with UUID: {}", snapshot_uuid);

        // Get snapshot details for response
        let snapshot_info = self.get_snapshot_details(&snapshot_uuid).await?;

        Ok(CreateSnapshotResponse {
            snapshot_uuid,
            snapshot_name: snapshot_name.to_string(),
            source_lvol: lvol_name.to_string(),
            lvs_name: snapshot_info.lvs_name,
            creation_time: chrono::Utc::now().to_rfc3339(),
            size_bytes: snapshot_info.size_bytes,
        })
    }

    /// Clone snapshot to create writable volume
    /// 
    /// Uses `bdev_lvol_clone` RPC to create a writable lvol from a read-only snapshot.
    /// Clones also use copy-on-write, so creation is fast.
    /// 
    /// # Arguments
    /// * `snapshot_name` - Source snapshot UUID or name
    /// * `clone_name` - Name for the new writable clone
    /// 
    /// # Returns
    /// SPDK clone UUID on success
    pub async fn clone_snapshot(
        &self,
        snapshot_name: &str,
        clone_name: &str,
    ) -> Result<CloneSnapshotResponse, MinimalStateError> {
        println!("🔄 [SNAPSHOT_SERVICE] Cloning snapshot: {} to: {}", 
                 snapshot_name, clone_name);

        let socket_path = self.spdk_socket_path.trim_start_matches("unix://");

        let spdk = SpdkNative::new(Some(socket_path.to_string())).await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to connect to SPDK: {}", e),
            })?;

        // Call bdev_lvol_clone
        let params = json!({
            "snapshot_name": snapshot_name,
            "clone_name": clone_name
        });

        let response = spdk.call_method("bdev_lvol_clone", Some(params))
            .await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("SPDK clone creation failed: {}", e),
            })?;

        let clone_uuid = response.as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError {
                message: "No UUID in SPDK clone response".to_string(),
            })?
            .to_string();

        println!("✅ [SNAPSHOT_SERVICE] Clone created with UUID: {}", clone_uuid);

        // Get clone details
        println!("🔍 [SNAPSHOT_SERVICE] Getting clone details for: {}", clone_uuid);
        let clone_info = self.get_snapshot_details(&clone_uuid).await?;
        
        println!("📋 [SNAPSHOT_SERVICE] Clone details: lvs_name={:?}, size={} bytes", 
                 clone_info.lvs_name, clone_info.size_bytes);

        Ok(CloneSnapshotResponse {
            clone_uuid,
            clone_name: clone_name.to_string(),
            lvs_name: clone_info.lvs_name,
            size_bytes: clone_info.size_bytes,
        })
    }

    /// Delete a snapshot
    /// 
    /// Uses `bdev_lvol_delete` RPC (same as deleting a regular lvol).
    /// Snapshots can be deleted even if clones exist (clones become independent).
    /// 
    /// # Arguments
    /// * `snapshot_uuid` - SPDK UUID of snapshot to delete
    pub async fn delete_snapshot(
        &self,
        snapshot_uuid: &str,
    ) -> Result<DeleteSnapshotResponse, MinimalStateError> {
        println!("🗑️ [SNAPSHOT_SERVICE] Deleting snapshot: {}", snapshot_uuid);

        let socket_path = self.spdk_socket_path.trim_start_matches("unix://");

        let spdk = SpdkNative::new(Some(socket_path.to_string())).await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to connect to SPDK: {}", e),
            })?;

        // Call bdev_lvol_delete
        let params = json!({
            "name": snapshot_uuid
        });

        spdk.call_method("bdev_lvol_delete", Some(params))
            .await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("SPDK snapshot deletion failed: {}", e),
            })?;

        println!("✅ [SNAPSHOT_SERVICE] Snapshot deleted: {}", snapshot_uuid);

        Ok(DeleteSnapshotResponse {
            success: true,
            message: Some(format!("Snapshot {} deleted", snapshot_uuid)),
        })
    }

    /// List all snapshots on this node
    /// 
    /// Queries all lvols from SPDK and filters for snapshots based on naming convention.
    /// Our snapshots have names starting with "snap_".
    pub async fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>, MinimalStateError> {
        println!("📋 [SNAPSHOT_SERVICE] Listing snapshots on node: {}", self.node_name);

        let socket_path = self.spdk_socket_path.trim_start_matches("unix://");

        let spdk = SpdkNative::new(Some(socket_path.to_string())).await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to connect to SPDK: {}", e),
            })?;

        // Get all bdevs (includes full driver_specific info with snapshot flag)
        let bdevs = spdk.call_method("bdev_get_bdevs", None)
            .await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to list bdevs: {}", e),
            })?;

        let mut snapshots = Vec::new();

        if let Some(lvol_list) = bdevs.as_array() {
            for lvol in lvol_list {
                // Check if this lvol is a snapshot using SPDK's snapshot flag
                let is_snapshot = lvol.get("driver_specific")
                    .and_then(|ds| ds.get("lvol"))
                    .and_then(|lv| lv.get("snapshot"))
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);
                
                if is_snapshot {
                    let uuid = lvol["uuid"].as_str().unwrap_or("");
                    
                    // Get the human-readable name from aliases
                    let snapshot_name = lvol.get("aliases")
                        .and_then(|a| a.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|a| a.as_str())
                        .unwrap_or(uuid);
                    
                    // Extract just the snapshot part from alias (lvs_name/snap_...)
                    let simple_name = snapshot_name.split('/').last().unwrap_or(snapshot_name);
                    
                    let snapshot_info = SnapshotInfo {
                        snapshot_uuid: uuid.to_string(),
                        snapshot_name: simple_name.to_string(),
                        source_volume_id: SnapshotInfo::volume_id_from_snapshot_name(simple_name),
                        node_name: self.node_name.clone(),
                        lvs_name: self.extract_lvs_name_from_lvol(lvol),
                        size_bytes: self.calculate_lvol_size(lvol),
                        creation_time: chrono::Utc::now().to_rfc3339(), // TODO: Store actual creation time
                        ready_to_use: true,
                    };
                    snapshots.push(snapshot_info);
                }
            }
        }

        println!("✅ [SNAPSHOT_SERVICE] Found {} snapshots", snapshots.len());
        Ok(snapshots)
    }

    /// Find a specific snapshot by UUID
    pub async fn find_snapshot(&self, snapshot_uuid: &str) -> Result<Option<SnapshotInfo>, MinimalStateError> {
        let snapshots = self.list_snapshots().await?;
        Ok(snapshots.into_iter().find(|s| s.snapshot_uuid == snapshot_uuid))
    }

    // === Private Helper Methods ===

    /// Get detailed information about a snapshot/lvol
    async fn get_snapshot_details(&self, uuid: &str) -> Result<SnapshotInfo, MinimalStateError> {
        let socket_path = self.spdk_socket_path.trim_start_matches("unix://");

        let spdk = SpdkNative::new(Some(socket_path.to_string())).await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to connect to SPDK: {}", e),
            })?;

        // Get lvol details
        let params = json!({
            "name": uuid
        });

        let response = spdk.call_method("bdev_get_bdevs", Some(params))
            .await
            .map_err(|e| MinimalStateError::SpdkRpcError {
                message: format!("Failed to get bdev info: {}", e),
            })?;

        if let Some(bdev_list) = response.as_array() {
            if let Some(bdev) = bdev_list.first() {
                // Get the human-readable name from aliases
                let snapshot_name = bdev.get("aliases")
                    .and_then(|a| a.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|a| a.as_str())
                    .unwrap_or(uuid);
                
                // Extract just the snapshot part from alias (lvs_name/snap_...)
                let simple_name = snapshot_name.split('/').last().unwrap_or(snapshot_name);
                
                return Ok(SnapshotInfo {
                    snapshot_uuid: uuid.to_string(),
                    snapshot_name: simple_name.to_string(),
                    source_volume_id: SnapshotInfo::volume_id_from_snapshot_name(simple_name),
                    node_name: self.node_name.clone(),
                    lvs_name: self.extract_lvs_name_from_lvol(bdev),
                    size_bytes: self.calculate_lvol_size(bdev),
                    creation_time: chrono::Utc::now().to_rfc3339(),
                    ready_to_use: true,
                });
            }
        }

        Err(MinimalStateError::SpdkRpcError {
            message: format!("Snapshot {} not found", uuid),
        })
    }

    /// Extract LVS name from lvol JSON
    fn extract_lvs_name_from_lvol(&self, lvol: &Value) -> Option<String> {
        println!("🔍 [SNAPSHOT_SERVICE] Extracting LVS name from lvol JSON");
        
        // The lvol alias contains the LVS name: "lvs_name/vol_name"
        let alias = lvol.get("aliases")
            .and_then(|a| a.as_array())
            .and_then(|arr| arr.first())
            .and_then(|a| a.as_str());
        
        if let Some(alias_str) = alias {
            println!("🔍 [SNAPSHOT_SERVICE] Found alias: {}", alias_str);
            // Extract LVS name from alias (format: "lvs_name/vol_or_snap_name")
            if let Some(lvs_name) = alias_str.split('/').next() {
                println!("✅ [SNAPSHOT_SERVICE] Extracted LVS name from alias: {}", lvs_name);
                return Some(lvs_name.to_string());
            }
        }
        
        println!("⚠️ [SNAPSHOT_SERVICE] Could not extract LVS name from lvol JSON");
        println!("   Aliases: {:?}", lvol.get("aliases"));
        // Note: Full lvol JSON not logged (too verbose)
        None
    }

    /// Calculate lvol size from JSON
    fn calculate_lvol_size(&self, lvol: &Value) -> u64 {
        let num_blocks = lvol["num_blocks"].as_u64().unwrap_or(0);
        let block_size = lvol["block_size"].as_u64().unwrap_or(0);
        num_blocks * block_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_cleaning() {
        let service = SnapshotService::new(
            "test-node".to_string(),
            "unix:///var/tmp/spdk.sock".to_string(),
        );
        
        let cleaned = service.spdk_socket_path.trim_start_matches("unix://");
        assert_eq!(cleaned, "/var/tmp/spdk.sock");
    }
}

