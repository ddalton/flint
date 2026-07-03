// catchup.rs — replica catch-up orchestrator (warm standby) and Tier-1
// reassembly admission. Phases 3 and 4 of docs/incremental-replica-rebuild.md §9.
//
// Phase 3 detects a returned stale replica, brings it to a warm standby with
// the §5 catch-up sequence — revert to its own base epoch, hygiene,
// re-export, attach on the source node, shallow-copy the epoch chain — and
// keeps every standby chasing new epochs as the scheduler (phase 2) cuts
// them. A standby is persistent, thin, and trails the array by ≤ T_snap +
// one delta copy (§6), but it is NOT a raid member and never a read source.
//
// Phase 4 (`admit_standbys_at_stage`, called from NodeStage's raid
// assembly) admits a standby back to `in_sync` via the §6 fenced final
// delta: with every surviving in-sync replica attached — and therefore
// fenced — to the staging node and the raid not yet created, there are no
// writers anywhere, so one more common epoch cut equals every head exactly;
// one more base-inclusive chase session onto the standby equalizes it; the
// record flips to in_sync (clearing `reverted_to`); and the head is
// re-exported fenced to the consumer and joins the `bdev_raid_create` base
// list, which admits all listed bases as in-sync with no rebuild.
//
// How the §5 correctness pieces map to code:
// - **Base epoch E_b** (`select_base_epoch`): newest common epoch recorded
//   ≥ T_back before the replica was marked stale AND still present on the
//   returning replica (an epoch can be in the record yet missing on R_dst —
//   e.g. R_dst's snapshot set after an earlier catch-up has gaps). T_back's
//   default (120s) covers the NVMe-oF I/O timeout (~30s) PLUS the health
//   monitor's detection lag (60s tick): `since` is stamped at detection,
//   which trails the true leg failure. Unknown failure time → the oldest
//   present epoch (strictly more copying, never less safe).
// - **Revert** (`revert_head`): no in-place revert RPC exists, so the head
//   is deleted and re-cloned from R_dst's own E_b. The clone keeps the lvol
//   *name* (so the `lvs/name` alias stays stable — which also makes the
//   revert idempotent across crashes) but gets a fresh uuid, recorded as
//   `active_lvol_uuid`. `reverted_to` marks the head as a write-virgin clone
//   of E_b: resume skips the revert only while that exact base still stands,
//   and any future in_sync admission (phase 4) must clear the marker.
// - **E_b-inclusive lineage copy** (`lineage_chain`, §11): every copy
//   session — bulk, chase, AND final delta — copies the source's actual
//   blob lineage from the destination's base epoch *inclusive* through the
//   session's target epoch. Base-inclusivity is §5 step 4's load-bearing
//   rule, and for the chase it additionally makes switching copy sources
//   safe: replica A's and B's cuts of the same epoch are skewed, so a chain
//   that continued exclusive-of-base from a new source could permanently
//   lose a write acked between the two sources' base cuts. The chain is
//   discovered by walking parent links on the source (NOT derived from
//   recorded epoch names): a user `VolumeSnapshot` interleaved between two
//   epochs splits the newer epoch's delta — shallow copy moves only
//   clusters allocated in the source blob itself — so a name-derived chain
//   would silently lose the slice held by the user snapshot's blob (§11
//   delta-split hazard). The destination is re-snapshotted at every user
//   snapshot it replays (so healed replicas re-acquire bit-identical
//   copies; tombstoned names are not re-created) and at the target epoch.
//   Interrupted copies simply re-run: the lineage is immutable, so
//   re-copying the same chain onto the same head converges.
// - **Superblock hygiene** (`clear_head_sb`): the reverted head inherits a
//   valid raid superblock through its clone parent (reads of cluster 0 fall
//   through to E_b). If the head were exported and attached on the source
//   node carrying that sb, the §3 examine hook would spawn a phantom there —
//   or, if the source node is the live consumer, ONLINE examine would
//   re-add the stale replica to the running raid *with a blind full
//   rebuild*: exactly what this design exists to avoid. So before every
//   export we force-examine the head on R_dst's node, let the phantom
//   assemble, and delete it with clear_sb — the attached bdev then presents
//   a zeroed block 0 and examine on the source node finds nothing. The
//   src-side phantom pass after attach is belt-and-braces for v26.01 nodes
//   (no clear_sb); an ONLINE raid already claiming the destination aborts
//   the catch-up loudly rather than fighting it.
// - **Fencing**: the re-export admits exactly one host — the source node
//   (the copy writer). A previous consumer's auto-reconnecting initiator is
//   locked out; phase-4 staging re-flips the fence to the new consumer.
// - **Retention pinning**: E_b is pinned (record-level, §5) *before* the
//   revert, so the scheduler cannot retire the catch-up's foundation. The
//   pin is held through standby and released at phase-4 admission, once
//   no replica depends on a pinned base (§10-14: retiring a standby
//   chain's base is data-safe — snapshot-delete merges fold a retired
//   epoch's delta into its retained descendant — but the node-side epoch
//   GC then grinds against the chain's clone-parents, warning every
//   cycle until admission frees them anyway).
//
// Phase-4 admission ordering (each step's failure mode considered):
// - The final epoch is cut on exactly the replicas that ATTACHED at this
//   stage (a recorded in-sync replica whose attach failed is about to be
//   marked stale; cutting on it is impossible and recording an epoch it
//   lacks would break source interchangeability).
// - `in_sync` is recorded BEFORE the consumer-side attach and raid create:
//   at that moment it is simply true (the head equals the survivors' and no
//   writers exist until the raid does), and the reverse order would leave a
//   replica taking raid writes while the chase still considers it a standby
//   target. If the attach or create then fails, the health monitor re-marks
//   the replica stale once an online raid exists without it.
// - An ONLINE raid already on the staging node defers all admissions:
//   `ensure_raid1_bdev` will reuse it, and adding a base to an ONLINE raid
//   is exactly the stock blind rebuild this design avoids (§7).
// - Budget overrun (NodeStage runs under kubelet's CSI timeout) defers the
//   standby: the abandoned copy is outside the raid and idempotent, the
//   replica stays standby, the background chase converges it, and the next
//   reassembly retries. The final epoch already cut stays recorded — it is
//   a perfectly normal common epoch for the chase to consume.
//
// Interactions left deliberately open (documented):
// - A replica whose base aged out of retention (or that never shared an
//   epoch) needs the phase-5 thin-aware full build; it is marked with a
//   `ReplicaNeedsFullRebuild` event and left stale.
// - A concurrent chase from the controller can overlap the final delta
//   (both copy the same immutable epochs onto the same head — convergent,
//   merely wasteful); the in-flight set bounds it to one cycle.
//
// Hosting mirrors the epoch scheduler: a background loop in the controller
// process, default-disabled via FLINT_CATCHUP until phase 4 lands. Long
// copies run as one task per volume (an in-flight set prevents pile-up);
// one stale replica per volume is processed per cycle (§10-5's
// two-stale-replicas question stays open).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::driver::{NvmeofConnectionInfo, SpdkCsiDriver};
use crate::epoch_scheduler::{
    execute_cut, is_already_exists, is_missing, CutOutcome, CutPlan, EpochTarget, NodeRpc,
};
use crate::minimal_models::ReplicaInfo;
use crate::replica_sync::{
    self, epoch_name, epoch_seq, expected_remote_base_bdev, ReplicaSyncRecord, SyncState,
    VolumeSyncRecord,
};

pub type RpcError = Box<dyn std::error::Error + Send + Sync>;

/// SPDK access plus the convergent fenced export — everything the catch-up
/// choreography needs from a node (faked in unit tests).
#[async_trait]
pub trait CatchupRpc: Sync {
    async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError>;
    /// Export `bdev_name` on `node` under the per-replica NQN for
    /// `export_volume_id` ("{volume}_{replica_index}"), with the fence
    /// admitting exactly `consumer_node`. Converges from any partial state
    /// (nvmeof_export::ensure_export), including swapping the namespace to a
    /// new bdev after a revert.
    async fn export_replica(
        &self,
        node: &str,
        bdev_name: &str,
        export_volume_id: &str,
        consumer_node: &str,
    ) -> Result<NvmeofConnectionInfo, RpcError>;
}

#[async_trait]
impl CatchupRpc for SpdkCsiDriver {
    async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError> {
        self.call_node_agent(node, "/api/spdk/rpc", payload).await
    }

    async fn export_replica(
        &self,
        node: &str,
        bdev_name: &str,
        export_volume_id: &str,
        consumer_node: &str,
    ) -> Result<NvmeofConnectionInfo, RpcError> {
        self.setup_nvmeof_target_on_node(node, bdev_name, export_volume_id, consumer_node)
            .await
    }
}

/// Persistence boundary: PV sync-record transitions and events. Split from
/// the RPC choreography so the full catch-up flow is unit-testable; the kube
/// implementation is a thin wrapper over replica_sync::update_sync_record.
#[async_trait]
pub trait CatchupStore: Sync {
    async fn load(&self, volume_id: &str) -> Result<Option<VolumeSyncRecord>, RpcError>;
    /// Pin retention at `epoch` (§5) — must be durable BEFORE the revert.
    async fn pin_retention(&self, volume_id: &str, epoch: &str) -> Result<(), RpcError>;
    /// Record the revert: live head uuid override + write-virgin marker.
    async fn record_revert(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        base_epoch: &str,
        new_head_uuid: &str,
    ) -> Result<(), RpcError>;
    /// Transition to standby (or advance an existing standby's mark).
    /// The retention pin set before the revert is NOT cleared here: it is
    /// held until phase-4 admission (§10-14 — retiring a standby chain's
    /// base just makes the node-side epoch GC grind against the chain);
    /// `mark_in_sync` releases it once no replica depends on one.
    async fn record_standby(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        caught_up_through: &str,
    ) -> Result<(), RpcError>;
    /// Update a replica's operator-facing reason without a state change.
    async fn record_reason(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        reason: &str,
    ) -> Result<(), RpcError>;
    /// Demote a standby to stale: its chase is definitively impossible (no
    /// in-sync source lineage covers its mark), so the bulk catch-up path
    /// owns it from here. Default refuses loudly — test fakes implement
    /// what they exercise.
    async fn record_stale(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        reason: &str,
    ) -> Result<(), RpcError> {
        Err(format!(
            "record_stale({volume_id}, {replica_uuid}, {reason}): not implemented by this store"
        )
        .into())
    }
    /// Record a common epoch cut on exactly `cut_uuids` (phase-4 final
    /// delta; identity uuids). Appends the epoch, advances current_epoch,
    /// stamps last_epoch on the cut replicas.
    async fn record_epoch_cut(
        &self,
        volume_id: &str,
        epoch: &str,
        cut_uuids: &[String],
    ) -> Result<(), RpcError>;
    /// Admit a replica to in_sync at `last_epoch` (phase 4) — clears the
    /// `reverted_to` write-virgin marker (the documented phase-3 obligation).
    async fn record_in_sync(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        last_epoch: &str,
    ) -> Result<(), RpcError>;
    /// Clear a §11 snapshot tombstone once every current replica confirmed
    /// the deleted snapshot's copy is gone (phase 5b).
    async fn clear_snapshot_tombstone(&self, volume_id: &str, name: &str)
        -> Result<(), RpcError>;
    async fn emit(&self, volume_id: &str, event_type: &str, reason: &str, message: &str);

    // --- Tier-2 7b hot-rejoin transitions -----------------------------------
    // Implemented by KubeStore; test fakes implement what they exercise —
    // the defaults refuse loudly rather than silently no-op.

    /// Claim a stale replica for the quiesce window about to open (marker =
    /// intent + the E_f name). Durable BEFORE the window so every later
    /// crash point is reconciler-recoverable.
    async fn record_hot_rejoin_intent(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        ef_epoch: &str,
    ) -> Result<(), RpcError> {
        Err(format!(
            "record_hot_rejoin_intent({volume_id}, {replica_uuid}, {ef_epoch}): not implemented by this store"
        )
        .into())
    }

    /// The post-add record flip: E_f becomes a recorded common epoch and the
    /// replica a standby whose live head is the esnap clone (marker rides).
    async fn record_hot_rejoin_flip(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        ef_epoch: &str,
        cut_uuids: &[String],
        head_uuid: &str,
    ) -> Result<(), RpcError> {
        let _ = cut_uuids;
        Err(format!(
            "record_hot_rejoin_flip({volume_id}, {replica_uuid}, {ef_epoch}, .., {head_uuid}): not implemented by this store"
        )
        .into())
    }

    /// Drop the marker without localization: scrub of an uncommitted window
    /// (`demote_to_stale: false`, state untouched) or demotion of a
    /// committed rejoin whose leg/E_f source is gone (`true`).
    async fn record_hot_rejoin_cleared(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        reason: &str,
        demote_to_stale: bool,
    ) -> Result<(), RpcError> {
        let _ = demote_to_stale;
        Err(format!(
            "record_hot_rejoin_cleared({volume_id}, {replica_uuid}, {reason}): not implemented by this store"
        )
        .into())
    }
}

/// Kube-backed store used in production.
pub struct KubeStore {
    pub client: kube::Client,
}

#[async_trait]
impl CatchupStore for KubeStore {
    async fn load(&self, volume_id: &str) -> Result<Option<VolumeSyncRecord>, RpcError> {
        replica_sync::update_sync_record(&self.client, volume_id, |_| {}).await
    }

    async fn pin_retention(&self, volume_id: &str, epoch: &str) -> Result<(), RpcError> {
        let epoch = epoch.to_string();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.pin_retention(volume_id, &epoch);
        })
        .await?;
        Ok(())
    }

    async fn record_revert(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        base_epoch: &str,
        new_head_uuid: &str,
    ) -> Result<(), RpcError> {
        let (uuid, base, new_head) = (
            replica_uuid.to_string(),
            base_epoch.to_string(),
            new_head_uuid.to_string(),
        );
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            if let Some(rec) = r.replicas.iter_mut().find(|rec| rec.lvol_uuid == uuid) {
                rec.active_lvol_uuid = Some(new_head.clone());
                rec.reverted_to = Some(base.clone());
                rec.reason = Some(format!("catch-up: head reverted to {}", base));
                // A revert supersedes any interrupted hot rejoin: the esnap
                // head is gone, the chain restarts from a local base.
                rec.hot_rejoin = None;
            }
        })
        .await?;
        Ok(())
    }

    async fn record_standby(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        caught_up_through: &str,
    ) -> Result<(), RpcError> {
        let (uuid, through) = (replica_uuid.to_string(), caught_up_through.to_string());
        let vid = volume_id.to_string();
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.mark_standby(&uuid, &through, "caught up; chasing epochs", &now);
            // The chase mark moved: retention can advance with it.
            r.advance_retention_pin(&vid);
        })
        .await?;
        Ok(())
    }

    async fn record_reason(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        reason: &str,
    ) -> Result<(), RpcError> {
        let (uuid, reason) = (replica_uuid.to_string(), reason.to_string());
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            if let Some(rec) = r.replicas.iter_mut().find(|rec| rec.lvol_uuid == uuid) {
                rec.reason = Some(reason.clone());
            }
        })
        .await?;
        Ok(())
    }

    async fn record_stale(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        reason: &str,
    ) -> Result<(), RpcError> {
        let (uuid, reason) = (replica_uuid.to_string(), reason.to_string());
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.mark_stale(&uuid, &reason, &now);
        })
        .await?;
        Ok(())
    }

    async fn record_epoch_cut(
        &self,
        volume_id: &str,
        epoch: &str,
        cut_uuids: &[String],
    ) -> Result<(), RpcError> {
        let (epoch, cut) = (epoch.to_string(), cut_uuids.to_vec());
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.apply_epoch_cut(&epoch, &cut, &now);
        })
        .await?;
        Ok(())
    }

    async fn record_in_sync(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        last_epoch: &str,
    ) -> Result<(), RpcError> {
        let (uuid, through) = (replica_uuid.to_string(), last_epoch.to_string());
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.mark_in_sync(
                &uuid,
                &through,
                "admitted at reassembly after fenced final delta",
                &now,
            );
        })
        .await?;
        Ok(())
    }

    async fn clear_snapshot_tombstone(
        &self,
        volume_id: &str,
        name: &str,
    ) -> Result<(), RpcError> {
        let name = name.to_string();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.clear_snapshot_tombstone(&name);
        })
        .await?;
        Ok(())
    }

    async fn emit(&self, volume_id: &str, event_type: &str, reason: &str, message: &str) {
        replica_sync::emit_pv_event(
            &self.client,
            "catchup-orchestrator",
            volume_id,
            event_type,
            reason,
            message,
        )
        .await;
    }

    async fn record_hot_rejoin_intent(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        ef_epoch: &str,
    ) -> Result<(), RpcError> {
        let (uuid, ef) = (replica_uuid.to_string(), ef_epoch.to_string());
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.mark_hot_rejoin_intent(&uuid, &ef, &now);
        })
        .await?;
        Ok(())
    }

    async fn record_hot_rejoin_flip(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        ef_epoch: &str,
        cut_uuids: &[String],
        head_uuid: &str,
    ) -> Result<(), RpcError> {
        let (uuid, ef, cut, head) = (
            replica_uuid.to_string(),
            ef_epoch.to_string(),
            cut_uuids.to_vec(),
            head_uuid.to_string(),
        );
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.mark_hot_rejoined(&uuid, &ef, &cut, &head, &now);
        })
        .await?;
        Ok(())
    }

    async fn record_hot_rejoin_cleared(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        reason: &str,
        demote_to_stale: bool,
    ) -> Result<(), RpcError> {
        let (uuid, reason) = (replica_uuid.to_string(), reason.to_string());
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.clear_hot_rejoin(&uuid, &reason, demote_to_stale, &now);
        })
        .await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CatchupConfig {
    /// FLINT_CATCHUP=enabled — default off until phase 4 consumes standbys.
    pub enabled: bool,
    /// T_back (§5): how far before the stale-marking an epoch's cut must
    /// have completed to be a safe catch-up base. Must cover the NVMe-oF
    /// I/O timeout plus the health monitor's detection lag plus clock skew.
    /// FLINT_CATCHUP_TBACK_SECS, default 120.
    pub t_back: Duration,
    /// Shallow-copy progress poll interval. FLINT_CATCHUP_POLL_SECS, default 2.
    pub poll_interval: Duration,
    /// §9-5: automatically run the thin-aware full build (revert to EMPTY +
    /// lineage replay from the source's chain root) for a returned stale
    /// replica with no usable shared epoch history. Disabled, such replicas
    /// are only classified (ReplicaNeedsFullRebuild) and stay stale.
    /// FLINT_CATCHUP_FULL_BUILD, default enabled (the orchestrator itself
    /// is already opt-in).
    pub full_build: bool,
}

impl Default for CatchupConfig {
    fn default() -> Self {
        CatchupConfig {
            enabled: false,
            t_back: Duration::from_secs(120),
            poll_interval: Duration::from_secs(2),
            full_build: true,
        }
    }
}

