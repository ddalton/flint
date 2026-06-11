//! Multi-replica user snapshots (incremental-rebuild §11, phase 5b).
//!
//! A multi-replica `VolumeSnapshot` is one snapshot with N equivalent
//! copies: the same name (`snap_<vol>_<suffix>`) cut on every in-sync
//! replica's lvolstore, exactly the epoch pattern and with the same
//! machinery (`execute_cut`: all-or-abort, EEXIST-converges, rollback on
//! partial failure, live-uuid addressing). The CSI snapshot id is the NAME
//! — each copy has its own SPDK uuid, so only the common name identifies
//! the snapshot, and it conveniently embeds the source volume.
//!
//! Per-copy skew is the documented semantic: cuts are not simultaneous;
//! raid1 fans every acked write to all legs, so each copy is a valid
//! crash-consistent image, but two copies (and so two restores) may differ
//! by writes acked inside the skew window. User snapshots play no role in
//! the §5 delta-resync correctness proof — but they DO interleave into the
//! replicas' snapshot chains, which is why the catch-up copies the actual
//! blob lineage rather than epochs-by-name (catchup.rs, §11 delta-split
//! hazard).
//!
//! Deletion is per-replica fan-out with tombstones for whatever could not
//! be confirmed gone (unreachable node, clone-pinned copy): reaping is
//! tombstone-driven, never absence-driven, and the catch-up reconciles
//! tombstones at heal time. Restore picks its source by VERIFIED presence
//! (listing the replica's lvols — never inferred from records), preferring
//! an in-sync replica off the consumer node.

use serde_json::json;
use tracing::{debug, warn};

use crate::epoch_scheduler::{
    execute_cut, is_missing, CutOutcome, CutPlan, EpochTarget, NodeRpc, RpcError,
};
use crate::minimal_models::ReplicaInfo;
use crate::replica_sync::{SyncState, VolumeSyncRecord};

/// Cut `snapshot_name` on every in-sync replica (live head uuids),
/// all-or-abort. Returns the identity uuids that were cut. An EEXIST on a
/// replica converges (a leftover copy from an aborted earlier attempt was
/// cut from the same head); a hard failure rolls the others back and
/// errors — the CSI retry re-runs the whole cut idempotently.
pub async fn cut_snapshot_on_replicas(
    rpc: &dyn NodeRpc,
    volume_id: &str,
    snapshot_name: &str,
    replicas: &[ReplicaInfo],
    record: &VolumeSyncRecord,
) -> Result<Vec<String>, RpcError> {
    let targets: Vec<EpochTarget> = record
        .replicas
        .iter()
        .filter(|rec| rec.sync_state == SyncState::InSync)
        .filter_map(|rec| {
            replicas
                .iter()
                .find(|ri| ri.lvol_uuid == rec.lvol_uuid)
                .map(|ri| EpochTarget {
                    node_name: ri.node_name.clone(),
                    lvol_uuid: ri.lvol_uuid.clone(),
                    snapshot_source: rec.live_lvol_uuid().to_string(),
                    lvs_name: ri.lvs_name.clone(),
                })
        })
        .collect();
    if targets.is_empty() {
        return Err(format!(
            "volume {} has no in-sync replica to snapshot",
            volume_id
        )
        .into());
    }

    let plan = CutPlan { epoch: snapshot_name.to_string(), targets };
    match execute_cut(rpc, &plan).await {
        CutOutcome::Recorded { cut_uuids } => Ok(cut_uuids),
        CutOutcome::Aborted { failures } => Err(format!(
            "snapshot {} failed on {} replica(s): {}",
            snapshot_name,
            failures.len(),
            failures
                .iter()
                .map(|(node, e)| format!("{}: {}", node, e))
                .collect::<Vec<_>>()
                .join("; ")
        )
        .into()),
    }
}

/// Delete `snapshot_name`'s copy from every replica. Absent = success
/// (idempotent). Returns the identity uuids whose copy could NOT be
/// confirmed gone — an unreachable node, or a copy pinned by a restore
/// clone (`-EPERM`, blob has clones; the pin is local to that node) — for
/// the caller to tombstone. A copy with only its chain descendant deletes
/// via the blobstore's snapshot-delete merge, like epoch retirement.
pub async fn delete_snapshot_on_replicas(
    rpc: &dyn NodeRpc,
    snapshot_name: &str,
    replicas: &[ReplicaInfo],
) -> Vec<String> {
    let mut pending: Vec<String> = Vec::new();
    for replica in replicas {
        let alias = format!("{}/{}", replica.lvs_name, snapshot_name);
        let payload = json!({ "method": "bdev_lvol_delete", "params": { "name": alias } });
        match rpc.spdk_rpc(&replica.node_name, &payload).await {
            Ok(_) => {}
            Err(e) if is_missing(&e.to_string()) => {} // already gone
            Err(e) => {
                debug!(
                    node = %replica.node_name, snapshot = %snapshot_name, error = %e,
                    "[SNAPSHOT_MR] Copy not deletable now — tombstoning for heal-time reconcile"
                );
                pending.push(replica.lvol_uuid.clone());
            }
        }
    }
    pending
}

