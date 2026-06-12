// cutover.rs — reassembly cutover orchestrator (incremental-rebuild phase 4,
// §6 "cutover opportunities").
//
// A warm standby (phase 3) only rejoins the raid at the next assembly —
// which `admit_standbys_at_stage` handles — but nothing guarantees an
// assembly ever happens: an RWO pod can run for months, an RWX NFS server
// pod likewise. This module creates the opportunity, deliberately and
// verifiably:
//
// - **RWX volumes**: bounce the volume's `flint-nfs-server` pod. It is a
//   bare pod (no controller recreates it), so the bounce is
//   delete → wait for the pod to be gone AND the synthetic PV's
//   VolumeAttachment to detach → recreate from the sanitized original spec.
//   The detach wait closes the §6 same-node race: recreating before kubelet
//   unstages can land the pod on the same node where the staged volume is
//   reused — no NodeStage, no reassembly, clients ate a restart for
//   nothing. Honest scoping (§6): the shipped NFS server holds NFSv4 state
//   in memory, so a bounce costs clients the 90s grace-window recovery;
//   stateless I/O rides through.
// - **RWO volumes**: opt-in only, via the PV annotation
//   `flint.csi.storage.io/rejoin-bounce: "enabled"` — bouncing a workload
//   pod is an application restart and never the driver's call to make
//   unilaterally. The pods using the volume's claim are deleted; their
//   owning controller reschedules them.
//
// **Verification, not hope** (§6: "verify the outcome, don't assume it"):
// every bounce is tracked and judged on later cycles. Standbys that flipped
// to in_sync → `CutoverSucceeded`. Still standby after the cooldown →
// `CutoverIneffective` (same-node reuse, failed stage, or a deferred
// admission) and the volume becomes eligible for another attempt. The
// scheduling-hint escalation (cordon/anti-affinity) is deliberately not
// implemented here — an ineffective bounce is surfaced, not silently
// retried forever.
//
// A bounce is only planned when every standby is READY: lag ≤ max_lag
// epochs (chase converged), so the NodeStage final delta is small and the
// admission will not blow its budget. Default-disabled via FLINT_CUTOVER.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{PersistentVolume, Pod};
use k8s_openapi::api::storage::v1::VolumeAttachment;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use tracing::{debug, info, warn};

use crate::driver::SpdkCsiDriver;
use crate::replica_sync::{self, epoch_seq, SyncState, VolumeSyncRecord};

pub type RpcError = Box<dyn std::error::Error + Send + Sync>;

/// PV annotation opting an RWO volume into workload-pod bounces.
pub const REJOIN_BOUNCE_ANNOTATION: &str = "flint.csi.storage.io/rejoin-bounce";

/// PV annotation set by the node agent when a volume is ATTACHED to a node
/// but its raid bdev does not exist there — a dead data path the health
/// monitor cannot see (its stale predicate requires an online raid; phase-6
/// yield, bug 1). Value = the flagging node's name; only that node clears
/// it (raid reappeared, or the attachment left). Consumers: operators
/// (event + annotation), and the future in-place repair / bounce fallback.
pub const DATA_PATH_LOST_ANNOTATION: &str = "flint.csi.storage.io/data-path-lost";

/// One node-agent observation about one attached volume's data path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DataPathAction {
    /// Set the annotation + Warning event (confirmed lost).
    Flag,
    /// Remove this node's annotation + Normal event (healed or moved).
    Clear,
    /// Do nothing this tick.
    Hold,
}

/// Pure verdict for the node agent's data-path pass. `strikes_with_this`
/// counts consecutive raid-missing observations INCLUDING the current one;
/// `threshold` rides out an in-flight NodeStage, whose VA legitimately
/// precedes the raid by up to the stage-delta budget.
pub fn data_path_verdict(
    attached_here: bool,
    raid_present: bool,
    flagged_by_me: bool,
    strikes_with_this: u32,
    threshold: u32,
) -> DataPathAction {
    if !attached_here || raid_present {
        // Healed, or no longer this node's concern: clear our own flag.
        return if flagged_by_me { DataPathAction::Clear } else { DataPathAction::Hold };
    }
    if strikes_with_this >= threshold && !flagged_by_me {
        return DataPathAction::Flag;
    }
    DataPathAction::Hold
}

#[derive(Debug, Clone)]
pub struct CutoverConfig {
    /// FLINT_CUTOVER=enabled — default off.
    pub enabled: bool,
    /// Minimum wall clock between bounce attempts for one volume, and the
    /// window after which an unverified bounce is declared ineffective.
    /// FLINT_CUTOVER_COOLDOWN_SECS, default 900.
    pub cooldown: Duration,
    /// A standby may trail by at most this many epochs to be "ready" —
    /// beyond it the NodeStage final delta would likely blow its budget.
    /// FLINT_CUTOVER_MAX_LAG, default 1.
    pub max_lag: u64,
    /// How long the NFS bounce waits for the old pod to disappear and the
    /// synthetic PV to detach before recreating (closes the same-node
    /// reuse race). FLINT_CUTOVER_DETACH_TIMEOUT_SECS, default 120.
    pub detach_timeout: Duration,
}

