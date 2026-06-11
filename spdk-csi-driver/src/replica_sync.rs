// replica_sync.rs — persistent per-replica sync state on the PV.
// Phase 1 of docs/incremental-replica-rebuild.md §9.
//
// The PV — not the raid superblock — is the authoritative record of which
// replicas hold the volume's acknowledged write history (§2 governing
// principle). Immutable replica identity lives in
// PV.spec.csi.volumeAttributes["flint.csi.storage.io/replicas"]; this module
// owns the mutable companion record in the PV annotation
// "flint.csi.storage.io/replica-sync-state":
//
//   { "current_epoch": null,
//     "replicas": [ { "node_name": "...", "node_uid": "...",
//                     "lvol_uuid": "...", "sync_state": "in_sync",
//                     "last_epoch": null, "since": "...", "reason": "..." } ] }
//
// Phase 1 records state truthfully but changes no behavior: nothing consumes
// sync_state for raid membership yet (phase 4), and nothing transitions a
// replica back to in_sync (the catch-up orchestrator, phase 3). In
// particular, a replica that misses acknowledged writes stays `stale` even
// after a later reassembly re-admits it — that admission is today's
// documented divergence hazard, surfaced via a StaleReplicaAdmitted event.
//
// Writers: the controller seeds the record after CreateVolume; the consumer
// node's agent (raid health monitor) and NodeStage record stale transitions.
// Updates are read-modify-write patches guarded by resourceVersion, so a
// concurrent writer costs a retry, never a silently lost transition.

use k8s_openapi::api::core::v1::PersistentVolume;
use kube::api::{Patch, PatchParams};
use kube::Api;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::minimal_models::ReplicaInfo;

/// Mutable sync record (this module). volumeAttributes hold the immutable
/// identities; the annotation holds everything that changes over time.
pub const SYNC_STATE_ANNOTATION: &str = "flint.csi.storage.io/replica-sync-state";
/// Immutable replica identity list, written by CreateVolume.
pub const REPLICAS_ATTRIBUTE: &str = "flint.csi.storage.io/replicas";
pub const REPLICA_COUNT_ATTRIBUTE: &str = "flint.csi.storage.io/replica-count";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Holds every acknowledged write; eligible raid member.
    InSync,
    /// Missed acknowledged writes (leg failure, excluded from assembly, node
    /// outage). Not a valid read source until caught up (phase 3).
    Stale,
    /// Caught up and chasing epochs; rejoins at the next assembly (phase 3/4).
    Standby,
}

impl SyncState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncState::InSync => "in_sync",
            SyncState::Stale => "stale",
            SyncState::Standby => "standby",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaSyncRecord {
    pub node_name: String,
    pub node_uid: String,
    pub lvol_uuid: String,
    pub sync_state: SyncState,
    /// Newest common epoch known present on this replica. None until the
    /// snapshot scheduler (phase 2) cuts the first epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_epoch: Option<String>,
    /// RFC3339 time of the last sync_state transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Why the last transition happened (operator-facing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeSyncRecord {
    /// The volume's current common epoch name (phase 2 owns this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_epoch: Option<String>,
    pub replicas: Vec<ReplicaSyncRecord>,
}

impl VolumeSyncRecord {
    /// Fresh record at volume creation: every replica in_sync, no epochs yet.
    /// Order mirrors the volumeAttributes replica list — replica index is
    /// positional everywhere else (per-replica NQNs, base bdev names).
    pub fn initial(replicas: &[ReplicaInfo]) -> Self {
        VolumeSyncRecord {
            current_epoch: None,
            replicas: replicas
                .iter()
                .map(|r| ReplicaSyncRecord {
                    node_name: r.node_name.clone(),
                    node_uid: r.node_uid.clone(),
                    lvol_uuid: r.lvol_uuid.clone(),
                    sync_state: SyncState::InSync,
                    last_epoch: None,
                    since: None,
                    reason: None,
                })
                .collect(),
        }
    }

    pub fn from_annotation(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }

    pub fn to_annotation(&self) -> String {
        serde_json::to_string(self).expect("VolumeSyncRecord serialization cannot fail")
    }