impl CatchupConfig {
    pub fn from_env() -> Self {
        let d = CatchupConfig::default();
        CatchupConfig {
            enabled: std::env::var("FLINT_CATCHUP")
                .map(|v| {
                    v.eq_ignore_ascii_case("enabled")
                        || v.eq_ignore_ascii_case("true")
                        || v == "1"
                })
                .unwrap_or(d.enabled),
            t_back: std::env::var("FLINT_CATCHUP_TBACK_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.t_back),
            poll_interval: std::env::var("FLINT_CATCHUP_POLL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.poll_interval),
            full_build: std::env::var("FLINT_CATCHUP_FULL_BUILD")
                .map(|v| {
                    !(v.eq_ignore_ascii_case("disabled")
                        || v.eq_ignore_ascii_case("false")
                        || v == "0")
                })
                .unwrap_or(d.full_build),
        }
    }
}

/// The `reverted_to` resume marker for a full build (§9-5): the head is a
/// write-virgin EMPTY lvol, not a clone of any epoch. Can never collide
/// with an epoch name (`epoch-<vol>-<seq>`). Phase 4's in_sync admission
/// clears it like any other revert marker.
pub const FULL_BUILD_BASE: &str = "empty";

/// Phase-4 reassembly-admission tunables (NodeStage's final delta).
#[derive(Debug, Clone)]
pub struct StageConfig {
    /// Wall-clock budget for one standby's final delta (cut + copy + align +
    /// admission). NodeStageVolume runs under kubelet's CSI timeout (~2 min
    /// with retries); overrun stages degraded without the standby and lets
    /// the background chase finish (§6 step 2).
    /// FLINT_STAGE_DELTA_BUDGET_SECS, default 60.
    pub final_delta_budget: Duration,
    /// Pre-check: a standby more than this many epochs behind is deferred
    /// without attempting the copy — the chase normally bounds lag to ≤ 1
    /// epoch, so a large lag means the chase is not converging and the copy
    /// would blow the budget anyway. FLINT_STAGE_MAX_EPOCHS_BEHIND, default 4.
    pub max_epochs_behind: u64,
    /// Shallow-copy progress poll interval (shares the catch-up default).
    /// FLINT_CATCHUP_POLL_SECS, default 2.
    pub poll_interval: Duration,
}

impl Default for StageConfig {
    fn default() -> Self {
        StageConfig {
            final_delta_budget: Duration::from_secs(60),
            max_epochs_behind: 4,
            poll_interval: Duration::from_secs(2),
        }
    }
}

impl StageConfig {
    pub fn from_env() -> Self {
        let d = StageConfig::default();
        StageConfig {
            final_delta_budget: std::env::var("FLINT_STAGE_DELTA_BUDGET_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.final_delta_budget),
            max_epochs_behind: std::env::var("FLINT_STAGE_MAX_EPOCHS_BEHIND")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(d.max_epochs_behind),
            poll_interval: std::env::var("FLINT_CATCHUP_POLL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.poll_interval),
        }
    }
}

/// The §5 base epoch E_b for a stale replica: the newest recorded epoch that
/// (a) is still present on the returning replica (the revert needs R_dst's
/// own copy of it) and (b) was recorded at least `t_back` before the replica
/// was marked stale — `recorded_at` is an upper bound on every per-replica
/// cut time, so this comparison only ever errs toward an older, safer base.
/// An unknown or unparseable failure time degrades to the oldest present
/// epoch (maximum back-off). None = no usable shared history: phase 5's
/// thin-aware full build is required.
pub fn select_base_epoch(
    volume_id: &str,
    record: &VolumeSyncRecord,
    rec: &ReplicaSyncRecord,
    present_on_dst: &HashSet<String>,
    t_back: Duration,
) -> Option<String> {
    let deadline = rec
        .since
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                - chrono::Duration::from_std(t_back)
                    .unwrap_or_else(|_| chrono::Duration::seconds(120))
        });

    let candidates: Vec<&str> = record
        .epochs
        .iter()
        .filter(|e| epoch_seq(volume_id, &e.name).is_some())
        .filter(|e| present_on_dst.contains(&e.name))
        .filter(|e| match deadline {
            // Unparseable recorded_at cannot be proven old enough — skip it.
            Some(d) => chrono::DateTime::parse_from_rfc3339(&e.recorded_at)
                .map(|t| t.with_timezone(&chrono::Utc) <= d)
                .unwrap_or(false),
            None => true,
        })
        .map(|e| e.name.as_str())
        .collect();

    match deadline {
        Some(_) => candidates.last().map(|s| s.to_string()),
        // Unknown failure time: oldest present epoch (most conservative).
        None => candidates.first().map(|s| s.to_string()),
    }
}

/// The source's actual blob lineage from `base` (INCLUSIVE — load-bearing,
/// §5 step 4) through `target`, oldest first, discovered live by walking
/// parent links from the source head (§11). `base: None` is the §9-5 full
/// build: walk all the way to the chain root and collect everything — the
/// root element's blob holds every cluster written before its cut, so
/// replaying root → target reproduces the source's target image from
/// nothing, holes skipped (thin-aware by construction).
///
/// NOT derived from recorded epoch names: a user `VolumeSnapshot`
/// interleaved between two epochs splits the newer epoch's delta (shallow
/// copy moves only clusters allocated in the source blob itself), so a
/// name-derived chain would silently lose the slice held by the user
/// snapshot's blob. Elements newer than `target` (e.g. a user snapshot cut
/// after the newest recorded epoch) belong to a later session and are
/// skipped. A retired epoch's merge into its descendant is reflected by
/// construction.
/// The uuid addressing the LIVE head of a chase source, from the record.
/// The deterministic `{lvs}/{lvol_name}` alias is NOT authoritative for a
/// source that has itself been hot-rejoined: the esnap window promotes a
/// `_hr`-named head with a fresh uuid (recorded as `active_lvol_uuid`)
/// that keeps serving after localization. Resolving such a source by name
/// fails on every chase cycle, and `pick_source`'s non-consumer preference
/// re-picks it deterministically — the 3-replica drill wedge (2026-07-03):
/// a standby chasing forever with unbounded lag. Unreachable at
/// replicas=2 (the only possible source never failed), guaranteed
/// reachable at ≥3 replicas with ≥2 failures.
fn source_live_uuid<'a>(record: &'a VolumeSyncRecord, src: &ReplicaInfo) -> Option<&'a str> {
    record
        .replicas
        .iter()
        .find(|rec| rec.lvol_uuid == src.lvol_uuid)
        .map(|rec| rec.live_lvol_uuid())
}

/// Resolve the source's live head bdev: the record's live uuid first (an
/// lvol bdev's name IS its uuid), then the canonical alias (correct
/// whenever the source never went through a rejoin, and for records from
/// pre-Tier-2 builds that carry no `active_lvol_uuid`). No `_hr`
/// name-guessing: a stranded `_hr` lvol the record does not reference may
/// hold stale data and must never be adopted as a source.
pub(crate) async fn source_head_bdev(
    rpc: &dyn CatchupRpc,
    src: &ReplicaInfo,
    live_uuid: Option<&str>,
) -> Result<Value, RpcError> {
    if let Some(uuid) = live_uuid {
        if let Some(bdev) = get_bdev(rpc, &src.node_name, uuid).await? {
            return Ok(bdev);
        }
    }
    let head_alias = format!("{}/{}", src.lvs_name, src.lvol_name);
    get_bdev(rpc, &src.node_name, &head_alias).await?.ok_or_else(|| {
        format!(
            "source head {} (live uuid {}) not found on {}",
            head_alias,
            live_uuid.unwrap_or("<unrecorded>"),
            src.node_name
        )
        .into()
    })
}

/// Marker carried by `lineage_chain`'s DEFINITIVE non-coverage errors (the
/// walk reached the chain root without finding the session's base/target).
/// `select_covering_source` skips a candidate only on this verdict — never
/// on a transport error, which may be transient.
pub(crate) const LINEAGE_NOT_COVERED: &str = "not found in the source lineage";

pub(crate) async fn lineage_chain(
    rpc: &dyn CatchupRpc,
    src: &ReplicaInfo,
    src_live_uuid: Option<&str>,
    target: &str,
    base: Option<&str>,
) -> Result<Vec<String>, RpcError> {
    // Parent links cannot cycle (the blobstore chain is a tree); the bound
    // is purely defensive against a corrupted fake of reality.
    const MAX_DEPTH: usize = 4096;
    let head_alias = format!("{}/{}", src.lvs_name, src.lvol_name);
    let mut cursor = source_head_bdev(rpc, src, src_live_uuid).await?;

    let mut chain: Vec<String> = Vec::new();
    let mut collecting = false;
    for _ in 0..MAX_DEPTH {
        let Some(parent) = cursor
            .get("driver_specific")
            .and_then(|d| d.get("lvol"))
            .and_then(|l| l.get("base_snapshot"))
            .and_then(|b| b.as_str())
            .map(String::from)
        else {
            // Chain root reached. For a full build that IS the stopping
            // condition — everything from the root is collected; for a
            // based session it means the base (or target) never appeared.
            return match base {
                None if collecting => {
                    chain.reverse();
                    Ok(chain)
                }
                None => Err(format!(
                    "target {} {} on {}",
                    target, LINEAGE_NOT_COVERED, src.node_name
                )
                .into()),
                Some(b) => Err(format!(
                    "{} {} on {} (walked {} to the chain root)",
                    if collecting { b } else { target },
                    LINEAGE_NOT_COVERED,
                    src.node_name,
                    head_alias
                )
                .into()),
            };
        };
        if parent == target {
            collecting = true;
        }
        if collecting {
            chain.push(parent.clone());
            if base == Some(parent.as_str()) {
                chain.reverse();
                return Ok(chain);
            }
        }
        cursor = get_bdev(rpc, &src.node_name, &format!("{}/{}", src.lvs_name, parent))
            .await?
            .ok_or_else(|| {
                format!(
                    "lineage element {} missing on {} (broken chain)",
                    parent, src.node_name
                )
            })?;
    }
    Err(format!(
        "source lineage on {} exceeds {} elements — refusing to walk further",
        src.node_name, MAX_DEPTH
    )
    .into())
}

/// Pick the copy source: an in-sync replica, preferring one whose node is
/// not the volume's consumer (keeps copy read load off the data path; also
/// the smaller §3 hazard surface). Common epochs make any in-sync replica an
/// equivalent source — the base-inclusive chain re-copy makes even
/// switching sources between sessions safe.
pub fn pick_source<'a>(
    record: &VolumeSyncRecord,
    replicas: &'a [ReplicaInfo],
    consumer_node: Option<&str>,
) -> Option<&'a ReplicaInfo> {
    in_sync_sources(record, replicas, consumer_node).first().copied()
}

/// Every in-sync replica in source-preference order: non-consumer nodes
/// first (record order within each group). `pick_source` is the head of
/// this list; the coverage-aware selection walks all of it.
fn in_sync_sources<'a>(
    record: &VolumeSyncRecord,
    replicas: &'a [ReplicaInfo],
    consumer_node: Option<&str>,
) -> Vec<&'a ReplicaInfo> {
    let mut sources: Vec<&ReplicaInfo> = record
        .replicas
        .iter()
        .filter(|rec| rec.sync_state == SyncState::InSync)
        .filter_map(|rec| replicas.iter().find(|ri| ri.lvol_uuid == rec.lvol_uuid))
        .collect();
    sources.sort_by_key(|ri| consumer_node == Some(ri.node_name.as_str()));
    sources
}

/// Outcome of coverage-aware source selection.
enum CoveringSource<'a> {
    Covering(&'a ReplicaInfo),
    /// No in-sync replica exists at all.
    NoneInSync,
    /// Every reachable in-sync lineage walked to its chain root without
    /// covering the session, and none failed indeterminately — a delta from
    /// `base` is DEFINITIVELY impossible cluster-wide right now.
    NoneCovering,
}

/// Pick the copy source among the in-sync replicas whose live lineage
/// actually COVERS the session: contains `target`, and `base` when the
/// session is based. `pick_source`'s single preferred answer is not enough
/// after staggered multi-failures: the preferred (non-consumer) source may
/// itself have been rebuilt from a base NEWER than this session's — its
/// rebuilt chain roots there, so the walk can never reach an older base
/// (the second 3-replica-drill wedge, 2026-07-03) — while another in-sync
/// replica still holds the older history (the record's retention pin keeps
/// a needed base alive on every chain that has it).
///
/// Candidates keep `pick_source`'s order (non-consumer first); the first
/// covering one wins. A candidate is skipped on the definitive
/// walked-to-root verdict ([`LINEAGE_NOT_COVERED`]) or on a transport error
/// (failing over past an unreachable node), but `NoneCovering` — the
/// verdict that licenses demoting a delta to a full rebuild — is returned
/// only when every candidate was definitive: on any indeterminate error the
/// cycle fails and retries instead, because a spurious full build on a
/// transient would be far worse than a 60s delay.
async fn select_covering_source<'a>(
    rpc: &dyn CatchupRpc,
    record: &VolumeSyncRecord,
    replicas: &'a [ReplicaInfo],
    consumer_node: Option<&str>,
    target: &str,
    base: Option<&str>,
) -> Result<CoveringSource<'a>, RpcError> {
    let candidates = in_sync_sources(record, replicas, consumer_node);
    if candidates.is_empty() {
        return Ok(CoveringSource::NoneInSync);
    }
    let mut indeterminate: Option<RpcError> = None;
    for src in candidates {
        match lineage_chain(rpc, src, source_live_uuid(record, src), target, base).await {
            Ok(_) => return Ok(CoveringSource::Covering(src)),
            Err(e) if e.to_string().contains(LINEAGE_NOT_COVERED) => {
                debug!(node = %src.node_name, error = %e,
                    "[CATCHUP] Source lineage does not cover the session — trying the next in-sync source");
            }
            Err(e) => {
                debug!(node = %src.node_name, error = %e,
                    "[CATCHUP] Source probe failed — trying the next in-sync source");
                indeterminate = Some(e);
            }
        }
    }
    match indeterminate {
        Some(e) => Err(e),
        None => Ok(CoveringSource::NoneCovering),
    }
}

