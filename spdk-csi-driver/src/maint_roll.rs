//! The maintenance roller — the csi-node roll landmine fix
//! (docs/maintenance-drain-csi-node-roll.md; formal model: the maintenance
//! tranche of formal/FlintReplication.tla).
//!
//! A DaemonSet roll restarts spdk-tgt on every node — a PLANNED data-plane
//! outage per node that the raid cannot distinguish from a failure. On
//! RollingUpdate semantics (k8s default), P4's dead-target detection faults
//! each rolled serving leg out, and rolling the next node on pod-readiness
//! while the previous leg is still un-readmitted takes the volume to
//! serving = {} with ZERO real failures — TLC finds it in 5 steps
//! (FlintReplicationRollUnfenced.cfg). The fix is three separately
//! necessary guards, each with a TLC mutation that rediscovers its loss:
//!
//!   FENCE    drain-before-restart: gracefully remove each serving leg the
//!            node hosts (one record CAS: stale-mark = writer-set exit +
//!            leased suppression mark, THEN the raid-level remove), so the
//!            restart only ever touches non-serving legs. P4 stays
//!            always-on — no maintenance awareness, nothing to hide a real
//!            reclaim behind.
//!   BARRIER  the next node waits for FULL readmission (every replica
//!            in_sync, no markers, no marks) — NOT pod-readiness, which is
//!            all the DS controller knows (FlintReplicationRollBarrier).
//!   LEASE    the suppression mark self-clears: readers treat an expired
//!            deadline as absent, so a dead roller's drain readmits
//!            (FlintReplicationRollLease's parked-standby lasso).
//!
//! The roller is level-triggered: each tick observes (DS revision, pods,
//! records) and performs ONE step — drain, delete, or clear — so a
//! controller restart resumes the campaign from the observable state
//! alone. It runs only when the DS updateStrategy is OnDelete (the chart
//! sets it with maintenance.drainRoll.enabled): under RollingUpdate the
//! DS controller deletes pods on its own schedule and a concurrent drain
//! would be uncoordinated.
//!
//! The LOCAL half of the landmine — consumers co-located with the rolled
//! tgt lose their staged device — is below this module's abstraction
//! (kernel/ublk continuity; see the design doc). The roller warns when a
//! node it is about to roll hosts consumers.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::{ControllerRevision, DaemonSet};
use k8s_openapi::api::core::v1::{Pod, PersistentVolume};
use k8s_openapi::api::storage::v1::VolumeAttachment;
use kube::api::{Api, ListParams};
use serde_json::json;
use tracing::{info, warn};

use crate::catchup::{CatchupRpc, CatchupStore, RpcError};
use crate::driver::SpdkCsiDriver;
use crate::replica_sync::{self, SyncState};

/// Pod label of the csi-node DaemonSet's pods (chart node.yaml).
pub const NODE_POD_LABEL: &str = "app=flint-csi-node";
/// The label the DS controller stamps on both pods and ControllerRevisions.
pub const REVISION_HASH_LABEL: &str = "controller-revision-hash";

// ---------------------------------------------------------------------------
// Kill switch — the S2 pattern: env-free core + thin wrapper.
// ---------------------------------------------------------------------------

/// Maintenance-drain kill switch — pure core (the F43 lesson: env-free
/// logic + a thin wrapper, so tests never mutate process env). Default ON:
/// only an explicit disabled/false/0 turns the roller off. The roller is
/// additionally inert unless the DS updateStrategy is OnDelete, so the
/// code-side default is safe on charts that never opted in.
pub fn maint_drain(setting: Option<&str>) -> bool {
    !setting
        .map(|v| {
            v.eq_ignore_ascii_case("disabled") || v.eq_ignore_ascii_case("false") || v == "0"
        })
        .unwrap_or(false)
}

