// Replicated volume expansion — the block-side fan-out core (v1.21.0,
// docs/volume-expansion-status-2026-07.md), extracted from main.rs behind an
// injectable environment so the sim tier can reach it (the F56 composition:
// expand × catch-up × admission) and the belt/fan-out logic has unit
// coverage. main.rs keeps the CSI/tonic surface, the legacy single-replica
// arm, and the RWX orchestration; this module owns everything between them:
// the shrink/no-op guard, the C2 sync belt, and the live-uuid fan-out.
//
// The refusal split is load-bearing for the external-resizer's retry
// semantics: Unavailable = converges on retry (lagging legs, partial
// fan-out), FailedPrecondition = will not converge without intervention
// (unreadable capacity — the shrink guard cannot run).

use crate::minimal_models::ReplicaInfo;
use crate::replica_sync::{replicas_not_in_sync, VolumeSyncRecord};
use async_trait::async_trait;

/// The world the replicated expand touches.
#[async_trait]
pub trait ExpandEnv: Send + Sync {
    /// Record-home PV state: (capacity bytes when readable AND parseable,
    /// sync record when the annotation is present and parses). Err = the PV
    /// itself was unreadable (retryable).
    async fn record_home_state(
        &self,
        record_home: &str,
    ) -> Result<(Option<u64>, Option<VolumeSyncRecord>), String>;

    /// Grow one leg's lvol via its node agent (SPDK bdev_lvol_resize —
    /// same-size is a blobstore no-op, which is what makes the resizer's
    /// whole-RPC retry safe over a partial fan-out).
    async fn resize_leg(
        &self,
        node: &str,
        lvol_uuid: &str,
        new_size_bytes: u64,
    ) -> Result<(), String>;

    /// PV-scoped Kubernetes event.
    async fn emit(&self, record_home: &str, type_: &str, reason: &str, msg: &str);
}

/// Mapped by the caller onto ControllerExpandVolumeResponse.
#[derive(Debug, PartialEq, Eq)]
pub struct ExpandDone {
    pub capacity_bytes: u64,
    pub node_expansion_required: bool,
}

#[derive(Debug)]
pub enum ExpandRefusal {
    /// Will not converge without intervention.
    FailedPrecondition(String),
    /// The resizer's retry converges.
    Unavailable(String),
}

