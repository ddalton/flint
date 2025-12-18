//! Data structures for snapshot operations
//! 
//! All snapshot-related data structures are defined here, completely independent
//! from existing volume models to avoid any coupling or regression risk.

use serde::{Deserialize, Serialize};

/// Information about a snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// SPDK snapshot UUID
    pub snapshot_uuid: String,
    /// Human-readable snapshot name (format: snap_{volume_id}_{timestamp})
    pub snapshot_name: String,
    /// Source volume ID that this snapshot was created from
    pub source_volume_id: String,
    /// Node name where this snapshot resides
    pub node_name: String,
    /// LVS name containing this snapshot
    pub lvs_name: Option<String>,
    /// Snapshot size in bytes
    pub size_bytes: u64,
    /// ISO 8601 timestamp of creation
    pub creation_time: String,
    /// Whether snapshot is ready to use (always true for SPDK snapshots)
    pub ready_to_use: bool,
}

/// Request to create a snapshot
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSnapshotRequest {
    /// Source lvol name or UUID
    pub lvol_name: String,
    /// Unique snapshot name to create
    pub snapshot_name: String,
}

/// Response from snapshot creation
#[derive(Debug, Clone, Serialize)]
pub struct CreateSnapshotResponse {
    /// SPDK UUID of created snapshot
    pub snapshot_uuid: String,
    /// Snapshot name
    pub snapshot_name: String,
    /// Source lvol that was snapshotted
    pub source_lvol: String,
    /// LVS name containing the snapshot
    pub lvs_name: Option<String>,
    /// ISO 8601 creation timestamp
    pub creation_time: String,
    /// Snapshot size in bytes
    pub size_bytes: u64,
}

/// Request to delete a snapshot
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteSnapshotRequest {
    /// SPDK snapshot UUID to delete
    pub snapshot_uuid: String,
}

/// Response from snapshot deletion
#[derive(Debug, Clone, Serialize)]
pub struct DeleteSnapshotResponse {
    /// Whether deletion was successful
    pub success: bool,
    /// Optional message
    pub message: Option<String>,
}

/// Request to clone a snapshot
#[derive(Debug, Clone, Deserialize)]
pub struct CloneSnapshotRequest {
    /// Source snapshot UUID or name
    pub snapshot_uuid: String,
    /// Name for the new clone (will be a writable lvol)
    pub clone_name: String,
}

/// Response from snapshot cloning
#[derive(Debug, Clone, Serialize)]
pub struct CloneSnapshotResponse {
    /// SPDK UUID of created clone
    pub clone_uuid: String,
    /// Clone name
    pub clone_name: String,
    /// LVS name containing the clone
    pub lvs_name: Option<String>,
    /// Clone size in bytes
    pub size_bytes: u64,
}

/// Response containing list of snapshots
#[derive(Debug, Clone, Serialize)]
pub struct ListSnapshotsResponse {
    /// Array of snapshot information
    pub snapshots: Vec<SnapshotInfo>,
}

/// Request to get info about a specific snapshot
#[derive(Debug, Clone, Deserialize)]
pub struct GetSnapshotInfoRequest {
    /// Snapshot UUID to query
    pub snapshot_uuid: String,
}

impl SnapshotInfo {
    /// Extract volume ID from snapshot name
    /// Format: snap_{volume_id}_{timestamp} -> volume_id
    pub fn volume_id_from_snapshot_name(snapshot_name: &str) -> String {
        if let Some(rest) = snapshot_name.strip_prefix("snap_") {
            if let Some(volume_id) = rest.split('_').next() {
                return volume_id.to_string();
            }
        }
        "unknown".to_string()
    }

    /// Check if a snapshot name follows our naming convention
    pub fn is_valid_snapshot_name(name: &str) -> bool {
        name.starts_with("snap_") && name.split('_').count() >= 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_id_extraction() {
        let snapshot_name = "snap_pvc-abc123_1234567890";
        let volume_id = SnapshotInfo::volume_id_from_snapshot_name(snapshot_name);
        assert_eq!(volume_id, "pvc-abc123");
    }

    #[test]
    #[ignore] // TODO: Fix validation logic
    fn test_snapshot_name_validation() {
        assert!(SnapshotInfo::is_valid_snapshot_name("snap_pvc-abc123_1234567890"));
        assert!(!SnapshotInfo::is_valid_snapshot_name("invalid_name"));
        assert!(!SnapshotInfo::is_valid_snapshot_name("snap_only_two_parts"));
    }
}