/// Thin env wrapper over [`maint_drain`].
pub fn maint_drain_enabled() -> bool {
    maint_drain(std::env::var("FLINT_MAINT_DRAIN").ok().as_deref())
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MaintRollConfig {
    pub enabled: bool,
    /// Namespace of the csi-node DaemonSet.
    pub namespace: String,
    /// DaemonSet name (chart: flint-csi-node).
    pub ds_name: String,
    /// Suppression-mark lease. Renewed every tick by a live roller; an
    /// expired mark reads as absent (readmission resumes). Must exceed the
    /// tick by a comfortable margin.
    pub suppress_ttl: Duration,
    pub tick: Duration,
}

impl Default for MaintRollConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            namespace: "default".to_string(),
            ds_name: "flint-csi-node".to_string(),
            suppress_ttl: Duration::from_secs(900),
            tick: Duration::from_secs(60),
        }
    }
}

impl MaintRollConfig {
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            enabled: maint_drain_enabled(),
            namespace: std::env::var("FLINT_NAMESPACE").unwrap_or(d.namespace),
            ds_name: std::env::var("FLINT_MAINT_DS_NAME").unwrap_or(d.ds_name),
            suppress_ttl: env_secs("FLINT_MAINT_SUPPRESS_TTL_SECS", d.suppress_ttl),
            tick: env_secs("FLINT_MAINT_TICK_SECS", d.tick),
        }
    }
}

