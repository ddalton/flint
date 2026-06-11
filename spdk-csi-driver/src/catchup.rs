// catchup.rs — replica catch-up orchestrator (warm standby).
// Phase 3 of docs/incremental-replica-rebuild.md §9.
//
// Detects a returned stale replica, brings it to a warm standby with the §5
// catch-up sequence — revert to its own base epoch, hygiene, re-export,
// attach on the source node, shallow-copy the epoch chain — and keeps every
// standby chasing new epochs as the scheduler (phase 2) cuts them. Nothing
// here admits a replica back to `in_sync`: that is phase 4's fenced
// final-delta at reassembly. A standby is persistent, thin, and trails the
// array by ≤ T_snap + one delta copy (§6), but it is NOT a raid member and
// never a read source.
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
// - **E_b-inclusive copy** (`chain_from`): every copy session — bulk AND
//   chase — starts at the destination's base epoch *inclusive*. For the bulk
//   this is §5 step 4's load-bearing rule. For the chase it additionally
//   makes switching copy sources safe: replica A's and B's cuts of the same
//   epoch are skewed, so a chain that continued exclusive-of-base from a new
//   source could permanently lose a write acked between the two sources'
//   base cuts. Re-copying the base's own delta from the current source
//   closes that window. Interrupted copies simply re-run: epoch snapshots
//   are immutable, so re-copying the same chain onto the same head converges.
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
//   pin clears when the replica reaches standby; the chase needs no pin —
//   snapshot-delete merges fold a retired epoch's delta into its retained
//   descendant, so copying the surviving chain still covers everything.
//
// Interactions left deliberately asymmetric in phase 3 (documented):
// - NodeStage still tries to export the *identity* uuid from
//   volumeAttributes; after a revert that bdev is gone, the export fails,
//   and the replica is (correctly) excluded from assembly and stays out of
//   the raid. The stage attempt's namespace-swap tramples the catch-up
//   export — the next chase cycle converges it back. Phase 4 makes staging
//   sync-state-aware and ends the churn.
// - A replica whose base aged out of retention (or that never shared an
//   epoch) needs the phase-5 thin-aware full build; it is marked with a
//   `ReplicaNeedsFullRebuild` event and left stale.
//
// Hosting mirrors the epoch scheduler: a background loop in the controller
// process, default-disabled via FLINT_CATCHUP until phase 4 lands. Long
// copies run as one task per volume (an in-flight set prevents pile-up);
// one stale replica per volume is processed per cycle (§10-5's
// two-stale-replicas question stays open).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::driver::{NvmeofConnectionInfo, SpdkCsiDriver};
use crate::epoch_scheduler::{is_already_exists, is_missing};
use crate::minimal_models::ReplicaInfo;
use crate::replica_sync::{
    self, epoch_seq, expected_remote_base_bdev, ReplicaSyncRecord, SyncState, VolumeSyncRecord,
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
    /// Transition to standby (or advance an existing standby's mark),
    /// clearing the catch-up's own retention pin if given.
    async fn record_standby(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        caught_up_through: &str,
        clear_pin: Option<&str>,
    ) -> Result<(), RpcError>;
    /// Update a replica's operator-facing reason without a state change.
    async fn record_reason(
        &self,
        volume_id: &str,
        replica_uuid: &str,
        reason: &str,
    ) -> Result<(), RpcError>;
    async fn emit(&self, volume_id: &str, event_type: &str, reason: &str, message: &str);
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
        clear_pin: Option<&str>,
    ) -> Result<(), RpcError> {
        let (uuid, through) = (replica_uuid.to_string(), caught_up_through.to_string());
        let pin = clear_pin.map(String::from);
        let now = replica_sync::now_rfc3339();
        replica_sync::update_sync_record(&self.client, volume_id, |r| {
            r.mark_standby(&uuid, &through, "caught up; chasing epochs", &now);
            if let Some(p) = &pin {
                r.clear_pin_if(p);
            }
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
}

impl Default for CatchupConfig {
    fn default() -> Self {
        CatchupConfig {
            enabled: false,
            t_back: Duration::from_secs(120),
            poll_interval: Duration::from_secs(2),
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

/// The copy chain from `base` (INCLUSIVE — load-bearing, §5 step 4) through
/// the newest retained epoch, in sequence order. If `base` itself has been
/// retired from the record, its delta was merged into its retained successor
/// by the snapshot-delete merge, so the surviving chain still covers it.
pub fn chain_from(volume_id: &str, record: &VolumeSyncRecord, base: &str) -> Vec<String> {
    let Some(base_seq) = epoch_seq(volume_id, base) else {
        return Vec::new();
    };
    let mut chain: Vec<(u64, String)> = record
        .epochs
        .iter()
        .filter_map(|e| epoch_seq(volume_id, &e.name).map(|seq| (seq, e.name.clone())))
        .filter(|(seq, _)| *seq >= base_seq)
        .collect();
    chain.sort_by_key(|(seq, _)| *seq);
    chain.into_iter().map(|(_, name)| name).collect()
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
    let in_sync: Vec<&ReplicaInfo> = record
        .replicas
        .iter()
        .filter(|rec| rec.sync_state == SyncState::InSync)
        .filter_map(|rec| replicas.iter().find(|ri| ri.lvol_uuid == rec.lvol_uuid))
        .collect();
    in_sync
        .iter()
        .find(|ri| consumer_node != Some(ri.node_name.as_str()))
        .or_else(|| in_sync.first())
        .copied()
}

async fn get_bdev(
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

async fn get_raids(rpc: &dyn CatchupRpc, node: &str) -> Result<Vec<Value>, RpcError> {
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
async fn list_lvol_names(
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
async fn revert_head(
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

async fn detach_controller(rpc: &dyn CatchupRpc, node: &str, controller: &str) {
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
/// destination.
async fn shallow_copy(
    rpc: &dyn CatchupRpc,
    src_node: &str,
    src_lvol: &str,
    dst_bdev: &str,
    poll_interval: Duration,
) -> Result<(), RpcError> {
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
async fn align_head(
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

/// Copy the base-inclusive chain onto the attached destination and align the
/// newest epoch. Shared by the bulk catch-up and the chase. Returns the
/// epoch the destination is now consistent at.
async fn copy_chain_and_align(
    rpc: &dyn CatchupRpc,
    volume_id: &str,
    record: &VolumeSyncRecord,
    src: &ReplicaInfo,
    dst: &ReplicaInfo,
    head_alias: &str,
    dst_bdev: &str,
    base: &str,
    cfg: &CatchupConfig,
) -> Result<String, RpcError> {
    let chain = chain_from(volume_id, record, base);
    if chain.is_empty() {
        return Err(format!("no copyable epochs from base {} for {}", base, volume_id).into());
    }
    for epoch in &chain {
        let src_lvol = format!("{}/{}", src.lvs_name, epoch);
        debug!(volume_id, epoch = %epoch, src = %src.node_name, dst = %dst.node_name, "[CATCHUP] Copying epoch delta");
        shallow_copy(rpc, &src.node_name, &src_lvol, dst_bdev, cfg.poll_interval).await?;
    }
    let newest = chain.last().expect("chain checked non-empty").clone();
    // Degenerate single-epoch chain where the newest IS the base: the
    // destination already holds a snapshot by this name (the revert source /
    // the chase mark); the head now also carries the source's copy of that
    // epoch's delta — consistent without a new snapshot (§5 correctness
    // note, the E_latest = E_b case).
    if newest != base {
        align_head(rpc, &dst.node_name, head_alias, &newest).await?;
    }
    Ok(newest)
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
    let Some(src) = pick_source(record, replicas, consumer_node) else {
        debug!(volume_id, "[CATCHUP] No in-sync source replica — cannot catch up");
        return Ok(());
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

    let Some(base) = select_base_epoch(volume_id, record, rec, &present, cfg.t_back) else {
        let reason =
            "full rebuild required: no shared epoch history within retention (phase 5)".to_string();
        if rec.reason.as_deref() != Some(reason.as_str()) {
            store.record_reason(volume_id, &rec.lvol_uuid, &reason).await?;
            store
                .emit(
                    volume_id,
                    "Warning",
                    "ReplicaNeedsFullRebuild",
                    &format!(
                        "Replica {} on {} has no shared epoch history within retention; \
                         it needs a thin-aware full build (phase 5) and stays stale",
                        rec.lvol_uuid, identity.node_name
                    ),
                )
                .await;
        }
        return Ok(());
    };

    // Pin BEFORE the revert: from here the catch-up's foundation must not be
    // retired (§5 retention pinning; survives orchestrator restarts).
    store.pin_retention(volume_id, &base).await?;

    let head_alias = format!("{}/{}", identity.lvs_name, identity.lvol_name);
    let live_uuid = if rec.reverted_to.as_deref() == Some(base.as_str()) {
        // Resume: the head is still a write-virgin clone of this base (it
        // has only ever received copy writes) — re-copying the chain onto it
        // converges without a re-revert.
        rec.live_lvol_uuid().to_string()
    } else {
        let base_alias = format!("{}/{}", identity.lvs_name, base);
        let new_uuid =
            revert_head(rpc, &identity.node_name, &head_alias, &identity.lvol_name, &base_alias)
                .await?;
        store.record_revert(volume_id, &rec.lvol_uuid, &base, &new_uuid).await?;
        store
            .emit(
                volume_id,
                "Normal",
                "ReplicaCatchupStarted",
                &format!(
                    "Catch-up of replica on {} started: head reverted to {}, copying {} → latest from {}",
                    identity.node_name, base, base, src.node_name
                ),
            )
            .await;
        new_uuid
    };

    let dst_bdev = ensure_dst_attached(
        rpc, volume_id, index, identity, &head_alias, &live_uuid, &src.node_name, raid_name,
    )
    .await?;

    let newest = copy_chain_and_align(
        rpc, volume_id, record, src, identity, &head_alias, &dst_bdev, &base, cfg,
    )
    .await?;

    store
        .record_standby(volume_id, &rec.lvol_uuid, &newest, Some(&base))
        .await?;
    store
        .emit(
            volume_id,
            "Normal",
            "ReplicaStandby",
            &format!(
                "Replica on {} is a warm standby: caught up through {} (chain from {}), chasing new epochs; \
                 it rejoins the raid at the next reassembly (phase 4)",
                identity.node_name, newest, base
            ),
        )
        .await;
    info!(volume_id, node = %identity.node_name, base = %base, through = %newest, "[CATCHUP] Replica caught up to warm standby");
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
    let Some(src) = pick_source(record, replicas, consumer_node) else {
        return Ok(());
    };

    let head_alias = format!("{}/{}", identity.lvs_name, identity.lvol_name);
    let live_uuid = rec.live_lvol_uuid().to_string();
    let dst_bdev = ensure_dst_attached(
        rpc, volume_id, index, identity, &head_alias, &live_uuid, &src.node_name, raid_name,
    )
    .await?;
    let newest = copy_chain_and_align(
        rpc, volume_id, record, src, identity, &head_alias, &dst_bdev, base, cfg,
    )
    .await?;
    store.record_standby(volume_id, &rec.lvol_uuid, &newest, None).await?;
    info!(volume_id, node = %identity.node_name, through = %newest, "[CATCHUP] Standby chased to latest epoch");
    Ok(())
}

/// One orchestrator pass over a single volume: chase every standby, then run
/// at most one stale replica's bulk catch-up. Per-replica failures are
/// contained (warned + evented) so one replica cannot starve the others.
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
    if record.epochs.is_empty() {
        // No common epochs yet (scheduler disabled or volume too new):
        // nothing to catch up from, and nothing to classify as full-build —
        // an empty record must never condemn a healable replica.
        return Ok(());
    }
    let raid_name = format!("raid_{}", volume_id);

    for rec in record.replicas.iter().filter(|r| r.sync_state == SyncState::Standby) {
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

    if let Some(rec) = record.replicas.iter().find(|r| r.sync_state == SyncState::Stale) {
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
/// other volume's chase — guarded by an in-flight set so a slow volume is
/// simply skipped by later ticks until its task finishes.
pub async fn run_catchup_orchestrator(driver: Arc<SpdkCsiDriver>, cfg: CatchupConfig) {
    info!(
        t_back_secs = cfg.t_back.as_secs(),
        "[CATCHUP] Replica catch-up orchestrator started"
    );
    let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Err(e) = orchestrator_tick(&driver, &cfg, &in_flight).await {
            warn!(error = %e, "[CATCHUP] Orchestrator tick failed (non-fatal)");
        }
    }
}

async fn orchestrator_tick(
    driver: &Arc<SpdkCsiDriver>,
    cfg: &CatchupConfig,
    in_flight: &Arc<Mutex<HashSet<String>>>,
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
        let replicas = match replica_sync::replicas_from_pv(&pv) {
            Ok(Some(r)) => r,
            Ok(None) => continue, // single replica
            Err(e) => {
                debug!(volume_id, error = %e, "[CATCHUP] Skipping PV with unreadable replica info");
                continue;
            }
        };

        {
            let mut guard = in_flight.lock().expect("in-flight lock poisoned");
            if !guard.insert(volume_id.clone()) {
                continue; // previous cycle's task still running
            }
        }
        let driver = driver.clone();
        let cfg = cfg.clone();
        let in_flight = in_flight.clone();
        let consumer = consumers.get(&volume_id).cloned();
        tokio::spawn(async move {
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
            in_flight.lock().expect("in-flight lock poisoned").remove(&volume_id);
        });
    }
    Ok(())
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
            clear_pin: Option<&str>,
        ) -> Result<(), RpcError> {
            let mut record = self.record.lock().unwrap();
            record.mark_standby(replica_uuid, caught_up_through, "caught up; chasing epochs", "t");
            if let Some(pin) = clear_pin {
                record.clear_pin_if(pin);
            }
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

        async fn emit(&self, _volume_id: &str, event_type: &str, reason: &str, _message: &str) {
            self.events
                .lock()
                .unwrap()
                .push((reason.to_string(), event_type.to_string()));
        }
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

    #[test]
    fn chain_from_is_base_inclusive_and_survives_retirement() {
        let record = stale_b_record(); // epochs 3, 4, 5
        assert_eq!(
            chain_from("vol1", &record, &epoch("vol1", 4)),
            vec![epoch("vol1", 4), epoch("vol1", 5)]
        );
        // Base retired from the record: the surviving chain still covers it
        // (snapshot-delete merged its delta into epoch 3's successor).
        assert_eq!(
            chain_from("vol1", &record, &epoch("vol1", 2)),
            vec![epoch("vol1", 3), epoch("vol1", 4), epoch("vol1", 5)]
        );
        // Foreign name: nothing to copy.
        assert!(chain_from("vol1", &record, "snap_user_1").is_empty());
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
        shallow_copy(&rpc, "node-a", "lvs0/epoch-vol1-4", "dst", Duration::ZERO)
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
        let err = shallow_copy(&rpc, "node-a", "lvs0/epoch-vol1-4", "dst", Duration::ZERO)
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
        // write-virgin marker, pin cleared.
        let record = store.record.lock().unwrap();
        let rec = record.get("uuid-b").unwrap();
        assert_eq!(rec.sync_state, SyncState::Standby);
        assert_eq!(rec.last_epoch.as_deref(), Some(epoch("vol1", 5).as_str()));
        assert_eq!(rec.active_lvol_uuid.as_deref(), Some("uuid-b-v2"));
        assert_eq!(rec.reverted_to.as_deref(), Some(epoch("vol1", 4).as_str()));
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
    async fn no_shared_history_classifies_full_build_once() {
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

        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();
        // Marked, evented, nothing pinned or reverted.
        let ops = store.ops.lock().unwrap().clone();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].starts_with("reason:full rebuild required"));
        let events = store.events.lock().unwrap().clone();
        assert_eq!(events, vec![("ReplicaNeedsFullRebuild".to_string(), "Warning".to_string())]);
        assert!(rpc.calls_of("bdev_lvol_clone").is_empty());

        // Second cycle: reason already recorded — no duplicate event.
        run_catchup_for_volume(&rpc, &store, "vol1", &replicas3(), None, &cfg())
            .await
            .unwrap();
        assert_eq!(store.events.lock().unwrap().len(), 1);
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
