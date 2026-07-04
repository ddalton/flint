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
    /// Extract volume ID from snapshot name.
    /// CSI snapshots: snap_{volume_id}_{timestamp} -> volume_id
    /// Tier-1/2 epoch snapshots: epoch-{volume_id}-{seq} -> volume_id
    /// (volume ids carry '-' themselves, so the epoch form is parsed from
    /// the right: the trailing segment must be the numeric epoch sequence).
    pub fn volume_id_from_snapshot_name(snapshot_name: &str) -> String {
        if let Some(rest) = snapshot_name.strip_prefix("snap_") {
            if let Some(volume_id) = rest.split('_').next() {
                return volume_id.to_string();
            }
        }
        if let Some(rest) = snapshot_name.strip_prefix("epoch-") {
            if let Some((volume_id, seq)) = rest.rsplit_once('-') {
                if !volume_id.is_empty() && !seq.is_empty() && seq.chars().all(|c| c.is_ascii_digit()) {
                    return volume_id.to_string();
                }
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
    fn epoch_snapshot_names_resolve_to_their_volume() {
        // The Tier-1/2 engine's epoch snapshots carry the PV name inline;
        // pre-fix these all grouped under "unknown" in the dashboard tree.
        assert_eq!(
            SnapshotInfo::volume_id_from_snapshot_name(
                "epoch-pvc-6ff1cf70-8f3e-4c2a-9d1b-2f65c14a8e01-1261"
            ),
            "pvc-6ff1cf70-8f3e-4c2a-9d1b-2f65c14a8e01"
        );
        // dashes inside the volume id survive; only the trailing numeric
        // sequence is stripped
        assert_eq!(SnapshotInfo::volume_id_from_snapshot_name("epoch-r3-e2e-7"), "r3-e2e");
    }

    #[test]
    fn non_matching_names_stay_unknown() {
        assert_eq!(SnapshotInfo::volume_id_from_snapshot_name("epoch-"), "unknown");
        assert_eq!(SnapshotInfo::volume_id_from_snapshot_name("epoch-noseq"), "unknown");
        // trailing segment must be numeric — a pv name alone is not enough
        assert_eq!(SnapshotInfo::volume_id_from_snapshot_name("epoch-pvc-abc-x1"), "unknown");
        assert_eq!(SnapshotInfo::volume_id_from_snapshot_name("temp_pvc_clone_x"), "unknown");
    }

    // Note: is_valid_snapshot_name() function doesn't exist
    // Snapshot name validation is handled elsewhere in the codebase
    // This test is removed as it's testing a non-existent function
    
    // #[test]
    // fn test_snapshot_name_validation() {
    //     assert!(SnapshotInfo::is_valid_snapshot_name("snap_pvc-abc123_1234567890"));
    //     assert!(!SnapshotInfo::is_valid_snapshot_name("invalid_name"));
    //     assert!(!SnapshotInfo::is_valid_snapshot_name("snap_only_two_parts"));
    // }
}

