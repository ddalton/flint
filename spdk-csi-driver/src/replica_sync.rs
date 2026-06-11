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
// sync_state for raid membership yet (phase 4). Phase 3's catch-up
// orchestrator (catchup.rs) transitions stale → standby; in_sync admission
// remains phase 4's. A replica that misses acknowledged writes therefore
// still stays out of `in_sync` even after a later reassembly re-admits it —
// that admission is today's documented divergence hazard, surfaced via a
// StaleReplicaAdmitted event.
//
// Writers: the controller seeds the record after CreateVolume; the consumer
// node's agent (raid health monitor) and NodeStage record stale transitions;
// the catch-up orchestrator records revert/standby/chase progress.
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
    /// Live head lvol uuid when it differs from the immutable identity in
    /// volumeAttributes — the catch-up revert (§5 step 0) deletes the head
    /// and re-creates it as a clone, which assigns a new uuid. The lvol
    /// *name* (and so the `lvs/name` alias) is preserved across the revert;
    /// this override is how anything addressing the replica by uuid finds
    /// the live bdev. None = volumeAttributes' uuid is still live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_lvol_uuid: Option<String>,
    /// Epoch the head was last reverted to (catch-up §5 step 0). While set,
    /// the head is a write-virgin clone of this epoch that has only ever
    /// received catch-up copy writes — the orchestrator may resume an
    /// interrupted copy onto it without re-reverting. Any transition to
    /// in_sync (phase 4) MUST clear it: from then on the head takes raid
    /// writes and a later catch-up must revert again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverted_to: Option<String>,
}

impl ReplicaSyncRecord {
    /// The uuid that addresses the live head lvol (post-revert override, or
    /// the immutable identity uuid).
    pub fn live_lvol_uuid(&self) -> &str {
        self.active_lvol_uuid.as_deref().unwrap_or(&self.lvol_uuid)
    }
}

/// One retained common epoch (phase 2). An epoch is recorded here only after
/// its snapshot was cut on *every* in-sync replica — that is what "common"
/// means and what the §5 catch-up correctness argument relies on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpochEntry {
    pub name: String,
    /// RFC3339 time the epoch was recorded common. A conservative upper
    /// bound on every per-replica cut time (an EEXIST-converged retry may
    /// have cut a replica earlier) — phase 3's `T_back` back-off must
    /// compare against this, which only ever errs toward an older, safer
    /// base epoch.
    pub recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeSyncRecord {
    /// The volume's current common epoch name (phase 2 owns this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_epoch: Option<String>,
    /// Retained common epochs, oldest first; the newest entry is
    /// `current_epoch`. Maintained by the epoch scheduler (phase 2);
    /// serde defaults keep records written by phase-1 builds parseable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub epochs: Vec<EpochEntry>,
    /// Oldest epoch a pending or active catch-up still needs (§5 retention
    /// pinning). Phase 3 sets it; the scheduler refuses to retire this
    /// epoch or anything newer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_pin: Option<String>,
    pub replicas: Vec<ReplicaSyncRecord>,
}