pub(crate) async fn get_bdev(
    rpc: &dyn CatchupRpc,
    node: &str,
    name: &str,
) -> Result<Option<Value>, RpcError> {
    let payload = json!({ "method": "bdev_get_bdevs", "params": { "name": name } });
    match rpc.spdk_rpc(node, &payload).await {
        Ok(resp) => Ok(resp
            .get("result")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .cloned()),
        Err(e) if is_missing(&e.to_string()) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) async fn get_raids(rpc: &dyn CatchupRpc, node: &str) -> Result<Vec<Value>, RpcError> {
    let payload = json!({ "method": "bdev_raid_get_bdevs", "params": { "category": "all" } });
    let resp = rpc.spdk_rpc(node, &payload).await?;
    Ok(resp
        .get("result")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Lvol names on a node's lvolstore (the catch-up's reachability probe
/// doubles as the present-epochs listing).
pub(crate) async fn list_lvol_names(
    rpc: &dyn CatchupRpc,
    node: &str,
    lvs_name: &str,
) -> Result<Vec<String>, RpcError> {
    let payload = json!({ "method": "bdev_lvol_get_lvols", "params": { "lvs_name": lvs_name } });
    let resp = rpc.spdk_rpc(node, &payload).await?;
    Ok(resp
        .get("result")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// §5 step 0: revert the head to the replica's own `base_alias` snapshot —
/// delete the head and re-create it as a clone, keeping the lvol name (the
/// stable alias makes this idempotent: a crash between delete and clone, or
/// between clone and the record write, re-runs cleanly). Returns the new
/// head's uuid.
pub(crate) async fn revert_head(
    rpc: &dyn CatchupRpc,
    node: &str,
    head_alias: &str,
    clone_name: &str,
    base_alias: &str,
) -> Result<String, RpcError> {
    let delete = json!({ "method": "bdev_lvol_delete", "params": { "name": head_alias } });
    if let Err(e) = rpc.spdk_rpc(node, &delete).await {
        if !is_missing(&e.to_string()) {
            return Err(format!("failed to delete head {} on {}: {}", head_alias, node, e).into());
        }
    }
    let clone = json!({
        "method": "bdev_lvol_clone",
        "params": { "snapshot_name": base_alias, "clone_name": clone_name }
    });
    let resp = rpc.spdk_rpc(node, &clone).await.map_err(|e| {
        format!("failed to clone {} from {} on {}: {}", clone_name, base_alias, node, e)
    })?;
    resp.get("result")
        .and_then(|r| r.as_str())
        .map(String::from)
        .ok_or_else(|| format!("bdev_lvol_clone of {} returned no uuid", base_alias).into())
}

/// §9-5 full build, step 0 (E = "empty"): discard the head entirely and
/// re-create it as a fresh EMPTY thin lvol with the same name, sized to the
/// source head. Safe for the same reason the §5 revert is: a stale
/// replica's unique data is only its unacked tail (the survivors hold every
/// acknowledged write), and a no-shared-history chain is too old to delta
/// from. The old chain's snapshots are NOT ours to reap — deleting only the
/// head leaves them as orphaned, restorable blobs (§9-5 obligation); the
/// epoch GC retires the epoch-named ones in its own time. Idempotent across
/// crashes via the stable lvol name, like `revert_head`.
pub(crate) async fn revert_head_to_empty(
    rpc: &dyn CatchupRpc,
    node: &str,
    head_alias: &str,
    lvol_name: &str,
    lvs_name: &str,
    size_mib: u64,
) -> Result<String, RpcError> {
    let delete = json!({ "method": "bdev_lvol_delete", "params": { "name": head_alias } });
    if let Err(e) = rpc.spdk_rpc(node, &delete).await {
        if !is_missing(&e.to_string()) {
            return Err(format!("failed to delete head {} on {}: {}", head_alias, node, e).into());
        }
    }
    let create = json!({
        "method": "bdev_lvol_create",
        "params": {
            "lvol_name": lvol_name,
            "lvs_name": lvs_name,
            "size_in_mib": size_mib,
            "thin_provision": true
        }
    });
    let resp = rpc.spdk_rpc(node, &create).await.map_err(|e| {
        format!("failed to create empty head {} on {}: {}", lvol_name, node, e)
    })?;
    resp.get("result")
        .and_then(|r| r.as_str())
        .map(String::from)
        .ok_or_else(|| format!("bdev_lvol_create of {} returned no uuid", lvol_name).into())
}

/// §3 discipline on the replica node before export: the head inherits a
/// valid raid superblock through its clone parent (and a resumed head may
/// carry one copied from R_src), so force examine, let the phantom raid
/// assemble, and delete it WITH clear_sb — the head then presents a zeroed
/// block 0 wherever it registers next (critically: on the source node at
/// attach, where a surviving sb would mean a phantom — or a blind rebuild if
/// the source node's consumer raid is ONLINE). An ONLINE raid under the
/// volume's name on the replica node itself is a zombie consumer or a live
/// local assembly: refuse to touch it.
async fn clear_head_sb(
    rpc: &dyn CatchupRpc,
    node: &str,
    head_alias: &str,
    raid_name: &str,
) -> Result<(), RpcError> {
    let examine = json!({ "method": "bdev_examine", "params": { "name": head_alias } });
    let _ = rpc.spdk_rpc(node, &examine).await; // no sb / already claimed: fine
    let _ = rpc
        .spdk_rpc(node, &json!({ "method": "bdev_wait_for_examine" }))
        .await;

    for raid in get_raids(rpc, node).await? {
        if raid.get("name").and_then(|n| n.as_str()) != Some(raid_name) {
            continue;
        }
        let state = raid.get("state").and_then(|s| s.as_str()).unwrap_or("unknown");
        if state == "online" {
            return Err(format!(
                "ONLINE raid {} exists on replica node {} — a zombie consumer or live local \
                 assembly still holds the replica; catch-up refused until hygiene clears it",
                raid_name, node
            )
            .into());
        }
        let delete = json!({
            "method": "bdev_raid_delete",
            "params": { "name": raid_name, "clear_sb": true }
        });
        if let Err(e) = rpc.spdk_rpc(node, &delete).await {
            // v26.01 fallback: clear_sb unsupported. Delete without — the sb
            // survives on disk and the src-side phantom pass below covers
            // the attach; reboots re-arm the hazard until the image is
            // bumped (phase 0 ships v26.05).
            let plain = json!({ "method": "bdev_raid_delete", "params": { "name": raid_name } });
            rpc.spdk_rpc(node, &plain).await.map_err(|e2| {
                format!(
                    "failed to delete phantom raid {} on {}: {} (clear_sb attempt: {})",
                    raid_name, node, e2, e
                )
            })?;
        }
    }
    Ok(())
}

async fn controller_exists(rpc: &dyn CatchupRpc, node: &str, controller: &str) -> bool {
    let payload = json!({ "method": "bdev_nvme_get_controllers", "params": { "name": controller } });
    match rpc.spdk_rpc(node, &payload).await {
        Ok(resp) => resp
            .get("result")
            .and_then(|r| r.as_array())
            .map(|c| !c.is_empty())
            .unwrap_or(false),
        Err(_) => false,
    }
}

pub(crate) async fn detach_controller(rpc: &dyn CatchupRpc, node: &str, controller: &str) {
    let payload = json!({ "method": "bdev_nvme_detach_controller", "params": { "name": controller } });
    if let Err(e) = rpc.spdk_rpc(node, &payload).await {
        debug!(node, controller, error = %e, "[CATCHUP] Stale controller detach failed (continuing)");
    }
}

/// Make the destination head reachable as a bdev on the source node,
/// converging from any state: a live attachment to the current head is
/// reused; an attachment to a previous head (pre-revert) is torn down and
/// rebuilt; otherwise sb-clear → fenced export → attach → §3 examine
/// discipline. The returned bdev name is the copy destination.
async fn ensure_dst_attached(
    rpc: &dyn CatchupRpc,
    volume_id: &str,
    replica_index: usize,
    dst: &ReplicaInfo,
    head_alias: &str,
    head_uuid: &str,
    src_node: &str,
    raid_name: &str,
) -> Result<String, RpcError> {
    let expected = expected_remote_base_bdev(volume_id, replica_index);

    if let Some(bdev) = get_bdev(rpc, src_node, &expected).await? {
        if bdev.get("uuid").and_then(|u| u.as_str()) == Some(head_uuid) {
            return Ok(expected);
        }
        // Attached to a previous head: the namespace swap below re-points
        // it, but the initiator caches the old namespace — replace the
        // controller outright.
        let controller = expected.strip_suffix("n1").unwrap_or(&expected).to_string();
        detach_controller(rpc, src_node, &controller).await;
    }

    clear_head_sb(rpc, &dst.node_name, head_alias, raid_name).await?;

    let conn = rpc
        .export_replica(
            &dst.node_name,
            head_uuid,
            &format!("{}_{}", volume_id, replica_index),
            src_node,
        )
        .await?;

    let controller = format!("nvme_{}", conn.nqn.replace(':', "_").replace('.', "_"));
    if controller_exists(rpc, src_node, &controller).await {
        // Exists but served no usable bdev (checked above): dead weight.
        detach_controller(rpc, src_node, &controller).await;
    }
    let attach = json!({
        "method": "bdev_nvme_attach_controller",
        "params": {
            "name": controller,
            "trtype": conn.transport.to_uppercase(),
            "traddr": conn.target_ip,
            "trsvcid": conn.target_port.to_string(),
            "subnqn": conn.nqn,
            "adrfam": "IPv4",
            "hostnqn": crate::nvmeof_export::flint_host_nqn(src_node)
        }
    });
    if let Err(e) = rpc.spdk_rpc(src_node, &attach).await {
        if get_bdev(rpc, src_node, &expected).await?.is_none() {
            return Err(format!("attach of {} on {} failed: {}", conn.nqn, src_node, e).into());
        }
    }

    // §3 discipline on the source node: settle examine, then deal with any
    // raid that claimed the freshly attached destination (possible only if
    // its sb survived — v26.01 fallback path).
    let _ = rpc
        .spdk_rpc(src_node, &json!({ "method": "bdev_wait_for_examine" }))
        .await;
    for raid in get_raids(rpc, src_node).await? {
        let claims_dst = raid
            .get("base_bdevs_list")
            .and_then(|b| b.as_array())
            .map(|bases| {
                bases
                    .iter()
                    .any(|b| b.get("name").and_then(|n| n.as_str()) == Some(expected.as_str()))
            })
            .unwrap_or(false);
        if !claims_dst {
            continue;
        }
        let state = raid.get("state").and_then(|s| s.as_str()).unwrap_or("unknown");
        let name = raid.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if state == "online" {
            // ONLINE examine re-added the stale replica to the live raid —
            // a blind full rebuild is running. Do not fight it.
            return Err(format!(
                "catch-up destination {} was admitted to ONLINE raid {} on {} — aborting \
                 (stock SPDK is blind-rebuilding it; see doc §3)",
                expected, name, src_node
            )
            .into());
        }
        // CONFIGURING phantom claiming the destination: release the claim.
        // No clear_sb here — the phantom's bases can include this node's
        // own live replica lvols, whose superblocks we do not own.
        let delete = json!({ "method": "bdev_raid_delete", "params": { "name": name } });
        let _ = rpc.spdk_rpc(src_node, &delete).await;
    }

    match get_bdev(rpc, src_node, &expected).await? {
        Some(b) if b.get("uuid").and_then(|u| u.as_str()) == Some(head_uuid) => Ok(expected),
        Some(_) => Err(format!(
            "bdev {} on {} is not backed by the live head {} (stale namespace?)",
            expected, src_node, head_uuid
        )
        .into()),
        None => {
            Err(format!("bdev {} did not appear after attach on {}", expected, src_node).into())
        }
    }
}

/// One epoch delta: start the shallow copy on the source node and poll to a
/// terminal state. Copies only clusters allocated in the snapshot itself
/// (§5) — exactly the epoch's delta — at identical offsets on the
/// destination. `deadline` (phase-4 final delta) bounds the wall clock: an
/// overrun abandons the poll (a started copy keeps running server-side;
/// epochs are immutable, so the chase's re-copy converges over it) and the
/// caller stages without the standby.
pub(crate) async fn shallow_copy(
    rpc: &dyn CatchupRpc,
    src_node: &str,
    src_lvol: &str,
    dst_bdev: &str,
    poll_interval: Duration,
    deadline: Option<Instant>,
) -> Result<(), RpcError> {
    if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
        return Err(format!(
            "final-delta budget exceeded before copying {} — staging degraded, chase continues",
            src_lvol
        )
        .into());
    }
    let start = json!({
        "method": "bdev_lvol_start_shallow_copy",
        "params": { "src_lvol_name": src_lvol, "dst_bdev_name": dst_bdev }
    });
    let resp = rpc.spdk_rpc(src_node, &start).await.map_err(|e| {
        format!("shallow copy {} → {} failed to start: {}", src_lvol, dst_bdev, e)
    })?;
    let op = resp
        .get("result")
        .and_then(|r| r.get("operation_id"))
        .and_then(|o| o.as_u64())
        .ok_or_else(|| format!("shallow copy of {} returned no operation_id", src_lvol))?;

    loop {
        let check = json!({
            "method": "bdev_lvol_check_shallow_copy",
            "params": { "operation_id": op }
        });
        let resp = rpc.spdk_rpc(src_node, &check).await?;
        let result = resp.get("result").cloned().unwrap_or_default();
        match result.get("state").and_then(|s| s.as_str()) {
            Some("complete") => return Ok(()),
            Some("in progress") => {
                if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                    return Err(format!(
                        "final-delta budget exceeded while copying {} (copy abandoned mid-flight; \
                         the chase re-copy converges it)",
                        src_lvol
                    )
                    .into());
                }
                if !poll_interval.is_zero() {
                    tokio::time::sleep(poll_interval).await;
                }
            }
            Some("error") => {
                // Includes destination ENOSPC (§5 step 4): abort, stay
                // stale, surface the error — never retry into a full pool.
                let detail = result.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                return Err(format!("shallow copy of {} failed: {}", src_lvol, detail).into());
            }
            other => {
                return Err(format!(
                    "shallow copy of {} returned unexpected state {:?}",
                    src_lvol, other
                )
                .into())
            }
        }
    }
}

/// §5 step 5: snapshot the head as `epoch` so the destination carries the
/// epoch lineage — the snapshot is the standby's consistent resume point
/// (an interrupted later copy leaves the head dirty; the chain re-copy from
/// this epoch converges it). "Already exists" is a resume after a crash
/// between align and record write: same head, same content — converged.
pub(crate) async fn align_head(
    rpc: &dyn CatchupRpc,
    dst_node: &str,
    head_alias: &str,
    epoch: &str,
) -> Result<(), RpcError> {
    let payload = json!({
        "method": "bdev_lvol_snapshot",
        "params": { "lvol_name": head_alias, "snapshot_name": epoch }
    });
    match rpc.spdk_rpc(dst_node, &payload).await {
        Ok(_) => Ok(()),
        Err(e) if is_already_exists(&e.to_string()) => Ok(()),
        Err(e) => Err(format!("failed to align {} on {}: {}", epoch, dst_node, e).into()),
    }
}

/// Copy the base-inclusive source lineage onto the attached destination,
/// re-snapshotting the destination at every user snapshot it replays (§11)
/// and at the target epoch. `base: None` is the §9-5 full build (replay
/// from the chain root onto an empty head). Shared by the bulk catch-up,
/// the full build, the chase, and the phase-4 final delta. Returns the
/// epoch the destination is now consistent at.
async fn copy_chain_and_align(
    rpc: &dyn CatchupRpc,
    volume_id: &str,
    record: &VolumeSyncRecord,
    src: &ReplicaInfo,
    dst: &ReplicaInfo,
    head_alias: &str,
    dst_bdev: &str,
    base: Option<&str>,
    poll_interval: Duration,
    deadline: Option<Instant>,
) -> Result<String, RpcError> {
    let target = record
        .latest_epoch(volume_id)
        .ok_or_else(|| format!("no recorded epoch to copy toward for {}", volume_id))?
        .to_string();
    copy_chain_to(
        rpc, volume_id, record, src, dst, head_alias, dst_bdev, base, &target, poll_interval,
        deadline,
    )
    .await
}

/// `copy_chain_and_align` with an explicit replay target instead of the
/// record's newest epoch — the hot-rejoin backfill (Tier-2 7b) replays the
/// landing pad exactly to `E_f`, never past it (the esnap head already
/// carries everything newer via raid writes).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn copy_chain_to(
    rpc: &dyn CatchupRpc,
    volume_id: &str,
    record: &VolumeSyncRecord,
    src: &ReplicaInfo,
    dst: &ReplicaInfo,
    head_alias: &str,
    dst_bdev: &str,
    base: Option<&str>,
    target: &str,
    poll_interval: Duration,
    deadline: Option<Instant>,
) -> Result<String, RpcError> {
    let target = target.to_string();
    let chain = lineage_chain(rpc, src, source_live_uuid(record, src), &target, base).await?;
    for element in &chain {
        let src_lvol = format!("{}/{}", src.lvs_name, element);
        debug!(volume_id, element = %element, src = %src.node_name, dst = %dst.node_name, "[CATCHUP] Copying lineage delta");
        shallow_copy(rpc, &src.node_name, &src_lvol, dst_bdev, poll_interval, deadline).await?;
        // §11: the head at this instant equals the source's image of this
        // element exactly — materialize the destination's copy of each user
        // snapshot it replays (EEXIST = its own pre-failure copy, kept).
        // A tombstoned (deleted) snapshot is never re-created; its delta
        // was still copied, which correctness requires.
        if replica_sync::user_snapshot_ts(volume_id, element).is_some()
            && !record.deleted_snapshots.iter().any(|t| t == element)
        {
            align_head(rpc, &dst.node_name, head_alias, element).await?;
        }
    }
    // Degenerate chain where the target IS the base: the destination
    // already holds a snapshot by this name (the revert source / the chase
    // mark); the head now also carries the source's copy of that epoch's
    // delta — consistent without a new snapshot (§5 correctness note, the
    // E_latest = E_b case). A full build (no base) always aligns.
    if base != Some(target.as_str()) {
        align_head(rpc, &dst.node_name, head_alias, &target).await?;
    }
    Ok(target)
}

/// Bulk catch-up of one stale replica (§5 sequence). Quietly skips while the
/// replica's node is unreachable — "returned" is detected by the lvol
/// listing succeeding.
async fn catchup_stale(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    record: &VolumeSyncRecord,
    rec: &ReplicaSyncRecord,
    replicas: &[ReplicaInfo],
    consumer_node: Option<&str>,
    raid_name: &str,
    cfg: &CatchupConfig,
) -> Result<(), RpcError> {
    let Some((index, identity)) = replicas
        .iter()
        .enumerate()
        .find(|(_, ri)| ri.lvol_uuid == rec.lvol_uuid)
    else {
        return Ok(()); // identity replaced; reconcile will drop the record
    };

    // Reachability probe + present-epochs listing in one call.
    let names = match list_lvol_names(rpc, &identity.node_name, &identity.lvs_name).await {
        Ok(names) => names,
        Err(e) => {
            debug!(
                volume_id, node = %identity.node_name, error = %e,
                "[CATCHUP] Stale replica's node not reachable — not returned yet"
            );
            return Ok(());
        }
    };
    let present: HashSet<String> = names
        .into_iter()
        .filter(|n| epoch_seq(volume_id, n).is_some())
        .collect();

    // No usable shared history → the §9-5 thin-aware full build (E =
    // "empty", lineage replay from the source's chain root). When the full
    // build is disabled, the replica is only classified and stays stale.
    let base = select_base_epoch(volume_id, record, rec, &present, cfg.t_back);
    if base.is_none() && !cfg.full_build {
        let reason =
            "full rebuild required: no shared epoch history within retention (full build disabled)"
                .to_string();
        if rec.reason.as_deref() != Some(reason.as_str()) {
            store.record_reason(volume_id, &rec.lvol_uuid, &reason).await?;
            store
                .emit(
                    volume_id,
                    "Warning",
                    "ReplicaNeedsFullRebuild",
                    &format!(
                        "Replica {} on {} has no shared epoch history within retention; \
                         the thin-aware full build is disabled (FLINT_CATCHUP_FULL_BUILD), \
                         so it stays stale",
                        rec.lvol_uuid, identity.node_name
                    ),
                )
                .await;
        }
        return Ok(());
    }

    // Coverage-aware source selection (base first: the probe needs it). A
    // shared base present on this replica is worthless when no in-sync
    // lineage reaches back to it — every survivor rebuilt from a newer base
    // (staggered multi-failure). Fall back to the full build then, exactly
    // as if no shared history existed.
    let target = record
        .latest_epoch(volume_id)
        .ok_or_else(|| format!("no recorded epoch to copy toward for {}", volume_id))?
        .to_string();
    let (src, base) = match select_covering_source(
        rpc, record, replicas, consumer_node, &target, base.as_deref(),
    )
    .await?
    {
        CoveringSource::Covering(src) => (src, base),
        CoveringSource::NoneInSync => {
            debug!(volume_id, "[CATCHUP] No in-sync source replica — cannot catch up");
            return Ok(());
        }
        CoveringSource::NoneCovering => {
            let Some(b) = base else {
                // No in-sync lineage holds even the newest recorded epoch —
                // anomalous (an in-sync replica carries it by definition);
                // retry next cycle rather than guess.
                return Err(format!(
                    "no in-sync source lineage contains {} on any node",
                    target
                )
                .into());
            };
            if !cfg.full_build {
                let reason = "full rebuild required: no in-sync source lineage covers the \
                              shared base (full build disabled)"
                    .to_string();
                if rec.reason.as_deref() != Some(reason.as_str()) {
                    store.record_reason(volume_id, &rec.lvol_uuid, &reason).await?;
                    store
                        .emit(
                            volume_id,
                            "Warning",
                            "ReplicaNeedsFullRebuild",
                            &format!(
                                "Replica {} on {} has shared base {} but no in-sync source \
                                 lineage reaches back to it; the thin-aware full build is \
                                 disabled (FLINT_CATCHUP_FULL_BUILD), so it stays stale",
                                rec.lvol_uuid, identity.node_name, b
                            ),
                        )
                        .await;
                }
                return Ok(());
            }
            store
                .emit(
                    volume_id,
                    "Warning",
                    "ReplicaCatchupBaseUncovered",
                    &format!(
                        "No in-sync source lineage covers shared base {} for the replica on {} \
                         — falling back to the thin-aware full build",
                        b, identity.node_name
                    ),
                )
                .await;
            match select_covering_source(rpc, record, replicas, consumer_node, &target, None)
                .await?
            {
                CoveringSource::Covering(src) => (src, None),
                _ => {
                    return Err(format!(
                        "no in-sync source lineage contains {} on any node",
                        target
                    )
                    .into())
                }
            }
        }
    };

    // Pin BEFORE the revert: from here the session's foundation must not be
    // retired (§5 retention pinning; survives orchestrator restarts). A
    // full build replays from the source's chain root, so it pins the
    // OLDEST retained epoch — nothing retained may retire mid-build.
    let pinned = match &base {
        Some(b) => b.clone(),
        None => record
            .oldest_epoch(volume_id)
            .ok_or("no recorded epoch to anchor the full build (cannot happen: epochs checked)")?
            .to_string(),
    };
    store.pin_retention(volume_id, &pinned).await?;

    let head_alias = format!("{}/{}", identity.lvs_name, identity.lvol_name);
    let resume_marker = base.as_deref().unwrap_or(FULL_BUILD_BASE);
    let live_uuid = if rec.reverted_to.as_deref() == Some(resume_marker) {
        // Resume: the head is still write-virgin for this exact base (it
        // has only ever received copy writes) — re-copying the chain onto
        // it converges without a re-revert.
        rec.live_lvol_uuid().to_string()
    } else {
        let new_uuid = match &base {
            Some(b) => {
                let base_alias = format!("{}/{}", identity.lvs_name, b);
                revert_head(
                    rpc, &identity.node_name, &head_alias, &identity.lvol_name, &base_alias,
                )
                .await?
            }
            None => {
                // Size the empty head to the source head exactly.
                let src_head =
                    source_head_bdev(rpc, src, source_live_uuid(record, src)).await?;
                let bytes = src_head.get("num_blocks").and_then(|v| v.as_u64()).unwrap_or(0)
                    * src_head.get("block_size").and_then(|v| v.as_u64()).unwrap_or(0);
                if bytes == 0 {
                    return Err(format!(
                        "cannot size the rebuilt head: source head {}/{} on {} reports no size",
                        src.lvs_name, src.lvol_name, src.node_name
                    )
                    .into());
                }
                let size_mib = bytes.div_ceil(1024 * 1024);
                revert_head_to_empty(
                    rpc,
                    &identity.node_name,
                    &head_alias,
                    &identity.lvol_name,
                    &identity.lvs_name,
                    size_mib,
                )
                .await?
            }
        };
        store.record_revert(volume_id, &rec.lvol_uuid, resume_marker, &new_uuid).await?;
        // The revert supersedes the record's previous live head. When that
        // head was a promoted hot-rejoin clone it holds the `_hr` alias —
        // not `identity.lvol_name`, so revert_head's delete-by-name never
        // touched it — and the orphan sweep protects `_hr` shapes while the
        // PV exists (§10-14). Left behind it holds the head name hostage:
        // every later rejoin window EEXISTs at the esnap clone (7b-3 P1).
        // The record no longer references it and a stale replica is not a
        // raid member, so reap it here; the uuid match guarantees this only
        // ever removes the exact lvol the revert just superseded.
        let superseded = rec.live_lvol_uuid();
        if superseded != new_uuid {
            let hr_alias = format!(
                "{}/{}",
                identity.lvs_name,
                crate::hot_rejoin::head_lvol_name(volume_id, index)
            );
            if let Some(holder) = get_bdev(rpc, &identity.node_name, &hr_alias).await? {
                if holder.get("uuid").and_then(|u| u.as_str()) == Some(superseded) {
                    let del = json!({ "method": "bdev_lvol_delete", "params": { "name": hr_alias } });
                    match rpc.spdk_rpc(&identity.node_name, &del).await {
                        Ok(_) => info!(volume_id, node = %identity.node_name, head = %hr_alias,
                            "[CATCHUP] Reaped the superseded hot-rejoin head (revert replaced it)"),
                        Err(e) if is_missing(&e.to_string()) => {}
                        Err(e) => warn!(volume_id, node = %identity.node_name, head = %hr_alias, error = %e,
                            "[CATCHUP] Failed to reap the superseded hot-rejoin head — the next window scrubs it"),
                    }
                }
            }
        }
        match &base {
            Some(b) => {
                store
                    .emit(
                        volume_id,
                        "Normal",
                        "ReplicaCatchupStarted",
                        &format!(
                            "Catch-up of replica on {} started: head reverted to {}, copying {} → latest from {}",
                            identity.node_name, b, b, src.node_name
                        ),
                    )
                    .await;
            }
            None => {
                store
                    .emit(
                        volume_id,
                        "Normal",
                        "ReplicaFullBuildStarted",
                        &format!(
                            "Thin-aware full build of replica on {} started: no shared epoch history \
                             within retention — head recreated empty, replaying the full source \
                             lineage from {} (all allocated clusters, holes skipped)",
                            identity.node_name, src.node_name
                        ),
                    )
                    .await;
            }
        }
        new_uuid
    };

    let dst_bdev = ensure_dst_attached(
        rpc, volume_id, index, identity, &head_alias, &live_uuid, &src.node_name, raid_name,
    )
    .await?;

    let newest = copy_chain_and_align(
        rpc, volume_id, record, src, identity, &head_alias, &dst_bdev, base.as_deref(),
        cfg.poll_interval, None,
    )
    .await?;

    store
        .record_standby(volume_id, &rec.lvol_uuid, &newest)
        .await?;
    store
        .emit(
            volume_id,
            "Normal",
            "ReplicaStandby",
            &format!(
                "Replica on {} is a warm standby: caught up through {} (chain from {}), chasing new epochs; \
                 it rejoins the raid at the next reassembly (phase 4)",
                identity.node_name, newest, resume_marker
            ),
        )
        .await;
    info!(volume_id, node = %identity.node_name, base = %resume_marker, through = %newest, "[CATCHUP] Replica caught up to warm standby");
    Ok(())
}

/// Keep a standby chasing: copy any epochs newer than its mark, starting at
/// the mark itself (base-inclusive — source-switch safety, see module note).
async fn chase_standby(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    record: &VolumeSyncRecord,
    rec: &ReplicaSyncRecord,
    replicas: &[ReplicaInfo],
    consumer_node: Option<&str>,
    raid_name: &str,
    cfg: &CatchupConfig,
) -> Result<(), RpcError> {
    let Some((index, identity)) = replicas
        .iter()
        .enumerate()
        .find(|(_, ri)| ri.lvol_uuid == rec.lvol_uuid)
    else {
        return Ok(());
    };
    let Some(base) = rec.last_epoch.as_deref() else {
        warn!(volume_id, replica = %rec.lvol_uuid, "[CATCHUP] Standby without last_epoch — skipping chase");
        return Ok(());
    };
    let Some(base_seq) = epoch_seq(volume_id, base) else {
        warn!(volume_id, replica = %rec.lvol_uuid, base, "[CATCHUP] Standby mark is not an epoch of this volume — skipping chase");
        return Ok(());
    };
    if record.latest_epoch_seq(volume_id) <= base_seq {
        return Ok(()); // current — nothing to chase
    }
    let target = record
        .latest_epoch(volume_id)
        .ok_or_else(|| format!("no recorded epoch to chase toward for {}", volume_id))?
        .to_string();
    let src = match select_covering_source(rpc, record, replicas, consumer_node, &target, Some(base))
        .await?
    {
        CoveringSource::Covering(src) => src,
        CoveringSource::NoneInSync => return Ok(()),
        CoveringSource::NoneCovering => {
            // Every in-sync lineage roots NEWER than this standby's mark —
            // each surviving source was itself rebuilt from a newer base
            // (staggered multi-failure), so a delta from `base` is
            // impossible cluster-wide and every retry would fail the same
            // way. Demote to stale: the bulk path owns it from here and
            // ends in the thin-aware full build when no shared base
            // qualifies.
            let reason = format!(
                "standby chase impossible: no in-sync source lineage covers {}",
                base
            );
            store.record_stale(volume_id, &rec.lvol_uuid, &reason).await?;
            store
                .emit(
                    volume_id,
                    "Warning",
                    "ReplicaChaseSourcesExhausted",
                    &format!(
                        "Standby on {} cannot chase from {}: no in-sync source lineage reaches \
                         back to it — demoted to stale for rebuild",
                        identity.node_name, base
                    ),
                )
                .await;
            return Ok(());
        }
    };

    let head_alias = format!("{}/{}", identity.lvs_name, identity.lvol_name);
    let live_uuid = rec.live_lvol_uuid().to_string();
    let dst_bdev = ensure_dst_attached(
        rpc, volume_id, index, identity, &head_alias, &live_uuid, &src.node_name, raid_name,
    )
    .await?;
    let newest = copy_chain_and_align(
        rpc, volume_id, record, src, identity, &head_alias, &dst_bdev, Some(base),
        cfg.poll_interval, None,
    )
    .await?;
    store.record_standby(volume_id, &rec.lvol_uuid, &newest).await?;
    info!(volume_id, node = %identity.node_name, through = %newest, "[CATCHUP] Standby chased to latest epoch");
    Ok(())
}

/// Adapts the catch-up transport to the epoch scheduler's `NodeRpc` so the
/// final delta reuses `execute_cut` (all-or-abort + rollback) verbatim.
pub(crate) struct NodeRpcAdapter<'a>(pub(crate) &'a dyn CatchupRpc);

#[async_trait]
impl NodeRpc for NodeRpcAdapter<'_> {
    async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError> {
        self.0.spdk_rpc(node, payload).await
    }
}