    /// Align the record's membership with the authoritative identity list,
    /// preserving known states and positional order. New identities (replica
    /// replacement, records written by an older build) enter as in_sync —
    /// CreateVolume's initial state; replacement flows own their state from
    /// phase 5 on. Returns true if anything changed.
    pub fn reconcile_membership(&mut self, replicas: &[ReplicaInfo]) -> bool {
        let rebuilt: Vec<ReplicaSyncRecord> = replicas
            .iter()
            .map(|r| {
                self.replicas
                    .iter()
                    .find(|rec| rec.lvol_uuid == r.lvol_uuid)
                    .cloned()
                    .unwrap_or_else(|| ReplicaSyncRecord {
                        node_name: r.node_name.clone(),
                        node_uid: r.node_uid.clone(),
                        lvol_uuid: r.lvol_uuid.clone(),
                        sync_state: SyncState::InSync,
                        last_epoch: None,
                        since: None,
                        reason: None,
                    })
            })
            .collect();
        let changed = rebuilt != self.replicas;
        self.replicas = rebuilt;
        changed
    }

    pub fn get(&self, lvol_uuid: &str) -> Option<&ReplicaSyncRecord> {
        self.replicas.iter().find(|r| r.lvol_uuid == lvol_uuid)
    }

    /// Transition a replica to stale. Idempotent: an already-stale replica
    /// keeps its original timestamp/reason (they mark when divergence began).
    /// Returns true only on a real transition.
    pub fn mark_stale(&mut self, lvol_uuid: &str, reason: &str, now_rfc3339: &str) -> bool {
        match self.replicas.iter_mut().find(|r| r.lvol_uuid == lvol_uuid) {
            Some(rec) if rec.sync_state != SyncState::Stale => {
                rec.sync_state = SyncState::Stale;
                rec.since = Some(now_rfc3339.to_string());
                rec.reason = Some(reason.to_string());
                true
            }
            _ => false,
        }
    }
}

/// The base bdev name `connect_to_nvmeof_target` produces for a remote
/// replica: "nvme_" + per-replica NQN with ':' and '.' mangled to '_', plus
/// the "n1" namespace suffix. Local replicas need no equivalent — an lvol
/// bdev's name is its uuid.
pub fn expected_remote_base_bdev(volume_id: &str, replica_index: usize) -> String {
    let nqn = format!("nqn.2024-11.com.flint:volume:{}_{}", volume_id, replica_index);
    format!("nvme_{}n1", nqn.replace(':', "_").replace('.', "_"))
}

