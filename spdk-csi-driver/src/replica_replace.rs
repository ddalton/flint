//! U11 — replica re-placement after permanent node loss.
//!
//! The catch-up orchestrator heals replicas whose node RETURNS; nothing
//! rebuilt redundancy when a node was permanently gone (EC2 terminate, spot
//! reclaim, dead hardware) — the volume served degraded forever. This module
//! swaps the lost leg's identity onto a healthy node and lets the existing
//! machinery do the heavy lifting.
//!
//! Flow (a pre-pass of the per-volume catch-up task, under the same
//! volume claim):
//!   1. Candidate: a Stale, un-hot-rejoin-marked replica whose Node object
//!      is deleted, or NotReady for longer than
//!      FLINT_REPLICA_REPLACE_AFTER_SECS. One replacement per volume/tick.
//!   2. Guards: an in_sync source exists, epoch history exists (the full
//!      build needs a target), the PV is not RWX (its synthetic backing PV
//!      mirrors identity attributes — swap choreography is future work),
//!      not being deleted, autoRebuild != "false", orchestrator enabled.
//!   3. A placeholder lvol named {volume}_replica_{index} is created on the
//!      max-free healthy node hosting no other leg. The thin-aware full
//!      build later recreates it empty, sized to the source
//!      (`revert_head_to_empty`), so size/thin here only reserve capacity.
//!   4. ONE resourceVersion-guarded PV metadata patch atomically writes the
//!      identity override annotation, the swapped sync record (new uuid
//!      enters STALE explicitly; since the C2 laundering pins,
//!      `reconcile_membership` also defaults unknown identities to STALE —
//!      but identity and record must still never be observable apart), the
//!      writer-set prune for the lost leg (`prune_writers_for_replacement`
//!      — replacement is the ONLY writer-set exit), and the replica
//!      node-label swap.
//!   5. The same tick's catch-up finds a stale replica with no shared
//!      history on a reachable node → §9-5 thin-aware full build → standby
//!      → chase → hot-rejoin admits it into the live (degraded) raid. A
//!      direct-serve volume (single survivor, no raid object) admits at its
//!      next stage instead.
//!
//! The lost node's old lvol is unreferenced from the moment the override
//! lands; if the node ever returns, the orphan sweep reaps it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Node, PersistentVolume};
use kube::api::{Api, Patch, PatchParams};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::catchup::{CatchupRpc, CatchupStore, RpcError};
use crate::driver::SpdkCsiDriver;
use crate::minimal_models::ReplicaInfo;
use crate::replica_sync::{
    self, ReplicaSyncRecord, SyncState, VolumeSyncRecord, REPLICAS_OVERRIDE_ANNOTATION,
    SYNC_STATE_ANNOTATION,
};

#[derive(Clone, Debug)]
pub struct ReplaceConfig {
    /// FLINT_REPLICA_REPLACE — anything but "disabled" enables. Rendered by
    /// the chart inside the replication.orchestrators block: re-placement
    /// without catch-up would strand an empty leg in STALE forever.
    pub enabled: bool,
    /// FLINT_REPLICA_REPLACE_AFTER_SECS (default 600): how long a node must
    /// be NotReady before its legs are treated as permanently lost. A
    /// DELETED Node object skips the wait — deletion is the explicit
    /// "not coming back" signal (cloud node lifecycle, drill 2.4, operator).
    pub after: Duration,
}