/// The replicated expand: shrink/no-op guard → C2 belt → fan-out. The
/// caller holds the volume claim (OP_EXPAND) and has already resolved the
/// replica list from the handle's PV.
pub async fn expand_replicated(
    env: &dyn ExpandEnv,
    record_home: &str,
    replicas: &[ReplicaInfo],
    new_size_bytes: u64,
) -> Result<ExpandDone, ExpandRefusal> {
    let (current_bytes, record) = env
        .record_home_state(record_home)
        .await
        .map_err(|e| {
            ExpandRefusal::Unavailable(format!(
                "record PV {} unreadable (retried automatically): {}",
                record_home, e
            ))
        })?;

    match current_bytes {
        Some(cur) if new_size_bytes <= cur => {
            println!(
                "ℹ️ [CONTROLLER] New size {} <= current size {}, no expansion needed",
                new_size_bytes, cur
            );
            return Ok(ExpandDone { capacity_bytes: cur, node_expansion_required: false });
        }
        Some(_) => {}
        // bdev_lvol_resize would happily SHRINK; without a readable current
        // size the shrink guard cannot run — refuse.
        None => {
            return Err(ExpandRefusal::FailedPrecondition(format!(
                "Volume {}: PV capacity unreadable — refusing a resize whose shrink \
                 guard cannot run",
                record_home
            )))
        }
    }

    // The belt (F43 ordering constraint, C2): never grow while a leg lags —
    // the raid grows on the serving legs only and the lagging leg returns
    // undersized (the leg-size guard then refuses it loudly, and without
    // the F56 catch-up size alignment the volume is left needing manual
    // repair). One leg with no record to prove anything: same refusal —
    // absence of evidence.
    if replicas.len() > 1 {
        let Some(record) = record.as_ref() else {
            return Err(ExpandRefusal::Unavailable(format!(
                "Volume {}: no replica sync record — cannot prove every leg holds the \
                 acknowledged history; expansion retries automatically",
                record_home
            )));
        };
        let lagging = replicas_not_in_sync(record, replicas);
        if !lagging.is_empty() {
            let msg = format!("expansion refused while replicas lag: {}", lagging.join("; "));
            println!("📏 [CONTROLLER] {}: {}", record_home, msg);
            env.emit(record_home, "Warning", "ExpandRefusedReplicasNotInSync", &msg).await;
            return Err(ExpandRefusal::Unavailable(format!(
                "Volume {}: {} — expansion retries automatically once all replicas \
                 are in_sync",
                record_home, msg
            )));
        }
    }

    // Fan-out. Addressed by the LIVE lvol uuid: after a catch-up revert the
    // head is a re-created clone under a new uuid (`active_lvol_uuid`) —
    // resizing the identity uuid would target a dead lvol. Same-size resize
    // is a blobstore no-op, so the external-resizer's retry safely
    // re-drives a partial failure.
    let mut failures: Vec<String> = Vec::new();
    for r in replicas {
        let live_uuid = record
            .as_ref()
            .and_then(|rec| rec.get(&r.lvol_uuid))
            .map(|rec| rec.live_lvol_uuid().to_string())
            .unwrap_or_else(|| r.lvol_uuid.clone());
        match env.resize_leg(&r.node_name, &live_uuid, new_size_bytes).await {
            Ok(_) => println!(
                "✅ [CONTROLLER] Resized leg {} on {} to {} bytes",
                live_uuid, r.node_name, new_size_bytes
            ),
            Err(e) => failures.push(format!("{} on {}: {}", live_uuid, r.node_name, e)),
        }
    }
    if !failures.is_empty() {
        let msg = format!(
            "resize applied to {}/{} legs; failed: {}",
            replicas.len() - failures.len(),
            replicas.len(),
            failures.join("; ")
        );
        env.emit(record_home, "Warning", "ExpandReplicaFanoutIncomplete", &msg).await;
        return Err(ExpandRefusal::Unavailable(format!(
            "Volume {}: {} — retried automatically (lvol resize is idempotent)",
            record_home, msg
        )));
    }

    println!(
        "✅ [CONTROLLER] Volume expanded to {} bytes on all {} replicas \
         (raid + namespace growth propagate via SPDK resize events)",
        new_size_bytes,
        replicas.len()
    );
    Ok(ExpandDone { capacity_bytes: new_size_bytes, node_expansion_required: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeEnv {
        capacity: Option<u64>,
        pv_unreadable: bool,
        record: Option<VolumeSyncRecord>,
        /// (node, lvol_uuid) → error message for resize_leg
        fail: HashMap<(String, String), String>,
        resized: Mutex<Vec<(String, String, u64)>>,
        events: Mutex<Vec<(String, String)>>,
    }

    impl FakeEnv {
        fn new(capacity: Option<u64>, record: Option<VolumeSyncRecord>) -> Self {
            FakeEnv {
                capacity,
                pv_unreadable: false,
                record,
                fail: HashMap::new(),
                resized: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ExpandEnv for FakeEnv {
        async fn record_home_state(
            &self,
            _record_home: &str,
        ) -> Result<(Option<u64>, Option<VolumeSyncRecord>), String> {
            if self.pv_unreadable {
                return Err("connection refused".to_string());
            }
            Ok((self.capacity, self.record.clone()))
        }

        async fn resize_leg(
            &self,
            node: &str,
            lvol_uuid: &str,
            new_size_bytes: u64,
        ) -> Result<(), String> {
            if let Some(e) = self.fail.get(&(node.to_string(), lvol_uuid.to_string())) {
                return Err(e.clone());
            }
            self.resized.lock().unwrap().push((
                node.to_string(),
                lvol_uuid.to_string(),
                new_size_bytes,
            ));
            Ok(())
        }

        async fn emit(&self, _record_home: &str, type_: &str, reason: &str, _msg: &str) {
            self.events.lock().unwrap().push((reason.to_string(), type_.to_string()));
        }
    }

    fn replica(node: &str, uuid: &str) -> ReplicaInfo {
        ReplicaInfo {
            node_name: node.to_string(),
            node_uid: format!("{}-uid", node),
            disk_pci_address: "0000:00:00.0".to_string(),
            lvol_uuid: uuid.to_string(),
            lvol_name: format!("lvol-{}", uuid),
            lvs_name: "lvs0".to_string(),
            nqn: None,
            target_ip: None,
            target_port: None,
            health: "online".to_string(),
        }
    }

    fn record_all_in_sync(uuids: &[&str]) -> VolumeSyncRecord {
        let mut rec = VolumeSyncRecord::initial(
            &uuids.iter().map(|u| replica("n", u)).collect::<Vec<_>>(),
        );
        for u in uuids {
            rec.mark_in_sync(u, "epoch-v-1", "in sync", "t1");
        }
        rec
    }

    const GIB: u64 = 1 << 30;

    #[tokio::test]
    async fn belt_refuses_lagging_legs_before_any_resize() {
        let mut record = record_all_in_sync(&["uuid-a", "uuid-b"]);
        record.mark_stale("uuid-b", "leg failed", "t2");
        let env = FakeEnv::new(Some(GIB), Some(record));
        let legs = vec![replica("node-a", "uuid-a"), replica("node-b", "uuid-b")];

        let err = expand_replicated(&env, "vol1", &legs, 2 * GIB).await.unwrap_err();

        assert!(matches!(err, ExpandRefusal::Unavailable(ref m) if m.contains("replicas lag")));
        assert!(env.resized.lock().unwrap().is_empty(), "no partial fan-out behind the belt");
        assert_eq!(
            *env.events.lock().unwrap(),
            vec![("ExpandRefusedReplicasNotInSync".to_string(), "Warning".to_string())]
        );
    }

    #[tokio::test]
    async fn no_record_refuses_absence_of_evidence() {
        let env = FakeEnv::new(Some(GIB), None);
        let legs = vec![replica("node-a", "uuid-a"), replica("node-b", "uuid-b")];

        let err = expand_replicated(&env, "vol1", &legs, 2 * GIB).await.unwrap_err();

        assert!(matches!(err, ExpandRefusal::Unavailable(ref m) if m.contains("no replica sync record")));
        assert!(env.resized.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn partial_fanout_applies_survivors_and_reports_incomplete() {
        // The F56 precondition, pinned: a mid-fan-out failure leaves the
        // reachable legs GROWN — the divergence the catch-up size
        // alignment exists to heal.
        let record = record_all_in_sync(&["uuid-a", "uuid-b"]);
        let mut env = FakeEnv::new(Some(GIB), Some(record));
        env.fail.insert(
            ("node-b".to_string(), "uuid-b".to_string()),
            "connection refused".to_string(),
        );
        let legs = vec![replica("node-a", "uuid-a"), replica("node-b", "uuid-b")];

        let err = expand_replicated(&env, "vol1", &legs, 2 * GIB).await.unwrap_err();

        assert!(matches!(err, ExpandRefusal::Unavailable(ref m) if m.contains("1/2 legs")));
        assert_eq!(
            *env.resized.lock().unwrap(),
            vec![("node-a".to_string(), "uuid-a".to_string(), 2 * GIB)],
            "the reachable leg was grown before the failure was reported"
        );
        assert_eq!(
            *env.events.lock().unwrap(),
            vec![("ExpandReplicaFanoutIncomplete".to_string(), "Warning".to_string())]
        );
    }

    #[tokio::test]
    async fn fanout_targets_the_live_uuid_after_a_revert() {
        let mut record = record_all_in_sync(&["uuid-a", "uuid-b"]);
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        let env = FakeEnv::new(Some(GIB), Some(record));
        let legs = vec![replica("node-a", "uuid-a"), replica("node-b", "uuid-b")];

        let done = expand_replicated(&env, "vol1", &legs, 2 * GIB).await.unwrap();

        assert_eq!(done, ExpandDone { capacity_bytes: 2 * GIB, node_expansion_required: true });
        let resized = env.resized.lock().unwrap();
        assert!(
            resized.iter().any(|(_, u, _)| u == "uuid-b-v2"),
            "post-revert head must be addressed by active_lvol_uuid, got {:?}",
            *resized
        );
        assert!(resized.iter().all(|(_, u, _)| u != "uuid-b"));
    }

    #[tokio::test]
    async fn noop_when_new_size_not_larger() {
        let env = FakeEnv::new(Some(2 * GIB), Some(record_all_in_sync(&["uuid-a"])));
        let legs = vec![replica("node-a", "uuid-a")];

        let done = expand_replicated(&env, "vol1", &legs, GIB).await.unwrap();

        assert_eq!(done, ExpandDone { capacity_bytes: 2 * GIB, node_expansion_required: false });
        assert!(env.resized.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unreadable_capacity_refuses_failed_precondition() {
        let env = FakeEnv::new(None, Some(record_all_in_sync(&["uuid-a"])));
        let legs = vec![replica("node-a", "uuid-a")];

        let err = expand_replicated(&env, "vol1", &legs, 2 * GIB).await.unwrap_err();

        assert!(matches!(err, ExpandRefusal::FailedPrecondition(ref m) if m.contains("shrink")));
        assert!(env.resized.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn single_replica_in_list_skips_belt_but_fans_out() {
        // len == 1: no belt (nothing to disagree), fan-out still runs.
        let env = FakeEnv::new(Some(GIB), None);
        let legs = vec![replica("node-a", "uuid-a")];

        let done = expand_replicated(&env, "vol1", &legs, 2 * GIB).await.unwrap();

        assert_eq!(done.capacity_bytes, 2 * GIB);
        assert_eq!(env.resized.lock().unwrap().len(), 1);
    }
}