/// A standby admitted to in_sync by the phase-4 final delta, ready for the
/// `bdev_raid_create` base list.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedStandby {
    /// Identity uuid (volumeAttributes / record key).
    pub lvol_uuid: String,
    pub node_name: String,
    /// Base bdev name on the staging node (local live uuid, or the attached
    /// remote bdev).
    pub bdev: String,
    pub final_epoch: String,
}

/// Phase 4 (§6 "rejoin at the next assembly"): run the fenced final delta
/// for every standby and admit the equalized heads. Called from NodeStage's
/// raid assembly AFTER the surviving in-sync replicas attached (their
/// exports' fences now admit exactly `consumer_node`, so no writer exists
/// anywhere) and BEFORE `bdev_raid_create`. `volume_id` is the record/epoch
/// volume id (`replica_sync::record_pv_name` of the staged handle);
/// `raid_name` is the raid bdev the stage will create (named after the raw
/// handle); `attached_in_sync` are the identity uuids whose attach succeeded.
///
/// Never fails the stage: every deferral is contained per-standby (Warning
/// event + the replica simply stays a chasing standby) and an empty result
/// means "stage exactly as phase 3 did".
pub async fn admit_standbys_at_stage(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    raid_name: &str,
    replicas: &[ReplicaInfo],
    consumer_node: &str,
    attached_in_sync: &[String],
    cfg: &StageConfig,
) -> Vec<AdmittedStandby> {
    let mut admitted: Vec<AdmittedStandby> = Vec::new();

    let standby_uuids: Vec<String> = match store.load(volume_id).await {
        Ok(Some(record)) if !record.epochs.is_empty() => record
            .replicas
            .iter()
            .filter(|r| r.sync_state == SyncState::Standby)
            .map(|r| r.lvol_uuid.clone())
            .collect(),
        Ok(_) => return admitted, // single replica / no epoch machinery
        Err(e) => {
            warn!(volume_id, error = %e, "[ADMIT] Cannot load sync record — staging without admission");
            return admitted;
        }
    };
    if standby_uuids.is_empty() {
        return admitted;
    }
    if attached_in_sync.is_empty() {
        debug!(volume_id, "[ADMIT] No attached in-sync replica to source the final delta from");
        return admitted;
    }

    // Re-stage guard: an ONLINE raid here means ensure_raid1_bdev will reuse
    // it, and a base cannot join an ONLINE raid without the stock blind
    // rebuild (§7) — defer to the next real reassembly.
    match get_raids(rpc, consumer_node).await {
        Ok(raids) => {
            let online = raids.iter().any(|r| {
                r.get("name").and_then(|n| n.as_str()) == Some(raid_name)
                    && r.get("state").and_then(|s| s.as_str()) == Some("online")
            });
            if online {
                info!(
                    volume_id, raid_name,
                    "[ADMIT] Raid already ONLINE on {} (restage reuses it) — standby admission deferred",
                    consumer_node
                );
                return admitted;
            }
        }
        Err(e) => {
            warn!(volume_id, error = %e, "[ADMIT] Cannot inspect raids on {} — admission deferred", consumer_node);
            return admitted;
        }
    }

    // Replicas the final epoch may be cut on / copied from: the ones that
    // actually attached this stage, plus standbys admitted earlier in this
    // very loop (their heads are equalized and they hold the chain).
    let mut attached: Vec<String> = attached_in_sync.to_vec();

    for uuid in standby_uuids {
        match admit_one_standby(
            rpc, store, volume_id, raid_name, replicas, consumer_node, &attached, &uuid, cfg,
        )
        .await
        {
            Ok(one) => {
                store
                    .emit(
                        volume_id,
                        "Normal",
                        "ReplicaAdmitted",
                        &format!(
                            "Replica on {} admitted to the raid on {}: fenced final delta equalized \
                             its head at {} — in_sync, no rebuild",
                            one.node_name, consumer_node, one.final_epoch
                        ),
                    )
                    .await;
                info!(
                    volume_id, node = %one.node_name, epoch = %one.final_epoch,
                    "[ADMIT] Standby admitted in_sync at reassembly"
                );
                attached.push(one.lvol_uuid.clone());
                admitted.push(one);
            }
            Err(e) => {
                warn!(volume_id, replica = %uuid, error = %e, "[ADMIT] Standby admission deferred — staging degraded, chase continues");
                store
                    .emit(
                        volume_id,
                        "Warning",
                        "StandbyAdmissionDeferred",
                        &format!(
                            "Standby replica {} stays out of this assembly: {} \
                             (it keeps chasing and rejoins at the next reassembly)",
                            uuid, e
                        ),
                    )
                    .await;
            }
        }
    }
    admitted
}

/// One standby's §6 admission: cut the final common epoch on the attached
/// survivors (no writers exist — the cut equals every head), run one
/// base-inclusive chase session onto the standby under the stage budget,
/// record in_sync (clearing `reverted_to`), and re-export the head fenced to
/// the consumer for the base list.
async fn admit_one_standby(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    raid_name: &str,
    replicas: &[ReplicaInfo],
    consumer_node: &str,
    attached: &[String],
    replica_uuid: &str,
    cfg: &StageConfig,
) -> Result<AdmittedStandby, RpcError> {
    let deadline = Instant::now() + cfg.final_delta_budget;

    // Fresh record per standby: earlier admissions in this stage appended an
    // epoch and flipped states.
    let record = store
        .load(volume_id)
        .await?
        .ok_or("sync record disappeared during staging")?;
    let rec = record
        .get(replica_uuid)
        .cloned()
        .ok_or("replica missing from sync record")?;
    if rec.sync_state != SyncState::Standby {
        return Err(format!("replica is {} — not a standby", rec.sync_state.as_str()).into());
    }
    let Some((index, identity)) = replicas
        .iter()
        .enumerate()
        .find(|(_, ri)| ri.lvol_uuid == replica_uuid)
    else {
        return Err("replica identity not in volumeAttributes".into());
    };
    let base = rec
        .last_epoch
        .clone()
        .ok_or("standby has no last_epoch mark")?;
    let Some(base_seq) = epoch_seq(volume_id, &base) else {
        return Err(format!("standby mark {} is not an epoch of this volume", base).into());
    };
    let behind = record.latest_epoch_seq(volume_id).saturating_sub(base_seq);
    if behind > cfg.max_epochs_behind {
        return Err(format!(
            "standby is {} epochs behind (limit {}) — the chase has not converged",
            behind, cfg.max_epochs_behind
        )
        .into());
    }

    // Final common epoch on exactly the attached in-sync replicas. With all
    // of them fenced to this node and the raid not yet created, no writer
    // exists: the cut equals each head, and skew is zero.
    let final_epoch = epoch_name(volume_id, record.latest_epoch_seq(volume_id) + 1);
    let targets: Vec<EpochTarget> = record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::InSync)
        .filter(|r| attached.iter().any(|a| *a == r.lvol_uuid))
        .filter_map(|r| {
            replicas
                .iter()
                .find(|ri| ri.lvol_uuid == r.lvol_uuid)
                .map(|ri| EpochTarget {
                    node_name: ri.node_name.clone(),
                    lvol_uuid: ri.lvol_uuid.clone(),
                    snapshot_source: r.live_lvol_uuid().to_string(),
                    lvs_name: ri.lvs_name.clone(),
                })
        })
        .collect();
    if targets.is_empty() {
        return Err("no attached in-sync replica to cut the final epoch on".into());
    }
    let plan = CutPlan { epoch: final_epoch.clone(), targets };
    let cut_uuids = match execute_cut(&NodeRpcAdapter(rpc), &plan).await {
        CutOutcome::Recorded { cut_uuids } => cut_uuids,
        CutOutcome::Aborted { failures } => {
            return Err(format!(
                "final epoch cut failed on {} replica(s): {}",
                failures.len(),
                failures
                    .iter()
                    .map(|(node, e)| format!("{}: {}", node, e))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
            .into());
        }
    };
    store.record_epoch_cut(volume_id, &final_epoch, &cut_uuids).await?;

    // Local view with the new epoch, for chain selection and source pick.
    let mut record = record;
    record.apply_epoch_cut(&final_epoch, &cut_uuids, &replica_sync::now_rfc3339());

    // One more chase session: base-inclusive from the standby's mark through
    // the final epoch, from an attached source (prefer off the consumer).
    let attached_replicas: Vec<ReplicaInfo> = replicas
        .iter()
        .filter(|ri| attached.iter().any(|a| *a == ri.lvol_uuid))
        .cloned()
        .collect();
    let src = pick_source(&record, &attached_replicas, Some(consumer_node))
        .ok_or("no in-sync source among the attached replicas")?;
    let head_alias = format!("{}/{}", identity.lvs_name, identity.lvol_name);
    let live_uuid = rec.live_lvol_uuid().to_string();
    let dst_bdev = ensure_dst_attached(
        rpc, volume_id, index, identity, &head_alias, &live_uuid, &src.node_name, raid_name,
    )
    .await?;
    let newest = copy_chain_and_align(
        rpc, volume_id, &record, src, identity, &head_alias, &dst_bdev, Some(&base),
        cfg.poll_interval, Some(deadline),
    )
    .await?;

    // Admission order is load-bearing (module note): record in_sync —
    // clearing the write-virgin marker — BEFORE the head joins the raid.
    store.record_in_sync(volume_id, replica_uuid, &newest).await?;

    // The head joins the base list: locally as the live lvol; remotely via
    // the §3-disciplined fenced re-export, which flips the subsystem's host
    // list from the copy source to the consumer.
    let bdev = if identity.node_name == consumer_node {
        clear_head_sb(rpc, consumer_node, &head_alias, raid_name).await?;
        live_uuid.clone()
    } else {
        let bdev = ensure_dst_attached(
            rpc, volume_id, index, identity, &head_alias, &live_uuid, consumer_node, raid_name,
        )
        .await?;
        // The copy controller on the source node is fenced out now — detach
        // it best-effort so it doesn't linger as dead weight.
        if src.node_name != consumer_node {
            let expected = expected_remote_base_bdev(volume_id, index);
            let controller = expected.strip_suffix("n1").unwrap_or(&expected).to_string();
            detach_controller(rpc, &src.node_name, &controller).await;
        }
        bdev
    };

    Ok(AdmittedStandby {
        lvol_uuid: replica_uuid.to_string(),
        node_name: identity.node_name.clone(),
        bdev,
        final_epoch: newest,
    })
}

/// §11 tombstone reconcile: delete tombstoned user-snapshot copies from
/// every replica, clearing each tombstone once ALL current replicas confirm
/// absence (delete succeeded or copy already missing). An unreachable node
/// or a clone-pinned copy (`-EPERM`) keeps the tombstone for a later cycle.
/// Reaping is driven exclusively by these positively-recorded tombstones —
/// never by a name's absence from any listing.
async fn reconcile_snapshot_tombstones(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    record: &VolumeSyncRecord,
    replicas: &[ReplicaInfo],
) {
    for name in &record.deleted_snapshots {
        let mut all_confirmed_absent = true;
        for replica in replicas {
            let alias = format!("{}/{}", replica.lvs_name, name);
            let payload = json!({ "method": "bdev_lvol_delete", "params": { "name": alias } });
            match rpc.spdk_rpc(&replica.node_name, &payload).await {
                Ok(_) => {}
                Err(e) if is_missing(&e.to_string()) => {}
                Err(e) => {
                    debug!(
                        volume_id, node = %replica.node_name, snapshot = %name, error = %e,
                        "[CATCHUP] Tombstoned snapshot copy not yet deletable — keeping tombstone"
                    );
                    all_confirmed_absent = false;
                }
            }
        }
        if all_confirmed_absent {
            match store.clear_snapshot_tombstone(volume_id, name).await {
                Ok(()) => {
                    info!(volume_id, snapshot = %name, "[CATCHUP] Deleted snapshot reconciled off every replica");
                }
                Err(e) => {
                    warn!(volume_id, snapshot = %name, error = %e, "[CATCHUP] Failed to clear snapshot tombstone (retried next cycle)");
                }
            }
        }
    }
}