fn env_secs(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// The pure planner — one step per tick, mirroring the model's actions.
// ---------------------------------------------------------------------------

/// One csi-node pod as the planner sees it.
#[derive(Debug, Clone)]
pub struct RollPodView {
    pub pod_name: String,
    pub node_name: String,
    /// Pod's controller-revision-hash matches the DS's latest revision.
    pub current_rev: bool,
    pub ready: bool,
}

/// Everything the planner needs, observed by the tick.
#[derive(Debug, Clone)]
pub struct RollView {
    /// DS updateStrategy is OnDelete — the roller owns pod deletion.
    pub on_delete: bool,
    pub pods: Vec<RollPodView>,
    /// Nodes holding at least one LIVE suppression mark (expired marks
    /// read as absent — the lease).
    pub marked_nodes: Vec<String>,
    /// The barrier input: every multi-replica volume fully redundant —
    /// all replicas in_sync, no hot-rejoin markers, no live marks.
    pub fully_redundant: bool,
    /// Operator-facing note when the barrier fails (which volume, why).
    pub barrier_note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RollStep {
    /// Nothing to do: no pending pods, no marks.
    Idle,
    /// MaintClear: `node`'s restart completed (pod current + Ready) — lift
    /// its suppression marks; the normal hot-rejoin machinery readmits.
    ClearMarks { node: String },
    /// RollStart: `node` is drained (marks live) and its pod still runs
    /// the old revision — delete it; the DS controller recreates it at
    /// the current template (OnDelete).
    DeletePod { pod: String, node: String },
    /// MaintDrain: the barrier holds and `node` awaits its roll — drain
    /// every serving leg it hosts (the marks make this observable; the
    /// pod delete happens on a later tick).
    Drain { node: String },
    /// Blocked, with the reason (barrier not satisfied, pod mid-recreate,
    /// strategy not OnDelete). The campaign resumes when the world moves.
    Blocked { reason: String },
}

/// Decide this tick's single step. Pure — the tick owns observation and
/// execution. One node in maintenance at a time is an invariant the
/// planner PRESERVES (it never drains while marks exist) and the drain
/// step ESTABLISHES (marks before delete).
pub fn plan_roll(view: &RollView) -> RollStep {
    if !view.on_delete {
        return RollStep::Blocked {
            reason: "csi-node DS updateStrategy is not OnDelete — the roller stands down \
                     (set maintenance.drainRoll.enabled in the chart)"
                .to_string(),
        };
    }
    // A marked node is mid-roll: finish it before anything else.
    if let Some(node) = view.marked_nodes.first() {
        let pod = view.pods.iter().find(|p| &p.node_name == node);
        return match pod {
            Some(p) if p.current_rev && p.ready => RollStep::ClearMarks { node: node.clone() },
            Some(p) if !p.current_rev => RollStep::DeletePod {
                pod: p.pod_name.clone(),
                node: node.clone(),
            },
            Some(_) => RollStep::Blocked {
                reason: format!("waiting for {node}'s recreated pod to become Ready"),
            },
            None => RollStep::Blocked {
                reason: format!("waiting for the DS controller to recreate {node}'s pod"),
            },
        };
    }
    // No marks: pick the next pending node (stale revision), behind the
    // barrier. Sorted for a deterministic campaign order.
    let mut pending: Vec<&RollPodView> = view.pods.iter().filter(|p| !p.current_rev).collect();
    pending.sort_by(|a, b| a.node_name.cmp(&b.node_name));
    let Some(next) = pending.first() else {
        return RollStep::Idle;
    };
    if !view.fully_redundant {
        return RollStep::Blocked {
            reason: format!(
                "barrier: not fully redundant ({}) — the roll waits (readmitted, not pod-ready)",
                view.barrier_note
            ),
        };
    }
    RollStep::Drain {
        node: next.node_name.clone(),
    }
}

// ---------------------------------------------------------------------------
// Kube observation ops (mockable, the KubeCutoverOps pattern).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DsState {
    pub on_delete: bool,
    /// Latest ControllerRevision's hash; None when no revision exists yet.
    pub latest_revision: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PodInfo {
    pub name: String,
    pub node: String,
    pub revision_hash: Option<String>,
    pub ready: bool,
}

#[async_trait]
pub trait RollOps: Send + Sync {
    /// None = the DaemonSet does not exist (nothing to roll).
    async fn ds_state(&self) -> Result<Option<DsState>, RpcError>;
    async fn list_node_pods(&self) -> Result<Vec<PodInfo>, RpcError>;
    async fn delete_pod(&self, name: &str) -> Result<(), RpcError>;
}

pub struct KubeRollOps {
    pub client: kube::Client,
    pub namespace: String,
    pub ds_name: String,
}

#[async_trait]
impl RollOps for KubeRollOps {
    async fn ds_state(&self) -> Result<Option<DsState>, RpcError> {
        let api: Api<DaemonSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(ds) = api.get_opt(&self.ds_name).await? else {
            return Ok(None);
        };
        let on_delete = ds
            .spec
            .as_ref()
            .and_then(|s| s.update_strategy.as_ref())
            .and_then(|u| u.type_.as_deref())
            == Some("OnDelete");
        let ds_uid = ds.metadata.uid.clone().unwrap_or_default();
        // The DS controller names revisions <ds>-<hash> and labels them
        // with controller-revision-hash; latest = highest .revision owned
        // by this DS.
        let revs: Api<ControllerRevision> = Api::namespaced(self.client.clone(), &self.namespace);
        let latest_revision = revs
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|cr| {
                cr.metadata
                    .owner_references
                    .as_ref()
                    .map(|refs| refs.iter().any(|r| r.uid == ds_uid))
                    .unwrap_or(false)
            })
            .max_by_key(|cr| cr.revision)
            .and_then(|cr| {
                cr.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(REVISION_HASH_LABEL).cloned())
                    .or_else(|| {
                        cr.metadata
                            .name
                            .as_deref()
                            .and_then(|n| n.strip_prefix(&format!("{}-", self.ds_name)))
                            .map(String::from)
                    })
            });
        Ok(Some(DsState {
            on_delete,
            latest_revision,
        }))
    }

    async fn list_node_pods(&self) -> Result<Vec<PodInfo>, RpcError> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods = api.list(&ListParams::default().labels(NODE_POD_LABEL)).await?;
        Ok(pods
            .items
            .into_iter()
            .filter_map(|p| {
                let name = p.metadata.name.clone()?;
                let node = p.spec.as_ref().and_then(|s| s.node_name.clone())?;
                let revision_hash = p
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(REVISION_HASH_LABEL).cloned());
                let ready = p
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .map(|cs| {
                        cs.iter()
                            .any(|c| c.type_ == "Ready" && c.status == "True")
                    })
                    .unwrap_or(false);
                Some(PodInfo {
                    name,
                    node,
                    revision_hash,
                    ready,
                })
            })
            .collect())
    }

    async fn delete_pod(&self, name: &str) -> Result<(), RpcError> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        api.delete(name, &Default::default()).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The drain: record round first, then the raid-level graceful remove.