impl ReplaceConfig {
    pub fn from_env() -> Self {
        ReplaceConfig {
            enabled: std::env::var("FLINT_REPLICA_REPLACE")
                .map(|v| v != "disabled")
                .unwrap_or(true),
            after: Duration::from_secs(
                std::env::var("FLINT_REPLICA_REPLACE_AFTER_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(600),
            ),
        }
    }
}

/// What the API said about a replica's node. Timestamps are unix seconds
/// (k8s_openapi carries jiff timestamps; seconds keep the decision pure).
#[derive(Debug, Clone, PartialEq)]
pub enum NodePresence {
    Absent,
    Present { ready: bool, not_ready_since_epoch_s: Option<i64> },
}

/// Pure decision core: is this node permanently gone for re-placement
/// purposes? Absent = gone immediately. NotReady = gone only past the
/// threshold, and only when the transition time is known — a Ready
/// condition with unknown transition never condemns a node.
pub fn node_gone(presence: &NodePresence, after: Duration, now_epoch_s: i64) -> bool {
    match presence {
        NodePresence::Absent => true,
        NodePresence::Present { ready: true, .. } => false,
        NodePresence::Present { ready: false, not_ready_since_epoch_s: Some(t) } => {
            now_epoch_s.saturating_sub(*t) >= after.as_secs() as i64
        }
        NodePresence::Present { ready: false, not_ready_since_epoch_s: None } => false,
    }
}

/// Pure: pick the max-free candidate not hosting an existing leg.
pub fn pick_replacement_node(
    candidates: &[(String, u64)],
    excluded: &HashSet<String>,
) -> Option<String> {
    candidates
        .iter()
        .filter(|(name, _)| !excluded.contains(name))
        .max_by_key(|(_, free)| *free)
        .map(|(name, _)| name.clone())
}

/// Pure: the swapped sync record. The old uuid's entry is replaced in place
/// (STALE, reason recorded); membership is then reconciled against the new
/// identity list so order mirrors it positionally. None when the old uuid is
/// no longer in the record (a concurrent writer already swapped it).
pub fn build_swapped_record(
    record: &VolumeSyncRecord,
    old_uuid: &str,
    new_rec: ReplicaSyncRecord,
    new_replicas: &[ReplicaInfo],
) -> Option<VolumeSyncRecord> {
    let mut record = record.clone();
    let pos = record.replicas.iter().position(|r| r.lvol_uuid == old_uuid)?;
    record.replicas[pos] = new_rec;
    record.reconcile_membership(new_replicas);
    // C2 pin: replacement is the ONLY writer-set exit. The old leg's node is
    // verifiably gone (that is what qualified it for replacement), so its
    // acked tail is unrecoverable — release the F36c gate from waiting on it.
    record.prune_writers_for_replacement(&[old_uuid.to_string()]);
    Some(record)
}

async fn node_presence(client: &kube::Client, name: &str) -> Result<NodePresence, RpcError> {
    let nodes: Api<Node> = Api::all(client.clone());
    let node = match nodes.get(name).await {
        Ok(n) => n,
        Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(NodePresence::Absent),
        Err(e) => return Err(format!("node {} lookup failed: {}", name, e).into()),
    };
    let ready = node
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|conds| conds.iter().find(|c| c.type_ == "Ready"));
    Ok(match ready {
        Some(c) if c.status == "True" => {
            NodePresence::Present { ready: true, not_ready_since_epoch_s: None }
        }
        Some(c) => NodePresence::Present {
            ready: false,
            not_ready_since_epoch_s: c.last_transition_time.as_ref().map(|t| t.0.as_second()),
        },
        // No Ready condition at all: too little evidence to condemn.
        None => NodePresence::Present { ready: false, not_ready_since_epoch_s: None },
    })
}