/// Replicas of `record` not backed by a configured base of `raid`
/// (a `bdev_raid_get_bdevs` entry), i.e. replicas missing acknowledged
/// writes. Returns None unless the raid is online: only an online raid
/// serves writes, so a CONFIGURING phantom or an offline leftover implies
/// nothing about replica data.
///
/// Matching is by set difference against the *healthy* bases — when a leg
/// fails on an online raid, SPDK nulls both the slot's name and uuid
/// (raid_bdev_free_base_bdev_resource), so the failed slot itself is
/// unidentifiable. A configured base matches a replica by uuid (lvols expose
/// their uuid as the bdev uuid, and the NVMe-oF target propagates the
/// backing bdev's uuid into the namespace, subsystem.c:2608), by name equal
/// to the lvol uuid (local base), or by the deterministic remote bdev name.
pub fn replicas_missing_from_raid(
    raid: &Value,
    volume_id: &str,
    record: &VolumeSyncRecord,
) -> Option<Vec<String>> {
    let state = raid.get("state").and_then(|s| s.as_str()).unwrap_or("unknown");
    if state != "online" {
        return None;
    }

    let configured: Vec<&Value> = raid
        .get("base_bdevs_list")
        .and_then(|b| b.as_array())
        .map(|bases| {
            bases
                .iter()
                .filter(|b| b.get("is_configured").and_then(|c| c.as_bool()).unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();

    let missing = record
        .replicas
        .iter()
        .enumerate()
        .filter(|(index, rec)| {
            let remote_name = expected_remote_base_bdev(volume_id, *index);
            !configured.iter().any(|base| {
                let uuid = base.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
                let name = base.get("name").and_then(|n| n.as_str()).unwrap_or("");
                uuid == rec.lvol_uuid || name == rec.lvol_uuid || name == remote_name
            })
        })
        .map(|(_, rec)| rec.lvol_uuid.clone())
        .collect();
    Some(missing)
}

/// Extract the immutable replica identity list from a PV's volumeAttributes.
/// Ok(None) for single-replica volumes (no sync record applies).
pub fn replicas_from_pv(
    pv: &PersistentVolume,
) -> Result<Option<Vec<ReplicaInfo>>, Box<dyn std::error::Error + Send + Sync>> {
    let attrs = pv
        .spec
        .as_ref()
        .ok_or("PV has no spec")?
        .csi
        .as_ref()
        .ok_or("PV has no CSI spec")?
        .volume_attributes
        .as_ref()
        .ok_or("PV has no volumeAttributes")?;

    let replica_count = attrs
        .get(REPLICA_COUNT_ATTRIBUTE)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);
    if replica_count <= 1 {
        return Ok(None);
    }

    let replicas_json = attrs
        .get(REPLICAS_ATTRIBUTE)
        .ok_or("Multi-replica volume missing replicas attribute")?;
    let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)?;
    Ok(Some(replicas))
}

/// Read-modify-write the volume's sync record. Loads the record from the PV
/// annotation (synthesizing the initial record from volumeAttributes when
/// absent or unparseable), reconciles membership, applies `mutate`, and
/// patches it back guarded by resourceVersion — a concurrent writer costs a
/// conflict retry, never a lost transition. `mutate` may therefore run more
/// than once and must be idempotent. Ok(None) for single-replica volumes.
pub async fn update_sync_record<F>(
    client: &kube::Client,
    volume_id: &str,
    mut mutate: F,
) -> Result<Option<VolumeSyncRecord>, Box<dyn std::error::Error + Send + Sync>>
where
    F: FnMut(&mut VolumeSyncRecord),
{
    const MAX_ATTEMPTS: usize = 3;
    let pvs: Api<PersistentVolume> = Api::all(client.clone());

    for attempt in 1..=MAX_ATTEMPTS {
        let pv = pvs.get(volume_id).await?;
        let Some(replicas) = replicas_from_pv(&pv)? else {
            return Ok(None);
        };

        let annotation = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(SYNC_STATE_ANNOTATION));
        let parsed = annotation.map(|s| VolumeSyncRecord::from_annotation(s));
        if let Some(Err(e)) = &parsed {
            warn!(
                volume_id, error = %e,
                "[REPLICA_SYNC] Unparseable sync record annotation — rebuilding from volumeAttributes"
            );
        }
        let baseline = parsed.and_then(|p| p.ok());
        let mut record = baseline
            .clone()
            .unwrap_or_else(|| VolumeSyncRecord::initial(&replicas));
        record.reconcile_membership(&replicas);
        mutate(&mut record);

        if baseline.as_ref() == Some(&record) {
            return Ok(Some(record));
        }

        // resourceVersion in a merge patch makes the API server reject the
        // write if the PV changed since our read.
        let patch = json!({
            "metadata": {
                "resourceVersion": pv.metadata.resource_version,
                "annotations": { SYNC_STATE_ANNOTATION: record.to_annotation() }
            }
        });
        match pvs
            .patch(volume_id, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(_) => return Ok(Some(record)),
            Err(kube::Error::Api(ae)) if ae.code == 409 && attempt < MAX_ATTEMPTS => {
                debug!(volume_id, attempt, "[REPLICA_SYNC] Sync record write conflict — retrying");
                continue;
            }
            Err(e) => return Err(format!("Failed to patch sync record: {}", e).into()),
        }
    }
    Err(format!(
        "Sync record for {} did not converge after {} attempts",
        volume_id, MAX_ATTEMPTS
    )
    .into())
}

