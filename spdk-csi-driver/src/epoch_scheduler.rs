// epoch_scheduler.rs — common-epoch snapshot scheduler.
// Phase 2 of docs/incremental-replica-rebuild.md §9.
//
// Cuts `epoch-<vol>-<seq>` snapshots (`bdev_lvol_snapshot`) on every in-sync
// replica of attached multi-replica volumes at a fixed cadence, records them
// in the PV sync record (phase 1), and rolls retention. The COW
// cluster-allocation map between consecutive epochs *is* the dirty tracking
// the phase-3 catch-up will copy — the scheduler itself moves no data.
//
// Design points (§5):
// - An epoch is recorded common only after the cut succeeded on ALL in-sync
//   replicas (all-or-abort). "Already exists" converges: a leftover from an
//   aborted attempt was cut from the same head, merely earlier — the §5 skew
//   argument tolerates per-replica cut-time skew of any size, it only widens
//   the delta the epoch covers.
// - Per-replica cuts are NOT simultaneous and need no quiesce; correctness
//   comes from the catch-up's back-off + revert, not from cut atomicity.
// - Retention retires from the oldest end (snapshot deletion merges clusters
//   into the descendant), respects the phase-3 retention pin, and is
//   record-first: epochs leave the record, then the GC pass reaps the
//   node-side snapshots — every step idempotent and convergent, so a crash
//   or unreachable node anywhere just retries next cycle.
// - Detached volumes are skipped: with no consumer there are no writes, so
//   new epochs would add nothing and cost metadata.
//
// Hosting (§9-2 decision): runs as a background loop in the controller
// process — common epochs need a single coordinator that can reach every
// replica's node agent, which is the controller's existing position. Assumes
// one controller instance (as CreateVolume placement already does);
// duplicate schedulers would be safe but noisy (convergent cuts, resource-
// version-guarded record writes). The dead controller-operator binary was
// not revived for this (§1: it is not built, not deployed, and its RPC
// routing is broken; a controller loop is the same code with none of that).
//
// Default-disabled via FLINT_EPOCH_SCHEDULER until the phase-3/4 consumers
// of epochs exist: epochs cost snapshot space on every multi-replica volume
// (§5 "space overhead", up to 2× for pre-1.1 thick volumes) and heal nothing
// on their own yet.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::PersistentVolume;
use kube::api::ListParams;
use kube::Api;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::driver::SpdkCsiDriver;
use crate::minimal_models::ReplicaInfo;
use crate::replica_sync::{self, epoch_name, epoch_seq, SyncState, VolumeSyncRecord};

pub type RpcError = Box<dyn std::error::Error + Send + Sync>;

/// SPDK JSON-RPC addressed to a specific node's SPDK (via its node agent in
/// production; faked in unit tests).
#[async_trait]
pub trait NodeRpc: Sync {
    async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError>;
}

#[async_trait]
impl NodeRpc for SpdkCsiDriver {
    async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError> {
        self.call_node_agent(node, "/api/spdk/rpc", payload).await
    }
}

#[derive(Debug, Clone)]
pub struct EpochConfig {
    /// FLINT_EPOCH_SCHEDULER=enabled — default off until phase 3/4 land.
    pub enabled: bool,
    /// T_snap (§5): epoch cadence. FLINT_EPOCH_INTERVAL_SECS, default 300.
    pub interval: Duration,
    /// K (§5): epochs retained. `K · T_snap` bounds the longest outage healed
    /// incrementally. FLINT_EPOCH_RETAIN, default 6, min 1.
    pub retain: usize,
}

impl Default for EpochConfig {
    fn default() -> Self {
        EpochConfig {
            enabled: false,
            interval: Duration::from_secs(300),
            retain: 6,
        }
    }
}