/// One orchestrator pass over a single volume: reconcile snapshot
/// tombstones, chase every standby, then run at most one stale replica's
/// bulk catch-up. Per-replica failures are contained (warned + evented) so
/// one replica cannot starve the others.
pub async fn run_catchup_for_volume(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    replicas: &[ReplicaInfo],
    consumer_node: Option<&str>,
    cfg: &CatchupConfig,
) -> Result<(), RpcError> {
    let Some(record) = store.load(volume_id).await? else {
        return Ok(()); // single-replica volume
    };
    if !record.deleted_snapshots.is_empty() {
        reconcile_snapshot_tombstones(rpc, store, volume_id, &record, replicas).await;
    }
    if record.epochs.is_empty() {
        // No common epochs yet (scheduler disabled or volume too new):
        // nothing to catch up from, and nothing to classify as full-build —
        // an empty record must never condemn a healable replica.
        return Ok(());
    }
    let raid_name = format!("raid_{}", volume_id);

    // Tier-2 7b: replicas claimed by a hot rejoin (marker set) belong to its
    // reconciler — resume localization, adopt a committed-but-unflipped
    // window, or scrub a dead one. They are excluded from the chase and the
    // bulk catch-up below: that choreography (export swap, head revert)
    // would fight the live raid leg the marker says they may hold.
    let record = if record.replicas.iter().any(|r| r.hot_rejoin.is_some()) {
        crate::hot_rejoin::reconcile_marked(
            rpc, store, volume_id, &record, replicas, consumer_node, cfg,
        )
        .await;
        // The reconcile may have flipped states — re-read before dispatch.
        match store.load(volume_id).await? {
            Some(r) => r,
            None => return Ok(()),
        }
    } else {
        record
    };

    for rec in record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::Standby && r.hot_rejoin.is_none())
    {
        if let Err(e) = chase_standby(
            rpc, store, volume_id, &record, rec, replicas, consumer_node, &raid_name, cfg,
        )
        .await
        {
            warn!(volume_id, replica = %rec.lvol_uuid, error = %e, "[CATCHUP] Chase failed — retrying next cycle");
            store
                .emit(
                    volume_id,
                    "Warning",
                    "ReplicaCatchupFailed",
                    &format!("Standby chase for replica on {} failed: {}", rec.node_name, e),
                )
                .await;
        }
    }

    if let Some(rec) = record
        .replicas
        .iter()
        .find(|r| r.sync_state == SyncState::Stale && r.hot_rejoin.is_none())
    {
        if let Err(e) = catchup_stale(
            rpc, store, volume_id, &record, rec, replicas, consumer_node, &raid_name, cfg,
        )
        .await
        {
            warn!(volume_id, replica = %rec.lvol_uuid, error = %e, "[CATCHUP] Catch-up failed — retrying next cycle");
            store
                .emit(
                    volume_id,
                    "Warning",
                    "ReplicaCatchupFailed",
                    &format!("Catch-up of replica on {} failed: {}", rec.node_name, e),
                )
                .await;
        }
    }
    Ok(())
}

/// Background orchestrator loop (controller role). Each volume runs as its
/// own task — a multi-hour bulk copy on one volume must not stall every
/// other volume's chase — guarded by the shared per-volume claim (Tier-2
/// design item 4): a slow volume is simply skipped by later ticks until its
/// task finishes, and cutover / hot-rejoin cannot land on it mid-copy.
pub async fn run_catchup_orchestrator(driver: Arc<SpdkCsiDriver>, cfg: CatchupConfig) {
    info!(
        t_back_secs = cfg.t_back.as_secs(),
        "[CATCHUP] Replica catch-up orchestrator started"
    );
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Err(e) = orchestrator_tick(&driver, &cfg).await {
            warn!(error = %e, "[CATCHUP] Orchestrator tick failed (non-fatal)");
        }
    }
}

