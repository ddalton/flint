//! Restore from a `VolumeSnapshot` with the StorageClass's replica count
//! (2026-10-27).
//!
//! A restore clones ONE verified copy of the snapshot
//! (`multi_replica::pick_snapshot_source`). The copies of a multi-replica
//! snapshot are equivalent crash-consistent images, not identical ones
//! (incremental-rebuild §11, "cut skew"), so cloning each node's own copy
//! would assemble a raid whose legs differ. Instead the clone is leg 0 and
//! every further leg starts EMPTY and STALE: the catch-up cuts a first
//! epoch on the clone (`catchup::ensure_epoch_for_rebuild`) and
//! full-builds the others from it — bit-identical copies, by the same
//! machinery a re-placed leg uses. Until 2026-10-27 a restore ignored
//! `numReplicas`: dash1's `r3-restore` came up single-replica on a
//! three-replica class, with no `size` attribute either.
//!
//! The seeded-stale list travels in volumeAttributes
//! ([`SEEDED_STALE_ATTRIBUTE`]), which the provisioner writes atomically
//! with the PV. The sync-record annotation is seeded best-effort after
//! the fact (the PV may not exist yet when the controller patches it),
//! and a record synthesized without it reads every leg as in_sync — the
//! one thing an empty leg must never be.

use std::collections::{HashMap, HashSet};

use crate::minimal_models::ReplicaInfo;
use crate::replica_sync::{
    VolumeSyncRecord, AUTO_REBUILD_ATTRIBUTE, REPLICAS_ATTRIBUTE, REPLICA_COUNT_ATTRIBUTE,
    SEEDED_STALE_ATTRIBUTE,
};

/// A node able to host an extra leg: its max-free initialized disk.
#[derive(Debug, Clone, PartialEq)]
pub struct LegCandidate {
    pub node: String,
    pub free_space: u64,
    pub lvs_name: String,
    pub pci_address: String,
}

/// Pure: the `needed` nodes for the extra legs — max-free first, never the
/// clone's node, one leg per node (two legs of a raid1 on one disk survive
/// nothing). `Err` names the shortfall; the caller refuses the create, as
/// the plain multi-replica create does, rather than deliver fewer copies
/// than the class promised.
pub fn pick_extra_leg_nodes(
    source_node: &str,
    candidates: &[LegCandidate],
    needed: usize,
) -> Result<Vec<LegCandidate>, String> {
    let mut eligible: Vec<&LegCandidate> =
        candidates.iter().filter(|c| c.node != source_node).collect();
    eligible.sort_by(|a, b| b.free_space.cmp(&a.free_space).then_with(|| a.node.cmp(&b.node)));
    let mut seen: HashSet<&str> = HashSet::new();
    let picked: Vec<LegCandidate> = eligible
        .into_iter()
        .filter(|c| seen.insert(c.node.as_str()))
        .take(needed)
        .cloned()
        .collect();
    if picked.len() < needed {
        return Err(format!(
            "only {} of the {} extra replica leg(s) can be placed: no other Ready node with an \
             initialized disk and room (the clone is on {})",
            picked.len(),
            needed,
            source_node
        ));
    }
    Ok(picked)
}

/// Pure: the volumeAttributes of a restored volume. One leg: the shape the
/// single-replica restore always wrote, plus `size`. More: the plain
/// multi-replica shape (`replica-count`, the `replicas` list) plus the
/// seeded-stale list of every leg but the clone.
pub fn restore_volume_context(
    snapshot_id: &str,
    size_bytes: u64,
    replicas: &[ReplicaInfo],
    auto_rebuild: Option<&str>,
) -> Result<HashMap<String, String>, serde_json::Error> {
    let mut ctx = HashMap::new();
    // `size` is what the dashboard and the inline-volume path read; it was
    // missing from every restored PV until 2026-10-27.
    ctx.insert("size".to_string(), format!("{}Gi", size_bytes / (1024 * 1024 * 1024)));
    ctx.insert(REPLICA_COUNT_ATTRIBUTE.to_string(), replicas.len().to_string());
    // The clone carries the snapshot's filesystem; without this the node
    // cannot tell SPDK block reuse from a real filesystem.
    ctx.insert("disk.chert.us/filesystem-initialized".to_string(), "true".to_string());
    ctx.insert("disk.chert.us/source-snapshot".to_string(), snapshot_id.to_string());
    ctx.insert(
        "nfs.chert.us/replica-nodes".to_string(),
        replicas.iter().map(|r| r.node_name.as_str()).collect::<Vec<_>>().join(","),
    );
    if let Some(auto_rebuild) = auto_rebuild {
        ctx.insert(AUTO_REBUILD_ATTRIBUTE.to_string(), auto_rebuild.to_string());
    }
    match replicas {
        [only] => {
            ctx.insert("disk.chert.us/node-name".to_string(), only.node_name.clone());
            ctx.insert("disk.chert.us/lvol-uuid".to_string(), only.lvol_uuid.clone());
            ctx.insert("disk.chert.us/lvs-name".to_string(), only.lvs_name.clone());
        }
        _ => {
            ctx.insert(REPLICAS_ATTRIBUTE.to_string(), serde_json::to_string(replicas)?);
            ctx.insert(
                SEEDED_STALE_ATTRIBUTE.to_string(),
                replicas.iter().skip(1).map(|r| r.lvol_uuid.as_str()).collect::<Vec<_>>().join(","),
            );
        }
    }
    Ok(ctx)
}