// ---------------------------------------------------------------------------

/// One volume's leg on the node being drained, as the tick resolved it.
#[derive(Debug, Clone)]
pub struct DrainTarget {
    pub volume_id: String,
    /// The record-home volume id (RWX resolves to the parent PV's record).
    pub replica_uuid: String,
    /// Index of the leg in the identity list — the deterministic
    /// per-replica export NQN (and so the initiator controller / base
    /// bdev name on the consumer) derives from it.
    pub replica_index: usize,
    /// Live head uuid (post-revert override aware) for base matching.
    pub live_uuid: String,
    /// The node hosting the serving raid.
    pub consumer: String,
    /// The serving raid's name in its staged domain.
    pub raid_name: String,
}

/// Gracefully remove `target`'s leg: ONE record round (stale-mark =
/// writer-set exit + leased suppression mark), then
/// `bdev_raid_remove_base_bdev` on the serving raid. Record-first is
/// load-bearing (the mirror of assembly's record-before-writes): a crash
/// between the two leaves a leg the record already excludes — safe, and
/// the next tick re-drives the remove. The remove is idempotent: a base
/// already absent from the raid is success.
pub async fn drain_leg(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    target: &DrainTarget,
    deadline_rfc3339: &str,
) -> Result<(), RpcError> {
    store
        .record_maint_drain(&target.volume_id, &target.replica_uuid, deadline_rfc3339)
        .await?;
    // Verify the record round actually ARMED the drain (the mutator
    // refuses a leg mid hot-rejoin window and the last in-sync leg;
    // record_maint_drain reports Ok either way). Removing a leg the
    // record still counts as a writer is the exact ordering hazard
    // record-first exists to prevent — never outrun a refusing record.
    let armed = store
        .load(&target.volume_id)
        .await?
        .map(|r| {
            r.replicas.iter().any(|rec| {
                rec.lvol_uuid == target.replica_uuid
                    && rec.sync_state == SyncState::Stale
                    && rec.maint_drain.is_some()
            })
        })
        .unwrap_or(false);
    if !armed {
        return Err(format!(
            "record refused the drain of {} on {} (hot-rejoin window in progress, \
             or it is the last in-sync leg)",
            target.replica_uuid, target.volume_id
        )
        .into());
    }

    // Find the leg's configured base in the serving raid: by the
    // deterministic remote base name (the canonical
    // expected_remote_base_bdev) or by uuid (covers a local-alias base
    // and post-revert heads — an lvol bdev's name is its uuid).
    let sid = crate::identity::StorageId::of_handle(&target.volume_id);
    let remote_base = replica_sync::expected_remote_base_bdev(&sid, target.replica_index);
    let raids = crate::catchup::get_raids(rpc, &target.consumer).await?;
    let Some(raid) = raids.iter().find(|r| {
        r.get("name").and_then(|n| n.as_str()) == Some(target.raid_name.as_str())
    }) else {
        // No raid on the consumer (already torn down / mid-transition):
        // nothing to remove — the record round did the work.
        return Ok(());
    };
    let bases: Vec<&serde_json::Value> = raid
        .get("base_bdevs_list")
        .and_then(|b| b.as_array())
        .map(|v| v.iter().collect())
        .unwrap_or_default();
    let is_target = |b: &&serde_json::Value| {
        [b.get("name"), b.get("uuid")].iter().any(|v| {
            v.and_then(|x| x.as_str())
                .map(|s| s == remote_base || s == target.live_uuid)
                .unwrap_or(false)
        })
    };
    let configured = |b: &&serde_json::Value| {
        b.get("is_configured").and_then(|c| c.as_bool()).unwrap_or(false)
    };
    let Some(base) = bases.iter().find(|b| is_target(b) && configured(b)) else {
        return Ok(()); // already out — idempotent re-drive
    };
    // Data-plane belt (mirrors drain_for_maintenance's record-level belt):
    // never remove the last configured base.
    if bases.iter().filter(|b| configured(b)).count() < 2 {
        return Err(format!(
            "refusing to remove the last configured base of {} on {}",
            target.raid_name, target.consumer
        )
        .into());
    }
    let base_name = base
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or("configured base has no name")?;
    let payload = json!({
        "method": "bdev_raid_remove_base_bdev",
        "params": { "name": base_name }
    });
    rpc.spdk_rpc(&target.consumer, &payload).await?;
    info!(
        volume_id = %target.volume_id,
        node = %target.consumer,
        base = %base_name,
        "[MAINT] Leg drained from serving raid (planned roll)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The orchestrator loop.
// ---------------------------------------------------------------------------

/// Per-volume maintenance facts the tick gathered from the records.
struct VolumeMaint {
    volume_id: String,
    /// (replica_uuid, node_name) of every LIVE mark.
    marked: Vec<(String, String)>,
    /// Barrier obstruction, if any (which replica, why).
    obstruction: Option<String>,
    /// Drainable in-sync legs by node: (replica_uuid, index, live_uuid).
    insync_by_node: std::collections::HashMap<String, (String, usize, String)>,
    consumer: Option<String>,
    raid_name: String,
}

pub async fn run_maint_roll_orchestrator(driver: Arc<SpdkCsiDriver>, cfg: MaintRollConfig) {
    info!(
        namespace = %cfg.namespace,
        ds = %cfg.ds_name,
        suppress_ttl_secs = cfg.suppress_ttl.as_secs(),
        "[MAINT] Maintenance roller started"
    );
    let ops = KubeRollOps {
        client: driver.kube_client.clone(),
        namespace: cfg.namespace.clone(),
        ds_name: cfg.ds_name.clone(),
    };
    let store = crate::catchup::KubeStore {
        client: driver.kube_client.clone(),
    };
    let mut tick = tokio::time::interval(cfg.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if !crate::orchestrator_lease::is_leader() {
            continue;
        }
        if let Err(e) = maint_roll_tick(driver.as_ref(), &ops, &store, &cfg).await {
            warn!(error = %e, "[MAINT] tick failed — retrying next cycle");
        }
    }
}

pub async fn maint_roll_tick(
    driver: &SpdkCsiDriver,
    ops: &dyn RollOps,
    store: &dyn CatchupStore,
    cfg: &MaintRollConfig,
) -> Result<(), RpcError> {
    let Some(ds) = ops.ds_state().await? else {
        return Ok(()); // no csi-node DS in this namespace
    };
    let pods_raw = ops.list_node_pods().await?;
    let pods: Vec<RollPodView> = pods_raw
        .iter()
        .map(|p| RollPodView {
            pod_name: p.name.clone(),
            node_name: p.node.clone(),
            current_rev: match (&ds.latest_revision, &p.revision_hash) {
                (Some(latest), Some(hash)) => latest == hash,
                // Unknown revisions read as CURRENT: never roll on
                // missing evidence.
                _ => true,
            },
            ready: p.ready,
        })
        .collect();

    // Nothing pending and nothing marked ⇒ skip the volume scan entirely
    // (the steady-state cost of the roller is one DS + one pod list).
    let volumes = gather_volume_maint(driver).await?;
    let now = replica_sync::now_rfc3339();
    let mut marked_nodes: Vec<String> = volumes
        .iter()
        .flat_map(|v| v.marked.iter().map(|(_, node)| node.clone()))
        .collect();
    marked_nodes.sort();
    marked_nodes.dedup();
    let obstruction = volumes.iter().find_map(|v| {
        v.obstruction
            .as_ref()
            .map(|o| format!("{}: {}", v.volume_id, o))
    });
    let view = RollView {
        on_delete: ds.on_delete,
        pods,
        marked_nodes,
        fully_redundant: obstruction.is_none(),
        barrier_note: obstruction.unwrap_or_default(),
    };
    let step = plan_roll(&view);
    match &step {
        RollStep::Idle => {}
        RollStep::Blocked { reason } => {
            if !view.marked_nodes.is_empty() || view.pods.iter().any(|p| !p.current_rev) {
                info!(reason = %reason, "[MAINT] roll blocked");
            }
            renew_marks(store, &volumes, cfg, &now).await;
        }
        RollStep::ClearMarks { node } => {
            info!(node = %node, "[MAINT] node restart complete — lifting suppression marks (MaintClear)");
            for v in &volumes {
                for (uuid, mark_node) in &v.marked {
                    if mark_node == node {
                        store.record_maint_mark(&v.volume_id, uuid, None).await?;
                        store
                            .emit(
                                &v.volume_id,
                                "Normal",
                                "MaintenanceMarkCleared",
                                &format!(
                                    "csi-node roll on {node} complete; leg readmission resumes"
                                ),
                            )
                            .await;
                    }
                }
            }
        }
        RollStep::DeletePod { pod, node } => {
            info!(pod = %pod, node = %node, "[MAINT] node drained — deleting csi-node pod (RollStart)");
            renew_marks(store, &volumes, cfg, &now).await;
            ops.delete_pod(pod).await?;
        }
        RollStep::Drain { node } => {
            let deadline =
                crate::freshness_gate::deadline_from(&now, cfg.suppress_ttl.as_secs());
            let mut drained = 0usize;
            for v in &volumes {
                let Some((uuid, index, live_uuid)) = v.insync_by_node.get(node) else {
                    continue;
                };
                let Some(consumer) = v.consumer.as_deref() else {
                    continue; // unattached: no serving raid to drain from
                };
                if consumer == node {
                    // The serving raid itself lives on the rolling node —
                    // the LOCAL half (staged-device continuity), out of
                    // this roller's reach. Warn and leave the volume
                    // alone; the roll will still restart this tgt.
                    store
                        .emit(
                            &v.volume_id,
                            "Warning",
                            "MaintenanceLocalConsumer",
                            &format!(
                                "csi-node roll will restart the tgt under this volume's \
                                 serving raid on {node}; staged-device continuity is not \
                                 yet covered (docs/maintenance-drain-csi-node-roll.md)"
                            ),
                        )
                        .await;
                    continue;
                }
                let Some(_claim) = crate::volume_claims::global()
                    .try_claim(&v.volume_id, crate::volume_claims::OP_MAINT_DRAIN)
                else {
                    crate::volume_claims::log_claim_skip(
                        &v.volume_id,
                        crate::volume_claims::OP_MAINT_DRAIN,
                        crate::volume_claims::global(),
                    );
                    continue; // next tick retries; marks stay consistent
                };
                let target = DrainTarget {
                    volume_id: v.volume_id.clone(),
                    replica_uuid: uuid.clone(),
                    replica_index: *index,
                    live_uuid: live_uuid.clone(),
                    consumer: consumer.to_string(),
                    raid_name: v.raid_name.clone(),
                };
                drain_leg(driver, store, &target, &deadline).await?;
                store
                    .emit(
                        &v.volume_id,
                        "Normal",
                        "MaintenanceDrain",
                        &format!(
                            "leg on {node} drained ahead of the csi-node roll \
                             (lease until {deadline})"
                        ),
                    )
                    .await;
                drained += 1;
            }
            info!(node = %node, drained, "[MAINT] drain pass complete (pod delete on a later tick)");
        }
    }
    Ok(())
}

/// A live roller renews every mark it is holding open — the lease. A dead
/// roller renews nothing and the marks expire into readmission.
async fn renew_marks(
    store: &dyn CatchupStore,
    volumes: &[VolumeMaint],
    cfg: &MaintRollConfig,
    now: &str,
) {
    let deadline = crate::freshness_gate::deadline_from(now, cfg.suppress_ttl.as_secs());
    for v in volumes {
        for (uuid, _) in &v.marked {
            if let Err(e) = store
                .record_maint_mark(&v.volume_id, uuid, Some(&deadline))
                .await
            {
                warn!(volume_id = %v.volume_id, error = %e, "[MAINT] mark renewal failed");
            }
        }
    }
}

/// Observe every flint multi-replica volume's maintenance-relevant state.
async fn gather_volume_maint(driver: &SpdkCsiDriver) -> Result<Vec<VolumeMaint>, RpcError> {
    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let vas: Api<VolumeAttachment> = Api::all(driver.kube_client.clone());
    let consumers: std::collections::HashMap<String, String> = vas
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter_map(|va| {
            let pv = va.spec.source.persistent_volume_name.clone()?;
            let node = va.spec.node_name.clone();
            va.status
                .as_ref()
                .filter(|s| s.attached)
                .map(|_| (pv, node))
        })
        .collect();
    let now = replica_sync::now_rfc3339();
    let mut out = Vec::new();
    for pv in pvs.list(&ListParams::default()).await?.items {
        let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) else {
            continue;
        };
        if csi.driver != "flint.csi.storage.io" {
            continue;
        }
        // Synthetic RWX backing PVs: the record lives on the parent.
        if replica_sync::nfs_backing_parent(&pv).is_some() {
            continue;
        }
        let Ok(Some(replicas)) = replica_sync::replicas_from_pv(&pv) else {
            continue; // single-replica or unreadable
        };
        let volume_id = csi.volume_handle.clone();
        let Some(record) = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(replica_sync::SYNC_STATE_ANNOTATION))
            .and_then(|s| replica_sync::VolumeSyncRecord::from_annotation(s).ok())
        else {
            continue;
        };
        let rwx = replica_sync::is_rwx_pv(&pv);
        let (consumer, staged_domain) = if rwx {
            let sid = crate::identity::StorageId::of_handle(&volume_id);
            (
                consumers
                    .get(&crate::identity::backing_pv_name(sid.as_str()))
                    .cloned(),
                crate::identity::StagedDomain::NfsBacking,
            )
        } else {
            (consumers.get(&volume_id).cloned(), crate::identity::StagedDomain::User)
        };
        let sid = crate::identity::StorageId::of_handle(&volume_id);
        let raid_name = crate::identity::raid_name(&staged_domain.handle_for(&sid));

        let mut marked = Vec::new();
        let mut obstruction = None;
        let mut insync_by_node = std::collections::HashMap::new();
        for rec in &record.replicas {
            if rec.maint_drain_live(&now) {
                marked.push((rec.lvol_uuid.clone(), rec.node_name.clone()));
            }
            if rec.hot_rejoin.is_some() {
                obstruction
                    .get_or_insert_with(|| format!("replica {} mid hot-rejoin", rec.lvol_uuid));
            }
            match rec.sync_state {
                SyncState::InSync => {
                    if let Some(idx) =
                        replicas.iter().position(|ri| ri.lvol_uuid == rec.lvol_uuid)
                    {
                        insync_by_node.insert(
                            rec.node_name.clone(),
                            (rec.lvol_uuid.clone(), idx, rec.live_lvol_uuid().to_string()),
                        );
                    }
                }
                _ => {
                    obstruction.get_or_insert_with(|| {
                        format!(
                            "replica {} on {} is {}",
                            rec.lvol_uuid,
                            rec.node_name,
                            rec.sync_state.as_str()
                        )
                    });
                }
            }
        }
        out.push(VolumeMaint {
            volume_id,
            marked,
            obstruction,
            insync_by_node,
            consumer,
            raid_name,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maint_drain_kill_switch_defaults_on_opt_out_semantics() {
        assert!(maint_drain(None));
        assert!(maint_drain(Some("enabled")));
        assert!(maint_drain(Some("garbage")));
        assert!(!maint_drain(Some("disabled")));
        assert!(!maint_drain(Some("FALSE")));
        assert!(!maint_drain(Some("0")));
    }

    fn pod(node: &str, current: bool, ready: bool) -> RollPodView {
        RollPodView {
            pod_name: format!("flint-csi-node-{node}"),
            node_name: node.to_string(),
            current_rev: current,
            ready,
        }
    }

    fn view(pods: Vec<RollPodView>, marked: &[&str], redundant: bool) -> RollView {
        RollView {
            on_delete: true,
            pods,
            marked_nodes: marked.iter().map(|s| s.to_string()).collect(),
            fully_redundant: redundant,
            barrier_note: if redundant { String::new() } else { "vol1: replica x is stale".into() },
        }
    }

    #[test]
    fn plan_roll_stands_down_without_on_delete() {
        let mut v = view(vec![pod("a", false, true)], &[], true);
        v.on_delete = false;
        assert!(matches!(plan_roll(&v), RollStep::Blocked { .. }));
    }

    #[test]
    fn plan_roll_idle_when_campaign_done() {
        let v = view(vec![pod("a", true, true), pod("b", true, true)], &[], true);
        assert_eq!(plan_roll(&v), RollStep::Idle);
    }

    #[test]
    fn plan_roll_drains_first_pending_node_behind_the_barrier() {
        // Deterministic order: node "a" before "b" even listed reversed.
        let v = view(vec![pod("b", false, true), pod("a", false, true)], &[], true);
        assert_eq!(plan_roll(&v), RollStep::Drain { node: "a".into() });
    }

    #[test]
    fn plan_roll_barrier_blocks_drain_not_pod_readiness() {
        // Every pod Ready — exactly the pod-level view k8s acts on — yet
        // the barrier (readmitted, not pod-ready) blocks: the TLC
        // RollBarrier counterexample as a unit test.
        let v = view(vec![pod("a", true, true), pod("b", false, true)], &[], false);
        match plan_roll(&v) {
            RollStep::Blocked { reason } => assert!(reason.contains("barrier")),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn plan_roll_finishes_marked_node_before_any_drain() {
        // Marked node's pod still on the old revision: delete it (the
        // drain already happened) — never drain a second node meanwhile.
        let v = view(vec![pod("a", false, true), pod("b", false, true)], &["a"], true);
        assert_eq!(
            plan_roll(&v),
            RollStep::DeletePod { pod: "flint-csi-node-a".into(), node: "a".into() }
        );
    }

    #[test]
    fn plan_roll_clears_marks_when_pod_current_and_ready() {
        let v = view(vec![pod("a", true, true), pod("b", false, true)], &["a"], true);
        assert_eq!(plan_roll(&v), RollStep::ClearMarks { node: "a".into() });
    }

    #[test]
    fn plan_roll_waits_while_recreated_pod_not_ready() {
        let v = view(vec![pod("a", true, false), pod("b", false, true)], &["a"], true);
        assert!(matches!(plan_roll(&v), RollStep::Blocked { .. }));
        // Pod gone entirely (mid-recreate): also wait.
        let v2 = view(vec![pod("b", false, true)], &["a"], true);
        assert!(matches!(plan_roll(&v2), RollStep::Blocked { .. }));
    }
}