impl EpochConfig {
    pub fn from_env() -> Self {
        let d = EpochConfig::default();
        EpochConfig {
            enabled: std::env::var("FLINT_EPOCH_SCHEDULER")
                .map(|v| {
                    v.eq_ignore_ascii_case("enabled")
                        || v.eq_ignore_ascii_case("true")
                        || v == "1"
                })
                .unwrap_or(d.enabled),
            interval: std::env::var("FLINT_EPOCH_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.interval),
            retain: std::env::var("FLINT_EPOCH_RETAIN")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .map(|k| k.max(1))
                .unwrap_or(d.retain),
        }
    }
}

/// One replica to snapshot in a cut. `lvol_uuid` is the immutable identity
/// (what the record is keyed by); `snapshot_source` is the bdev the snapshot
/// is actually taken of — the *live* head uuid, which differs from the
/// identity after a catch-up revert (phase 3 `active_lvol_uuid`). Cutting by
/// identity uuid on a reverted-then-admitted replica would fail forever.
#[derive(Debug, Clone, PartialEq)]
pub struct EpochTarget {
    pub node_name: String,
    pub lvol_uuid: String,
    pub snapshot_source: String,
    pub lvs_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CutPlan {
    pub epoch: String,
    pub targets: Vec<EpochTarget>,
}

/// Decide whether a cut is due. None when the volume is detached (no writes
/// to capture), has no in-sync replicas, or the newest epoch is younger than
/// the cadence. The sequence number never reuses a retired epoch's.
pub fn plan_cut(
    volume_id: &str,
    record: &VolumeSyncRecord,
    replicas: &[ReplicaInfo],
    attached: bool,
    now: chrono::DateTime<chrono::Utc>,
    cfg: &EpochConfig,
) -> Option<CutPlan> {
    if !attached {
        return None;
    }

    // In-sync per the record, joined with volumeAttributes for addressing.
    // Note this includes degraded volumes with a single in-sync replica —
    // that is exactly when epochs matter most: they capture the delta the
    // stale replica will need.
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
        return None;
    }

    let due = match record.epochs.last() {
        None => true,
        Some(e) => match chrono::DateTime::parse_from_rfc3339(&e.recorded_at) {
            Ok(t) => {
                let elapsed = now.signed_duration_since(t.with_timezone(&chrono::Utc));
                elapsed >= chrono::Duration::from_std(cfg.interval)
                    .unwrap_or_else(|_| chrono::Duration::seconds(300))
            }
            Err(_) => true,
        },
    };
    if !due {
        return None;
    }