/// Re-place at most one permanently-lost replica of this volume. Returns the
/// swapped identity list when a replacement landed (the caller feeds it to
/// the same tick's catch-up), None when there is nothing to do.
pub async fn maybe_replace_for_volume(
    driver: &Arc<SpdkCsiDriver>,
    store: &dyn CatchupStore,
    volume_id: &str,
    replicas: &[ReplicaInfo],
    cfg: &ReplaceConfig,
) -> Result<Option<Vec<ReplicaInfo>>, RpcError> {
    if !cfg.enabled {
        return Ok(None);
    }
    let pv_name = replica_sync::record_pv_name(volume_id);
    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let pv = pvs.get(pv_name).await?;
    if pv.metadata.deletion_timestamp.is_some() || replica_sync::is_rwx_pv(&pv) {
        return Ok(None);
    }
    if pv
        .spec
        .as_ref()
        .and_then(|s| s.csi.as_ref())
        .and_then(|c| c.volume_attributes.as_ref())
        .and_then(|a| a.get(replica_sync::AUTO_REBUILD_ATTRIBUTE))
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let Some(record) = store.load(volume_id).await? else {
        return Ok(None);
    };
    if record.epochs.is_empty() {
        // The full build needs an epoch target; the scheduler cuts one
        // within its interval — re-placement simply waits a tick.
        return Ok(None);
    }
    if !record.replicas.iter().any(|r| r.sync_state == SyncState::InSync) {
        return Ok(None); // nothing to rebuild from — never touch identities
    }

    // Candidate: first stale, unmarked replica whose node is gone.
    let now_epoch_s = k8s_openapi::jiff::Timestamp::now().as_second();
    let mut candidate: Option<(usize, &ReplicaInfo, &ReplicaSyncRecord)> = None;
    for rec in record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::Stale && r.hot_rejoin.is_none())
    {
        let Some((index, identity)) = replicas
            .iter()
            .enumerate()
            .find(|(_, ri)| ri.lvol_uuid == rec.lvol_uuid)
        else {
            continue;
        };
        let presence = node_presence(&driver.kube_client, &identity.node_name).await?;
        if node_gone(&presence, cfg.after, now_epoch_s) {
            candidate = Some((index, identity, rec));
            break;
        }
    }
    let Some((index, lost, _rec)) = candidate else {
        return Ok(None);
    };

    // Target: max-free Ready node with lvstore room, hosting no leg.
    let size_bytes = pv
        .spec
        .as_ref()
        .and_then(|s| s.capacity.as_ref())
        .and_then(|c| c.get("storage"))
        .ok_or("PV has no storage capacity")
        .and_then(|q| {
            SpdkCsiDriver::parse_quantity(&q.0).map_err(|_| "unparseable PV capacity")
        })?;
    let leg_nodes: HashSet<String> = replicas.iter().map(|r| r.node_name.clone()).collect();
    let mut candidates: Vec<(String, u64, String, String)> = Vec::new(); // (node, free, lvs, pci)
    for node in driver.get_all_nodes().await? {
        if leg_nodes.contains(&node) {
            continue;
        }
        if !matches!(
            node_presence(&driver.kube_client, &node).await?,
            NodePresence::Present { ready: true, .. }
        ) {
            continue;
        }
        match driver.get_initialized_disks_from_node(&node).await {
            Ok(disks) => {
                if let Some(disk) = disks
                    .iter()
                    .filter(|d| d.free_space >= size_bytes && d.lvs_name.is_some())
                    .max_by_key(|d| d.free_space)
                {
                    candidates.push((
                        node.clone(),
                        disk.free_space,
                        disk.lvs_name.clone().unwrap(),
                        disk.pci_address.clone(),
                    ));
                }
            }
            Err(e) => {
                debug!(node, error = %e, "[REPLACE] Skipping node (disk query failed)");
            }
        }
    }
    let ranked: Vec<(String, u64)> =
        candidates.iter().map(|(n, f, _, _)| (n.clone(), *f)).collect();
    let Some(target) = pick_replacement_node(&ranked, &HashSet::new()) else {
        // Loud: the operator must add capacity — nothing here can.
        store
            .emit(
                volume_id,
                "Warning",
                "ReplicaReplacementBlocked",
                &format!(
                    "Replica on lost node {} cannot be re-placed: no Ready node with {}B free \
                     outside the volume's current nodes",
                    lost.node_name, size_bytes
                ),
            )
            .await;
        return Ok(None);
    };
    let (_, _, lvs_name, pci_address) =
        candidates.into_iter().find(|(n, _, _, _)| *n == target).unwrap();

    // Placeholder lvol. A crashed prior attempt may have left one — delete
    // by alias first (idempotent; "no such device" is fine).
    let replica_volume_id = format!("{}_replica_{}", volume_id, index);
    let lvol_name = crate::identity::lvol_name(&replica_volume_id);
    let alias = format!("{}/{}", lvs_name, lvol_name);
    let _ = CatchupRpc::spdk_rpc(
        driver.as_ref(),
        &target,
        &json!({ "method": "bdev_lvol_delete", "params": { "name": alias } }),
    )
    .await;
    let new_uuid = driver
        .create_lvol(&target, &lvs_name, &replica_volume_id, size_bytes, true)
        .await
        .map_err(|e| format!("placeholder lvol on {} failed: {}", target, e))?;
    driver.capacity_cache.invalidate(&target).await;
    let node_uid = driver.get_node_uid(&target).await.unwrap_or_default();

    let new_replica = ReplicaInfo {
        node_name: target.clone(),
        node_uid: node_uid.clone(),
        disk_pci_address: pci_address,
        lvol_uuid: new_uuid.clone(),
        lvol_name,
        lvs_name,
        nqn: None,
        target_ip: None,
        target_port: None,
        health: "online".to_string(),
    };
    let mut new_replicas = replicas.to_vec();
    let old_uuid = new_replicas[index].lvol_uuid.clone();
    let old_uid = new_replicas[index].node_uid.clone();
    new_replicas[index] = new_replica.clone();

    let new_rec = ReplicaSyncRecord {
        node_name: target.clone(),
        node_uid: node_uid.clone(),
        lvol_uuid: new_uuid.clone(),
        sync_state: SyncState::Stale,
        last_epoch: None,
        since: Some(replica_sync::now_rfc3339()),
        reason: Some(format!("re-placed from lost node {} (U11)", lost.node_name)),
        active_lvol_uuid: None,
        reverted_to: None,
        hot_rejoin: None,
    };

    if let Err(e) = swap_identity_on_pv(
        &driver.kube_client,
        pv_name,
        replicas,
        &new_replicas,
        &old_uuid,
        new_rec,
        &old_uid,
        &node_uid,
    )
    .await
    {
        // Unwind the placeholder so the next tick starts clean.
        let _ = driver.delete_lvol(&target, &new_uuid).await;
        return Err(e);
    }

    store
        .emit(
            volume_id,
            "Warning",
            "ReplicaReplaced",
            &format!(
                "Replica on lost node {} re-placed to {} (lvol {}); thin-aware full rebuild \
                 starts this cycle",
                lost.node_name, target, new_uuid
            ),
        )
        .await;
    info!(
        volume_id,
        index,
        from = %lost.node_name,
        to = %target,
        new_uuid = %new_uuid,
        "[REPLACE] Replica identity swapped off lost node — full build queued"
    );
    Ok(Some(new_replicas))
}