/// Pure: the sync record a restore seeds — leg 0 (the clone) in_sync,
/// every other leg stale, no epochs. The catch-up's on-demand first epoch
/// turns the stale legs into full builds from the clone.
pub fn seed_record_for_restore(replicas: &[ReplicaInfo], now_rfc3339: &str) -> VolumeSyncRecord {
    let stale: Vec<String> = replicas.iter().skip(1).map(|r| r.lvol_uuid.clone()).collect();
    VolumeSyncRecord::initial_seeded(replicas, &stale, now_rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replica_sync::{SyncState, SEEDED_STALE_REASON};

    fn replica(node: &str, uuid: &str) -> ReplicaInfo {
        ReplicaInfo {
            node_name: node.to_string(),
            node_uid: format!("uid-{}", node),
            disk_pci_address: "0000:00:1f.0".to_string(),
            lvol_uuid: uuid.to_string(),
            lvol_name: format!("lvol-{}", uuid),
            lvs_name: "lvs0".to_string(),
            nqn: None,
            target_ip: None,
            target_port: None,
            health: "online".to_string(),
        }
    }

    fn cand(node: &str, free: u64) -> LegCandidate {
        LegCandidate {
            node: node.to_string(),
            free_space: free,
            lvs_name: format!("lvs_{}", node),
            pci_address: "0000:00:1f.0".to_string(),
        }
    }

    #[test]
    fn extra_legs_go_to_the_freest_other_nodes() {
        // The clone's node has the most room and is still never picked.
        let cands = vec![cand("src", 99), cand("a", 10), cand("b", 30), cand("c", 20)];
        let picked = pick_extra_leg_nodes("src", &cands, 2).unwrap();
        let nodes: Vec<&str> = picked.iter().map(|c| c.node.as_str()).collect();
        assert_eq!(nodes, vec!["b", "c"]);
        assert_eq!(picked[0].lvs_name, "lvs_b");
    }

    #[test]
    fn too_few_nodes_is_a_refusal_not_a_smaller_volume() {
        let cands = vec![cand("src", 99), cand("a", 10)];
        let err = pick_extra_leg_nodes("src", &cands, 2).unwrap_err();
        assert!(err.contains("only 1 of the 2"), "{}", err);
        // Zero extra legs always places.
        assert!(pick_extra_leg_nodes("src", &[], 0).unwrap().is_empty());
    }

    #[test]
    fn single_leg_context_keeps_the_legacy_shape_and_gains_size() {
        let ctx = restore_volume_context("snap_vol_1", 8 * 1024 * 1024 * 1024, &[replica("n1", "u1")], None)
            .unwrap();
        assert_eq!(ctx["size"], "8Gi");
        assert_eq!(ctx[REPLICA_COUNT_ATTRIBUTE], "1");
        assert_eq!(ctx["disk.chert.us/node-name"], "n1");
        assert_eq!(ctx["disk.chert.us/lvol-uuid"], "u1");
        assert_eq!(ctx["disk.chert.us/lvs-name"], "lvs0");
        assert_eq!(ctx["disk.chert.us/filesystem-initialized"], "true");
        assert_eq!(ctx["disk.chert.us/source-snapshot"], "snap_vol_1");
        assert_eq!(ctx["nfs.chert.us/replica-nodes"], "n1");
        assert!(!ctx.contains_key(REPLICAS_ATTRIBUTE));
        assert!(!ctx.contains_key(SEEDED_STALE_ATTRIBUTE));
        assert!(!ctx.contains_key(AUTO_REBUILD_ATTRIBUTE));
    }

    #[test]
    fn multi_leg_context_carries_the_replica_list_and_the_seeded_stale_legs() {
        let legs = vec![replica("n1", "u1"), replica("n2", "u2"), replica("n3", "u3")];
        let ctx = restore_volume_context("snap_vol_1", 4 * 1024 * 1024 * 1024, &legs, Some("false"))
            .unwrap();
        assert_eq!(ctx["size"], "4Gi");
        assert_eq!(ctx[REPLICA_COUNT_ATTRIBUTE], "3");
        let back: Vec<ReplicaInfo> = serde_json::from_str(&ctx[REPLICAS_ATTRIBUTE]).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].lvol_uuid, "u1");
        assert_eq!(ctx[SEEDED_STALE_ATTRIBUTE], "u2,u3");
        assert_eq!(ctx["nfs.chert.us/replica-nodes"], "n1,n2,n3");
        assert_eq!(ctx[AUTO_REBUILD_ATTRIBUTE], "false");
        assert!(!ctx.contains_key("disk.chert.us/node-name"));
    }

    #[test]
    fn seeded_record_has_the_clone_in_sync_and_the_rest_stale() {
        let legs = vec![replica("n1", "u1"), replica("n2", "u2"), replica("n3", "u3")];
        let record = seed_record_for_restore(&legs, "2026-10-27T21:00:00Z");
        let states: Vec<SyncState> = record.replicas.iter().map(|r| r.sync_state.clone()).collect();
        assert_eq!(states, vec![SyncState::InSync, SyncState::Stale, SyncState::Stale]);
        assert_eq!(record.get("u3").unwrap().reason.as_deref(), Some(SEEDED_STALE_REASON));
        assert!(record.epochs.is_empty());
    }
}