/// Pick the restore source: a replica VERIFIED (by listing its lvols) to
/// hold the snapshot's copy, preferring in-sync, preferring a node other
/// than the volume's consumer. Presence is never inferred from records —
/// a replica healed past the snapshot's cut has the copy (lineage replay),
/// a replaced one does not, and only the node knows.
pub async fn pick_snapshot_source(
    rpc: &dyn NodeRpc,
    snapshot_name: &str,
    replicas: &[ReplicaInfo],
    record: &VolumeSyncRecord,
    consumer_node: Option<&str>,
) -> Option<ReplicaInfo> {
    let mut holders: Vec<&ReplicaInfo> = Vec::new();
    for replica in replicas {
        let payload = json!({
            "method": "bdev_lvol_get_lvols",
            "params": { "lvs_name": replica.lvs_name }
        });
        let names: Vec<String> = match rpc.spdk_rpc(&replica.node_name, &payload).await {
            Ok(resp) => resp
                .get("result")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            Err(e) => {
                warn!(node = %replica.node_name, error = %e, "[SNAPSHOT_MR] Replica unreachable during restore source selection");
                continue;
            }
        };
        if names.iter().any(|n| n == snapshot_name) {
            holders.push(replica);
        }
    }

    let in_sync = |ri: &ReplicaInfo| {
        record
            .get(&ri.lvol_uuid)
            .map(|r| r.sync_state == SyncState::InSync)
            .unwrap_or(true)
    };
    let off_consumer = |ri: &ReplicaInfo| consumer_node != Some(ri.node_name.as_str());

    holders
        .iter()
        .find(|ri| in_sync(ri) && off_consumer(ri))
        .or_else(|| holders.iter().find(|ri| in_sync(ri)))
        .or_else(|| holders.first())
        .map(|ri| (*ri).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn replica(node: &str, uuid: &str) -> ReplicaInfo {
        ReplicaInfo {
            node_name: node.to_string(),
            node_uid: format!("uid-{}", node),
            disk_pci_address: "0000:00:1e.0".to_string(),
            lvol_uuid: uuid.to_string(),
            lvol_name: format!("lvol-{}", uuid),
            lvs_name: "lvs0".to_string(),
            nqn: None,
            target_ip: None,
            target_port: None,
            health: "online".to_string(),
        }
    }

    fn replicas3() -> Vec<ReplicaInfo> {
        vec![
            replica("node-a", "uuid-a"),
            replica("node-b", "uuid-b"),
            replica("node-c", "uuid-c"),
        ]
    }

    struct FakeRpc {
        calls: Mutex<Vec<(String, Value)>>,
        fail: HashMap<(String, String), String>,
        /// node → lvol names returned by bdev_lvol_get_lvols
        lvols: HashMap<String, Vec<String>>,
    }

    impl FakeRpc {
        fn new() -> Self {
            FakeRpc { calls: Mutex::new(Vec::new()), fail: HashMap::new(), lvols: HashMap::new() }
        }

        fn calls_of(&self, method: &str) -> Vec<(String, Value)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, p)| p["method"].as_str() == Some(method))
                .cloned()
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl NodeRpc for FakeRpc {
        async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError> {
            let method = payload["method"].as_str().unwrap_or("").to_string();
            self.calls.lock().unwrap().push((node.to_string(), payload.clone()));
            if let Some(err) = self.fail.get(&(node.to_string(), method.clone())) {
                return Err(err.clone().into());
            }
            if method == "bdev_lvol_get_lvols" {
                let arr: Vec<Value> = self
                    .lvols
                    .get(node)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|n| json!({ "name": n }))
                    .collect();
                return Ok(json!({ "result": arr }));
            }
            Ok(json!({ "result": "ok" }))
        }
    }

    /// a and c in sync, b stale with a reverted (live-uuid) head.
    fn record_b_stale() -> VolumeSyncRecord {
        let mut record = VolumeSyncRecord::initial(&replicas3());
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.replicas[0].active_lvol_uuid = Some("uuid-a-v2".to_string());
        record
    }

    #[tokio::test]
    async fn cut_targets_in_sync_replicas_by_live_uuid() {
        let rpc = FakeRpc::new();
        let cut = cut_snapshot_on_replicas(
            &rpc, "vol1", "snap_vol1_99", &replicas3(), &record_b_stale(),
        )
        .await
        .unwrap();
        // Identity uuids returned; the stale replica is not cut.
        assert_eq!(cut, vec!["uuid-a".to_string(), "uuid-c".to_string()]);
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        let targets: Vec<(&str, &str)> = snaps
            .iter()
            .map(|(n, p)| (n.as_str(), p["params"]["lvol_name"].as_str().unwrap()))
            .collect();
        // a is addressed by its LIVE (post-revert) head uuid.
        assert_eq!(targets, vec![("node-a", "uuid-a-v2"), ("node-c", "uuid-c")]);
        assert!(snaps.iter().all(|(_, p)| p["params"]["snapshot_name"] == "snap_vol1_99"));
    }

    #[tokio::test]
    async fn cut_aborts_and_rolls_back_on_partial_failure() {
        let mut rpc = FakeRpc::new();
        rpc.fail.insert(
            ("node-c".to_string(), "bdev_lvol_snapshot".to_string()),
            "connection refused".to_string(),
        );
        let err = cut_snapshot_on_replicas(
            &rpc, "vol1", "snap_vol1_99", &replicas3(), &record_b_stale(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("node-c"), "got: {}", err);
        // The copy that succeeded (node-a) was rolled back.
        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].0, "node-a");
        assert_eq!(deletes[0].1["params"]["name"], "lvs0/snap_vol1_99");
    }

    #[tokio::test]
    async fn cut_with_no_in_sync_replica_is_an_error() {
        let mut record = record_b_stale();
        record.mark_stale("uuid-a", "x", "t");
        record.mark_stale("uuid-c", "x", "t");
        let rpc = FakeRpc::new();
        let err = cut_snapshot_on_replicas(&rpc, "vol1", "snap_vol1_99", &replicas3(), &record)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no in-sync replica"), "got: {}", err);
    }

    #[tokio::test]
    async fn delete_fans_out_and_tombstones_only_failures() {
        let mut rpc = FakeRpc::new();
        // b's node is unreachable; c's copy is pinned by a restore clone.
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_delete".to_string()),
            "connection refused".to_string(),
        );
        rpc.fail.insert(
            ("node-c".to_string(), "bdev_lvol_delete".to_string()),
            "SPDK RPC error Code=-1: Operation not permitted".to_string(),
        );
        let pending = delete_snapshot_on_replicas(&rpc, "snap_vol1_99", &replicas3()).await;
        assert_eq!(pending, vec!["uuid-b".to_string(), "uuid-c".to_string()]);
        // Every replica was attempted, by lvs/name alias.
        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert_eq!(deletes.len(), 3);
        assert!(deletes.iter().all(|(_, p)| p["params"]["name"] == "lvs0/snap_vol1_99"));
    }

    #[tokio::test]
    async fn delete_treats_missing_copy_as_success() {
        let mut rpc = FakeRpc::new();
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_delete".to_string()),
            "Code=-19: No such device".to_string(),
        );
        let pending = delete_snapshot_on_replicas(&rpc, "snap_vol1_99", &replicas3()).await;
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn restore_source_requires_verified_presence() {
        let mut rpc = FakeRpc::new();
        // Only b and c actually hold the copy (a was healed past it... or
        // replaced); b is stale, c is in-sync.
        rpc.lvols.insert("node-a".to_string(), vec!["lvol-uuid-a".to_string()]);
        rpc.lvols.insert(
            "node-b".to_string(),
            vec!["lvol-uuid-b".to_string(), "snap_vol1_99".to_string()],
        );
        rpc.lvols.insert(
            "node-c".to_string(),
            vec!["lvol-uuid-c".to_string(), "snap_vol1_99".to_string()],
        );
        let record = record_b_stale();

        // In-sync holder preferred over the stale holder.
        let src = pick_snapshot_source(&rpc, "snap_vol1_99", &replicas3(), &record, None)
            .await
            .unwrap();
        assert_eq!(src.node_name, "node-c");

        // Consumer on c: no other in-sync holder exists, so c still wins
        // (off-consumer is a preference, not a requirement).
        let src = pick_snapshot_source(&rpc, "snap_vol1_99", &replicas3(), &record, Some("node-c"))
            .await
            .unwrap();
        assert_eq!(src.node_name, "node-c");

        // Only the stale replica holds it: better than nothing — its copy
        // is a valid crash-consistent image from when it was in-sync.
        rpc.lvols.insert("node-c".to_string(), vec!["lvol-uuid-c".to_string()]);
        let src = pick_snapshot_source(&rpc, "snap_vol1_99", &replicas3(), &record, None)
            .await
            .unwrap();
        assert_eq!(src.node_name, "node-b");

        // Nobody holds it: no source.
        rpc.lvols.insert("node-b".to_string(), vec![]);
        assert!(pick_snapshot_source(&rpc, "snap_vol1_99", &replicas3(), &record, None)
            .await
            .is_none());
    }
}