impl VolumeSyncRecord {
    /// Fresh record at volume creation: every replica in_sync, no epochs yet.
    /// Order mirrors the volumeAttributes replica list — replica index is
    /// positional everywhere else (per-replica NQNs, base bdev names).
    pub fn initial(replicas: &[ReplicaInfo]) -> Self {
        VolumeSyncRecord {
            current_epoch: None,
            epochs: Vec::new(),
            retention_pin: None,
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
                    active_lvol_uuid: None,
                    reverted_to: None,
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
                        active_lvol_uuid: None,
                        reverted_to: None,
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

    /// Transition a replica to standby: caught up through `last_epoch` and
    /// chasing (§6). Stamps last_epoch unconditionally — the chase also uses
    /// this to advance an existing standby's high-water mark. Returns true
    /// if anything changed.
    pub fn mark_standby(
        &mut self,
        lvol_uuid: &str,
        last_epoch: &str,
        reason: &str,
        now_rfc3339: &str,
    ) -> bool {
        match self.replicas.iter_mut().find(|r| r.lvol_uuid == lvol_uuid) {
            Some(rec) => {
                let mut changed = false;
                if rec.sync_state != SyncState::Standby {
                    rec.sync_state = SyncState::Standby;
                    rec.since = Some(now_rfc3339.to_string());
                    rec.reason = Some(reason.to_string());
                    changed = true;
                }
                if rec.last_epoch.as_deref() != Some(last_epoch) {
                    rec.last_epoch = Some(last_epoch.to_string());
                    changed = true;
                }
                changed
            }
            None => false,
        }
    }

    /// Admit a replica back to in_sync (phase 4: the fenced final delta at
    /// reassembly equalized its head with the survivors'). Stamps
    /// `last_epoch` with the final epoch and ALWAYS clears `reverted_to` —
    /// from now on the head takes raid writes, so a later catch-up must
    /// revert again (the documented phase-3 obligation). `active_lvol_uuid`
    /// is kept: the post-revert head stays the live lvol from here on.
    /// Returns true if anything changed.
    pub fn mark_in_sync(
        &mut self,
        lvol_uuid: &str,
        last_epoch: &str,
        reason: &str,
        now_rfc3339: &str,
    ) -> bool {
        match self.replicas.iter_mut().find(|r| r.lvol_uuid == lvol_uuid) {
            Some(rec) => {
                let mut changed = false;
                if rec.sync_state != SyncState::InSync {
                    rec.sync_state = SyncState::InSync;
                    rec.since = Some(now_rfc3339.to_string());
                    rec.reason = Some(reason.to_string());
                    changed = true;
                }
                if rec.last_epoch.as_deref() != Some(last_epoch) {
                    rec.last_epoch = Some(last_epoch.to_string());
                    changed = true;
                }
                if rec.reverted_to.is_some() {
                    rec.reverted_to = None;
                    changed = true;
                }
                changed
            }
            None => false,
        }
    }

    /// Lower the retention pin to `epoch` (§5 retention pinning). A pin only
    /// ever moves toward older epochs here — an existing older (or
    /// unparseable, i.e. pin-everything) pin is kept. Returns true if the
    /// pin changed.
    pub fn pin_retention(&mut self, volume_id: &str, epoch: &str) -> bool {
        let new_seq = epoch_seq(volume_id, epoch);
        let keep_existing = match (&self.retention_pin, new_seq) {
            (None, _) => false,
            // Unparseable existing pin pins everything already.
            (Some(existing), Some(new)) => match epoch_seq(volume_id, existing) {
                Some(old) => old <= new,
                None => true,
            },
            // Unparseable new pin: only "upgrade" if there is no pin at all.
            (Some(_), None) => true,
        };
        if keep_existing {
            return false;
        }
        self.retention_pin = Some(epoch.to_string());
        true
    }

    /// Clear the retention pin if it is exactly `epoch` (the pin this
    /// catch-up set). A different pin belongs to someone else — leave it.
    pub fn clear_pin_if(&mut self, epoch: &str) -> bool {
        if self.retention_pin.as_deref() == Some(epoch) {
            self.retention_pin = None;
            return true;
        }
        false
    }

    /// Highest epoch sequence number recorded for this volume (0 if none).
    /// The next cut uses this + 1, so a sequence number is never reused even
    /// after old epochs are retired.
    pub fn latest_epoch_seq(&self, volume_id: &str) -> u64 {
        self.epochs
            .iter()
            .filter_map(|e| epoch_seq(volume_id, &e.name))
            .max()
            .unwrap_or(0)
    }

    /// Record a common epoch after its snapshot was cut on every in-sync
    /// replica: append the entry, advance current_epoch, and stamp
    /// last_epoch on exactly the replicas that were cut (a stale replica's
    /// last_epoch stays frozen at the last epoch it participated in).
    /// Idempotent — the resourceVersion-guarded writer may re-apply.
    pub fn apply_epoch_cut(&mut self, epoch: &str, cut_lvol_uuids: &[String], now_rfc3339: &str) {
        if !self.epochs.iter().any(|e| e.name == epoch) {
            self.epochs.push(EpochEntry {
                name: epoch.to_string(),
                recorded_at: now_rfc3339.to_string(),
            });
        }
        self.current_epoch = Some(epoch.to_string());
        for rec in &mut self.replicas {
            if cut_lvol_uuids.iter().any(|u| *u == rec.lvol_uuid) {
                rec.last_epoch = Some(epoch.to_string());
            }
        }
    }

    /// Drop retired epochs from the record, oldest-first, refusing to retire
    /// the current epoch, the pinned epoch or anything newer (§5 retention
    /// pinning — the pin may have appeared between plan and write, so this
    /// re-checks), or anything once an unparseable name is hit (epochs merge
    /// from the oldest end; skipping would break that discipline). Returns
    /// the names actually removed — node-side snapshot deletion is the GC
    /// pass's job, keyed off the updated record.
    pub fn retire_epochs(&mut self, volume_id: &str, names: &[String]) -> Vec<String> {
        let pin_seq = match &self.retention_pin {
            // An unparseable pin pins everything (conservative).
            Some(pin) => match epoch_seq(volume_id, pin) {
                Some(seq) => Some(seq),
                None => return Vec::new(),
            },
            None => None,
        };

        let mut retired = Vec::new();
        for name in names {
            if self.current_epoch.as_deref() == Some(name.as_str()) {
                break;
            }
            let Some(seq) = epoch_seq(volume_id, name) else { break };
            if let Some(pin) = pin_seq {
                if seq >= pin {
                    break;
                }
            }
            let before = self.epochs.len();
            self.epochs.retain(|e| e.name != *name);
            if self.epochs.len() < before {
                retired.push(name.clone());
            }
        }
        retired
    }
}

/// Common-epoch snapshot name: `epoch-<volume>-<seq>` (§5). The lvol name
/// limit is 63 chars: "epoch-" + a 40-char PV name + "-" + seq leaves 16
/// digits of headroom, far beyond any realistic sequence number.
pub fn epoch_name(volume_id: &str, seq: u64) -> String {
    format!("epoch-{}-{}", volume_id, seq)
}

/// Parse the sequence number out of one of this volume's epoch names.
/// None for anything else (other volumes' epochs, user snapshots) — callers
/// use this as the "is it ours" filter, so it must stay strict.
pub fn epoch_seq(volume_id: &str, name: &str) -> Option<u64> {
    let rest = name.strip_prefix("epoch-")?.strip_prefix(volume_id)?;
    let digits = rest.strip_prefix('-')?;
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// The base bdev name `connect_to_nvmeof_target` produces for a remote
/// replica: "nvme_" + per-replica NQN with ':' and '.' mangled to '_', plus
/// the "n1" namespace suffix. Local replicas need no equivalent — an lvol
/// bdev's name is its uuid.
pub fn expected_remote_base_bdev(volume_id: &str, replica_index: usize) -> String {
    let nqn = format!("nqn.2024-11.com.flint:volume:{}_{}", volume_id, replica_index);
    format!("nvme_{}n1", nqn.replace(':', "_").replace('.', "_"))
}

/// In-sync replicas of `record` not backed by a configured base of `raid`
/// (a `bdev_raid_get_bdevs` entry), i.e. replicas newly missing acknowledged
/// writes. Returns None unless the raid is online: only an online raid
/// serves writes, so a CONFIGURING phantom or an offline leftover implies
/// nothing about replica data.
///
/// Only `in_sync` replicas are reported: a stale or standby replica is
/// already recorded as missing writes, and "not in the raid" is its expected
/// condition — reporting it would let the health monitor demote a chasing
/// standby back to stale on every tick (phase 4).
///
/// Matching is by set difference against the *healthy* bases — when a leg
/// fails on an online raid, SPDK nulls both the slot's name and uuid
/// (raid_bdev_free_base_bdev_resource), so the failed slot itself is
/// unidentifiable. A configured base matches a replica by uuid (lvols expose
/// their uuid as the bdev uuid, and the NVMe-oF target propagates the
/// backing bdev's uuid into the namespace, subsystem.c:2608), by name equal
/// to the lvol uuid (local base), or by the deterministic remote bdev name.
/// The *live* uuid is matched as well as the identity uuid: after a
/// catch-up revert the head carries `active_lvol_uuid`, and that is the
/// uuid a base admitted at reassembly (phase 4) exposes.
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
        .filter(|(_, rec)| rec.sync_state == SyncState::InSync)
        .filter(|(index, rec)| {
            let remote_name = expected_remote_base_bdev(volume_id, *index);
            let live = rec.live_lvol_uuid();
            !configured.iter().any(|base| {
                let uuid = base.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
                let name = base.get("name").and_then(|n| n.as_str()).unwrap_or("");
                uuid == rec.lvol_uuid
                    || uuid == live
                    || name == rec.lvol_uuid
                    || name == live
                    || name == remote_name
            })
        })
        .map(|(_, rec)| rec.lvol_uuid.clone())
        .collect();
    Some(missing)
}

/// The PV that holds a volume's replica record. RWX/ROX volumes stage
/// through a synthetic PV whose volumeHandle is "nfs-server-<volume>"
/// (rwx_nfs.rs); the replicas — and therefore the sync record — live on the
/// user volume's PV. Everything keyed off a volumeHandle (NodeStage, the
/// health monitor's raid-name strip) must resolve through this before
/// touching the record.
pub fn record_pv_name(volume_id: &str) -> &str {
    volume_id.strip_prefix("nfs-server-").unwrap_or(volume_id)
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
    // RWX synthetic handles ("nfs-server-<vol>") resolve to the user PV.
    let volume_id = record_pv_name(volume_id);

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

    let pv_name = record_pv_name(pv_name);
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

    #[test]
    fn phase1_record_without_epoch_fields_parses() {
        // Records written by phase-1 builds have no epochs/retention_pin.
        let phase1 = r#"{"current_epoch":null,"replicas":[{"node_name":"n","node_uid":"u","lvol_uuid":"x","sync_state":"in_sync"}]}"#;
        let record = VolumeSyncRecord::from_annotation(phase1).unwrap();
        assert!(record.epochs.is_empty());
        assert_eq!(record.retention_pin, None);
    }

    #[test]
    fn epoch_naming_roundtrip_and_strictness() {
        let name = epoch_name("pvc-123", 7);
        assert_eq!(name, "epoch-pvc-123-7");
        assert_eq!(epoch_seq("pvc-123", &name), Some(7));
        // Other volumes' epochs — including ones whose id is a prefix of
        // ours extended by '-' — must not match.
        assert_eq!(epoch_seq("pvc-123", "epoch-pvc-456-7"), None);
        assert_eq!(epoch_seq("pvc-123", "epoch-pvc-123-extra-7"), None);
        // User snapshots and junk must not parse.
        assert_eq!(epoch_seq("pvc-123", "snap_pvc-123_170000"), None);
        assert_eq!(epoch_seq("pvc-123", "epoch-pvc-123-"), None);
        assert_eq!(epoch_seq("pvc-123", "epoch-pvc-123-7x"), None);
    }

    #[test]
    fn apply_epoch_cut_records_and_freezes_stale_last_epoch() {
        let mut record = three_replica_record();
        record.mark_stale("uuid-c", "leg failed", "t0");

        let cut = vec!["uuid-a".to_string(), "uuid-b".to_string()];
        record.apply_epoch_cut("epoch-vol1-1", &cut, "t1");
        assert_eq!(record.current_epoch.as_deref(), Some("epoch-vol1-1"));
        assert_eq!(record.epochs.len(), 1);
        assert_eq!(record.latest_epoch_seq("vol1"), 1);
        assert_eq!(record.get("uuid-a").unwrap().last_epoch.as_deref(), Some("epoch-vol1-1"));
        // The stale replica did not participate: last_epoch stays frozen.
        assert_eq!(record.get("uuid-c").unwrap().last_epoch, None);

        // Idempotent re-apply (resourceVersion conflict retry).
        record.apply_epoch_cut("epoch-vol1-1", &cut, "t2");
        assert_eq!(record.epochs.len(), 1);
        assert_eq!(record.epochs[0].recorded_at, "t1");

        record.apply_epoch_cut("epoch-vol1-2", &cut, "t3");
        assert_eq!(record.latest_epoch_seq("vol1"), 2);
        assert_eq!(record.epochs.len(), 2);
    }

    #[test]
    fn mark_standby_transitions_and_advances() {
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.replicas[1].active_lvol_uuid = Some("uuid-b2".to_string());
        record.replicas[1].reverted_to = Some("epoch-vol1-3".to_string());

        // stale → standby stamps state, epoch, since, reason.
        assert!(record.mark_standby("uuid-b", "epoch-vol1-5", "caught up", "t1"));
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(rec.sync_state, SyncState::Standby);
        assert_eq!(rec.last_epoch.as_deref(), Some("epoch-vol1-5"));
        assert_eq!(rec.since.as_deref(), Some("t1"));
        // The revert bookkeeping survives the transition (the head is still
        // the write-virgin clone; phase 4 clears it on in_sync admission).
        assert_eq!(rec.live_lvol_uuid(), "uuid-b2");
        assert_eq!(rec.reverted_to.as_deref(), Some("epoch-vol1-3"));

        // Chase: same state, newer epoch — advances the mark only.
        assert!(record.mark_standby("uuid-b", "epoch-vol1-6", "chasing", "t2"));
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(rec.last_epoch.as_deref(), Some("epoch-vol1-6"));
        assert_eq!(rec.since.as_deref(), Some("t1")); // unchanged

        // Fully idempotent re-apply.
        assert!(!record.mark_standby("uuid-b", "epoch-vol1-6", "chasing", "t3"));
        // Unknown replica: no-op.
        assert!(!record.mark_standby("uuid-zz", "epoch-vol1-6", "?", "t4"));
    }

    #[test]
    fn live_lvol_uuid_prefers_override() {
        let mut record = three_replica_record();
        assert_eq!(record.get("uuid-a").unwrap().live_lvol_uuid(), "uuid-a");
        record.replicas[0].active_lvol_uuid = Some("uuid-a2".to_string());
        assert_eq!(record.get("uuid-a").unwrap().live_lvol_uuid(), "uuid-a2");
    }

    #[test]
    fn pin_retention_only_moves_older() {
        let mut record = three_replica_record();

        // No pin: any parseable pin takes.
        assert!(record.pin_retention("vol1", &epoch_name("vol1", 4)));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-4"));

        // Newer epoch never loosens the pin.
        assert!(!record.pin_retention("vol1", &epoch_name("vol1", 6)));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-4"));

        // Older epoch tightens it.
        assert!(record.pin_retention("vol1", &epoch_name("vol1", 2)));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-2"));

        // An unparseable existing pin (pins everything) is never replaced.
        record.retention_pin = Some("garbage".to_string());
        assert!(!record.pin_retention("vol1", &epoch_name("vol1", 1)));
        assert_eq!(record.retention_pin.as_deref(), Some("garbage"));

        // clear_pin_if only clears its own pin.
        let mut record = three_replica_record();
        record.retention_pin = Some(epoch_name("vol1", 2));
        assert!(!record.clear_pin_if(&epoch_name("vol1", 3)));
        assert!(record.clear_pin_if(&epoch_name("vol1", 2)));
        assert_eq!(record.retention_pin, None);
    }

    #[test]
    fn phase2_record_without_catchup_fields_parses() {
        // Records written by phase-1/2 builds have no active_lvol_uuid or
        // reverted_to; they must parse with both absent.
        let phase2 = r#"{"current_epoch":"epoch-v-1","epochs":[{"name":"epoch-v-1","recorded_at":"t"}],"replicas":[{"node_name":"n","node_uid":"u","lvol_uuid":"x","sync_state":"stale","last_epoch":"epoch-v-1"}]}"#;
        let record = VolumeSyncRecord::from_annotation(phase2).unwrap();
        let rec = record.get("x").unwrap();
        assert_eq!(rec.active_lvol_uuid, None);
        assert_eq!(rec.reverted_to, None);
        assert_eq!(rec.live_lvol_uuid(), "x");
    }

    #[test]
    fn mark_in_sync_clears_revert_marker_and_stamps_epoch() {
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.replicas[1].active_lvol_uuid = Some("uuid-b2".to_string());
        record.replicas[1].reverted_to = Some("epoch-vol1-3".to_string());
        record.mark_standby("uuid-b", "epoch-vol1-5", "chasing", "t1");

        // standby → in_sync: state, epoch, since, reason — and the
        // write-virgin marker MUST clear (the head takes raid writes now).
        assert!(record.mark_in_sync("uuid-b", "epoch-vol1-6", "admitted at reassembly", "t2"));
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(rec.sync_state, SyncState::InSync);
        assert_eq!(rec.last_epoch.as_deref(), Some("epoch-vol1-6"));
        assert_eq!(rec.since.as_deref(), Some("t2"));
        assert_eq!(rec.reverted_to, None);
        // The live head is still the post-revert clone.
        assert_eq!(rec.live_lvol_uuid(), "uuid-b2");

        // Idempotent re-apply (resourceVersion conflict retry).
        assert!(!record.mark_in_sync("uuid-b", "epoch-vol1-6", "again", "t3"));
        assert_eq!(record.get("uuid-b").unwrap().since.as_deref(), Some("t2"));

        // A lingering marker on an already-in_sync replica still clears.
        record.replicas[0].reverted_to = Some("epoch-vol1-2".to_string());
        assert!(record.mark_in_sync("uuid-a", "epoch-vol1-6", "x", "t4"));
        assert_eq!(record.get("uuid-a").unwrap().reverted_to, None);

        // Unknown replica: no-op.
        assert!(!record.mark_in_sync("uuid-zz", "epoch-vol1-6", "?", "t5"));
    }

    #[test]
    fn missing_skips_stale_and_standby_replicas() {
        // A standby is not in the raid by design — the monitor must not
        // demote it back to stale every tick while it chases (phase 4).
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.mark_standby("uuid-c", "epoch-vol1-4", "chasing", "t1");
        let raid = raid_json(
            "online",
            vec![json!({"name": "uuid-a", "uuid": "uuid-a", "is_configured": true})],
        );
        assert_eq!(
            replicas_missing_from_raid(&raid, "vol1", &record).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn missing_matches_admitted_replica_by_live_uuid() {
        // After a catch-up revert + phase-4 admission, the base in the raid
        // exposes the ACTIVE uuid, not the identity uuid — the in_sync
        // replica must still match (locally by name/uuid, remotely by
        // propagated uuid).
        let mut record = three_replica_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b2".to_string());
        let raid = raid_json(
            "online",
            vec![
                json!({"name": "uuid-a", "uuid": "uuid-a", "is_configured": true}),
                json!({"name": "uuid-b2", "uuid": "uuid-b2", "is_configured": true}),
                json!({"name": expected_remote_base_bdev("vol1", 2), "uuid": "uuid-c", "is_configured": true}),
            ],
        );
        assert_eq!(
            replicas_missing_from_raid(&raid, "vol1", &record).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn record_pv_name_strips_synthetic_nfs_handle() {
        assert_eq!(record_pv_name("pvc-123"), "pvc-123");
        assert_eq!(record_pv_name("nfs-server-pvc-123"), "pvc-123");
    }

    #[test]
    fn retire_epochs_respects_pin_and_current() {
        let mut record = three_replica_record();
        let all = vec!["uuid-a".to_string(), "uuid-b".to_string(), "uuid-c".to_string()];
        for seq in 1..=5 {
            record.apply_epoch_cut(&epoch_name("vol1", seq), &all, "t");
        }

        // No pin: requested epochs retire oldest-first.
        let retired = record.retire_epochs(
            "vol1",
            &[epoch_name("vol1", 1), epoch_name("vol1", 2)],
        );
        assert_eq!(retired, vec![epoch_name("vol1", 1), epoch_name("vol1", 2)]);
        assert_eq!(record.epochs.len(), 3);

        // Pin at epoch 4: epoch 3 (< pin) retires; 4 is blocked by the pin
        // (not by the current-epoch guard — current is 5) and blocking is
        // prefix semantics, so nothing after it is considered either.
        record.retention_pin = Some(epoch_name("vol1", 4));
        let retired = record.retire_epochs(
            "vol1",
            &[epoch_name("vol1", 3), epoch_name("vol1", 4)],
        );
        assert_eq!(retired, vec![epoch_name("vol1", 3)]);
        assert_eq!(record.epochs.len(), 2);

        // The current epoch never retires, pin or not.
        record.retention_pin = None;
        assert!(record.retire_epochs("vol1", &[epoch_name("vol1", 5)]).is_empty());

        // Unparseable pin pins everything.
        record.retention_pin = Some("garbage".to_string());
        assert!(record.retire_epochs("vol1", &[epoch_name("vol1", 4)]).is_empty());
    }
}