async fn orchestrator_tick(
    driver: &Arc<SpdkCsiDriver>,
    cfg: &CatchupConfig,
) -> Result<(), RpcError> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    use k8s_openapi::api::storage::v1::VolumeAttachment;
    use kube::api::ListParams;
    use kube::Api;

    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let vas: Api<VolumeAttachment> = Api::all(driver.kube_client.clone());

    let pv_list = pvs.list(&ListParams::default()).await?;
    // volume → consumer node, for fencing-aware source selection.
    let consumers: HashMap<String, String> = vas
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|va| va.status.as_ref().map(|s| s.attached).unwrap_or(false))
        .filter_map(|va| {
            va.spec
                .source
                .persistent_volume_name
                .map(|pv| (pv, va.spec.node_name))
        })
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
        // Skip the synthetic NFS backing PV — its replica attributes mirror
        // the parent RWX PV's, and a second catch-up stream driven from a
        // second sync record corrupts the shared snapshot lineage.
        if replica_sync::nfs_backing_parent(&pv).is_some() {
            continue;
        }
        let replicas = match replica_sync::replicas_from_pv(&pv) {
            Ok(Some(r)) => r,
            Ok(None) => continue, // single replica
            Err(e) => {
                debug!(volume_id, error = %e, "[CATCHUP] Skipping PV with unreadable replica info");
                continue;
            }
        };

        let Some(claim) = crate::volume_claims::global()
            .try_claim(&volume_id, crate::volume_claims::OP_CATCHUP)
        else {
            continue; // a previous cycle, a cutover, or a hot rejoin holds it
        };
        let driver = driver.clone();
        let cfg = cfg.clone();
        let consumer = consumers.get(&volume_id).cloned();
        tokio::spawn(async move {
            let _claim = claim; // released when this task ends, however it ends
            let store = KubeStore { client: driver.kube_client.clone() };
            if let Err(e) = run_catchup_for_volume(
                driver.as_ref(),
                &store,
                &volume_id,
                &replicas,
                consumer.as_deref(),
                &cfg,
            )
            .await
            {
                warn!(volume_id, error = %e, "[CATCHUP] Volume catch-up cycle failed (non-fatal)");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn cfg() -> CatchupConfig {
        CatchupConfig {
            enabled: true,
            t_back: Duration::from_secs(120),
            poll_interval: Duration::ZERO,
            full_build: true,
        }
    }

    fn epoch(volume: &str, seq: u64) -> String {
        replica_sync::epoch_name(volume, seq)
    }

    /// Record where epochs 3 (10:00) and 4 (10:05) were cut on all replicas,
    /// replica b went stale at 10:20, and epoch 5 (10:25) was cut on the
    /// survivors. With t_back=120s the deadline is 10:18: epochs 3 and 4
    /// qualify as bases, 5 does not.
    fn stale_b_record() -> VolumeSyncRecord {
        let mut record = VolumeSyncRecord::initial(&replicas3());
        let all: Vec<String> =
            vec!["uuid-a".to_string(), "uuid-b".to_string(), "uuid-c".to_string()];
        record.apply_epoch_cut(&epoch("vol1", 3), &all, "2026-06-11T10:00:00Z");
        record.apply_epoch_cut(&epoch("vol1", 4), &all, "2026-06-11T10:05:00Z");
        record.mark_stale("uuid-b", "leg failed", "2026-06-11T10:20:00Z");
        let survivors: Vec<String> = vec!["uuid-a".to_string(), "uuid-c".to_string()];
        record.apply_epoch_cut(&epoch("vol1", 5), &survivors, "2026-06-11T10:25:00Z");
        record
    }

    // ---- fakes ----------------------------------------------------------

    struct FakeRpc {
        calls: Mutex<Vec<(String, Value)>>,
        /// (node, method) → error message
        fail: HashMap<(String, String), String>,
        /// fail bdev_raid_delete only when it carries clear_sb (v26.01 sim)
        fail_clear_sb: bool,
        /// node → (lvol name, uuid)
        lvols: HashMap<String, Vec<(String, String)>>,
        /// node → raid bdev records
        raids: Mutex<HashMap<String, Vec<Value>>>,
        /// (node, bdev name) → bdev record
        bdevs: Mutex<HashMap<(String, String), Value>>,
        controllers: Mutex<HashSet<(String, String)>>,
        /// uuid bdev_lvol_clone returns
        clone_uuid: String,
        /// uuid the bdev registered by attach reports
        attach_uuid: String,
        /// states fed to checks of the next started copy (then "complete")
        pending_copy_states: Mutex<Vec<String>>,
        copy_states: Mutex<HashMap<u64, Vec<String>>>,
        next_op: Mutex<u64>,
        /// (node, bdev, export_volume_id, consumer)
        exports: Mutex<Vec<(String, String, String, String)>>,
    }

    impl FakeRpc {
        fn new(head_uuid: &str) -> Self {
            FakeRpc {
                calls: Mutex::new(Vec::new()),
                fail: HashMap::new(),
                fail_clear_sb: false,
                lvols: HashMap::new(),
                raids: Mutex::new(HashMap::new()),
                bdevs: Mutex::new(HashMap::new()),
                controllers: Mutex::new(HashSet::new()),
                clone_uuid: head_uuid.to_string(),
                attach_uuid: head_uuid.to_string(),
                pending_copy_states: Mutex::new(Vec::new()),
                copy_states: Mutex::new(HashMap::new()),
                next_op: Mutex::new(0),
                exports: Mutex::new(Vec::new()),
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
    impl CatchupRpc for FakeRpc {
        async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError> {
            let method = payload["method"].as_str().unwrap_or("").to_string();
            self.calls
                .lock()
                .unwrap()
                .push((node.to_string(), payload.clone()));
            if let Some(err) = self.fail.get(&(node.to_string(), method.clone())) {
                return Err(err.clone().into());
            }
            match method.as_str() {
                "bdev_lvol_get_lvols" => {
                    let arr: Vec<Value> = self
                        .lvols
                        .get(node)
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|(n, u)| json!({ "name": n, "uuid": u, "alias": format!("lvs0/{}", n) }))
                        .collect();
                    Ok(json!({ "result": arr }))
                }
                "bdev_get_bdevs" => {
                    let name = payload["params"]["name"].as_str().unwrap_or("");
                    match self.bdevs.lock().unwrap().get(&(node.to_string(), name.to_string())) {
                        Some(b) => Ok(json!({ "result": [b] })),
                        None => Err("No such device".into()),
                    }
                }
                "bdev_lvol_clone" => Ok(json!({ "result": self.clone_uuid })),
                "bdev_lvol_create" => Ok(json!({ "result": self.clone_uuid })),
                "bdev_raid_get_bdevs" => Ok(json!({
                    "result": self.raids.lock().unwrap().get(node).cloned().unwrap_or_default()
                })),
                "bdev_raid_delete" => {
                    if self.fail_clear_sb && payload["params"]["clear_sb"].as_bool() == Some(true) {
                        return Err("Invalid parameters".into());
                    }
                    let name = payload["params"]["name"].as_str().unwrap_or("");
                    if let Some(raids) = self.raids.lock().unwrap().get_mut(node) {
                        raids.retain(|r| r["name"].as_str() != Some(name));
                    }
                    Ok(json!({ "result": true }))
                }
                "bdev_nvme_get_controllers" => {
                    let name = payload["params"]["name"].as_str().unwrap_or("");
                    if self.controllers.lock().unwrap().contains(&(node.to_string(), name.to_string())) {
                        Ok(json!({ "result": [{ "name": name }] }))
                    } else {
                        Err("No such device".into())
                    }
                }
                "bdev_nvme_detach_controller" => {
                    let name = payload["params"]["name"].as_str().unwrap_or("").to_string();
                    self.controllers.lock().unwrap().remove(&(node.to_string(), name.clone()));
                    self.bdevs.lock().unwrap().remove(&(node.to_string(), format!("{}n1", name)));
                    Ok(json!({ "result": true }))
                }
                "bdev_nvme_attach_controller" => {
                    let name = payload["params"]["name"].as_str().unwrap_or("").to_string();
                    let bdev = format!("{}n1", name);
                    self.controllers.lock().unwrap().insert((node.to_string(), name));
                    self.bdevs.lock().unwrap().insert(
                        (node.to_string(), bdev.clone()),
                        json!({ "name": bdev, "uuid": self.attach_uuid }),
                    );
                    Ok(json!({ "result": [bdev] }))
                }
                "bdev_lvol_start_shallow_copy" => {
                    let mut op = self.next_op.lock().unwrap();
                    *op += 1;
                    let states: Vec<String> =
                        self.pending_copy_states.lock().unwrap().drain(..).collect();
                    self.copy_states.lock().unwrap().insert(*op, states);
                    Ok(json!({ "result": { "operation_id": *op } }))
                }
                "bdev_lvol_check_shallow_copy" => {
                    let op = payload["params"]["operation_id"].as_u64().unwrap_or(0);
                    let mut states = self.copy_states.lock().unwrap();
                    let state = states
                        .get_mut(&op)
                        .and_then(|s| if s.is_empty() { None } else { Some(s.remove(0)) })
                        .unwrap_or_else(|| "complete".to_string());
                    if let Some(msg) = state.strip_prefix("error:") {
                        Ok(json!({ "result": { "state": "error", "error": msg, "copied_clusters": 1, "total_clusters": 4 } }))
                    } else {
                        Ok(json!({ "result": { "state": state, "copied_clusters": 2, "total_clusters": 4 } }))
                    }
                }
                _ => Ok(json!({ "result": "ok" })),
            }
        }

        async fn export_replica(
            &self,
            node: &str,
            bdev_name: &str,
            export_volume_id: &str,
            consumer_node: &str,
        ) -> Result<NvmeofConnectionInfo, RpcError> {
            self.exports.lock().unwrap().push((
                node.to_string(),
                bdev_name.to_string(),
                export_volume_id.to_string(),
                consumer_node.to_string(),
            ));
            Ok(NvmeofConnectionInfo {
                nqn: format!("nqn.2024-11.com.flint:volume:{}", export_volume_id),
                target_ip: "10.0.0.99".to_string(),
                target_port: 4420,
                transport: "tcp".to_string(),
            })
        }
    }

    struct FakeStore {
        record: Mutex<VolumeSyncRecord>,
        ops: Mutex<Vec<String>>,
        events: Mutex<Vec<(String, String)>>,
    }

    impl FakeStore {
        fn new(record: VolumeSyncRecord) -> Self {
            FakeStore {
                record: Mutex::new(record),
                ops: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CatchupStore for FakeStore {
        async fn load(&self, _volume_id: &str) -> Result<Option<VolumeSyncRecord>, RpcError> {
            Ok(Some(self.record.lock().unwrap().clone()))
        }

        async fn pin_retention(&self, volume_id: &str, epoch: &str) -> Result<(), RpcError> {
            self.record.lock().unwrap().pin_retention(volume_id, epoch);
            self.ops.lock().unwrap().push(format!("pin:{}", epoch));
            Ok(())
        }

        async fn record_revert(
            &self,
            _volume_id: &str,
            replica_uuid: &str,
            base_epoch: &str,
            new_head_uuid: &str,
        ) -> Result<(), RpcError> {
            let mut record = self.record.lock().unwrap();
            if let Some(rec) = record.replicas.iter_mut().find(|r| r.lvol_uuid == replica_uuid) {
                rec.active_lvol_uuid = Some(new_head_uuid.to_string());
                rec.reverted_to = Some(base_epoch.to_string());
            }
            self.ops
                .lock()
                .unwrap()
                .push(format!("revert:{}:{}", base_epoch, new_head_uuid));
            Ok(())
        }

        async fn record_standby(
            &self,
            _volume_id: &str,
            replica_uuid: &str,
            caught_up_through: &str,
        ) -> Result<(), RpcError> {
            let mut record = self.record.lock().unwrap();
            record.mark_standby(replica_uuid, caught_up_through, "caught up; chasing epochs", "t");
            record.advance_retention_pin(_volume_id);
            self.ops.lock().unwrap().push(format!("standby:{}", caught_up_through));
            Ok(())
        }

        async fn record_reason(
            &self,
            _volume_id: &str,
            replica_uuid: &str,
            reason: &str,
        ) -> Result<(), RpcError> {
            let mut record = self.record.lock().unwrap();
            if let Some(rec) = record.replicas.iter_mut().find(|r| r.lvol_uuid == replica_uuid) {
                rec.reason = Some(reason.to_string());
            }
            self.ops.lock().unwrap().push(format!("reason:{}", reason));
            Ok(())
        }

        async fn record_stale(
            &self,
            _volume_id: &str,
            replica_uuid: &str,
            reason: &str,
        ) -> Result<(), RpcError> {
            self.record.lock().unwrap().mark_stale(replica_uuid, reason, "t-stale");
            self.ops.lock().unwrap().push(format!("stale:{}", replica_uuid));
            Ok(())
        }

        async fn record_epoch_cut(
            &self,
            _volume_id: &str,
            epoch: &str,
            cut_uuids: &[String],
        ) -> Result<(), RpcError> {
            self.record.lock().unwrap().apply_epoch_cut(epoch, cut_uuids, "t-cut");
            self.ops.lock().unwrap().push(format!("epoch_cut:{}", epoch));
            Ok(())
        }

        async fn record_in_sync(
            &self,
            _volume_id: &str,
            replica_uuid: &str,
            last_epoch: &str,
        ) -> Result<(), RpcError> {
            self.record
                .lock()
                .unwrap()
                .mark_in_sync(replica_uuid, last_epoch, "admitted", "t-insync");
            self.ops
                .lock()
                .unwrap()
                .push(format!("in_sync:{}:{}", replica_uuid, last_epoch));
            Ok(())
        }

        async fn clear_snapshot_tombstone(
            &self,
            _volume_id: &str,
            name: &str,
        ) -> Result<(), RpcError> {
            self.record.lock().unwrap().clear_snapshot_tombstone(name);
            self.ops.lock().unwrap().push(format!("clear_tombstone:{}", name));
            Ok(())
        }

        async fn emit(&self, _volume_id: &str, event_type: &str, reason: &str, _message: &str) {
            self.events
                .lock()
                .unwrap()
                .push((reason.to_string(), event_type.to_string()));
        }
    }

    /// Install a parent-linked snapshot chain plus head on a node's fake
    /// bdev table so the §11 lineage walk can discover it. `elements` are
    /// oldest first; the head's parent is the last element.
    fn install_chain(rpc: &FakeRpc, node: &str, lvs: &str, head_name: &str, elements: &[&str]) {
        let mut bdevs = rpc.bdevs.lock().unwrap();
        let mut prev: Option<&str> = None;
        for e in elements {
            let mut b = json!({
                "name": format!("uuid-of-{}", e),
                "uuid": format!("uuid-of-{}", e),
                "driver_specific": { "lvol": { "snapshot": true, "clone": prev.is_some() } }
            });
            if let Some(p) = prev {
                b["driver_specific"]["lvol"]["base_snapshot"] = json!(p);
            }
            bdevs.insert((node.to_string(), format!("{}/{}", lvs, e)), b);
            prev = Some(e);
        }
        let mut h = json!({
            "name": "head-of-chain",
            "uuid": "head-of-chain-uuid",
            "num_blocks": 2048,
            "block_size": 512,
            "driver_specific": { "lvol": { "snapshot": false, "clone": prev.is_some() } }
        });
        if let Some(p) = prev {
            h["driver_specific"]["lvol"]["base_snapshot"] = json!(p);
        }
        bdevs.insert((node.to_string(), format!("{}/{}", lvs, head_name)), h);
    }

    // ---- pure planning --------------------------------------------------

    #[test]
    fn select_base_backs_off_t_back() {
        let record = stale_b_record();
        let rec = record.get("uuid-b").unwrap();
        let present: HashSet<String> =
            [epoch("vol1", 3), epoch("vol1", 4), epoch("vol1", 5)].into_iter().collect();
        // Deadline 10:18 — newest qualifying is epoch 4 (10:05); epoch 5
        // (10:25) is inside the failure window even though present.
        assert_eq!(
            select_base_epoch("vol1", &record, rec, &present, Duration::from_secs(120)),
            Some(epoch("vol1", 4))
        );
        // Larger back-off pushes the base older.
        assert_eq!(
            select_base_epoch("vol1", &record, rec, &present, Duration::from_secs(1000)),
            Some(epoch("vol1", 3))
        );
    }

    #[test]
    fn select_base_requires_presence_on_dst() {
        let record = stale_b_record();
        let rec = record.get("uuid-b").unwrap();
        // Epoch 4 qualifies by time but is gone on R_dst: fall back to 3.
        let present: HashSet<String> = [epoch("vol1", 3)].into_iter().collect();
        assert_eq!(
            select_base_epoch("vol1", &record, rec, &present, Duration::from_secs(120)),
            Some(epoch("vol1", 3))
        );
        // Nothing present: full build.
        assert_eq!(
            select_base_epoch("vol1", &record, rec, &HashSet::new(), Duration::from_secs(120)),
            None
        );
    }

    #[test]
    fn select_base_unknown_failure_time_is_most_conservative() {
        let mut record = stale_b_record();
        let present: HashSet<String> =
            [epoch("vol1", 3), epoch("vol1", 4)].into_iter().collect();
        // Unparseable since → oldest present epoch.
        record.replicas[1].since = Some("garbage".to_string());
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(
            select_base_epoch("vol1", &record, rec, &present, Duration::from_secs(120)),
            Some(epoch("vol1", 3))
        );
    }

    #[test]
    fn select_base_skips_unparseable_recorded_at() {
        let mut record = stale_b_record();
        record.epochs[1].recorded_at = "garbage".to_string(); // epoch 4
        let rec = record.get("uuid-b").unwrap().clone();
        let present: HashSet<String> =
            [epoch("vol1", 3), epoch("vol1", 4)].into_iter().collect();
        // Epoch 4 can no longer be proven old enough — base falls to 3.
        assert_eq!(
            select_base_epoch("vol1", &record, &rec, &present, Duration::from_secs(120)),
            Some(epoch("vol1", 3))
        );
    }

    #[tokio::test]
    async fn lineage_walk_is_base_inclusive_and_carries_user_snapshots() {
        let rpc = FakeRpc::new("u");
        let src = replica("node-a", "uuid-a");
        // Chain on the source, oldest first: a user snapshot interleaves
        // between epochs 4 and 5 (its blob holds part of what a name-derived
        // epoch chain would attribute to epoch 5 — the §11 split hazard),
        // and another user snapshot is NEWER than the target epoch.
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[
                &epoch("vol1", 3),
                &epoch("vol1", 4),
                "snap_vol1_88",
                &epoch("vol1", 5),
                "snap_vol1_99",
            ],
        );

        // Base-inclusive from 4 through the target 5; the newer user
        // snapshot belongs to a later session and is skipped.
        let chain = lineage_chain(&rpc, &src, None, &epoch("vol1", 5), Some(&epoch("vol1", 4)))
            .await
            .unwrap();
        assert_eq!(
            chain,
            vec![epoch("vol1", 4), "snap_vol1_88".to_string(), epoch("vol1", 5)]
        );

        // Degenerate target == base.
        let chain = lineage_chain(&rpc, &src, None, &epoch("vol1", 4), Some(&epoch("vol1", 4)))
            .await
            .unwrap();
        assert_eq!(chain, vec![epoch("vol1", 4)]);

        // Full build (no base): everything from the chain root through the
        // target — the root element's blob holds all clusters written
        // before its cut, so this replays the volume from nothing.
        let chain = lineage_chain(&rpc, &src, None, &epoch("vol1", 5), None).await.unwrap();
        assert_eq!(
            chain,
            vec![
                epoch("vol1", 3),
                epoch("vol1", 4),
                "snap_vol1_88".to_string(),
                epoch("vol1", 5),
            ]
        );
    }

    #[tokio::test]
    async fn lineage_walk_errors_are_loud() {
        let rpc = FakeRpc::new("u");
        let src = replica("node-a", "uuid-a");

        // No head at all.
        let err = lineage_chain(&rpc, &src, None, &epoch("vol1", 5), Some(&epoch("vol1", 4)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("source head"), "got: {}", err);

        // Base not present in the lineage (walked to the chain root).
        install_chain(&rpc, "node-a", "lvs0", "lvol-uuid-a", &[&epoch("vol1", 5)]);
        let err = lineage_chain(&rpc, &src, None, &epoch("vol1", 5), Some(&epoch("vol1", 4)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found in the source lineage"), "got: {}", err);

        // Full build whose target is not in the lineage at all.
        let err = lineage_chain(&rpc, &src, None, &epoch("vol1", 9), None).await.unwrap_err();
        assert!(err.to_string().contains("not found in the source lineage"), "got: {}", err);
    }

    /// The 3-replica drill wedge (2026-07-03): the chase source has itself
    /// been hot-rejoined, so its live head is a `_hr`-named clone under a
    /// NEW uuid and the canonical `{lvs}/{lvol_name}` alias resolves to
    /// nothing. Resolution must go through the record's live uuid.
    #[tokio::test]
    async fn lineage_chain_resolves_hot_rejoined_source_by_live_uuid() {
        let rpc = FakeRpc::new("u");
        let src = replica("node-a", "uuid-a");
        // Install the chain under the live head's own name — the canonical
        // alias lvs0/lvol-uuid-a is deliberately ABSENT, as on a source
        // whose promoted `_hr` head replaced it.
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a_hr",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        // An lvol bdev is addressable by its uuid; mirror the `_hr` head
        // under the live uuid the record carries.
        {
            let mut bdevs = rpc.bdevs.lock().unwrap();
            let head = bdevs
                .get(&("node-a".to_string(), "lvs0/lvol-uuid-a_hr".to_string()))
                .cloned()
                .unwrap();
            bdevs.insert(("node-a".to_string(), "uuid-a-live".to_string()), head);
        }

        // Name-based resolution alone still fails loudly...
        let err = lineage_chain(&rpc, &src, None, &epoch("vol1", 5), Some(&epoch("vol1", 4)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("source head"), "got: {}", err);

        // ...and the record's live uuid resolves the same chain correctly.
        let chain = lineage_chain(
            &rpc, &src, Some("uuid-a-live"), &epoch("vol1", 5), Some(&epoch("vol1", 4)),
        )
        .await
        .unwrap();
        assert_eq!(chain, vec![epoch("vol1", 4), epoch("vol1", 5)]);
    }

    /// A recorded live uuid that no longer resolves (e.g. the record is a
    /// step behind a concurrent revert) must fall back to the canonical
    /// alias rather than fail — the alias is correct whenever no rejoin
    /// replaced the head.
    #[tokio::test]
    async fn source_head_resolution_falls_back_to_canonical_alias() {
        let rpc = FakeRpc::new("u");
        let src = replica("node-a", "uuid-a");
        install_chain(&rpc, "node-a", "lvs0", "lvol-uuid-a", &[&epoch("vol1", 4)]);

        let chain = lineage_chain(
            &rpc, &src, Some("uuid-gone"), &epoch("vol1", 4), Some(&epoch("vol1", 4)),
        )
        .await
        .unwrap();
        assert_eq!(chain, vec![epoch("vol1", 4)]);
    }

    /// source_live_uuid reads the record: identity uuid when no override,
    /// active_lvol_uuid once a revert/rejoin recorded a new live head.
    #[test]
    fn source_live_uuid_prefers_active_override() {
        let mut record = stale_b_record();
        let replicas = replicas3();
        let src = &replicas[0]; // node-a, uuid-a
        assert_eq!(source_live_uuid(&record, src), Some("uuid-a"));
        record.replicas[0].active_lvol_uuid = Some("uuid-a-live".to_string());
        assert_eq!(source_live_uuid(&record, src), Some("uuid-a-live"));
    }

    #[test]
    fn pick_source_prefers_non_consumer() {
        let record = stale_b_record(); // a, c in sync
        let replicas = replicas3();
        assert_eq!(
            pick_source(&record, &replicas, None).unwrap().node_name,
            "node-a"
        );
        // Consumer on node-a: keep the copy load off the data path.
        assert_eq!(
            pick_source(&record, &replicas, Some("node-a")).unwrap().node_name,
            "node-c"
        );
        // Sole in-sync replica is the consumer's: use it anyway.
        let mut record = record;
        record.mark_stale("uuid-c", "x", "t");
        assert_eq!(
            pick_source(&record, &replicas, Some("node-a")).unwrap().node_name,
            "node-a"
        );
        record.mark_stale("uuid-a", "x", "t");
        assert!(pick_source(&record, &replicas, None).is_none());
    }

    // ---- RPC choreography -----------------------------------------------

    #[tokio::test]
    async fn revert_head_deletes_and_clones_by_alias() {
        let rpc = FakeRpc::new("uuid-b-v2");
        let uuid = revert_head(&rpc, "node-b", "lvs0/lvol-uuid-b", "lvol-uuid-b", "lvs0/epoch-vol1-4")
            .await
            .unwrap();
        assert_eq!(uuid, "uuid-b-v2");
        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert_eq!(deletes[0].1["params"]["name"], "lvs0/lvol-uuid-b");
        let clones = rpc.calls_of("bdev_lvol_clone");
        assert_eq!(clones[0].1["params"]["snapshot_name"], "lvs0/epoch-vol1-4");
        assert_eq!(clones[0].1["params"]["clone_name"], "lvol-uuid-b");
    }

    #[tokio::test]
    async fn revert_head_tolerates_missing_head() {
        // Crash between a previous delete and clone: the head is gone.
        let mut rpc = FakeRpc::new("uuid-b-v2");
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_delete".to_string()),
            "Code=-19: No such device".to_string(),
        );
        let uuid = revert_head(&rpc, "node-b", "lvs0/lvol-uuid-b", "lvol-uuid-b", "lvs0/epoch-vol1-4")
            .await
            .unwrap();
        assert_eq!(uuid, "uuid-b-v2");
    }

    #[tokio::test]
    async fn clear_head_sb_deletes_phantom_with_clear_sb() {
        let rpc = FakeRpc::new("u");
        rpc.raids.lock().unwrap().insert(
            "node-b".to_string(),
            vec![json!({ "name": "raid_vol1", "state": "configuring",
                         "base_bdevs_list": [{ "name": "lvol-uuid-b" }] })],
        );
        clear_head_sb(&rpc, "node-b", "lvs0/lvol-uuid-b", "raid_vol1").await.unwrap();
        // examine → wait → delete with clear_sb.
        assert_eq!(rpc.calls_of("bdev_examine").len(), 1);
        assert_eq!(rpc.calls_of("bdev_wait_for_examine").len(), 1);
        let deletes = rpc.calls_of("bdev_raid_delete");
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].1["params"]["clear_sb"], true);
    }

    #[tokio::test]
    async fn clear_head_sb_falls_back_without_clear_sb() {
        // v26.01: clear_sb is an unknown parameter.
        let mut rpc = FakeRpc::new("u");
        rpc.fail_clear_sb = true;
        rpc.raids.lock().unwrap().insert(
            "node-b".to_string(),
            vec![json!({ "name": "raid_vol1", "state": "configuring" })],
        );
        clear_head_sb(&rpc, "node-b", "lvs0/lvol-uuid-b", "raid_vol1").await.unwrap();
        let deletes = rpc.calls_of("bdev_raid_delete");
        assert_eq!(deletes.len(), 2);
        assert_eq!(deletes[0].1["params"]["clear_sb"], true);
        assert!(deletes[1].1["params"].get("clear_sb").is_none());
    }

    #[tokio::test]
    async fn clear_head_sb_refuses_online_raid() {
        let rpc = FakeRpc::new("u");
        rpc.raids.lock().unwrap().insert(
            "node-b".to_string(),
            vec![json!({ "name": "raid_vol1", "state": "online" })],
        );
        let err = clear_head_sb(&rpc, "node-b", "lvs0/lvol-uuid-b", "raid_vol1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ONLINE raid"), "got: {}", err);
        assert!(rpc.calls_of("bdev_raid_delete").is_empty());
    }

    #[tokio::test]
    async fn ensure_dst_attached_reuses_live_attachment() {
        let rpc = FakeRpc::new("uuid-b-v2");
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.bdevs.lock().unwrap().insert(
            ("node-a".to_string(), expected.clone()),
            json!({ "name": expected, "uuid": "uuid-b-v2" }),
        );
        let dst = replica("node-b", "uuid-b");
        let bdev = ensure_dst_attached(
            &rpc, "vol1", 1, &dst, "lvs0/lvol-uuid-b", "uuid-b-v2", "node-a", "raid_vol1",
        )
        .await
        .unwrap();
        assert_eq!(bdev, expected);
        // Live attachment: no export, no attach, no dst-side hygiene.
        assert!(rpc.exports.lock().unwrap().is_empty());
        assert!(rpc.calls_of("bdev_nvme_attach_controller").is_empty());
        assert!(rpc.calls_of("bdev_examine").is_empty());
    }

    #[tokio::test]
    async fn ensure_dst_attached_full_path_exports_fenced_and_attaches() {
        let rpc = FakeRpc::new("uuid-b-v2");
        let dst = replica("node-b", "uuid-b");
        let bdev = ensure_dst_attached(
            &rpc, "vol1", 1, &dst, "lvs0/lvol-uuid-b", "uuid-b-v2", "node-a", "raid_vol1",
        )
        .await
        .unwrap();
        assert_eq!(bdev, expected_remote_base_bdev("vol1", 1));

        // Export on the replica node, fence admitting the source node.
        let exports = rpc.exports.lock().unwrap().clone();
        assert_eq!(
            exports,
            vec![("node-b".to_string(), "uuid-b-v2".to_string(), "vol1_1".to_string(), "node-a".to_string())]
        );
        // sb hygiene ran on the replica node before the export.
        let examines = rpc.calls_of("bdev_examine");
        assert_eq!(examines[0].0, "node-b");

        // Attach on the source node with the stable per-node host NQN.
        let attaches = rpc.calls_of("bdev_nvme_attach_controller");
        assert_eq!(attaches.len(), 1);
        assert_eq!(attaches[0].0, "node-a");
        assert_eq!(
            attaches[0].1["params"]["hostnqn"],
            crate::nvmeof_export::flint_host_nqn("node-a")
        );
        assert_eq!(
            attaches[0].1["params"]["subnqn"],
            "nqn.2024-11.com.flint:volume:vol1_1"
        );
    }

    #[tokio::test]
    async fn ensure_dst_attached_replaces_stale_attachment() {
        // The expected bdev exists but is backed by the pre-revert head.
        let rpc = FakeRpc::new("uuid-b-v2");
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.bdevs.lock().unwrap().insert(
            ("node-a".to_string(), expected.clone()),
            json!({ "name": expected, "uuid": "uuid-b-OLD" }),
        );
        let dst = replica("node-b", "uuid-b");
        let bdev = ensure_dst_attached(
            &rpc, "vol1", 1, &dst, "lvs0/lvol-uuid-b", "uuid-b-v2", "node-a", "raid_vol1",
        )
        .await
        .unwrap();
        assert_eq!(bdev, expected);
        // Stale controller detached, then re-attached to the live head.
        assert!(!rpc.calls_of("bdev_nvme_detach_controller").is_empty());
        assert_eq!(rpc.calls_of("bdev_nvme_attach_controller").len(), 1);
    }

    #[tokio::test]
    async fn ensure_dst_attached_aborts_when_online_raid_claims_dst() {
        let rpc = FakeRpc::new("uuid-b-v2");
        let expected = expected_remote_base_bdev("vol1", 1);
        // The consumer raid on the source node re-added the attached bdev
        // (ONLINE examine, §3) — a blind rebuild is running.
        rpc.raids.lock().unwrap().insert(
            "node-a".to_string(),
            vec![json!({ "name": "raid_vol1", "state": "online",
                         "base_bdevs_list": [{ "name": expected }] })],
        );
        let dst = replica("node-b", "uuid-b");
        let err = ensure_dst_attached(
            &rpc, "vol1", 1, &dst, "lvs0/lvol-uuid-b", "uuid-b-v2", "node-a", "raid_vol1",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("ONLINE raid"), "got: {}", err);
    }

    #[tokio::test]
    async fn ensure_dst_attached_releases_configuring_phantom_on_src() {
        let rpc = FakeRpc::new("uuid-b-v2");
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.raids.lock().unwrap().insert(
            "node-a".to_string(),
            vec![json!({ "name": "raid_vol1", "state": "configuring",
                         "base_bdevs_list": [{ "name": expected }] })],
        );
        let dst = replica("node-b", "uuid-b");
        ensure_dst_attached(
            &rpc, "vol1", 1, &dst, "lvs0/lvol-uuid-b", "uuid-b-v2", "node-a", "raid_vol1",
        )
        .await
        .unwrap();
        // Phantom deleted on the source node WITHOUT clear_sb (its bases can
        // include the source's own live lvols).
        let deletes: Vec<_> = rpc
            .calls_of("bdev_raid_delete")
            .into_iter()
            .filter(|(node, _)| node == "node-a")
            .collect();
        assert_eq!(deletes.len(), 1);
        assert!(deletes[0].1["params"].get("clear_sb").is_none());
    }

    #[tokio::test]
    async fn shallow_copy_polls_to_completion() {
        let rpc = FakeRpc::new("u");
        rpc.pending_copy_states
            .lock()
            .unwrap()
            .extend(["in progress".to_string(), "in progress".to_string()]);
        shallow_copy(&rpc, "node-a", "lvs0/epoch-vol1-4", "dst", Duration::ZERO, None)
            .await
            .unwrap();
        assert_eq!(rpc.calls_of("bdev_lvol_check_shallow_copy").len(), 3);
    }

    #[tokio::test]
    async fn shallow_copy_surfaces_error_state() {
        let rpc = FakeRpc::new("u");
        rpc.pending_copy_states
            .lock()
            .unwrap()
            .push("error:No space left on device".to_string());
        let err = shallow_copy(&rpc, "node-a", "lvs0/epoch-vol1-4", "dst", Duration::ZERO, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No space left"), "got: {}", err);
    }

    #[tokio::test]
    async fn align_head_tolerates_already_exists() {
        let mut rpc = FakeRpc::new("u");
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_snapshot".to_string()),
            "SPDK RPC error Code=-17: File exists".to_string(),
        );
        align_head(&rpc, "node-b", "lvs0/lvol-uuid-b", "epoch-vol1-5").await.unwrap();
    }

    // ---- full flows ------------------------------------------------------

    #[tokio::test]
    async fn bulk_catchup_full_choreography() {
        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![
                    ("lvol-uuid-b".to_string(), "uuid-b".to_string()),
                    (epoch("vol1", 3), "s3".to_string()),
                    (epoch("vol1", 4), "s4".to_string()),
                ],
            );
            rpc
        };
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        let store = FakeStore::new(stale_b_record());

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        // Pin lands before the revert; standby closes the flow.
        let ops = store.ops.lock().unwrap().clone();
        assert_eq!(
            ops,
            vec![
                format!("pin:{}", epoch("vol1", 4)),
                format!("revert:{}:uuid-b-v2", epoch("vol1", 4)),
                format!("standby:{}", epoch("vol1", 5)),
            ]
        );

        // Record end state: standby through epoch 5, live-uuid override,
        // write-virgin marker — and the pin HELD (§10-14: released at
        // admission, not copy completion, so retention never grinds the
        // GC against the standby chain's base) and ADVANCED to the chase
        // mark (the standby resumes base-inclusively from epoch 5, so
        // retention may retire everything older).
        let mut record = store.record.lock().unwrap();
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(rec.sync_state, SyncState::Standby);
        assert_eq!(rec.last_epoch.as_deref(), Some(epoch("vol1", 5).as_str()));
        assert_eq!(rec.active_lvol_uuid.as_deref(), Some("uuid-b-v2"));
        assert_eq!(rec.reverted_to.as_deref(), Some(epoch("vol1", 4).as_str()));
        assert_eq!(
            record.retention_pin.as_deref(),
            Some(epoch("vol1", 5).as_str())
        );
        // Phase-4 admission of the last dependent replica releases it.
        record.mark_in_sync("uuid-b", &epoch("vol1", 6), "admitted", "t");
        assert_eq!(record.retention_pin, None);
        drop(record);

        // Copies ran on the source node, base-INCLUSIVE, in order.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        let srcs: Vec<&str> = copies
            .iter()
            .map(|(_, p)| p["params"]["src_lvol_name"].as_str().unwrap())
            .collect();
        assert_eq!(srcs, vec!["lvs0/epoch-vol1-4", "lvs0/epoch-vol1-5"]);
        assert!(copies.iter().all(|(node, _)| node == "node-a"));
        let dst_bdev = expected_remote_base_bdev("vol1", 1);
        assert!(copies
            .iter()
            .all(|(_, p)| p["params"]["dst_bdev_name"].as_str() == Some(dst_bdev.as_str())));

        // Alignment snapshot on the destination, named the newest epoch.
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].0, "node-b");
        assert_eq!(snaps[0].1["params"]["lvol_name"], "lvs0/lvol-uuid-b");
        assert_eq!(snaps[0].1["params"]["snapshot_name"], epoch("vol1", 5));

        // Events: started + standby.
        let events = store.events.lock().unwrap().clone();
        let reasons: Vec<&str> = events.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(reasons, vec!["ReplicaCatchupStarted", "ReplicaStandby"]);
    }

    #[tokio::test]
    async fn revert_reaps_the_superseded_hot_rejoin_head() {
        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![
                    ("lvol-uuid-b".to_string(), "uuid-b".to_string()),
                    (epoch("vol1", 3), "s3".to_string()),
                    (epoch("vol1", 4), "s4".to_string()),
                ],
            );
            rpc
        };
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        // A promoted hot-rejoin head from an earlier window is the record's
        // live lvol; the revert supersedes it and must reap it, or it holds
        // the head name hostage against every later window (7b-3 P1).
        rpc.bdevs.lock().unwrap().insert(
            ("node-b".to_string(), "lvs0/vol_vol1_replica_1_hr".to_string()),
            json!({ "name": "lvs0/vol_vol1_replica_1_hr", "uuid": "uuid-head-old" }),
        );
        let mut record = stale_b_record();
        record
            .replicas
            .iter_mut()
            .find(|r| r.lvol_uuid == "uuid-b")
            .unwrap()
            .active_lvol_uuid = Some("uuid-head-old".to_string());
        let store = FakeStore::new(record);

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert!(
            deletes.iter().any(|(node, p)| node == "node-b"
                && p["params"]["name"].as_str() == Some("lvs0/vol_vol1_replica_1_hr")),
            "superseded head must be reaped, got {:?}",
            deletes
        );
    }

    #[tokio::test]
    async fn revert_leaves_an_unrelated_hr_namesake_alone() {
        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![
                    ("lvol-uuid-b".to_string(), "uuid-b".to_string()),
                    (epoch("vol1", 3), "s3".to_string()),
                    (epoch("vol1", 4), "s4".to_string()),
                ],
            );
            rpc
        };
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        // An `_hr` namesake exists but is NOT the lvol this revert
        // supersedes (the record's live lvol is the canonical uuid-b):
        // the uuid guard must keep the reap away from it.
        rpc.bdevs.lock().unwrap().insert(
            ("node-b".to_string(), "lvs0/vol_vol1_replica_1_hr".to_string()),
            json!({ "name": "lvs0/vol_vol1_replica_1_hr", "uuid": "uuid-someone-else" }),
        );
        let store = FakeStore::new(stale_b_record());

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert!(
            !deletes.iter().any(|(_, p)| p["params"]["name"].as_str()
                == Some("lvs0/vol_vol1_replica_1_hr")),
            "unrelated namesake must survive, got {:?}",
            deletes
        );
    }

    #[tokio::test]
    async fn bulk_catchup_resumes_without_rerevert() {
        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![
                    ("lvol-uuid-b".to_string(), "uuid-b-v2".to_string()),
                    (epoch("vol1", 3), "s3".to_string()),
                    (epoch("vol1", 4), "s4".to_string()),
                ],
            );
            rpc
        };
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        // A previous attempt already reverted to epoch 4 and crashed.
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.replicas[1].reverted_to = Some(epoch("vol1", 4));
        let store = FakeStore::new(record);

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        // The write-virgin head is reused: no delete, no clone.
        assert!(rpc.calls_of("bdev_lvol_delete").is_empty());
        assert!(rpc.calls_of("bdev_lvol_clone").is_empty());
        // The chain is still re-copied in full and the replica promoted.
        assert_eq!(rpc.calls_of("bdev_lvol_start_shallow_copy").len(), 2);
        let rec_state = store.record.lock().unwrap().get("uuid-b").unwrap().sync_state;
        assert_eq!(rec_state, SyncState::Standby);
        // No second ReplicaCatchupStarted on resume.
        let reasons: Vec<String> =
            store.events.lock().unwrap().iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(reasons, vec!["ReplicaStandby".to_string()]);
    }

    #[tokio::test]
    async fn degenerate_single_epoch_skips_align() {
        // Only one epoch exists and it qualifies as the base: copy E_b's own
        // delta (load-bearing) but cut no new snapshot — the destination
        // already holds a snapshot by that name (the revert source).
        let mut record = VolumeSyncRecord::initial(&replicas3());
        let all = vec!["uuid-a".to_string(), "uuid-b".to_string(), "uuid-c".to_string()];
        record.apply_epoch_cut(&epoch("vol1", 1), &all, "2026-06-11T10:00:00Z");
        record.mark_stale("uuid-b", "leg failed", "2026-06-11T10:20:00Z");
        let store = FakeStore::new(record);

        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![
                    ("lvol-uuid-b".to_string(), "uuid-b".to_string()),
                    (epoch("vol1", 1), "s1".to_string()),
                ],
            );
            rpc
        };
        install_chain(&rpc, "node-a", "lvs0", "lvol-uuid-a", &[&epoch("vol1", 1)]);

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].1["params"]["src_lvol_name"], "lvs0/epoch-vol1-1");
        assert!(rpc.calls_of("bdev_lvol_snapshot").is_empty());
        let record = store.record.lock().unwrap();
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(rec.sync_state, SyncState::Standby);
        assert_eq!(rec.last_epoch.as_deref(), Some(epoch("vol1", 1).as_str()));
    }

    #[tokio::test]
    async fn no_shared_history_classifies_once_when_full_build_disabled() {
        let rpc = {
            let mut rpc = FakeRpc::new("u");
            // The returned replica holds no epoch snapshots at all.
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![("lvol-uuid-b".to_string(), "uuid-b".to_string())],
            );
            rpc
        };
        let store = FakeStore::new(stale_b_record());
        let cfg = CatchupConfig { full_build: false, ..cfg() };

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg)
            .await
            .unwrap();
        // Marked, evented, nothing pinned or reverted.
        let ops = store.ops.lock().unwrap().clone();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].starts_with("reason:full rebuild required"));
        let events = store.events.lock().unwrap().clone();
        assert_eq!(events, vec![("ReplicaNeedsFullRebuild".to_string(), "Warning".to_string())]);
        assert!(rpc.calls_of("bdev_lvol_clone").is_empty());
        assert!(rpc.calls_of("bdev_lvol_create").is_empty());

        // Second cycle: reason already recorded — no duplicate event.
        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg)
            .await
            .unwrap();
        assert_eq!(store.events.lock().unwrap().len(), 1);
    }

    // ---- phase 5: thin-aware full build -----------------------------------

    #[tokio::test]
    async fn full_build_replays_entire_lineage_from_empty() {
        // The returned replica holds NO epoch snapshots (wiped disk /
        // retention expiry): revert to EMPTY and replay the source's whole
        // lineage — including the interleaved user snapshot.
        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![("lvol-uuid-b".to_string(), "uuid-b".to_string())],
            );
            rpc
        };
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), "snap_vol1_88", &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        let store = FakeStore::new(stale_b_record());

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        // Pin lands on the OLDEST retained epoch before the revert; the
        // full-build marker is recorded; standby closes the flow.
        let ops = store.ops.lock().unwrap().clone();
        assert_eq!(
            ops,
            vec![
                format!("pin:{}", epoch("vol1", 3)),
                "revert:empty:uuid-b-v2".to_string(),
                format!("standby:{}", epoch("vol1", 5)),
            ]
        );

        // The head was deleted and recreated EMPTY, thin, sized to the
        // source head (2048 × 512 = 1 MiB).
        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert_eq!(deletes[0].1["params"]["name"], "lvs0/lvol-uuid-b");
        let creates = rpc.calls_of("bdev_lvol_create");
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].0, "node-b");
        assert_eq!(creates[0].1["params"]["lvol_name"], "lvol-uuid-b");
        assert_eq!(creates[0].1["params"]["lvs_name"], "lvs0");
        assert_eq!(creates[0].1["params"]["size_in_mib"], 1);
        assert_eq!(creates[0].1["params"]["thin_provision"], true);
        assert!(rpc.calls_of("bdev_lvol_clone").is_empty());

        // The ENTIRE lineage is replayed in order, root first.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        let srcs: Vec<&str> = copies
            .iter()
            .map(|(_, p)| p["params"]["src_lvol_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            srcs,
            vec![
                "lvs0/epoch-vol1-3",
                "lvs0/snap_vol1_88",
                "lvs0/epoch-vol1-4",
                "lvs0/epoch-vol1-5",
            ]
        );
        // The destination re-acquires the user snapshot and the target.
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        let aligned: Vec<&str> = snaps
            .iter()
            .map(|(_, p)| p["params"]["snapshot_name"].as_str().unwrap())
            .collect();
        assert_eq!(aligned, vec!["snap_vol1_88", "epoch-vol1-5"]);

        // Record end state: standby at 5, live-uuid override, full-build
        // marker. The pin anchored at the build's OLDEST retained epoch
        // for the build's duration, then advanced to the chase mark at
        // standby — once the replay completed, the standby resumes from
        // its mark and needs nothing older.
        {
            let record = store.record.lock().unwrap();
            let rec = record.get("uuid-b").unwrap();
            assert_eq!(rec.sync_state, SyncState::Standby);
            assert_eq!(rec.last_epoch.as_deref(), Some(epoch("vol1", 5).as_str()));
            assert_eq!(rec.active_lvol_uuid.as_deref(), Some("uuid-b-v2"));
            assert_eq!(rec.reverted_to.as_deref(), Some(FULL_BUILD_BASE));
            assert_eq!(
                record.retention_pin.as_deref(),
                Some(epoch("vol1", 5).as_str())
            );
        }
        let reasons: Vec<String> =
            store.events.lock().unwrap().iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(
            reasons,
            vec!["ReplicaFullBuildStarted".to_string(), "ReplicaStandby".to_string()]
        );
    }

    #[tokio::test]
    async fn full_build_resumes_without_recreating_the_head() {
        // A previous full build crashed mid-copy: the marker stands and the
        // head has only ever received copy writes — replay converges onto
        // it without deleting or recreating anything.
        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![("lvol-uuid-b".to_string(), "uuid-b-v2".to_string())],
            );
            rpc
        };
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.replicas[1].reverted_to = Some(FULL_BUILD_BASE.to_string());
        let store = FakeStore::new(record);

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        assert!(rpc.calls_of("bdev_lvol_delete").is_empty());
        assert!(rpc.calls_of("bdev_lvol_create").is_empty());
        assert_eq!(rpc.calls_of("bdev_lvol_start_shallow_copy").len(), 3);
        let record = store.record.lock().unwrap();
        assert_eq!(record.get("uuid-b").unwrap().sync_state, SyncState::Standby);
        drop(record);
        // No second ReplicaFullBuildStarted on resume.
        let reasons: Vec<String> =
            store.events.lock().unwrap().iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(reasons, vec!["ReplicaStandby".to_string()]);
    }

    #[tokio::test]
    async fn unreachable_replica_is_not_an_error() {
        let mut rpc = FakeRpc::new("u");
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_get_lvols".to_string()),
            "connection refused".to_string(),
        );
        let store = FakeStore::new(stale_b_record());
        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();
        // Not returned yet: silent — no ops, no events, no failure noise.
        assert!(store.ops.lock().unwrap().is_empty());
        assert!(store.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn chase_advances_standby_base_inclusive() {
        // b is a standby caught up through epoch 4; epoch 5 was cut since.
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.replicas[1].reverted_to = Some(epoch("vol1", 4));
        record.mark_standby("uuid-b", &epoch("vol1", 4), "caught up", "t");
        let store = FakeStore::new(record);

        let rpc = FakeRpc::new("uuid-b-v2");
        // Live attachment from the bulk session.
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.bdevs.lock().unwrap().insert(
            ("node-a".to_string(), expected.clone()),
            json!({ "name": expected, "uuid": "uuid-b-v2" }),
        );
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        // Base-inclusive: epoch 4 is re-copied (source-switch safety), then 5.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        let srcs: Vec<&str> = copies
            .iter()
            .map(|(_, p)| p["params"]["src_lvol_name"].as_str().unwrap())
            .collect();
        assert_eq!(srcs, vec!["lvs0/epoch-vol1-4", "lvs0/epoch-vol1-5"]);
        // Aligned at 5, mark advanced, no pin involved.
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        assert_eq!(snaps[0].1["params"]["snapshot_name"], epoch("vol1", 5));
        let record = store.record.lock().unwrap();
        assert_eq!(
            record.get("uuid-b").unwrap().last_epoch.as_deref(),
            Some(epoch("vol1", 5).as_str())
        );
        assert_eq!(record.retention_pin, None);
    }

    #[tokio::test]
    async fn chase_fails_over_to_a_source_covering_the_base() {
        // b is a standby caught up through epoch 4. The PREFERRED source
        // (non-consumer node-a) was itself rebuilt after a staggered
        // failure: its chain roots at epoch 5, so epoch 4 is unreachable
        // from its head. The consumer (node-c) still holds the full
        // history. The chase must fail over to node-c instead of wedging
        // on node-a forever (the second 3-replica-drill wedge).
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.replicas[1].reverted_to = Some(epoch("vol1", 4));
        record.mark_standby("uuid-b", &epoch("vol1", 4), "caught up", "t");
        let store = FakeStore::new(record);

        let rpc = FakeRpc::new("uuid-b-v2");
        let expected = expected_remote_base_bdev("vol1", 1);
        for node in ["node-a", "node-c"] {
            rpc.bdevs.lock().unwrap().insert(
                (node.to_string(), expected.clone()),
                json!({ "name": expected, "uuid": "uuid-b-v2" }),
            );
        }
        install_chain(&rpc, "node-a", "lvs0", "lvol-uuid-a", &[&epoch("vol1", 5)]);
        install_chain(
            &rpc, "node-c", "lvs0", "lvol-uuid-c",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), Some("node-c"), &cfg())
            .await
            .unwrap();

        // Copied from node-c (base-inclusive), never from node-a.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        assert!(copies.iter().all(|(node, _)| node == "node-c"), "copies: {:?}", copies);
        let srcs: Vec<&str> = copies
            .iter()
            .map(|(_, p)| p["params"]["src_lvol_name"].as_str().unwrap())
            .collect();
        assert_eq!(srcs, vec!["lvs0/epoch-vol1-4", "lvs0/epoch-vol1-5"]);
        let record = store.record.lock().unwrap();
        let b = record.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Standby);
        assert_eq!(b.last_epoch.as_deref(), Some(epoch("vol1", 5).as_str()));
    }

    #[tokio::test]
    async fn chase_fails_over_past_an_unreachable_source() {
        // The preferred source's node errors on the probe (transient, not a
        // coverage verdict); node-c covers → the chase proceeds from node-c
        // this cycle instead of failing it.
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.mark_standby("uuid-b", &epoch("vol1", 4), "caught up", "t");
        let store = FakeStore::new(record.clone());

        let mut rpc = FakeRpc::new("uuid-b-v2");
        rpc.fail.insert(
            ("node-a".to_string(), "bdev_get_bdevs".to_string()),
            "connection refused".to_string(),
        );
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.bdevs.lock().unwrap().insert(
            ("node-c".to_string(), expected.clone()),
            json!({ "name": expected, "uuid": "uuid-b-v2" }),
        );
        install_chain(
            &rpc, "node-c", "lvs0", "lvol-uuid-c",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );

        chase_standby(
            &rpc, &store, "vol1", &record,
            record.get("uuid-b").unwrap(),
            &replicas3(), None, "raid_vol1", &cfg(),
        )
        .await
        .unwrap();

        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        assert!(!copies.is_empty());
        assert!(copies.iter().all(|(node, _)| node == "node-c"), "copies: {:?}", copies);
    }

    #[tokio::test]
    async fn chase_demotes_standby_when_no_source_covers_its_mark() {
        // Rolling failures: the only in-sync survivor (node-a) was itself
        // rebuilt from epoch 5 — no lineage anywhere reaches the standby's
        // epoch-4 mark. Retrying can never succeed: demote to stale so the
        // bulk path rebuilds it.
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.mark_standby("uuid-b", &epoch("vol1", 4), "caught up", "t");
        record.mark_stale("uuid-c", "leg failed", "2026-06-11T10:30:00Z");
        let store = FakeStore::new(record.clone());

        let rpc = FakeRpc::new("uuid-b-v2");
        install_chain(&rpc, "node-a", "lvs0", "lvol-uuid-a", &[&epoch("vol1", 5)]);

        chase_standby(
            &rpc, &store, "vol1", &record,
            record.get("uuid-b").unwrap(),
            &replicas3(), None, "raid_vol1", &cfg(),
        )
        .await
        .unwrap();

        assert!(rpc.calls_of("bdev_lvol_start_shallow_copy").is_empty());
        assert!(store.ops.lock().unwrap().contains(&"stale:uuid-b".to_string()));
        assert!(store
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(r, t)| r == "ReplicaChaseSourcesExhausted" && t == "Warning"));
        assert_eq!(
            store.record.lock().unwrap().get("uuid-b").unwrap().sync_state,
            SyncState::Stale
        );
    }

    #[tokio::test]
    async fn chase_source_probe_transport_error_retries_without_demotion() {
        // The sole in-sync source errors on the probe: indeterminate — the
        // cycle must fail and retry, never demote (a transient must not
        // cost a full rebuild).
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.mark_standby("uuid-b", &epoch("vol1", 4), "caught up", "t");
        record.mark_stale("uuid-c", "leg failed", "2026-06-11T10:30:00Z");
        let store = FakeStore::new(record.clone());

        let mut rpc = FakeRpc::new("uuid-b-v2");
        rpc.fail.insert(
            ("node-a".to_string(), "bdev_get_bdevs".to_string()),
            "connection refused".to_string(),
        );

        let err = chase_standby(
            &rpc, &store, "vol1", &record,
            record.get("uuid-b").unwrap(),
            &replicas3(), None, "raid_vol1", &cfg(),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("connection refused"), "got: {}", err);
        assert!(!store.ops.lock().unwrap().iter().any(|o| o.starts_with("stale:")));
        assert_eq!(
            store.record.lock().unwrap().get("uuid-b").unwrap().sync_state,
            SyncState::Standby
        );
    }

    #[tokio::test]
    async fn stale_catchup_full_build_fallback_when_base_uncovered() {
        // b is stale with epochs 3 and 4 still present locally — but the
        // only in-sync survivor's chain roots at epoch 5 (it was itself
        // rebuilt), so no delta base is coverable anywhere: fall back to
        // the thin-aware full build from that survivor instead of wedging.
        let mut record = stale_b_record();
        record.mark_stale("uuid-c", "leg failed", "2026-06-11T10:30:00Z");
        let store = FakeStore::new(record.clone());

        let mut rpc = FakeRpc::new("uuid-b-v2");
        rpc.lvols.insert(
            "node-b".to_string(),
            vec![(epoch("vol1", 3), "e3".to_string()), (epoch("vol1", 4), "e4".to_string())],
        );
        install_chain(&rpc, "node-a", "lvs0", "lvol-uuid-a", &[&epoch("vol1", 5)]);
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.bdevs.lock().unwrap().insert(
            ("node-a".to_string(), expected.clone()),
            json!({ "name": expected, "uuid": "uuid-b-v2" }),
        );

        catchup_stale(
            &rpc, &store, "vol1", &record,
            record.get("uuid-b").unwrap(),
            &replicas3(), None, "raid_vol1", &cfg(),
        )
        .await
        .unwrap();

        // The full build replayed the survivor's whole (truncated) lineage.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        let srcs: Vec<&str> = copies
            .iter()
            .map(|(_, p)| p["params"]["src_lvol_name"].as_str().unwrap())
            .collect();
        assert_eq!(srcs, vec!["lvs0/epoch-vol1-5"]);
        let ops = store.ops.lock().unwrap();
        assert!(
            ops.iter().any(|o| o == &format!("revert:{}:uuid-b-v2", FULL_BUILD_BASE)),
            "ops: {:?}",
            ops
        );
        assert!(ops.iter().any(|o| o == &format!("standby:{}", epoch("vol1", 5))));
        let events = store.events.lock().unwrap();
        assert!(events.iter().any(|(r, _)| r == "ReplicaCatchupBaseUncovered"));
        assert!(events.iter().any(|(r, _)| r == "ReplicaFullBuildStarted"));
    }

    #[tokio::test]
    async fn chase_is_noop_when_current() {
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.mark_standby("uuid-b", &epoch("vol1", 5), "caught up", "t");
        let store = FakeStore::new(record);
        let rpc = FakeRpc::new("uuid-b-v2");

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();
        assert!(rpc.calls_of("bdev_lvol_start_shallow_copy").is_empty());
        assert!(store.ops.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_epochs_means_no_action() {
        // Empty epoch list (scheduler off / record rebuilt after annotation
        // loss): never classify, never touch the replica.
        let mut record = VolumeSyncRecord::initial(&replicas3());
        record.mark_stale("uuid-b", "leg failed", "t");
        let store = FakeStore::new(record);
        let rpc = FakeRpc::new("u");
        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();
        assert!(rpc.calls.lock().unwrap().is_empty());
        assert!(store.ops.lock().unwrap().is_empty());
        assert!(store.events.lock().unwrap().is_empty());
    }

    // ---- phase 4: reassembly admission -----------------------------------

    /// b is a warm standby caught up through epoch 5 (the newest), with the
    /// phase-3 revert bookkeeping in place.
    fn standby_b_record() -> VolumeSyncRecord {
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.replicas[1].reverted_to = Some(epoch("vol1", 4));
        record.mark_standby("uuid-b", &epoch("vol1", 5), "caught up", "t");
        record
    }

    fn stage_cfg() -> StageConfig {
        StageConfig {
            final_delta_budget: Duration::from_secs(3600),
            max_epochs_behind: 4,
            poll_interval: Duration::ZERO,
        }
    }

    fn attached_ac() -> Vec<String> {
        vec!["uuid-a".to_string(), "uuid-c".to_string()]
    }

    #[tokio::test]
    async fn admission_full_choreography() {
        let rpc = FakeRpc::new("uuid-b-v2");
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 5), &epoch("vol1", 6)],
        );
        let store = FakeStore::new(standby_b_record());

        let admitted = admit_standbys_at_stage(
            &rpc, &store, "vol1", "raid_vol1", &replicas3(), "node-c", &attached_ac(),
            &stage_cfg(),
        )
        .await;

        assert_eq!(
            admitted,
            vec![AdmittedStandby {
                lvol_uuid: "uuid-b".to_string(),
                node_name: "node-b".to_string(),
                bdev: expected_remote_base_bdev("vol1", 1),
                final_epoch: epoch("vol1", 6),
            }]
        );

        // Final epoch 6 cut on the attached survivors (a on node-a, c on
        // node-c), then the standby's head aligned at 6 on node-b.
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        let cut: Vec<(&str, &str)> = snaps
            .iter()
            .map(|(n, p)| (n.as_str(), p["params"]["lvol_name"].as_str().unwrap()))
            .collect();
        assert_eq!(
            cut,
            vec![("node-a", "uuid-a"), ("node-c", "uuid-c"), ("node-b", "lvs0/lvol-uuid-b")]
        );
        assert!(snaps
            .iter()
            .all(|(_, p)| p["params"]["snapshot_name"] == epoch("vol1", 6)));

        // The copy is one more base-inclusive chase session: the standby's
        // mark (5) re-copied, then the final epoch — from the non-consumer
        // source node-a.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        let srcs: Vec<&str> = copies
            .iter()
            .map(|(_, p)| p["params"]["src_lvol_name"].as_str().unwrap())
            .collect();
        assert_eq!(srcs, vec!["lvs0/epoch-vol1-5", "lvs0/epoch-vol1-6"]);
        assert!(copies.iter().all(|(node, _)| node == "node-a"));

        // Two fenced exports of the live head: first to the copy source,
        // then the admission flip to the consumer.
        let exports = rpc.exports.lock().unwrap().clone();
        assert_eq!(
            exports,
            vec![
                ("node-b".into(), "uuid-b-v2".into(), "vol1_1".into(), "node-a".into()),
                ("node-b".into(), "uuid-b-v2".into(), "vol1_1".into(), "node-c".into()),
            ]
        );
        // The copy controller on the source node is detached after the flip.
        let detaches = rpc.calls_of("bdev_nvme_detach_controller");
        assert_eq!(detaches.len(), 1);
        assert_eq!(detaches[0].0, "node-a");

        // Store ordering: the cut is recorded before in_sync.
        let ops = store.ops.lock().unwrap().clone();
        assert_eq!(
            ops,
            vec![
                format!("epoch_cut:{}", epoch("vol1", 6)),
                format!("in_sync:uuid-b:{}", epoch("vol1", 6)),
            ]
        );

        // Record end state: admitted in_sync at 6, write-virgin marker
        // cleared, live-uuid override kept, survivors stamped at 6.
        {
            let record = store.record.lock().unwrap();
            let rec = record.get("uuid-b").unwrap();
            assert_eq!(rec.sync_state, SyncState::InSync);
            assert_eq!(rec.last_epoch.as_deref(), Some(epoch("vol1", 6).as_str()));
            assert_eq!(rec.reverted_to, None);
            assert_eq!(rec.active_lvol_uuid.as_deref(), Some("uuid-b-v2"));
            assert_eq!(record.current_epoch.as_deref(), Some(epoch("vol1", 6).as_str()));
            assert_eq!(
                record.get("uuid-a").unwrap().last_epoch.as_deref(),
                Some(epoch("vol1", 6).as_str())
            );
        }
        let events = store.events.lock().unwrap().clone();
        assert_eq!(events, vec![("ReplicaAdmitted".to_string(), "Normal".to_string())]);

        // Idempotent restage: nothing left to admit.
        let again = admit_standbys_at_stage(
            &rpc, &store, "vol1", "raid_vol1", &replicas3(), "node-c", &attached_ac(),
            &stage_cfg(),
        )
        .await;
        assert!(again.is_empty());
        assert_eq!(store.events.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn admission_defers_when_raid_already_online() {
        // Restage with the raid alive: ensure_raid1_bdev will reuse it, and
        // a base cannot join an ONLINE raid without the stock blind rebuild.
        let rpc = FakeRpc::new("uuid-b-v2");
        rpc.raids.lock().unwrap().insert(
            "node-c".to_string(),
            vec![json!({ "name": "raid_vol1", "state": "online" })],
        );
        let store = FakeStore::new(standby_b_record());

        let admitted = admit_standbys_at_stage(
            &rpc, &store, "vol1", "raid_vol1", &replicas3(), "node-c", &attached_ac(),
            &stage_cfg(),
        )
        .await;

        assert!(admitted.is_empty());
        assert!(rpc.calls_of("bdev_lvol_snapshot").is_empty());
        assert!(store.ops.lock().unwrap().is_empty());
        assert_eq!(
            store.record.lock().unwrap().get("uuid-b").unwrap().sync_state,
            SyncState::Standby
        );
    }

    #[tokio::test]
    async fn admission_defers_on_excessive_lag() {
        let mut record = standby_b_record();
        record.mark_standby("uuid-b", &epoch("vol1", 4), "behind", "t"); // 1 behind
        let store = FakeStore::new(record);
        let rpc = FakeRpc::new("uuid-b-v2");
        let cfg = StageConfig { max_epochs_behind: 0, ..stage_cfg() };

        let admitted = admit_standbys_at_stage(
            &rpc, &store, "vol1", "raid_vol1", &replicas3(), "node-c", &attached_ac(), &cfg,
        )
        .await;

        assert!(admitted.is_empty());
        // Nothing cut, nothing recorded — the standby just keeps chasing.
        assert!(rpc.calls_of("bdev_lvol_snapshot").is_empty());
        assert!(store.ops.lock().unwrap().is_empty());
        let events = store.events.lock().unwrap().clone();
        assert_eq!(events, vec![("StandbyAdmissionDeferred".to_string(), "Warning".to_string())]);
    }

    #[tokio::test]
    async fn admission_defers_when_final_cut_fails_and_rolls_back() {
        let mut rpc = FakeRpc::new("uuid-b-v2");
        rpc.fail.insert(
            ("node-c".to_string(), "bdev_lvol_snapshot".to_string()),
            "connection refused".to_string(),
        );
        let store = FakeStore::new(standby_b_record());

        let admitted = admit_standbys_at_stage(
            &rpc, &store, "vol1", "raid_vol1", &replicas3(), "node-c", &attached_ac(),
            &stage_cfg(),
        )
        .await;

        assert!(admitted.is_empty());
        // The partial cut rolled back (node-a's snapshot deleted), nothing
        // recorded, replica still standby.
        let deletes = rpc.calls_of("bdev_lvol_delete");
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].0, "node-a");
        assert_eq!(deletes[0].1["params"]["name"], format!("lvs0/{}", epoch("vol1", 6)));
        assert!(store.ops.lock().unwrap().is_empty());
        assert_eq!(
            store.record.lock().unwrap().get("uuid-b").unwrap().sync_state,
            SyncState::Standby
        );
        let events = store.events.lock().unwrap().clone();
        assert_eq!(events, vec![("StandbyAdmissionDeferred".to_string(), "Warning".to_string())]);
    }

    #[tokio::test]
    async fn admission_budget_overrun_stages_degraded() {
        let rpc = FakeRpc::new("uuid-b-v2");
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 5), &epoch("vol1", 6)],
        );
        let store = FakeStore::new(standby_b_record());
        let cfg = StageConfig { final_delta_budget: Duration::ZERO, ..stage_cfg() };

        let admitted = admit_standbys_at_stage(
            &rpc, &store, "vol1", "raid_vol1", &replicas3(), "node-c", &attached_ac(), &cfg,
        )
        .await;

        assert!(admitted.is_empty());
        // The final epoch was cut and recorded — a normal common epoch the
        // background chase consumes — but no copy started and the replica
        // stays a standby (never in_sync).
        let ops = store.ops.lock().unwrap().clone();
        assert_eq!(ops, vec![format!("epoch_cut:{}", epoch("vol1", 6))]);
        assert!(rpc.calls_of("bdev_lvol_start_shallow_copy").is_empty());
        assert_eq!(
            store.record.lock().unwrap().get("uuid-b").unwrap().sync_state,
            SyncState::Standby
        );
        let events = store.events.lock().unwrap().clone();
        assert_eq!(events, vec![("StandbyAdmissionDeferred".to_string(), "Warning".to_string())]);
    }

    #[tokio::test]
    async fn admission_of_local_standby_uses_live_lvol_directly() {
        // The standby lives on the staging node itself: the head joins the
        // base list as the local live lvol — no loopback export, which would
        // put an NVMe-oF hop on the data path forever.
        let rpc = FakeRpc::new("uuid-b-v2");
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 5), &epoch("vol1", 6)],
        );
        let store = FakeStore::new(standby_b_record());

        let admitted = admit_standbys_at_stage(
            &rpc, &store, "vol1", "raid_vol1", &replicas3(), "node-b", &attached_ac(),
            &stage_cfg(),
        )
        .await;

        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].bdev, "uuid-b-v2");
        // Exactly one export — to the copy source — and no consumer flip.
        let exports = rpc.exports.lock().unwrap().clone();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].3, "node-a");
        assert!(rpc
            .calls_of("bdev_nvme_attach_controller")
            .iter()
            .all(|(node, _)| node == "node-a"));
        assert_eq!(
            store.record.lock().unwrap().get("uuid-b").unwrap().sync_state,
            SyncState::InSync
        );
    }

    // ---- phase 5b: user snapshots in the chain + tombstones ---------------

    #[tokio::test]
    async fn chase_replays_interleaved_user_snapshot() {
        // b is a standby at epoch 4; a user snapshot was cut between epochs
        // 4 and 5 while b was away. The chase must copy its delta (the §11
        // split hazard) AND materialize b's copy of it.
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.mark_standby("uuid-b", &epoch("vol1", 4), "caught up", "t");
        let store = FakeStore::new(record);

        let rpc = FakeRpc::new("uuid-b-v2");
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.bdevs.lock().unwrap().insert(
            ("node-a".to_string(), expected.clone()),
            json!({ "name": expected, "uuid": "uuid-b-v2" }),
        );
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), "snap_vol1_88", &epoch("vol1", 5)],
        );

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        // The user snapshot's delta is copied in chain position.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        let srcs: Vec<&str> = copies
            .iter()
            .map(|(_, p)| p["params"]["src_lvol_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            srcs,
            vec!["lvs0/epoch-vol1-4", "lvs0/snap_vol1_88", "lvs0/epoch-vol1-5"]
        );
        // The destination is aligned at the user snapshot AND the target.
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        let aligned: Vec<(&str, &str)> = snaps
            .iter()
            .map(|(n, p)| (n.as_str(), p["params"]["snapshot_name"].as_str().unwrap()))
            .collect();
        assert_eq!(
            aligned,
            vec![("node-b", "snap_vol1_88"), ("node-b", "epoch-vol1-5")]
        );
    }

    #[tokio::test]
    async fn tombstoned_snapshot_is_reaped_not_replayed() {
        // The user snapshot was deleted while b was away, but its copy could
        // not be removed everywhere — a tombstone remains. The reconcile
        // deletes the copies and clears the tombstone; the chase still
        // copies the snapshot's delta (correctness) but does NOT re-create
        // the deleted snapshot on the destination.
        let mut record = stale_b_record();
        record.replicas[1].active_lvol_uuid = Some("uuid-b-v2".to_string());
        record.mark_standby("uuid-b", &epoch("vol1", 4), "caught up", "t");
        assert!(record.add_snapshot_tombstone("vol1", "snap_vol1_88"));
        let store = FakeStore::new(record);

        let rpc = FakeRpc::new("uuid-b-v2");
        let expected = expected_remote_base_bdev("vol1", 1);
        rpc.bdevs.lock().unwrap().insert(
            ("node-a".to_string(), expected.clone()),
            json!({ "name": expected, "uuid": "uuid-b-v2" }),
        );
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), "snap_vol1_88", &epoch("vol1", 5)],
        );

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        // Reconcile fanned the delete to every replica node, by alias.
        let deletes = rpc.calls_of("bdev_lvol_delete");
        let delete_nodes: Vec<&str> = deletes.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(delete_nodes, vec!["node-a", "node-b", "node-c"]);
        assert!(deletes.iter().all(|(_, p)| p["params"]["name"] == "lvs0/snap_vol1_88"));
        // All confirmed absent → tombstone cleared.
        assert!(store
            .ops
            .lock()
            .unwrap()
            .contains(&"clear_tombstone:snap_vol1_88".to_string()));
        assert!(store.record.lock().unwrap().deleted_snapshots.is_empty());

        // The delta still copied; the deleted name was NOT re-aligned.
        let copies = rpc.calls_of("bdev_lvol_start_shallow_copy");
        assert_eq!(copies.len(), 3);
        let snaps = rpc.calls_of("bdev_lvol_snapshot");
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].1["params"]["snapshot_name"], epoch("vol1", 5));
    }

    #[tokio::test]
    async fn tombstone_survives_unreachable_replica() {
        let mut record = stale_b_record();
        assert!(record.add_snapshot_tombstone("vol1", "snap_vol1_88"));
        let store = FakeStore::new(record);

        let mut rpc = FakeRpc::new("u");
        // node-c cannot confirm deletion (unreachable / clone-pinned).
        rpc.fail.insert(
            ("node-c".to_string(), "bdev_lvol_delete".to_string()),
            "connection refused".to_string(),
        );
        // The stale replica's node is also down → no catch-up either.
        rpc.fail.insert(
            ("node-b".to_string(), "bdev_lvol_get_lvols".to_string()),
            "connection refused".to_string(),
        );

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        // Tombstone kept for the next cycle; no clear op recorded.
        assert!(!store.ops.lock().unwrap().iter().any(|o| o.starts_with("clear_tombstone")));
        assert_eq!(
            store.record.lock().unwrap().deleted_snapshots,
            vec!["snap_vol1_88".to_string()]
        );
    }

    #[tokio::test]
    async fn copy_failure_emits_event_and_keeps_replica_stale() {
        let rpc = {
            let mut rpc = FakeRpc::new("uuid-b-v2");
            rpc.lvols.insert(
                "node-b".to_string(),
                vec![
                    ("lvol-uuid-b".to_string(), "uuid-b".to_string()),
                    (epoch("vol1", 4), "s4".to_string()),
                ],
            );
            rpc.pending_copy_states
                .lock()
                .unwrap()
                .push("error:No space left on device".to_string());
            rpc
        };
        install_chain(
            &rpc, "node-a", "lvs0", "lvol-uuid-a",
            &[&epoch("vol1", 3), &epoch("vol1", 4), &epoch("vol1", 5)],
        );
        let store = FakeStore::new(stale_b_record());

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();

        let record = store.record.lock().unwrap();
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(rec.sync_state, SyncState::Stale);
        // The pin survives the failure — the catch-up is still pending and
        // its base must not be retired (§5 "active or pending").
        assert_eq!(record.retention_pin.as_deref(), Some(epoch("vol1", 4).as_str()));
        drop(record);
        let reasons: Vec<String> =
            store.events.lock().unwrap().iter().map(|(r, _)| r.clone()).collect();
        assert!(reasons.contains(&"ReplicaCatchupFailed".to_string()));
    }
}