    Some(CutPlan {
        epoch: epoch_name(volume_id, record.latest_epoch_seq(volume_id) + 1),
        targets,
    })
}

/// Epochs beyond the retention window, oldest first. Proposes only; the
/// record's `retire_epochs` re-checks the pin and current epoch at write
/// time (the pin may appear between plan and write).
pub fn plan_retention(record: &VolumeSyncRecord, cfg: &EpochConfig) -> Vec<String> {
    if record.epochs.len() <= cfg.retain {
        return Vec::new();
    }
    let excess = record.epochs.len() - cfg.retain;
    record.epochs[..excess].iter().map(|e| e.name.clone()).collect()
}

#[derive(Debug)]
pub enum CutOutcome {
    /// Every in-sync replica holds the epoch — safe to record as common.
    Recorded { cut_uuids: Vec<String> },
    /// At least one replica could not be cut; nothing was recorded.
    Aborted { failures: Vec<(String, String)> },
}

pub(crate) fn is_already_exists(msg: &str) -> bool {
    msg.contains("File exists") || msg.contains("Code=-17") || msg.contains("already exists")
}

pub(crate) fn is_missing(msg: &str) -> bool {
    msg.contains("No such device")
        || msg.contains("Code=-19")
        || msg.contains("No such file or directory")
}

/// Cut the planned epoch on every target, all-or-abort. "Already exists"
/// counts as success — a leftover from an aborted attempt is a snapshot of
/// the same head taken earlier, which only widens the delta this epoch
/// covers (safe direction per §5). On abort, created snapshots are rolled
/// back best-effort; any survivor converges via EEXIST on the retry.
pub async fn execute_cut(rpc: &dyn NodeRpc, plan: &CutPlan) -> CutOutcome {
    let mut succeeded: Vec<&EpochTarget> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();

    for target in &plan.targets {
        let payload = json!({
            "method": "bdev_lvol_snapshot",
            "params": { "lvol_name": target.snapshot_source, "snapshot_name": plan.epoch }
        });
        match rpc.spdk_rpc(&target.node_name, &payload).await {
            Ok(_) => succeeded.push(target),
            Err(e) => {
                let msg = e.to_string();
                if is_already_exists(&msg) {
                    succeeded.push(target);
                } else {
                    failures.push((target.node_name.clone(), msg));
                }
            }
        }
    }

    if failures.is_empty() {
        return CutOutcome::Recorded {
            cut_uuids: succeeded.iter().map(|t| t.lvol_uuid.clone()).collect(),
        };
    }

    // A partially-cut epoch must never be recorded common.
    for target in &succeeded {
        let payload = json!({
            "method": "bdev_lvol_delete",
            "params": { "name": format!("{}/{}", target.lvs_name, plan.epoch) }
        });
        if let Err(e) = rpc.spdk_rpc(&target.node_name, &payload).await {
            let msg = e.to_string();
            if !is_missing(&msg) {
                debug!(
                    node = %target.node_name, epoch = %plan.epoch, error = %msg,
                    "[EPOCH] Rollback delete failed (non-fatal; retry converges via EEXIST)"
                );
            }
        }
    }
    CutOutcome::Aborted { failures }
}

/// Of this volume's epoch-named snapshots found on a node, the ones safe to
/// reap: strictly older than the oldest retained epoch. Everything from the
/// oldest retained through one-past-newest is live state (one-past-newest is
/// an aborted in-flight cut the next attempt converges); other names are not
/// ours. An empty epoch list GCs nothing — a record rebuilt after annotation
/// loss must never trigger blind deletion of chains a catch-up might need.
pub fn gc_candidates(volume_id: &str, record: &VolumeSyncRecord, found: &[String]) -> Vec<String> {
    let Some(oldest) = record
        .epochs
        .first()
        .and_then(|e| epoch_seq(volume_id, &e.name))
    else {
        return Vec::new();
    };
    found
        .iter()
        .filter(|name| epoch_seq(volume_id, name).map(|seq| seq < oldest).unwrap_or(false))
        .cloned()
        .collect()
}

/// Reap rolled-out epoch snapshots on every replica node. Convergent: an
/// unreachable node or a delete blocked by a user clone is retried next
/// cycle. Runs over all replicas regardless of sync_state — a stale
/// replica's rolled-out epochs are reclaimable space too.
pub async fn execute_gc(
    rpc: &dyn NodeRpc,
    volume_id: &str,
    record: &VolumeSyncRecord,
    replicas: &[ReplicaInfo],
) {
    if record.epochs.is_empty() {
        return;
    }
    for replica in replicas {
        let payload = json!({
            "method": "bdev_lvol_get_lvols",
            "params": { "lvs_name": replica.lvs_name }
        });
        let found: Vec<String> = match rpc.spdk_rpc(&replica.node_name, &payload).await {
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
                debug!(
                    volume_id, node = %replica.node_name, error = %e,
                    "[EPOCH] GC could not list lvols (node down?) — retrying next cycle"
                );
                continue;
            }
        };

        for name in gc_candidates(volume_id, record, &found) {
            let del = json!({
                "method": "bdev_lvol_delete",
                "params": { "name": format!("{}/{}", replica.lvs_name, name) }
            });
            match rpc.spdk_rpc(&replica.node_name, &del).await {
                Ok(_) => {
                    info!(volume_id, node = %replica.node_name, snapshot = %name, "[EPOCH] Reaped rolled-out epoch snapshot");
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !is_missing(&msg) {
                        warn!(
                            volume_id, node = %replica.node_name, snapshot = %name, error = %msg,
                            "[EPOCH] GC delete failed — retrying next cycle (a user clone of the snapshot blocks deletion)"
                        );
                    }
                }
            }
        }
    }
}