/// The atomic identity swap: override annotation + swapped sync record +
/// node-label swap in ONE resourceVersion-guarded merge patch. A conflict
/// aborts (the caller unwinds the placeholder; next tick retries cleanly).
#[allow(clippy::too_many_arguments)]
async fn swap_identity_on_pv(
    client: &kube::Client,
    pv_name: &str,
    old_replicas: &[ReplicaInfo],
    new_replicas: &[ReplicaInfo],
    old_uuid: &str,
    new_rec: ReplicaSyncRecord,
    old_node_uid: &str,
    new_node_uid: &str,
) -> Result<(), RpcError> {
    let pvs: Api<PersistentVolume> = Api::all(client.clone());
    let pv = pvs.get(pv_name).await?;

    let baseline = pv
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(SYNC_STATE_ANNOTATION))
        .and_then(|s| VolumeSyncRecord::from_annotation(s).ok());
    let mut record = baseline.unwrap_or_else(|| VolumeSyncRecord::initial(old_replicas));
    record.reconcile_membership(old_replicas);
    let record = build_swapped_record(&record, old_uuid, new_rec, new_replicas)
        .ok_or("replica identity already swapped by a concurrent writer")?;

    let mut labels = serde_json::Map::new();
    // The lost node's label goes unless another leg still lives on it (it
    // cannot on distinct-node placement, but stay correct regardless).
    if !old_node_uid.is_empty()
        && !new_replicas.iter().any(|r| r.node_uid == old_node_uid)
    {
        labels.insert(
            format!("flint.csi.storage.io/replica-{}", old_node_uid),
            serde_json::Value::Null,
        );
    }
    if !new_node_uid.is_empty() {
        labels.insert(
            format!("flint.csi.storage.io/replica-{}", new_node_uid),
            json!("true"),
        );
    }

    let replicas_json = serde_json::to_string(new_replicas)
        .map_err(|e| format!("serialize swapped replicas: {}", e))?;
    let patch = json!({
        "metadata": {
            "resourceVersion": pv.metadata.resource_version,
            "annotations": {
                REPLICAS_OVERRIDE_ANNOTATION: replicas_json,
                SYNC_STATE_ANNOTATION: record.to_annotation(),
            },
            "labels": labels,
        }
    });
    match pvs.patch(pv_name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        Ok(_) => Ok(()),
        Err(e) => {
            warn!(pv_name, error = %e, "[REPLACE] Identity swap patch failed");
            Err(format!("identity swap patch on {} failed: {}", pv_name, e).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn absent_node_is_gone_immediately() {
        assert!(node_gone(&NodePresence::Absent, Duration::from_secs(600), 1_000_000));
    }

    #[test]
    fn ready_node_is_never_gone() {
        let p = NodePresence::Present { ready: true, not_ready_since_epoch_s: None };
        assert!(!node_gone(&p, Duration::from_secs(0), 1_000_000));
    }

    #[test]
    fn notready_respects_threshold() {
        let p = NodePresence::Present { ready: false, not_ready_since_epoch_s: Some(1_000_000) };
        assert!(!node_gone(&p, Duration::from_secs(600), 1_000_300)); // 300s < 600s
        assert!(node_gone(&p, Duration::from_secs(600), 1_000_600)); // exactly at
        assert!(node_gone(&p, Duration::from_secs(600), 1_000_900));
    }

    #[test]
    fn notready_without_transition_time_never_condemns() {
        let p = NodePresence::Present { ready: false, not_ready_since_epoch_s: None };
        assert!(!node_gone(&p, Duration::from_secs(0), i64::MAX));
    }

    #[test]
    fn picks_max_free_excluding_leg_nodes() {
        let cands = vec![("a".into(), 10u64), ("b".into(), 30u64), ("c".into(), 20u64)];
        let mut excluded = HashSet::new();
        assert_eq!(pick_replacement_node(&cands, &excluded), Some("b".into()));
        excluded.insert("b".to_string());
        assert_eq!(pick_replacement_node(&cands, &excluded), Some("c".into()));
    }

    #[test]
    fn swapped_record_enters_stale_and_drops_old_identity() {
        let old = vec![replica("n1", "u1"), replica("n2", "u2")];
        let record = VolumeSyncRecord::initial(&old);
        let mut new_replicas = old.clone();
        new_replicas[1] = replica("n3", "u3");
        let new_rec = ReplicaSyncRecord {
            node_name: "n3".into(),
            node_uid: "uid-n3".into(),
            lvol_uuid: "u3".into(),
            sync_state: SyncState::Stale,
            last_epoch: None,
            since: None,
            reason: Some("re-placed from lost node n2 (U11)".into()),
            active_lvol_uuid: None,
            reverted_to: None,
            hot_rejoin: None,
        };
        let swapped = build_swapped_record(&record, "u2", new_rec, &new_replicas).unwrap();
        assert_eq!(swapped.replicas.len(), 2);
        assert_eq!(swapped.replicas[0].lvol_uuid, "u1");
        assert_eq!(swapped.replicas[0].sync_state, SyncState::InSync);
        assert_eq!(swapped.replicas[1].lvol_uuid, "u3");
        // The load-bearing property: the fresh identity must NOT enter
        // in_sync (reconcile_membership's default for unknown identities).
        assert_eq!(swapped.replicas[1].sync_state, SyncState::Stale);
        assert!(swapped.get("u2").is_none());
    }

    #[test]
    fn swap_aborts_when_old_uuid_already_gone() {
        let old = vec![replica("n1", "u1")];
        let record = VolumeSyncRecord::initial(&old);
        let new_rec = ReplicaSyncRecord {
            node_name: "n3".into(),
            node_uid: "uid-n3".into(),
            lvol_uuid: "u3".into(),
            sync_state: SyncState::Stale,
            last_epoch: None,
            since: None,
            reason: None,
            active_lvol_uuid: None,
            reverted_to: None,
            hot_rejoin: None,
        };
        assert!(build_swapped_record(&record, "nope", new_rec, &old).is_none());
    }
}
