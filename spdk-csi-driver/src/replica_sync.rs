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
    /// Hot-rejoin marker (Tier-2 7b): the E_f epoch of an in-progress hot
    /// rejoin. Written on the STALE record as intent immediately before the
    /// quiesce window opens, and carried through the flip to standby while
    /// the head is an esnap clone whose parent is the REMOTE E_f export —
    /// the local chain does not reach E_f until localization completes.
    /// While set, the replica belongs exclusively to the hot-rejoin
    /// reconciler: excluded from the chase and the bulk catch-up (their
    /// export/revert choreography would fight the live leg), and never
    /// admitted directly at reassembly (revert-first: the external parent
    /// may be gone). Cleared by localization (`mark_in_sync`), by the
    /// post-crash scrub of an uncommitted window, or by demotion back to
    /// stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_rejoin: Option<String>,
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
    /// Tombstones for deleted user snapshots whose copies could not be
    /// removed from every replica at delete time (§11, phase 5b). Reaping
    /// is tombstone-driven, never absence-driven: the catch-up reconciles
    /// these at heal time and clears each entry once every current replica
    /// confirms absence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_snapshots: Vec<String>,
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
            deleted_snapshots: Vec::new(),
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
                    hot_rejoin: None,
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
                        hot_rejoin: None,
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
        let mut changed = match self.replicas.iter_mut().find(|r| r.lvol_uuid == lvol_uuid) {
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
                // Hot-rejoin localization complete (or ordinary admission
                // superseded it): the head's chain is local from here — the
                // hot-rejoin reconciler's claim on this replica ends.
                if rec.hot_rejoin.is_some() {
                    rec.hot_rejoin = None;
                    changed = true;
                }
                changed
            }
            None => return false,
        };
        // §10-14: the retention pin is held until ADMISSION, not copy
        // completion — retiring a standby chain's base just makes the
        // node-side epoch GC grind against the chain (the campaign's
        // per-minute warnings). Release it only when no replica still
        // depends on a pinned base: a standby (resumes/admits from its
        // copied chain) or a mid-catch-up write-virgin head
        // (`reverted_to` set — its revert base must outlive a crash).
        // A merely-stale replica has no claim: its next catch-up
        // selects and pins a fresh base.
        if self.retention_pin.is_some()
            && !self.replicas.iter().any(|r| {
                r.sync_state == SyncState::Standby
                    || (r.sync_state == SyncState::Stale && r.reverted_to.is_some())
            })
        {
            self.retention_pin = None;
            changed = true;
        }
        changed
    }

    /// Hot-rejoin intent (Tier-2 7b): claim a replica for the quiesce window
    /// about to open, recording the E_f epoch name the window will cut.
    /// Written BEFORE the window so every crash point afterwards is
    /// recoverable by the marker-driven reconciler (leg live → complete the
    /// flip; leg absent → scrub the stranded artifacts).
    ///
    /// A STANDBY target (the 7b-2 trigger's class: chased to convergence,
    /// stuck waiting for a reassembly that never comes) is demoted to stale
    /// in the SAME record write — the marker state machine stays exact:
    /// stale+marker always means "window not yet flipped" (adopt/scrub),
    /// standby+marker always means "flipped, localizing" (resume/promote/
    /// demote). The demote keeps `last_epoch` and `reverted_to`: if the
    /// window unwinds, the untouched chain re-heals to standby cheaply.
    /// Refuses in_sync replicas and replicas already carrying a marker.
    /// Returns true if anything changed.
    pub fn mark_hot_rejoin_intent(
        &mut self,
        lvol_uuid: &str,
        ef_epoch: &str,
        now_rfc3339: &str,
    ) -> bool {
        match self.replicas.iter_mut().find(|r| r.lvol_uuid == lvol_uuid) {
            Some(rec) if rec.sync_state == SyncState::Standby && rec.hot_rejoin.is_none() => {
                rec.sync_state = SyncState::Stale;
                rec.since = Some(now_rfc3339.to_string());
                rec.reason =
                    Some("hot-rejoin window opening (converged standby target)".to_string());
                rec.hot_rejoin = Some(ef_epoch.to_string());
                true
            }
            Some(rec) if rec.sync_state == SyncState::Stale => {
                if rec.hot_rejoin.as_deref() != Some(ef_epoch) {
                    rec.hot_rejoin = Some(ef_epoch.to_string());
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Hot-rejoin record flip (Tier-2 7b, after `bdev_raid_add_base_bdev
    /// --skip-rebuild` succeeded): the E_f cut becomes a recorded common
    /// epoch, and the replica becomes a standby whose live head is the esnap
    /// clone — the marker (set at intent time) rides along until
    /// localization. Returns true if anything changed.
    pub fn mark_hot_rejoined(
        &mut self,
        lvol_uuid: &str,
        ef_epoch: &str,
        cut_lvol_uuids: &[String],
        head_uuid: &str,
        now_rfc3339: &str,
    ) -> bool {
        // Always a change: at minimum current_epoch advances to E_f.
        self.apply_epoch_cut(ef_epoch, cut_lvol_uuids, now_rfc3339);
        let mut changed = true;
        changed |= self.mark_standby(
            lvol_uuid,
            ef_epoch,
            "hot rejoined at E_f via skip_rebuild; localizing the esnap chain",
            now_rfc3339,
        );
        if let Some(rec) = self.replicas.iter_mut().find(|r| r.lvol_uuid == lvol_uuid) {
            if rec.active_lvol_uuid.as_deref() != Some(head_uuid) {
                rec.active_lvol_uuid = Some(head_uuid.to_string());
                changed = true;
            }
            if rec.hot_rejoin.as_deref() != Some(ef_epoch) {
                rec.hot_rejoin = Some(ef_epoch.to_string());
                changed = true;
            }
            // The esnap head is brand new — no write-virgin resume claim.
            if rec.reverted_to.is_some() {
                rec.reverted_to = None;
                changed = true;
            }
        }
        changed
    }

    /// Drop a replica's hot-rejoin marker without completing localization:
    /// an uncommitted window was scrubbed, or a committed rejoin was demoted
    /// (`demote_to_stale`) because its leg or its E_f source is gone.
    /// Returns true if anything changed.
    pub fn clear_hot_rejoin(
        &mut self,
        lvol_uuid: &str,
        reason: &str,
        demote_to_stale: bool,
        now_rfc3339: &str,
    ) -> bool {
        let mut changed = false;
        if demote_to_stale {
            changed |= self.mark_stale(lvol_uuid, reason, now_rfc3339);
        }
        if let Some(rec) = self.replicas.iter_mut().find(|r| r.lvol_uuid == lvol_uuid) {
            if rec.hot_rejoin.is_some() {
                rec.hot_rejoin = None;
                changed = true;
            }
        }
        changed
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

    /// §10-14 follow-up: advance the retention pin to the oldest epoch a
    /// dependent replica still needs. A standby resumes base-inclusively
    /// from its chase mark, so it needs nothing older than `last_epoch`;
    /// a mid-catch-up write-virgin head (`reverted_to: <epoch>`) needs
    /// its revert base; a full build (`reverted_to: "empty"`) anchors at
    /// the oldest recorded epoch and blocks all advance. Advance-only:
    /// never moves the pin backward, never touches an unparseable
    /// (pin-everything) pin, and leaves the pin alone when no dependent
    /// is visible — the pin is set *before* the revert is recorded, and
    /// that window must not lose it. Without this, a standby that cannot
    /// admit (e.g. an ineffective cutover bounce) holds retention at its
    /// original base and the epoch list grows unbounded — one blob per
    /// cut, cluster-wide after a node event (observed live 2026-06-12:
    /// 23 epochs against K=6 in 18 minutes on one volume).
    pub fn advance_retention_pin(&mut self, volume_id: &str) -> bool {
        let Some(current) = self.retention_pin.as_deref() else {
            return false;
        };
        let Some(current_seq) = epoch_seq(volume_id, current) else {
            return false;
        };
        let mut needs: Vec<u64> = Vec::new();
        for r in &self.replicas {
            match r.sync_state {
                SyncState::Standby => {
                    match r.last_epoch.as_deref().and_then(|e| epoch_seq(volume_id, e)) {
                        Some(seq) => needs.push(seq),
                        None => return false, // unparseable mark: hold everything
                    }
                }
                SyncState::Stale => {
                    if let Some(base) = r.reverted_to.as_deref() {
                        match epoch_seq(volume_id, base) {
                            Some(seq) => needs.push(seq),
                            // "empty" (full build mid-flight) or
                            // unparseable: anchored, no advance.
                            None => return false,
                        }
                    }
                }
                SyncState::InSync => {}
            }
        }
        let Some(min_need) = needs.into_iter().min() else {
            return false;
        };
        if min_need <= current_seq {
            return false;
        }
        self.retention_pin = Some(epoch_name(volume_id, min_need));
        true
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

    /// Name of the newest recorded epoch (by sequence number), if any —
    /// the target of every copy session (§11 lineage walk).
    pub fn latest_epoch(&self, volume_id: &str) -> Option<&str> {
        self.epochs
            .iter()
            .filter_map(|e| epoch_seq(volume_id, &e.name).map(|seq| (seq, e.name.as_str())))
            .max_by_key(|(seq, _)| *seq)
            .map(|(_, name)| name)
    }

    /// Name of the oldest recorded epoch, if any — the retention-pin anchor
    /// for a full build (§9-5): the lineage replay walks to the source's
    /// chain root, so nothing retained may retire mid-build.
    pub fn oldest_epoch(&self, volume_id: &str) -> Option<&str> {
        self.epochs
            .iter()
            .filter_map(|e| epoch_seq(volume_id, &e.name).map(|seq| (seq, e.name.as_str())))
            .min_by_key(|(seq, _)| *seq)
            .map(|(_, name)| name)
    }

    /// Record a tombstone for a deleted user snapshot whose copy could not
    /// be removed from every replica (§11 deletion). Only names strictly
    /// parseable as this volume's user snapshots are accepted — reaping is
    /// tombstone-driven and the tombstone set must never be able to name
    /// anything we don't own. Returns true if newly added.
    pub fn add_snapshot_tombstone(&mut self, volume_id: &str, name: &str) -> bool {
        if user_snapshot_ts(volume_id, name).is_none() {
            return false;
        }
        if self.deleted_snapshots.iter().any(|t| t == name) {
            return false;
        }
        self.deleted_snapshots.push(name.to_string());
        true
    }

    /// Drop a tombstone once every current replica has confirmed the copy
    /// is gone. Returns true if it was present.
    pub fn clear_snapshot_tombstone(&mut self, name: &str) -> bool {
        let before = self.deleted_snapshots.len();
        self.deleted_snapshots.retain(|t| t != name);
        self.deleted_snapshots.len() < before
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
    crate::identity::epoch_snapshot_name(volume_id, seq)
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

/// Parse the timestamp/id suffix out of one of this volume's user snapshot
/// names (`snap_<volume>_<suffix>`, snapshot_csi.rs). None for anything
/// else — like `epoch_seq`, callers use this as the "is it ours" filter
/// (alignment and tombstone reaping must never touch foreign names), so it
/// must stay strict.
pub fn user_snapshot_ts(volume_id: &str, name: &str) -> Option<u64> {
    let rest = name.strip_prefix("snap_")?.strip_prefix(volume_id)?;
    let digits = rest.strip_prefix('_')?;
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Split a name-shaped CSI snapshot id (`snap_<volume>_<suffix>`) into its
/// volume id and suffix. Multi-replica snapshots use the NAME as the CSI
/// snapshot id (phase 5b) — there is one copy per replica, each with its
/// own uuid, so only the common name identifies the snapshot; conveniently
/// it also embeds the source volume. Single-replica snapshot ids are SPDK
/// uuids and never parse here.
pub fn parse_user_snapshot_id(snapshot_id: &str) -> Option<(&str, u64)> {
    let rest = snapshot_id.strip_prefix("snap_")?;
    let (volume_id, suffix) = rest.rsplit_once('_')?;
    if volume_id.is_empty() {
        return None;
    }
    suffix.parse().ok().map(|ts| (volume_id, ts))
}

/// The base bdev name `connect_to_nvmeof_target` produces for a remote
/// replica: "nvme_" + per-replica NQN with ':' and '.' mangled to '_', plus
/// the "n1" namespace suffix. Local replicas need no equivalent — an lvol
/// bdev's name is its uuid.
pub fn expected_remote_base_bdev(volume_id: &str, replica_index: usize) -> String {
    let nqn = crate::identity::replica_export_nqn(volume_id, replica_index);
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
    crate::identity::storage_id_of_handle(volume_id)
}

/// `Some(parent_pv_name)` when this PV is the synthetic backing PV behind an
/// RWX volume's NFS server (volumeHandle `nfs-server-<parent>`). The backing
/// PV carries the same replica volumeAttributes as the parent, so every
/// control-plane orchestrator that iterates "flint multi-replica PVs" would
/// otherwise run a second, alias-named control stream (epochs, sync record,
/// catch-up) against the same lvols — the two streams corrupt each other's
/// snapshot lineage. Orchestrators must skip these and key everything on the
/// parent PV; the node-side data plane (stage, export, raid names) keys on
/// the synthetic volumeHandle.
pub fn nfs_backing_parent(pv: &PersistentVolume) -> Option<String> {
    let handle = pv.spec.as_ref()?.csi.as_ref()?.volume_handle.as_str();
    handle
        .strip_prefix("nfs-server-")
        .map(|parent| parent.to_string())
}

/// RWX PVs are NFS-mounted by their consumers: the workload node holds a
/// VolumeAttachment but never assembles a raid (the raid lives under the
/// NFS server pod, staged via the synthetic backing PV). Raid-presence
/// checks keyed on this PV's attachment are structurally wrong on every
/// node and must skip it.
pub fn is_rwx_pv(pv: &PersistentVolume) -> bool {
    pv.spec
        .as_ref()
        .and_then(|s| s.access_modes.as_ref())
        .map(|m| m.iter().any(|a| a == "ReadWriteMany"))
        .unwrap_or(false)
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

    fn pv_with(handle: &str, access_modes: &[&str]) -> PersistentVolume {
        use k8s_openapi::api::core::v1::{CSIPersistentVolumeSource, PersistentVolumeSpec};
        PersistentVolume {
            spec: Some(PersistentVolumeSpec {
                access_modes: Some(access_modes.iter().map(|s| s.to_string()).collect()),
                csi: Some(CSIPersistentVolumeSource {
                    driver: "flint.csi.storage.io".to_string(),
                    volume_handle: handle.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn nfs_backing_parent_only_for_nfs_server_handles() {
        let backing = pv_with("nfs-server-pvc-abc", &["ReadWriteOnce"]);
        assert_eq!(nfs_backing_parent(&backing).as_deref(), Some("pvc-abc"));
        let regular = pv_with("pvc-abc", &["ReadWriteOnce"]);
        assert_eq!(nfs_backing_parent(&regular), None);
        let rwx = pv_with("pvc-abc", &["ReadWriteMany"]);
        assert_eq!(nfs_backing_parent(&rwx), None);
    }

    #[test]
    fn rwx_pv_detected_by_access_mode() {
        assert!(is_rwx_pv(&pv_with("pvc-abc", &["ReadWriteMany"])));
        assert!(is_rwx_pv(&pv_with("pvc-abc", &["ReadWriteOnce", "ReadWriteMany"])));
        assert!(!is_rwx_pv(&pv_with("pvc-abc", &["ReadWriteOnce"])));
        // The synthetic backing PV is RWO — only the user-facing PV is RWX.
        assert!(!is_rwx_pv(&pv_with("nfs-server-pvc-abc", &["ReadWriteOnce"])));
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
    fn pin_held_until_last_dependent_replica_admitted() {
        // §10-14: the pin survives standby and releases only when no
        // replica still depends on a pinned base.
        let mut record = three_replica_record();
        record.apply_epoch_cut("epoch-vol1-3", &[], "t0");
        record.apply_epoch_cut("epoch-vol1-4", &[], "t0");
        record.pin_retention("vol1", "epoch-vol1-3");

        // b: standby. c: mid-catch-up (stale with a write-virgin head).
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.mark_standby("uuid-b", "epoch-vol1-4", "chasing", "t1");
        record.mark_stale("uuid-c", "leg failed", "t0");
        record.replicas[2].reverted_to = Some("epoch-vol1-3".to_string());

        // Admitting b releases nothing: c's revert base is still pinned.
        record.mark_in_sync("uuid-b", "epoch-vol1-4", "admitted", "t2");
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-3"));

        // Admitting c (clears its reverted_to) releases the pin.
        record.mark_in_sync("uuid-c", "epoch-vol1-4", "admitted", "t3");
        assert_eq!(record.retention_pin, None);

        // A merely-stale replica holds no claim: re-pin, mark b stale
        // (no reverted_to), admit a — pin releases despite b being stale.
        record.pin_retention("vol1", "epoch-vol1-4");
        record.mark_stale("uuid-b", "leg failed again", "t4");
        record.mark_in_sync("uuid-a", "epoch-vol1-4", "x", "t5");
        assert_eq!(record.retention_pin, None);
    }

    #[test]
    fn pin_advances_with_the_chase_mark() {
        let mut record = three_replica_record();
        record.pin_retention("vol1", "epoch-vol1-2");
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.mark_standby("uuid-b", "epoch-vol1-5", "chasing", "t1");

        // The standby resumes base-inclusively from its mark: nothing
        // older than epoch 5 is needed — retention may advance.
        assert!(record.advance_retention_pin("vol1"));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-5"));
        // Idempotent; never moves backward.
        assert!(!record.advance_retention_pin("vol1"));
        record.mark_standby("uuid-b", "epoch-vol1-7", "chasing", "t2");
        assert!(record.advance_retention_pin("vol1"));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-7"));
    }

    #[test]
    fn pin_advance_bounded_by_every_dependent() {
        let mut record = three_replica_record();
        record.pin_retention("vol1", "epoch-vol1-2");
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.mark_standby("uuid-b", "epoch-vol1-6", "chasing", "t1");
        record.mark_stale("uuid-c", "leg failed", "t0");
        record.replicas[2].reverted_to = Some("epoch-vol1-3".to_string());

        // c's revert base (3) caps the advance, not b's mark (6).
        assert!(record.advance_retention_pin("vol1"));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-3"));

        // A full build anchors everything: no advance at all.
        record.replicas[2].reverted_to = Some("empty".to_string());
        record.retention_pin = Some("epoch-vol1-2".to_string());
        assert!(!record.advance_retention_pin("vol1"));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-2"));
    }

    #[test]
    fn pin_advance_conservative_edges() {
        let mut record = three_replica_record();
        // No pin: nothing to advance.
        assert!(!record.advance_retention_pin("vol1"));
        // No dependents: the pin→revert-record window must keep the pin
        // pin_retention just set.
        record.pin_retention("vol1", "epoch-vol1-2");
        assert!(!record.advance_retention_pin("vol1"));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-2"));
        // A standby whose frozen mark is OLDER than the pin: no backward
        // move (mark 1, pin 2).
        record.mark_stale("uuid-b", "x", "t0");
        record.mark_standby("uuid-b", "epoch-vol1-1", "behind", "t1");
        assert!(!record.advance_retention_pin("vol1"));
        assert_eq!(record.retention_pin.as_deref(), Some("epoch-vol1-2"));
        // Unparseable (pin-everything) pin: untouched.
        record.retention_pin = Some("garbage".to_string());
        record.mark_standby("uuid-b", "epoch-vol1-5", "chasing", "t2");
        assert!(!record.advance_retention_pin("vol1"));
        assert_eq!(record.retention_pin.as_deref(), Some("garbage"));
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
    fn user_snapshot_naming_roundtrip_and_strictness() {
        assert_eq!(user_snapshot_ts("pvc-123", "snap_pvc-123_1699999999"), Some(1699999999));
        // Other volumes, epochs, junk: never ours.
        assert_eq!(user_snapshot_ts("pvc-123", "snap_pvc-456_1699999999"), None);
        assert_eq!(user_snapshot_ts("pvc-123", "snap_pvc-123-extra_169"), None);
        assert_eq!(user_snapshot_ts("pvc-123", "epoch-pvc-123-7"), None);
        assert_eq!(user_snapshot_ts("pvc-123", "snap_pvc-123_"), None);
        assert_eq!(user_snapshot_ts("pvc-123", "snap_pvc-123_17x"), None);

        // Id-form parse (volume unknown a priori): splits at the LAST '_'.
        assert_eq!(
            parse_user_snapshot_id("snap_pvc-123_1699999999"),
            Some(("pvc-123", 1699999999))
        );
        // A single-replica snapshot id is an SPDK uuid: never parses.
        assert_eq!(parse_user_snapshot_id("8b2c9d7e-1234-4a5b-9c8d-7e6f5a4b3c2d"), None);
        assert_eq!(parse_user_snapshot_id("snap__1699999999"), None);
    }

    #[test]
    fn latest_epoch_names_newest_by_sequence() {
        let mut record = three_replica_record();
        assert_eq!(record.latest_epoch("vol1"), None);
        assert_eq!(record.oldest_epoch("vol1"), None);
        let all = vec!["uuid-a".to_string(), "uuid-b".to_string(), "uuid-c".to_string()];
        record.apply_epoch_cut(&epoch_name("vol1", 2), &all, "t");
        record.apply_epoch_cut(&epoch_name("vol1", 10), &all, "t");
        assert_eq!(record.latest_epoch("vol1"), Some("epoch-vol1-10"));
        assert_eq!(record.oldest_epoch("vol1"), Some("epoch-vol1-2"));
    }

    #[test]
    fn snapshot_tombstones_are_strict_and_idempotent() {
        let mut record = three_replica_record();

        // Only this volume's user-snapshot names are ever accepted.
        assert!(record.add_snapshot_tombstone("vol1", "snap_vol1_99"));
        assert!(!record.add_snapshot_tombstone("vol1", "snap_vol1_99")); // dedup
        assert!(!record.add_snapshot_tombstone("vol1", "snap_other_99"));
        assert!(!record.add_snapshot_tombstone("vol1", "epoch-vol1-3"));
        assert_eq!(record.deleted_snapshots, vec!["snap_vol1_99".to_string()]);

        // Wire format roundtrips; phase-4 records without the field parse.
        let parsed = VolumeSyncRecord::from_annotation(&record.to_annotation()).unwrap();
        assert_eq!(parsed.deleted_snapshots, vec!["snap_vol1_99".to_string()]);
        let phase4 = r#"{"current_epoch":null,"replicas":[]}"#;
        assert!(VolumeSyncRecord::from_annotation(phase4).unwrap().deleted_snapshots.is_empty());

        assert!(record.clear_snapshot_tombstone("snap_vol1_99"));
        assert!(!record.clear_snapshot_tombstone("snap_vol1_99"));
        assert!(record.deleted_snapshots.is_empty());
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

    // -- Tier-2 7b hot-rejoin marker ------------------------------------------

    #[test]
    fn hot_rejoin_intent_claims_stale_and_standby_only() {
        let mut record = three_replica_record();
        // in_sync replicas cannot be claimed.
        assert!(!record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-3", "t"));
        assert!(record.get("uuid-b").unwrap().hot_rejoin.is_none());

        record.mark_stale("uuid-b", "leg failed", "2026-07-01T00:00:00Z");
        assert!(record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-3", "t"));
        assert_eq!(
            record.get("uuid-b").unwrap().hot_rejoin.as_deref(),
            Some("epoch-vol1-3")
        );
        // Idempotent re-claim of the same E_f is not a change.
        assert!(!record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-3", "t"));
    }

    #[test]
    fn hot_rejoin_intent_demotes_a_standby_in_one_write() {
        // The 7b-2 trigger's class: a converged standby is claimed by
        // demoting it to stale AND setting the marker in the same record
        // write — stale+marker must always mean "window not yet flipped".
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.mark_standby("uuid-b", "epoch-vol1-2", "chased", "t1");
        record
            .replicas
            .iter_mut()
            .find(|r| r.lvol_uuid == "uuid-b")
            .unwrap()
            .reverted_to = Some("epoch-vol1-1".to_string());

        assert!(record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-3", "t2"));
        let b = record.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Stale);
        assert_eq!(b.hot_rejoin.as_deref(), Some("epoch-vol1-3"));
        assert_eq!(b.since.as_deref(), Some("t2"));
        // The chain bookkeeping survives the demote: an unwound window
        // re-heals to standby cheaply.
        assert_eq!(b.last_epoch.as_deref(), Some("epoch-vol1-2"));
        assert_eq!(b.reverted_to.as_deref(), Some("epoch-vol1-1"));

        // A marked standby (post-flip, localizing) is never re-claimed.
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", "t0");
        record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-3", "t1");
        record.mark_hot_rejoined("uuid-b", "epoch-vol1-3", &["uuid-a".into()], "uuid-head", "t2");
        assert!(!record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-4", "t3"));
        assert_eq!(
            record.get("uuid-b").unwrap().hot_rejoin.as_deref(),
            Some("epoch-vol1-3")
        );
    }

    #[test]
    fn hot_rejoin_flip_records_epoch_standby_and_head() {
        let now = "2026-07-01T00:00:00Z";
        let mut record = three_replica_record();
        record.apply_epoch_cut(
            &epoch_name("vol1", 2),
            &["uuid-a".into(), "uuid-b".into(), "uuid-c".into()],
            now,
        );
        record.mark_stale("uuid-b", "leg failed", now);
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name("vol1", 3), "t");

        assert!(record.mark_hot_rejoined(
            "uuid-b",
            &epoch_name("vol1", 3),
            &["uuid-a".into(), "uuid-c".into()],
            "uuid-head",
            now,
        ));

        // E_f is a recorded common epoch, cut on exactly the survivors.
        assert_eq!(record.current_epoch.as_deref(), Some("epoch-vol1-3"));
        assert_eq!(record.get("uuid-a").unwrap().last_epoch.as_deref(), Some("epoch-vol1-3"));
        // The rejoined replica: standby at E_f, live head overridden to the
        // esnap clone, marker riding, no write-virgin resume claim.
        let b = record.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Standby);
        assert_eq!(b.last_epoch.as_deref(), Some("epoch-vol1-3"));
        assert_eq!(b.active_lvol_uuid.as_deref(), Some("uuid-head"));
        assert_eq!(b.hot_rejoin.as_deref(), Some("epoch-vol1-3"));
        assert!(b.reverted_to.is_none());
    }

    #[test]
    fn mark_in_sync_clears_hot_rejoin_marker() {
        let now = "2026-07-01T00:00:00Z";
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", now);
        record.mark_hot_rejoined("uuid-b", &epoch_name("vol1", 3), &["uuid-a".into()], "h", now);
        assert!(record.get("uuid-b").unwrap().hot_rejoin.is_some());

        record.mark_in_sync("uuid-b", &epoch_name("vol1", 3), "localized", now);
        let b = record.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::InSync);
        assert!(b.hot_rejoin.is_none());
    }

    #[test]
    fn clear_hot_rejoin_scrub_and_demote_paths() {
        let now = "2026-07-01T00:00:00Z";
        // Scrub of an uncommitted window: marker gone, state untouched.
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", now);
        record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-3", "t");
        assert!(record.clear_hot_rejoin("uuid-b", "window died", false, now));
        let b = record.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Stale);
        assert!(b.hot_rejoin.is_none());

        // Demotion of a committed rejoin: standby → stale + marker gone.
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", now);
        record.mark_hot_rejoined("uuid-b", "epoch-vol1-3", &["uuid-a".into()], "h", now);
        assert!(record.clear_hot_rejoin("uuid-b", "leg lost", true, now));
        let b = record.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Stale);
        assert!(b.hot_rejoin.is_none());
        assert_eq!(b.reason.as_deref(), Some("leg lost"));
    }

    #[test]
    fn hot_rejoin_marker_survives_serde_round_trip() {
        let now = "2026-07-01T00:00:00Z";
        let mut record = three_replica_record();
        record.mark_stale("uuid-b", "leg failed", now);
        record.mark_hot_rejoin_intent("uuid-b", "epoch-vol1-3", "t");
        let round =
            VolumeSyncRecord::from_annotation(&record.to_annotation()).expect("round trip");
        assert_eq!(round, record);
        // Records written by pre-7b builds (no marker field) still parse.
        let legacy = r#"{"replicas":[{"node_name":"n","node_uid":"u","lvol_uuid":"x","sync_state":"in_sync"}]}"#;
        let parsed = VolumeSyncRecord::from_annotation(legacy).expect("legacy parse");
        assert!(parsed.replicas[0].hot_rejoin.is_none());
    }
}