/// One scheduler pass over a single volume: load (and lazily seed) the sync
/// record, cut if due, record the epoch as common, roll retention, reap.
pub async fn run_epoch_cycle(
    rpc: &dyn NodeRpc,
    client: &kube::Client,
    volume_id: &str,
    replicas: &[ReplicaInfo],
    attached: bool,
    cfg: &EpochConfig,
) -> Result<(), RpcError> {
    let Some(mut record) = replica_sync::update_sync_record(client, volume_id, |_| {}).await?
    else {
        return Ok(()); // single-replica volume: no epochs apply
    };

    if let Some(cut) = plan_cut(volume_id, &record, replicas, attached, chrono::Utc::now(), cfg) {
        match execute_cut(rpc, &cut).await {
            CutOutcome::Recorded { cut_uuids } => {
                let now = replica_sync::now_rfc3339();
                if let Some(updated) =
                    replica_sync::update_sync_record(client, volume_id, |r| {
                        r.apply_epoch_cut(&cut.epoch, &cut_uuids, &now);
                    })
                    .await?
                {
                    record = updated;
                }
                info!(
                    volume_id, epoch = %cut.epoch, replicas = cut_uuids.len(),
                    "[EPOCH] Common epoch recorded"
                );
            }
            CutOutcome::Aborted { failures } => {
                let detail = failures
                    .iter()
                    .map(|(node, err)| format!("{}: {}", node, err))
                    .collect::<Vec<_>>()
                    .join("; ");
                warn!(volume_id, epoch = %cut.epoch, detail, "[EPOCH] Epoch cut aborted — retrying next cycle");
                replica_sync::emit_pv_event(
                    client,
                    "epoch-scheduler",
                    volume_id,
                    "Warning",
                    "EpochCutFailed",
                    &format!(
                        "Epoch {} could not be cut on all in-sync replicas: {}",
                        cut.epoch, detail
                    ),
                )
                .await;
            }
        }
    }

    let retire = plan_retention(&record, cfg);
    if !retire.is_empty() {
        let mut retired: Vec<String> = Vec::new();
        if let Some(updated) = replica_sync::update_sync_record(client, volume_id, |r| {
            retired = r.retire_epochs(volume_id, &retire);
        })
        .await?
        {
            record = updated;
        }
        if !retired.is_empty() {
            debug!(volume_id, ?retired, "[EPOCH] Epochs retired from record");
        }
    }

    execute_gc(rpc, volume_id, &record, replicas).await;
    Ok(())
}

/// Background scheduler loop (controller role). Ticks every 60s; cut
/// due-ness is computed from the record's timestamps, so the cadence
/// survives controller restarts with no extra state.
pub async fn run_epoch_scheduler(driver: Arc<SpdkCsiDriver>, cfg: EpochConfig) {
    info!(
        interval_secs = cfg.interval.as_secs(),
        retain = cfg.retain,
        "[EPOCH] Epoch snapshot scheduler started"
    );
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Err(e) = scheduler_tick(&driver, &cfg).await {
            warn!(error = %e, "[EPOCH] Scheduler tick failed (non-fatal)");
        }
    }
}