impl Default for CutoverConfig {
    fn default() -> Self {
        CutoverConfig {
            enabled: false,
            cooldown: Duration::from_secs(900),
            max_lag: 1,
            detach_timeout: Duration::from_secs(120),
        }
    }
}

impl CutoverConfig {
    pub fn from_env() -> Self {
        let d = CutoverConfig::default();
        CutoverConfig {
            enabled: std::env::var("FLINT_CUTOVER")
                .map(|v| {
                    v.eq_ignore_ascii_case("enabled")
                        || v.eq_ignore_ascii_case("true")
                        || v == "1"
                })
                .unwrap_or(d.enabled),
            cooldown: std::env::var("FLINT_CUTOVER_COOLDOWN_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.cooldown),
            max_lag: std::env::var("FLINT_CUTOVER_MAX_LAG")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(d.max_lag),
            detach_timeout: std::env::var("FLINT_CUTOVER_DETACH_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.detach_timeout),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NfsPodRef {
    pub namespace: String,
    pub name: String,
    /// Only a PVC-backed NFS pod stages the volume — an emptyDir one has no
    /// raid to reassemble.
    pub pvc_backed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PodRef {
    pub namespace: String,
    pub name: String,
}

/// Everything the planner needs about one volume, gathered by the tick.
#[derive(Debug, Clone)]
pub struct VolumeCutoverView {
    pub volume_id: String,
    pub record: VolumeSyncRecord,
    /// Node consuming the volume per its VolumeAttachment (RWO path).
    pub consumer: Option<String>,
    /// The volume's `flint-nfs-{vol}` server pod, if any (RWX path).
    pub nfs_pod: Option<NfsPodRef>,
    /// PV annotation `flint.csi.storage.io/rejoin-bounce` == "enabled".
    pub rwo_bounce_enabled: bool,
    /// Workload pods mounting the volume's claim.
    pub workload_pods: Vec<PodRef>,
    /// The data-path-lost annotation is set (layer-1 detection flagged a
    /// dead consumer data path AND the layer-2 in-place repair failed —
    /// ublk frontend, aborted filesystem, or an unrecoverable export).
    /// Debounced by the loop before it reaches the planner.
    pub data_path_lost: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CutoverDecision {
    /// Delete + recreate the bare NFS server pod (RWX).
    BounceNfsPod,
    /// Delete the workload pods using the claim (RWO, opt-in).
    BounceWorkloadPods,
    /// Nothing to do; the reason is for operator-facing logs.
    Wait(&'static str),
}

/// Decide whether this volume gets a bounce now. Pure — the §6 conditions:
/// a standby exists, every standby has converged (lag ≤ max_lag, so the
/// NodeStage final delta is small), and the volume is actually consumed
/// (otherwise the next natural stage admits the standby for free).
pub fn plan_cutover(view: &VolumeCutoverView, cfg: &CutoverConfig) -> CutoverDecision {
    let vol = &view.volume_id;

    // Layer 3 (phase-6): a dead data path the in-place repair could not
    // fix. The bounce IS the remediation — a restage rebuilds the raid
    // from the in-sync replicas — so the standby/lag gates below do not
    // apply (there is nothing to admit, only a data path to rebuild).
    if view.data_path_lost {
        if let Some(nfs) = &view.nfs_pod {
            if nfs.pvc_backed {
                return CutoverDecision::BounceNfsPod;
            }
            return CutoverDecision::Wait("data path lost but NFS pod is not PVC-backed");
        }
        if view.consumer.is_some() {
            if !view.rwo_bounce_enabled {
                return CutoverDecision::Wait(
                    "data path lost and in-place repair failing; rejoin-bounce not enabled — \
                     operator must bounce the workload (or enable the annotation)",
                );
            }
            if view.workload_pods.is_empty() {
                return CutoverDecision::Wait("data path lost but no workload pods found");
            }
            return CutoverDecision::BounceWorkloadPods;
        }
        return CutoverDecision::Wait(
            "data path lost but volume not attached — the next stage rebuilds it",
        );
    }

    let standbys: Vec<&_> = view
        .record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::Standby)
        .collect();
    if standbys.is_empty() {
        return CutoverDecision::Wait("no standby replica");
    }
    let latest = view.record.latest_epoch_seq(vol);
    if latest == 0 {
        return CutoverDecision::Wait("no epoch history");
    }
    for rec in &standbys {
        let Some(seq) = rec.last_epoch.as_deref().and_then(|e| epoch_seq(vol, e)) else {
            return CutoverDecision::Wait("standby mark unreadable — not ready");
        };
        if latest.saturating_sub(seq) > cfg.max_lag {
            return CutoverDecision::Wait("standby lag above threshold — chase has not converged");
        }
    }

    if let Some(nfs) = &view.nfs_pod {
        if !nfs.pvc_backed {
            return CutoverDecision::Wait("NFS pod is not PVC-backed — nothing to reassemble");
        }
        return CutoverDecision::BounceNfsPod;
    }
    if view.consumer.is_some() {
        if !view.rwo_bounce_enabled {
            return CutoverDecision::Wait(
                "volume attached; rejoin-bounce annotation not enabled — waiting for a natural reassembly",
            );
        }
        if view.workload_pods.is_empty() {
            return CutoverDecision::Wait("no workload pods found using the claim");
        }
        return CutoverDecision::BounceWorkloadPods;
    }
    CutoverDecision::Wait("volume not attached — the next stage admits the standby naturally")
}

/// Strip the server-populated fields so a fetched pod can be re-created.
/// `node_name` is cleared on purpose: the scheduler must place the
/// replacement fresh (the spec's affinity still steers it to replica
/// nodes); a pinned node guarantees the same-node staged-volume reuse race.
pub fn sanitized_for_recreate(mut pod: Pod) -> Pod {
    pod.metadata.resource_version = None;
    pod.metadata.uid = None;
    pod.metadata.creation_timestamp = None;
    pod.metadata.deletion_timestamp = None;
    pod.metadata.deletion_grace_period_seconds = None;
    pod.metadata.managed_fields = None;
    pod.metadata.owner_references = None;
    pod.status = None;
    if let Some(spec) = pod.spec.as_mut() {
        spec.node_name = None;
    }
    pod
}

/// The subset of `uuids` whose replica is still a standby in `record` — a
/// bounce attempt is resolved once none remain (they were admitted in_sync,
/// or fell back to stale and need the catch-up again first).
pub fn standbys_still_pending(record: &VolumeSyncRecord, uuids: &[String]) -> Vec<String> {
    uuids
        .iter()
        .filter(|u| {
            record
                .get(u)
                .map(|r| r.sync_state == SyncState::Standby)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Cluster effects of a bounce, faked in unit tests.
#[async_trait]
pub trait CutoverOps: Sync {
    async fn get_pod(&self, namespace: &str, name: &str) -> Result<Option<Pod>, RpcError>;
    /// Delete a pod; absent is success (idempotent).
    async fn delete_pod(&self, namespace: &str, name: &str) -> Result<(), RpcError>;
    /// Wait (bounded) until the pod is gone and the PV's VolumeAttachment
    /// is detached. False on timeout.
    async fn await_detached(
        &self,
        namespace: &str,
        pod_name: &str,
        pv_name: &str,
        timeout: Duration,
    ) -> bool;
    async fn recreate_pod(&self, pod: Pod) -> Result<(), RpcError>;
    async fn emit(&self, volume_id: &str, event_type: &str, reason: &str, message: &str);
}

/// Execute a planned bounce. Returns whether a bounce was actually issued
/// (the caller starts the verification clock on true).
pub async fn execute_cutover(
    ops: &dyn CutoverOps,
    view: &VolumeCutoverView,
    decision: &CutoverDecision,
    cfg: &CutoverConfig,
) -> Result<bool, RpcError> {
    match decision {
        CutoverDecision::Wait(_) => Ok(false),
        CutoverDecision::BounceNfsPod => {
            let nfs = view
                .nfs_pod
                .as_ref()
                .ok_or("planned an NFS bounce without an NFS pod")?;
            // The NFS server is a bare pod — capture its spec BEFORE the
            // delete; nothing else can recreate it.
            let Some(pod) = ops.get_pod(&nfs.namespace, &nfs.name).await? else {
                return Err("NFS pod disappeared before the bounce".into());
            };
            let replacement = sanitized_for_recreate(pod);
            ops.delete_pod(&nfs.namespace, &nfs.name).await?;

            let pv_name = format!("flint-nfs-pv-{}", view.volume_id);
            if !ops
                .await_detached(&nfs.namespace, &nfs.name, &pv_name, cfg.detach_timeout)
                .await
            {
                warn!(
                    volume_id = %view.volume_id,
                    "[CUTOVER] Synthetic PV did not detach within the timeout — recreating anyway \
                     (a same-node reuse will surface as CutoverIneffective)"
                );
            }
            ops.recreate_pod(replacement).await?;
            ops.emit(
                &view.volume_id,
                "Normal",
                "CutoverStarted",
                &format!(
                    "NFS server pod {} bounced so the next stage reassembles the raid with the \
                     caught-up standby (NFSv4 clients recover via the grace window)",
                    nfs.name
                ),
            )
            .await;
            Ok(true)
        }
        CutoverDecision::BounceWorkloadPods => {
            for pod in &view.workload_pods {
                ops.delete_pod(&pod.namespace, &pod.name).await?;
            }
            ops.emit(
                &view.volume_id,
                "Normal",
                "CutoverStarted",
                &format!(
                    "{} workload pod(s) bounced (rejoin-bounce annotation) so the reschedule \
                     reassembles the raid with the caught-up standby",
                    view.workload_pods.len()
                ),
            )
            .await;
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------------------
// Kubernetes-backed ops + orchestrator loop (controller role)
// ---------------------------------------------------------------------------

pub struct KubeCutoverOps {
    pub client: kube::Client,
}

#[async_trait]
impl CutoverOps for KubeCutoverOps {
    async fn get_pod(&self, namespace: &str, name: &str) -> Result<Option<Pod>, RpcError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        match pods.get(name).await {
            Ok(pod) => Ok(Some(pod)),
            Err(e) if e.to_string().contains("NotFound") => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_pod(&self, namespace: &str, name: &str) -> Result<(), RpcError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        match pods.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("NotFound") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn await_detached(
        &self,
        namespace: &str,
        pod_name: &str,
        pv_name: &str,
        timeout: Duration,
    ) -> bool {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let vas: Api<VolumeAttachment> = Api::all(self.client.clone());
        let deadline = Instant::now() + timeout;
        loop {
            let pod_gone = match pods.get(pod_name).await {
                Ok(_) => false,
                Err(e) => e.to_string().contains("NotFound"),
            };
            let attached = vas
                .list(&ListParams::default())
                .await
                .map(|l| {
                    l.items.iter().any(|va| {
                        va.spec.source.persistent_volume_name.as_deref() == Some(pv_name)
                            && va.status.as_ref().map(|s| s.attached).unwrap_or(false)
                    })
                })
                .unwrap_or(true); // can't tell → keep waiting
            if pod_gone && !attached {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn recreate_pod(&self, pod: Pod) -> Result<(), RpcError> {
        let namespace = pod
            .metadata
            .namespace
            .clone()
            .ok_or("recreated pod has no namespace")?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
        pods.create(&PostParams::default(), &pod).await?;
        Ok(())
    }

    async fn emit(&self, volume_id: &str, event_type: &str, reason: &str, message: &str) {
        replica_sync::emit_pv_event(
            &self.client,
            "cutover-orchestrator",
            volume_id,
            event_type,
            reason,
            message,
        )
        .await;
    }
}

struct BounceAttempt {
    at: Instant,
    standbys: Vec<String>,
    /// Bounce issued for a dead data path (layer 3) rather than standby
    /// admission — judged by the data-path-lost annotation clearing, not
    /// by standby state.
    data_path: bool,
}

/// Background cutover loop (controller role, default-disabled).
pub async fn run_cutover_orchestrator(driver: Arc<SpdkCsiDriver>, cfg: CutoverConfig) {
    info!(
        cooldown_secs = cfg.cooldown.as_secs(),
        max_lag = cfg.max_lag,
        "[CUTOVER] Reassembly cutover orchestrator started"
    );
    let ops = KubeCutoverOps { client: driver.kube_client.clone() };
    let mut bounces: HashMap<String, BounceAttempt> = HashMap::new();
    // First-seen times for data-path-lost annotations: a 90s debounce so
    // a transient repair failure (replica node briefly down) doesn't cost
    // a workload bounce the next repair tick would have avoided.
    let mut data_path_seen: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Err(e) =
            cutover_tick(&driver, &ops, &cfg, &mut bounces, &mut data_path_seen).await
        {
            warn!(error = %e, "[CUTOVER] Tick failed (non-fatal)");
        }
    }
}

async fn cutover_tick(
    driver: &Arc<SpdkCsiDriver>,
    ops: &KubeCutoverOps,
    cfg: &CutoverConfig,
    bounces: &mut HashMap<String, BounceAttempt>,
    data_path_seen: &mut HashMap<String, Instant>,
) -> Result<(), RpcError> {
    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let vas: Api<VolumeAttachment> = Api::all(driver.kube_client.clone());

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
    let nfs_cfg = crate::rwx_nfs::NfsConfig::from_env();

    for pv in pvs.list(&ListParams::default()).await?.items {
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
        if !matches!(replica_sync::replicas_from_pv(&pv), Ok(Some(_))) {
            continue; // single replica (or unreadable)
        }
        let Some(record) = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(replica_sync::SYNC_STATE_ANNOTATION))
            .and_then(|s| VolumeSyncRecord::from_annotation(s).ok())
        else {
            continue;
        };

        let data_path_flagged = pv
            .metadata
            .annotations
            .as_ref()
            .map(|a| a.contains_key(DATA_PATH_LOST_ANNOTATION))
            .unwrap_or(false);

        // Judge a pending bounce before planning a new one.
        if let Some(attempt) = bounces.get(&volume_id) {
            // Data-path bounces are judged by the annotation: the node
            // agent clears it once the restage put the raid back.
            if attempt.data_path {
                if !data_path_flagged {
                    ops.emit(
                        &volume_id,
                        "Normal",
                        "CutoverSucceeded",
                        "Data path restored after the bounce (restage rebuilt the raid)",
                    )
                    .await;
                    bounces.remove(&volume_id);
                    data_path_seen.remove(&volume_id);
                } else if attempt.at.elapsed() >= cfg.cooldown {
                    ops.emit(
                        &volume_id,
                        "Warning",
                        "CutoverIneffective",
                        &format!(
                            "Bounce did not restore the data path within {}s — the restage may \
                             be failing (check NodeStage errors); eligible to retry",
                            cfg.cooldown.as_secs()
                        ),
                    )
                    .await;
                    bounces.remove(&volume_id);
                    continue;
                } else {
                    continue; // verification window still open
                }
                continue;
            }
            let pending = standbys_still_pending(&record, &attempt.standbys);
            if pending.is_empty() {
                let admitted = attempt
                    .standbys
                    .iter()
                    .filter(|u| {
                        record
                            .get(u)
                            .map(|r| r.sync_state == SyncState::InSync)
                            .unwrap_or(false)
                    })
                    .count();
                if admitted > 0 {
                    ops.emit(
                        &volume_id,
                        "Normal",
                        "CutoverSucceeded",
                        &format!(
                            "Reassembly after the bounce admitted {} standby replica(s) in_sync",
                            admitted
                        ),
                    )
                    .await;
                }
                bounces.remove(&volume_id);
            } else if attempt.at.elapsed() >= cfg.cooldown {
                ops.emit(
                    &volume_id,
                    "Warning",
                    "CutoverIneffective",
                    &format!(
                        "Bounce did not flip standby replica(s) {:?} to in_sync within {}s — \
                         same-node staged-volume reuse, a failed stage, or a deferred admission; \
                         eligible to retry",
                        pending,
                        cfg.cooldown.as_secs()
                    ),
                )
                .await;
                bounces.remove(&volume_id);
                continue; // retry no earlier than the next tick
            } else {
                continue; // verification window still open
            }
        }

        // Assemble the planner's view.
        let nfs_pod = match &nfs_cfg {
            Some(c) => ops
                .get_pod(&c.namespace, &format!("flint-nfs-{}", volume_id))
                .await
                .ok()
                .flatten()
                .map(|p| NfsPodRef {
                    namespace: c.namespace.clone(),
                    name: format!("flint-nfs-{}", volume_id),
                    pvc_backed: p
                        .spec
                        .as_ref()
                        .and_then(|s| s.volumes.as_ref())
                        .map(|vols| vols.iter().any(|v| v.persistent_volume_claim.is_some()))
                        .unwrap_or(false),
                }),
            None => None,
        };
        let rwo_bounce_enabled = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(REJOIN_BOUNCE_ANNOTATION))
            .map(|v| v.eq_ignore_ascii_case("enabled") || v == "true" || v == "1")
            .unwrap_or(false);
        let consumer = consumers.get(&volume_id).cloned();
        let workload_pods = if rwo_bounce_enabled && consumer.is_some() && nfs_pod.is_none() {
            match pv
                .spec
                .as_ref()
                .and_then(|s| s.claim_ref.as_ref())
                .and_then(|c| c.namespace.clone().zip(c.name.clone()))
            {
                Some((ns, claim)) => pods_using_claim(&driver.kube_client, &ns, &claim).await,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        // Debounce the data-path flag: 90s of continuous presence before
        // the planner sees it (a transient repair failure clears itself).
        let data_path_lost = if data_path_flagged {
            let first = data_path_seen.entry(volume_id.clone()).or_insert_with(Instant::now);
            first.elapsed() >= Duration::from_secs(90)
        } else {
            data_path_seen.remove(&volume_id);
            false
        };

        let view = VolumeCutoverView {
            volume_id: volume_id.clone(),
            record,
            consumer,
            nfs_pod,
            rwo_bounce_enabled,
            workload_pods,
            data_path_lost,
        };
        match plan_cutover(&view, cfg) {
            CutoverDecision::Wait(reason) => {
                debug!(volume_id, reason, "[CUTOVER] Waiting");
            }
            decision => {
                let standbys: Vec<String> = view
                    .record
                    .replicas
                    .iter()
                    .filter(|r| r.sync_state == SyncState::Standby)
                    .map(|r| r.lvol_uuid.clone())
                    .collect();
                info!(volume_id, ?decision, "[CUTOVER] Bouncing for reassembly");
                match execute_cutover(ops, &view, &decision, cfg).await {
                    Ok(true) => {
                        bounces.insert(
                            volume_id.clone(),
                            BounceAttempt {
                                at: Instant::now(),
                                standbys,
                                data_path: view.data_path_lost,
                            },
                        );
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(volume_id, error = %e, "[CUTOVER] Bounce failed");
                        ops.emit(
                            &volume_id,
                            "Warning",
                            "CutoverFailed",
                            &format!("Cutover bounce failed: {}", e),
                        )
                        .await;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Pods in `namespace` mounting `claim` — the RWO bounce targets.
async fn pods_using_claim(client: &kube::Client, namespace: &str, claim: &str) -> Vec<PodRef> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    match pods.list(&ListParams::default()).await {
        Ok(list) => list
            .items
            .into_iter()
            .filter(|p| {
                p.spec
                    .as_ref()
                    .and_then(|s| s.volumes.as_ref())
                    .map(|vols| {
                        vols.iter().any(|v| {
                            v.persistent_volume_claim
                                .as_ref()
                                .map(|c| c.claim_name == claim)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .filter_map(|p| {
                p.metadata.name.map(|name| PodRef {
                    namespace: namespace.to_string(),
                    name,
                })
            })
            .collect(),
        Err(e) => {
            warn!(namespace, claim, error = %e, "[CUTOVER] Pod listing failed");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minimal_models::ReplicaInfo;
    use crate::replica_sync::epoch_name;
    use std::sync::Mutex;

    #[test]
    fn data_path_verdict_flags_after_threshold_only() {
        // In-flight stage (strikes below threshold): hold.
        assert_eq!(data_path_verdict(true, false, false, 1, 3), DataPathAction::Hold);
        assert_eq!(data_path_verdict(true, false, false, 2, 3), DataPathAction::Hold);
        // Third consecutive miss: flag.
        assert_eq!(data_path_verdict(true, false, false, 3, 3), DataPathAction::Flag);
        // Already flagged by us: nothing to re-do.
        assert_eq!(data_path_verdict(true, false, true, 5, 3), DataPathAction::Hold);
    }

    #[test]
    fn data_path_lost_bounces_without_any_standby() {
        // Layer 3: the bounce is the remediation for a dead data path —
        // no standby is required (all replicas may be in_sync).
        let record = VolumeSyncRecord::initial(&[
            replica("node-a", "uuid-a"),
            replica("node-b", "uuid-b"),
        ]);
        let mut v = rwo_view(record.clone());
        v.data_path_lost = true;
        assert_eq!(plan_cutover(&v, &CutoverConfig::default()), CutoverDecision::BounceWorkloadPods);

        let mut n = nfs_view(record.clone());
        n.data_path_lost = true;
        assert_eq!(plan_cutover(&n, &CutoverConfig::default()), CutoverDecision::BounceNfsPod);

        // RWO without the opt-in: surfaced, not bounced.
        let mut v2 = rwo_view(record.clone());
        v2.data_path_lost = true;
        v2.rwo_bounce_enabled = false;
        assert!(matches!(
            plan_cutover(&v2, &CutoverConfig::default()),
            CutoverDecision::Wait(r) if r.contains("rejoin-bounce")
        ));

        // Not attached: nothing to bounce; the next stage rebuilds.
        let mut v3 = rwo_view(record);
        v3.data_path_lost = true;
        v3.consumer = None;
        v3.nfs_pod = None;
        assert!(matches!(
            plan_cutover(&v3, &CutoverConfig::default()),
            CutoverDecision::Wait(r) if r.contains("not attached")
        ));
    }

    #[test]
    fn data_path_verdict_clears_only_its_own_flag() {
        // Raid back: clear ours, hold if not ours.
        assert_eq!(data_path_verdict(true, true, true, 0, 3), DataPathAction::Clear);
        assert_eq!(data_path_verdict(true, true, false, 0, 3), DataPathAction::Hold);
        // Attachment left this node: same rule.
        assert_eq!(data_path_verdict(false, false, true, 0, 3), DataPathAction::Clear);
        assert_eq!(data_path_verdict(false, false, false, 0, 3), DataPathAction::Hold);
        // Healthy steady state: hold.
        assert_eq!(data_path_verdict(true, true, false, 0, 3), DataPathAction::Hold);
    }

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

    /// vol1 with epochs 1..=5 and replica b a standby caught up through 5.
    fn ready_record() -> VolumeSyncRecord {
        let mut record = VolumeSyncRecord::initial(&[
            replica("node-a", "uuid-a"),
            replica("node-b", "uuid-b"),
            replica("node-c", "uuid-c"),
        ]);
        let all = vec!["uuid-a".to_string(), "uuid-b".to_string(), "uuid-c".to_string()];
        for seq in 1..=5 {
            record.apply_epoch_cut(&epoch_name("vol1", seq), &all, "t");
        }
        record.mark_stale("uuid-b", "leg failed", "t");
        record.mark_standby("uuid-b", &epoch_name("vol1", 5), "caught up", "t");
        record
    }

    fn cfg() -> CutoverConfig {
        CutoverConfig {
            enabled: true,
            cooldown: Duration::from_secs(900),
            max_lag: 1,
            detach_timeout: Duration::from_secs(120),
        }
    }

    fn nfs_view(record: VolumeSyncRecord) -> VolumeCutoverView {
        VolumeCutoverView {
            volume_id: "vol1".to_string(),
            record,
            consumer: None,
            nfs_pod: Some(NfsPodRef {
                namespace: "flint-system".to_string(),
                name: "flint-nfs-vol1".to_string(),
                pvc_backed: true,
            }),
            rwo_bounce_enabled: false,
            workload_pods: vec![],
            data_path_lost: false,
        }
    }

    fn rwo_view(record: VolumeSyncRecord) -> VolumeCutoverView {
        VolumeCutoverView {
            volume_id: "vol1".to_string(),
            record,
            consumer: Some("node-a".to_string()),
            nfs_pod: None,
            rwo_bounce_enabled: true,
            workload_pods: vec![PodRef { namespace: "default".to_string(), name: "app-0".to_string() }],
            data_path_lost: false,
        }
    }

    // ---- planner ----------------------------------------------------------

    #[test]
    fn plan_requires_a_ready_standby() {
        // No standby at all.
        let mut record = ready_record();
        record.mark_in_sync("uuid-b", &epoch_name("vol1", 5), "x", "t");
        assert_eq!(
            plan_cutover(&nfs_view(record), &cfg()),
            CutoverDecision::Wait("no standby replica")
        );

        // Standby lagging beyond max_lag: the chase has not converged.
        let mut record = ready_record();
        record.mark_standby("uuid-b", &epoch_name("vol1", 3), "behind", "t");
        assert!(matches!(
            plan_cutover(&nfs_view(record), &cfg()),
            CutoverDecision::Wait(r) if r.contains("lag")
        ));

        // Unreadable mark: not ready.
        let mut record = ready_record();
        record.replicas[1].last_epoch = Some("garbage".to_string());
        assert!(matches!(
            plan_cutover(&nfs_view(record), &cfg()),
            CutoverDecision::Wait(r) if r.contains("unreadable")
        ));
    }

    #[test]
    fn plan_bounces_pvc_backed_nfs_pod() {
        assert_eq!(plan_cutover(&nfs_view(ready_record()), &cfg()), CutoverDecision::BounceNfsPod);

        // emptyDir-backed NFS pod has no raid to reassemble.
        let mut view = nfs_view(ready_record());
        view.nfs_pod.as_mut().unwrap().pvc_backed = false;
        assert!(matches!(plan_cutover(&view, &cfg()), CutoverDecision::Wait(_)));
    }

    #[test]
    fn plan_rwo_bounce_is_strictly_opt_in() {
        assert_eq!(
            plan_cutover(&rwo_view(ready_record()), &cfg()),
            CutoverDecision::BounceWorkloadPods
        );

        // Knob off: never bounce a workload uninvited.
        let mut view = rwo_view(ready_record());
        view.rwo_bounce_enabled = false;
        assert!(matches!(
            plan_cutover(&view, &cfg()),
            CutoverDecision::Wait(r) if r.contains("rejoin-bounce")
        ));

        // Detached volume: the next natural stage admits the standby free.
        let mut view = rwo_view(ready_record());
        view.consumer = None;
        view.workload_pods.clear();
        assert!(matches!(
            plan_cutover(&view, &cfg()),
            CutoverDecision::Wait(r) if r.contains("not attached")
        ));
    }

    // ---- bounce execution -------------------------------------------------

    struct FakeOps {
        pod: Mutex<Option<Pod>>,
        detached: bool,
        log: Mutex<Vec<String>>,
        recreated: Mutex<Option<Pod>>,
        events: Mutex<Vec<(String, String)>>,
    }

    impl FakeOps {
        fn with_nfs_pod() -> Self {
            let mut pod = Pod::default();
            pod.metadata.name = Some("flint-nfs-vol1".to_string());
            pod.metadata.namespace = Some("flint-system".to_string());
            pod.metadata.resource_version = Some("12345".to_string());
            pod.metadata.uid = Some("uid-xyz".to_string());
            pod.spec = Some(k8s_openapi::api::core::v1::PodSpec {
                node_name: Some("node-a".to_string()),
                ..Default::default()
            });
            pod.status = Some(Default::default());
            FakeOps {
                pod: Mutex::new(Some(pod)),
                detached: true,
                log: Mutex::new(Vec::new()),
                recreated: Mutex::new(None),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CutoverOps for FakeOps {
        async fn get_pod(&self, _ns: &str, name: &str) -> Result<Option<Pod>, RpcError> {
            self.log.lock().unwrap().push(format!("get:{}", name));
            Ok(self.pod.lock().unwrap().clone())
        }
        async fn delete_pod(&self, _ns: &str, name: &str) -> Result<(), RpcError> {
            self.log.lock().unwrap().push(format!("delete:{}", name));
            *self.pod.lock().unwrap() = None;
            Ok(())
        }
        async fn await_detached(
            &self,
            _ns: &str,
            pod: &str,
            pv: &str,
            _timeout: Duration,
        ) -> bool {
            self.log.lock().unwrap().push(format!("await:{}:{}", pod, pv));
            self.detached
        }
        async fn recreate_pod(&self, pod: Pod) -> Result<(), RpcError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("recreate:{}", pod.metadata.name.as_deref().unwrap_or("?")));
            *self.recreated.lock().unwrap() = Some(pod);
            Ok(())
        }
        async fn emit(&self, _volume_id: &str, event_type: &str, reason: &str, _message: &str) {
            self.events
                .lock()
                .unwrap()
                .push((reason.to_string(), event_type.to_string()));
        }
    }

    #[tokio::test]
    async fn nfs_bounce_captures_deletes_waits_and_recreates() {
        let ops = FakeOps::with_nfs_pod();
        let view = nfs_view(ready_record());
        let bounced = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        assert!(bounced);
        // Spec captured before the delete; detach awaited on the synthetic
        // PV; recreation last.
        assert_eq!(
            ops.log.lock().unwrap().clone(),
            vec![
                "get:flint-nfs-vol1",
                "delete:flint-nfs-vol1",
                "await:flint-nfs-vol1:flint-nfs-pv-vol1",
                "recreate:flint-nfs-vol1",
            ]
        );
        // The replacement is sanitized: no server fields, no pinned node.
        let recreated = ops.recreated.lock().unwrap().clone().unwrap();
        assert_eq!(recreated.metadata.resource_version, None);
        assert_eq!(recreated.metadata.uid, None);
        assert!(recreated.status.is_none());
        assert_eq!(recreated.spec.as_ref().unwrap().node_name, None);
        assert_eq!(
            ops.events.lock().unwrap().clone(),
            vec![("CutoverStarted".to_string(), "Normal".to_string())]
        );
    }

    #[tokio::test]
    async fn nfs_bounce_proceeds_when_detach_times_out() {
        // The recreation must not be held hostage by a stuck detach — an
        // ineffective bounce is caught by verification, a missing NFS pod
        // is an outage.
        let mut ops = FakeOps::with_nfs_pod();
        ops.detached = false;
        let view = nfs_view(ready_record());
        let bounced = execute_cutover(&ops, &view, &CutoverDecision::BounceNfsPod, &cfg())
            .await
            .unwrap();
        assert!(bounced);
        assert!(ops.recreated.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn workload_bounce_deletes_every_claim_pod() {
        let ops = FakeOps::with_nfs_pod();
        let mut view = rwo_view(ready_record());
        view.workload_pods.push(PodRef {
            namespace: "default".to_string(),
            name: "app-1".to_string(),
        });
        let bounced = execute_cutover(&ops, &view, &CutoverDecision::BounceWorkloadPods, &cfg())
            .await
            .unwrap();
        assert!(bounced);
        assert_eq!(
            ops.log.lock().unwrap().clone(),
            vec!["delete:app-0", "delete:app-1"]
        );
        assert_eq!(
            ops.events.lock().unwrap().clone(),
            vec![("CutoverStarted".to_string(), "Normal".to_string())]
        );
    }

    // ---- verification helpers ---------------------------------------------

    #[test]
    fn pending_standbys_resolve_on_admission_or_derail() {
        let uuids = vec!["uuid-b".to_string()];
        let record = ready_record();
        assert_eq!(standbys_still_pending(&record, &uuids), vec!["uuid-b".to_string()]);

        // Admitted in_sync: resolved.
        let mut record = ready_record();
        record.mark_in_sync("uuid-b", &epoch_name("vol1", 6), "admitted", "t");
        assert!(standbys_still_pending(&record, &uuids).is_empty());

        // Fell back to stale (failed again): also resolved — the catch-up
        // must run before another bounce makes sense.
        let mut record = ready_record();
        record.mark_stale("uuid-b", "lost again", "t");
        assert!(standbys_still_pending(&record, &uuids).is_empty());
    }

    #[test]
    fn sanitize_clears_all_server_populated_fields() {
        let mut pod = Pod::default();
        pod.metadata.name = Some("p".to_string());
        pod.metadata.resource_version = Some("1".to_string());
        pod.metadata.uid = Some("u".to_string());
        pod.metadata.owner_references = Some(vec![]);
        pod.spec = Some(k8s_openapi::api::core::v1::PodSpec {
            node_name: Some("node-x".to_string()),
            ..Default::default()
        });
        pod.status = Some(Default::default());
        let clean = sanitized_for_recreate(pod);
        assert_eq!(clean.metadata.name.as_deref(), Some("p"));
        assert_eq!(clean.metadata.resource_version, None);
        assert_eq!(clean.metadata.uid, None);
        assert_eq!(clean.metadata.owner_references, None);
        assert_eq!(clean.spec.unwrap().node_name, None);
        assert!(clean.status.is_none());
    }
}