/// Best-effort Kubernetes event attached to a PV (shared by the node agent's
/// health monitor and the driver's NodeStage path).
pub async fn emit_pv_event(
    client: &kube::Client,
    reporting_instance: &str,
    pv_name: &str,
    event_type: &str,
    reason: &str,
    message: &str,
) {
    use k8s_openapi::api::core::v1::Event;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use kube::api::PostParams;

    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let event = Event {
        metadata: ObjectMeta {
            // Unique-enough name; events are best effort
            name: Some(format!("{}.{:x}", pv_name, now_nanos)),
            namespace: Some("default".to_string()),
            ..Default::default()
        },
        involved_object: k8s_openapi::api::core::v1::ObjectReference {
            api_version: Some("v1".to_string()),
            kind: Some("PersistentVolume".to_string()),
            name: Some(pv_name.to_string()),
            ..Default::default()
        },
        type_: Some(event_type.to_string()),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        reporting_component: Some("flint.csi.storage.io/node-agent".to_string()),
        reporting_instance: Some(reporting_instance.to_string()),
        event_time: Some(MicroTime(k8s_openapi::jiff::Timestamp::now())),
        action: Some("HealthCheck".to_string()),
        ..Default::default()
    };

    let events: Api<Event> = Api::namespaced(client.clone(), "default");
    if let Err(e) = events.create(&PostParams::default(), &event).await {
        debug!(pv_name, error = %e, "[REPLICA_SYNC] Failed to emit event (non-fatal)");
    }
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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

    fn three_replica_record() -> VolumeSyncRecord {
        VolumeSyncRecord::initial(&[
            replica("node-a", "uuid-a"),
            replica("node-b", "uuid-b"),
            replica("node-c", "uuid-c"),
        ])
    }

    #[test]
    fn sync_state_wire_format_is_stable() {
        // These strings are the §6 state machine vocabulary and end up in PV
        // annotations — changing them breaks records written by older builds.
        assert_eq!(serde_json::to_string(&SyncState::InSync).unwrap(), "\"in_sync\"");
        assert_eq!(serde_json::to_string(&SyncState::Stale).unwrap(), "\"stale\"");
        assert_eq!(serde_json::to_string(&SyncState::Standby).unwrap(), "\"standby\"");
    }

    #[test]
    fn initial_record_all_in_sync_in_order() {
        let record = three_replica_record();
        assert_eq!(record.current_epoch, None);
        assert_eq!(record.replicas.len(), 3);
        let uuids: Vec<&str> = record.replicas.iter().map(|r| r.lvol_uuid.as_str()).collect();
        assert_eq!(uuids, vec!["uuid-a", "uuid-b", "uuid-c"]);
        assert!(record.replicas.iter().all(|r| r.sync_state == SyncState::InSync));
    }

    #[test]
    fn annotation_roundtrip() {
        let mut record = three_replica_record();
        record.current_epoch = Some("epoch-vol-7".to_string());
        record.mark_stale("uuid-b", "test", "2026-06-11T00:00:00Z");
        let parsed = VolumeSyncRecord::from_annotation(&record.to_annotation()).unwrap();
        assert_eq!(parsed, record);
    }

    #[test]
    fn mark_stale_transitions_once() {
        let mut record = three_replica_record();
        assert!(record.mark_stale("uuid-b", "leg failed", "t0"));
        assert_eq!(record.get("uuid-b").unwrap().sync_state, SyncState::Stale);
        assert_eq!(record.get("uuid-b").unwrap().since.as_deref(), Some("t0"));

        // Already stale: no transition, original timestamp/reason preserved.
        assert!(!record.mark_stale("uuid-b", "leg failed again", "t1"));
        assert_eq!(record.get("uuid-b").unwrap().since.as_deref(), Some("t0"));
        assert_eq!(record.get("uuid-b").unwrap().reason.as_deref(), Some("leg failed"));

        // Unknown replica: no-op.
        assert!(!record.mark_stale("uuid-zz", "?", "t2"));

        // standby → stale is a legal transition (standby failed again).
        record.replicas[2].sync_state = SyncState::Standby;
        assert!(record.mark_stale("uuid-c", "standby lost", "t3"));
    }

    #[test]
    fn reconcile_membership_adds_drops_and_orders() {
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", "t0");

        // uuid-c replaced by uuid-d; order comes from the identity list.
        let replicas = vec![
            replica("node-b", "uuid-b"),
            replica("node-a", "uuid-a"),
            replica("node-d", "uuid-d"),
        ];
        assert!(record.reconcile_membership(&replicas));
        let uuids: Vec<&str> = record.replicas.iter().map(|r| r.lvol_uuid.as_str()).collect();
        assert_eq!(uuids, vec!["uuid-b", "uuid-a", "uuid-d"]);
        // Known state survives reordering; the new identity enters in_sync.
        assert_eq!(record.get("uuid-b").unwrap().sync_state, SyncState::Stale);
        assert_eq!(record.get("uuid-d").unwrap().sync_state, SyncState::InSync);

        // Idempotent.
        assert!(!record.reconcile_membership(&replicas));
    }

    fn raid_json(state: &str, bases: Vec<Value>) -> Value {
        json!({
            "name": "raid_vol1",
            "state": state,
            "num_base_bdevs": bases.len(),
            "base_bdevs_list": bases,
        })
    }

    #[test]
    fn missing_none_when_all_configured() {
        let record = three_replica_record();
        let raid = raid_json(
            "online",
            vec![
                json!({"name": "uuid-a", "uuid": "uuid-a", "is_configured": true}),
                json!({"name": expected_remote_base_bdev("vol1", 1), "uuid": "uuid-b", "is_configured": true}),
                json!({"name": expected_remote_base_bdev("vol1", 2), "uuid": "uuid-c", "is_configured": true}),
            ],
        );
        assert_eq!(replicas_missing_from_raid(&raid, "vol1", &record).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn missing_detects_failed_slot_with_nulled_identity() {
        // When a leg fails on an online raid, SPDK frees the slot's name and
        // nulls its uuid — the failed slot itself cannot identify the
        // replica. Set difference against healthy bases must find it.
        let record = three_replica_record();
        let raid = raid_json(
            "online",
            vec![
                json!({"name": "uuid-a", "uuid": "uuid-a", "is_configured": true}),
                json!({"name": null, "uuid": "00000000-0000-0000-0000-000000000000", "is_configured": false}),
                json!({"name": expected_remote_base_bdev("vol1", 2), "uuid": "uuid-c", "is_configured": true}),
            ],
        );
        assert_eq!(
            replicas_missing_from_raid(&raid, "vol1", &record).unwrap(),
            vec!["uuid-b".to_string()]
        );
    }

    #[test]
    fn missing_detects_replica_never_attached() {
        // Degraded creation: the raid has slots only for the bases it was
        // created with; an excluded replica has no slot at all.
        let record = three_replica_record();
        let raid = raid_json(
            "online",
            vec![
                json!({"name": "uuid-a", "uuid": "uuid-a", "is_configured": true}),
                json!({"name": expected_remote_base_bdev("vol1", 1), "uuid": "uuid-b", "is_configured": true}),
            ],
        );
        assert_eq!(
            replicas_missing_from_raid(&raid, "vol1", &record).unwrap(),
            vec!["uuid-c".to_string()]
        );
    }

    #[test]
    fn missing_is_none_unless_online() {
        // A CONFIGURING phantom (examine hook) or offline leftover serves no
        // writes — it implies nothing about replica staleness.
        let record = three_replica_record();
        let raid = raid_json("configuring", vec![]);
        assert!(replicas_missing_from_raid(&raid, "vol1", &record).is_none());
    }

    #[test]
    fn missing_matches_remote_base_by_name_when_uuid_differs() {
        // Belt-and-braces: if uuid propagation over NVMe-oF ever fails, the
        // deterministic remote bdev name still matches.
        let record = three_replica_record();
        let raid = raid_json(
            "online",
            vec![
                json!({"name": "uuid-a", "uuid": "uuid-a", "is_configured": true}),
                json!({"name": expected_remote_base_bdev("vol1", 1), "uuid": "some-nguid", "is_configured": true}),
                json!({"name": expected_remote_base_bdev("vol1", 2), "uuid": "other-nguid", "is_configured": true}),
            ],
        );
        assert_eq!(replicas_missing_from_raid(&raid, "vol1", &record).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn expected_remote_base_bdev_matches_connect_convention() {
        // Mirrors connect_to_nvmeof_target: "nvme_" + NQN with ':' and '.'
        // replaced by '_', plus namespace suffix "n1".
        assert_eq!(
            expected_remote_base_bdev("pvc-123", 1),
            "nvme_nqn_2024-11_com_flint_volume_pvc-123_1n1"
        );
    }

    #[test]
    fn corrupt_annotation_fails_parse() {
        assert!(VolumeSyncRecord::from_annotation("not json").is_err());
        // Unknown fields from a newer build must not break older parsers.
        let forward = r#"{"current_epoch":null,"replicas":[],"future_field":1}"#;
        assert!(VolumeSyncRecord::from_annotation(forward).is_ok());
    }
}