async fn scheduler_tick(driver: &Arc<SpdkCsiDriver>, cfg: &EpochConfig) -> Result<(), RpcError> {
    use k8s_openapi::api::storage::v1::VolumeAttachment;

    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let vas: Api<VolumeAttachment> = Api::all(driver.kube_client.clone());

    let pv_list = pvs.list(&ListParams::default()).await?;
    // A volume is "attached" when some node consumes it — only then do
    // writes flow and epochs capture anything.
    let attached: HashSet<String> = vas
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|va| va.status.as_ref().map(|s| s.attached).unwrap_or(false))
        .filter_map(|va| va.spec.source.persistent_volume_name)
        .collect();

    for pv in pv_list.items {
        let Some(volume_id) = pv.metadata.name.clone() else { continue };
        let is_flint = pv
            .spec
            .as_ref()
            .and_then(|s| s.csi.as_ref())
            .map(|c| c.driver == "flint.csi.storage.io")
            .unwrap_or(false);
        if !is_flint {
            continue;
        }
        let replicas = match replica_sync::replicas_from_pv(&pv) {
            Ok(Some(r)) => r,
            Ok(None) => continue, // single replica
            Err(e) => {
                debug!(volume_id, error = %e, "[EPOCH] Skipping PV with unreadable replica info");
                continue;
            }
        };
        if let Err(e) = run_epoch_cycle(
            driver.as_ref(),
            &driver.kube_client,
            &volume_id,
            &replicas,
            attached.contains(&volume_id),
            cfg,
        )
        .await
        {
            warn!(volume_id, error = %e, "[EPOCH] Epoch cycle failed (non-fatal)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn record3() -> VolumeSyncRecord {
        VolumeSyncRecord::initial(&replicas3())
    }

    fn cfg() -> EpochConfig {
        EpochConfig {
            enabled: true,
            interval: Duration::from_secs(300),
            retain: 2,
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-06-11T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    struct FakeNodeRpc {
        calls: Mutex<Vec<(String, Value)>>,
        /// (node, method) → error message to return
        fail: HashMap<(String, String), String>,
        /// node → lvol names returned by bdev_lvol_get_lvols
        lvols: HashMap<String, Vec<String>>,
    }

    impl FakeNodeRpc {
        fn new() -> Self {
            FakeNodeRpc {
                calls: Mutex::new(Vec::new()),
                fail: HashMap::new(),
                lvols: HashMap::new(),
            }
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

    #[async_trait]
    impl NodeRpc for FakeNodeRpc {
        async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError> {
            let method = payload["method"].as_str().unwrap_or("").to_string();
            self.calls
                .lock()
                .unwrap()
                .push((node.to_string(), payload.clone()));
            if let Some(err) = self.fail.get(&(node.to_string(), method.clone())) {
                return Err(err.clone().into());
            }
            if method == "bdev_lvol_get_lvols" {
                let names = self.lvols.get(node).cloned().unwrap_or_default();
                let arr: Vec<Value> = names
                    .iter()
                    .map(|n| json!({ "name": n, "alias": format!("lvs0/{}", n) }))
                    .collect();
                return Ok(json!({ "result": arr }));
            }
            Ok(json!({ "result": "ok" }))
        }
    }

    #[test]
    fn plan_cut_skips_detached_and_stale() {
        let mut record = record3();

        // Detached: no writes flow, no cut.
        assert!(plan_cut("vol1", &record, &replicas3(), false, now(), &cfg()).is_none());

        // Stale replicas are not cut; in-sync ones are.
        record.mark_stale("uuid-b", "leg failed", "t0");
        let plan = plan_cut("vol1", &record, &replicas3(), true, now(), &cfg()).unwrap();
        assert_eq!(plan.epoch, "epoch-vol1-1");
        let nodes: Vec<&str> = plan.targets.iter().map(|t| t.node_name.as_str()).collect();
        assert_eq!(nodes, vec!["node-a", "node-c"]);

        // All stale: nothing to cut.
        record.mark_stale("uuid-a", "x", "t1");
        record.mark_stale("uuid-c", "x", "t1");
        assert!(plan_cut("vol1", &record, &replicas3(), true, now(), &cfg()).is_none());
    }

    #[test]
    fn plan_cut_cadence_and_sequence() {
        let mut record = record3();
        let all: Vec<String> = replicas3().iter().map(|r| r.lvol_uuid.clone()).collect();

        // Newest epoch is recent (T_snap = 300s): not due.
        record.apply_epoch_cut(&epoch_name("vol1", 3), &all, "2026-06-11T11:58:00Z");
        assert!(plan_cut("vol1", &record, &replicas3(), true, now(), &cfg()).is_none());

        // Newest epoch is old: due, and the sequence advances past the
        // highest ever recorded — never reusing a retired epoch's number.
        record.epochs[0].recorded_at = "2026-06-11T11:00:00Z".to_string();
        let plan = plan_cut("vol1", &record, &replicas3(), true, now(), &cfg()).unwrap();
        assert_eq!(plan.epoch, "epoch-vol1-4");

        // Unparseable timestamp counts as due (fail open: an extra epoch is
        // harmless, a stalled scheduler is not).
        record.epochs[0].recorded_at = "garbage".to_string();
        assert!(plan_cut("vol1", &record, &replicas3(), true, now(), &cfg()).is_some());
    }

    #[test]
    fn plan_retention_proposes_oldest_excess() {
        let mut record = record3();
        let all: Vec<String> = replicas3().iter().map(|r| r.lvol_uuid.clone()).collect();
        assert!(plan_retention(&record, &cfg()).is_empty());
        for seq in 1..=5 {
            record.apply_epoch_cut(&epoch_name("vol1", seq), &all, "t");
        }
        // retain = 2 → the three oldest go, oldest first.
        assert_eq!(
            plan_retention(&record, &cfg()),
            vec![epoch_name("vol1", 1), epoch_name("vol1", 2), epoch_name("vol1", 3)]
        );
    }

    #[tokio::test]
    async fn execute_cut_snapshots_every_target() {
        let rpc = FakeNodeRpc::new();
        let plan = plan_cut("vol1", &record3(), &replicas3(), true, now(), &cfg()).unwrap();
        match execute_cut(&rpc, &plan).await {
            CutOutcome::Recorded { cut_uuids } => {
                assert_eq!(cut_uuids, vec!["uuid-a", "uuid-b", "uuid-c"]);
            }
            other => panic!("expected Recorded, got {:?}", other),
        }
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        assert_eq!(snaps.len(), 3);
        assert_eq!(snaps[0].1["params"]["snapshot_name"], "epoch-vol1-1");
        assert_eq!(snaps[0].1["params"]["lvol_name"], "uuid-a");
        assert!(rpc.calls_of("bdev_lvol_delete").is_empty());
    }

    #[tokio::test]
    async fn execute_cut_uses_live_uuid_after_revert() {
        // A replica reverted by catch-up and admitted back in_sync (phase 4)
        // is addressed by its live head uuid; the record still keys — and
        // last_epoch still stamps — by the identity uuid.
        let mut record = record3();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        let rpc = FakeNodeRpc::new();
        let plan = plan_cut("vol1", &record, &replicas3(), true, now(), &cfg()).unwrap();
        match execute_cut(&rpc, &plan).await {
            CutOutcome::Recorded { cut_uuids } => {
                assert_eq!(cut_uuids, vec!["uuid-a", "uuid-b", "uuid-c"]);
            }
            other => panic!("expected Recorded, got {:?}", other),
        }
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        assert_eq!(snaps[1].1["params"]["lvol_name"], "uuid-b-v2");
    }

    #[tokio::test]
    async fn execute_cut_converges_on_already_exists() {
        // A leftover snapshot from an aborted earlier attempt: same head,
        // earlier cut — counts as success, the epoch records.
        let mut rpc = FakeNodeRpc::new();
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_snapshot".to_string()),
            "SPDK RPC error Code=-17: File exists".to_string(),
        );
        let plan = plan_cut("vol1", &record3(), &replicas3(), true, now(), &cfg()).unwrap();
        match execute_cut(&rpc, &plan).await {
            CutOutcome::Recorded { cut_uuids } => assert_eq!(cut_uuids.len(), 3),
            other => panic!("expected Recorded, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn execute_cut_aborts_and_rolls_back_on_hard_failure() {
        let mut rpc = FakeNodeRpc::new();
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_snapshot".to_string()),
            "connection refused".to_string(),
        );
        let plan = plan_cut("vol1", &record3(), &replicas3(), true, now(), &cfg()).unwrap();
        match execute_cut(&rpc, &plan).await {
            CutOutcome::Aborted { failures } => {
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].0, "node-b");
            }
            other => panic!("expected Aborted, got {:?}", other),
        }
        // Rollback deletes exactly the snapshots that were created — the
        // failed node gets no delete.
        let deletes = rpc.calls_of("bdev_lvol_delete");
        let delete_nodes: Vec<&str> = deletes.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(delete_nodes, vec!["node-a", "node-c"]);
        assert_eq!(deletes[0].1["params"]["name"], "lvs0/epoch-vol1-1");
    }

    #[test]
    fn gc_candidates_reaps_only_below_retained_window() {
        let mut record = record3();
        let all: Vec<String> = replicas3().iter().map(|r| r.lvol_uuid.clone()).collect();
        for seq in 3..=5 {
            record.apply_epoch_cut(&epoch_name("vol1", seq), &all, "t");
        }
        let found = vec![
            epoch_name("vol1", 1),       // rolled out → reap
            epoch_name("vol1", 2),       // rolled out → reap
            epoch_name("vol1", 3),       // oldest retained → keep
            epoch_name("vol1", 5),       // current → keep
            epoch_name("vol1", 6),       // aborted in-flight cut → keep (converges)
            epoch_name("vol2", 1),       // another volume → not ours
            "snap_user_123".to_string(), // user snapshot → not ours
        ];
        assert_eq!(
            gc_candidates("vol1", &record, &found),
            vec![epoch_name("vol1", 1), epoch_name("vol1", 2)]
        );

        // A record with no retained epochs (fresh, or rebuilt after
        // annotation loss) must never trigger blind deletion.
        assert!(gc_candidates("vol1", &record3(), &found).is_empty());
    }

    #[tokio::test]
    async fn execute_gc_deletes_candidates_and_tolerates_missing() {
        let mut record = record3();
        let all: Vec<String> = replicas3().iter().map(|r| r.lvol_uuid.clone()).collect();
        for seq in 3..=4 {
            record.apply_epoch_cut(&epoch_name("vol1", seq), &all, "t");
        }

        let mut rpc = FakeNodeRpc::new();
        rpc.lvols.insert(
            "node-a".to_string(),
            vec![epoch_name("vol1", 1), epoch_name("vol1", 3), epoch_name("vol1", 4)],
        );
        rpc.lvols.insert("node-b".to_string(), vec![epoch_name("vol1", 2)]);
        // node-c's delete races something that already removed it.
        rpc.lvols.insert("node-c".to_string(), vec![epoch_name("vol1", 1)]);
        rpc.fail.insert(
            ("node-c".to_string(), "bdev_lvol_delete".to_string()),
            "Code=-19: No such device".to_string(),
        );

        execute_gc(&rpc, "vol1", &record, &replicas3()).await;

        let deletes = rpc.calls_of("bdev_lvol_delete");
        let deleted: Vec<(String, String)> = deletes
            .iter()
            .map(|(n, p)| (n.clone(), p["params"]["name"].as_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            deleted,
            vec![
                ("node-a".to_string(), format!("lvs0/{}", epoch_name("vol1", 1))),
                ("node-b".to_string(), format!("lvs0/{}", epoch_name("vol1", 2))),
                ("node-c".to_string(), format!("lvs0/{}", epoch_name("vol1", 1))),
            ]
        );
    }

    #[tokio::test]
    async fn execute_gc_skips_unreachable_nodes() {
        let mut record = record3();
        let all: Vec<String> = replicas3().iter().map(|r| r.lvol_uuid.clone()).collect();
        record.apply_epoch_cut(&epoch_name("vol1", 3), &all, "t");

        let mut rpc = FakeNodeRpc::new();
        rpc.lvols.insert("node-a".to_string(), vec![epoch_name("vol1", 1)]);
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_get_lvols".to_string()),
            "connection refused".to_string(),
        );
        rpc.lvols.insert("node-c".to_string(), vec![]);

        execute_gc(&rpc, "vol1", &record, &replicas3()).await;
        // node-a's orphan is reaped; node-b is skipped without aborting the
        // pass; node-c has nothing to do.
        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].0, "node-a");
    }
}
