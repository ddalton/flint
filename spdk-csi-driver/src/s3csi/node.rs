//! The CSI Identity + Node services of `s3.csi.chert.us` (design §3.4, §3.5,
//! §3.7).
//!
//! `NodePublishVolume` is the whole product: parse → resolve → authorize
//! → worker → credential → mount → bind. Every refusal names what the
//! tenant must change, because the message becomes their `FailedMount`
//! event. Only `Unavailable` is retryable; kubelet retries with backoff
//! (500 ms → 2m2s) and its 2-minute per-call deadline bounds each
//! attempt, so no single wait here exceeds ~45 s.
//!
//! Republish (`requiresRepublish: true`, ~every 60-90 s per pod) hits the
//! same RPC with the target already mounted: refresh the credential,
//! probe liveness, and return OK — NEVER fail a republish on a mounted
//! target, and never remount (kubernetes#121271).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Event, ObjectReference, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::api::{Api, PostParams};
use kube::Client;
use tonic::{Request, Response, Status};

use crate::csi;
use crate::csi::volume_capability::AccessType;

use super::attrs::{self, PublishRequest};
use super::creds::{self, BrokerClient, Creds, ExchangeError, Materialized, Registration};
use super::fuse::{self, Launch};
use super::policy::{Access, CredentialMode};
use super::quota;
use super::resolve::{self, Refusal, Resolved};
use super::state::{self, SharedMount, TenantRef, VolumeState, STATE_VERSION};
use super::worker::{self, WaitOutcome, WorkerInputs, WorkerWatch};
use super::DRIVER_NAME;

/// Waits inside one RPC. Their sum stays under kubelet's 2-minute call
/// deadline with headroom for the API round-trips.
const WORKER_RUNNING_WAIT: Duration = Duration::from_secs(45);
const FUSE_READY_WAIT: Duration = Duration::from_secs(40);
const LAUNCH_REPLY_WAIT: Duration = Duration::from_secs(20);
/// Lean: how long one publish attempt waits for the checkout marker
/// before answering `Unavailable` (kubelet retries; the syncer keeps
/// going in between).
const MARKER_WAIT: Duration = Duration::from_secs(60);
/// Lean: how long one unpublish attempt waits for the drained syncer.
const DRAIN_WAIT: Duration = Duration::from_secs(90);
/// Where the syncer sees the tree (its hostPath mount).
const SYNCER_ROOT: &str = "/workspace";
const DEFAULT_OWNER: u32 = 65534;
/// How long the broker's backend name (its `/v1/status`) is believed
/// before it is read again for a sharing decision.
const BROKER_BACKEND_TTL: Duration = Duration::from_secs(600);
/// How long a publish that found a DEAD shared mounter waits for the
/// old worker to leave the API before giving kubelet a retry.
const DEAD_SHARED_WORKER_WAIT: Duration = Duration::from_secs(20);

pub struct Config {
    pub node_name: String,
    pub node_uid: Option<String>,
    pub worker_namespace: String,
    pub passthrough_image: String,
    pub lean_image: Option<String>,
    pub worker_resources: Option<ResourceRequirements>,
    /// Resources for a SHARED read-only mounter
    /// (FLINT_S3CSI_SHARED_WORKER_RESOURCES): one worker serving every
    /// read-only member of a class on the node, so one memory limit and
    /// one `--memory-target` for all of them. Unset ⇒ `worker_resources`.
    pub shared_worker_resources: Option<ResourceRequirements>,
    pub priority_class: Option<String>,
    /// How long a worker's preStop hook holds it open waiting for its
    /// volume to be released (FLINT_S3CSI_PRESTOP_SECS). This is the
    /// ordering mechanism for drain and node shutdown; there is no
    /// PodDisruptionBudget for workers.
    pub prestop_secs: Option<i64>,
    /// F72: how long an unpublish waits for a passthrough mounter to exit
    /// ON ITS OWN after its source mount is detached, before the preStop
    /// marker and the delete — the SIGTERM — go out
    /// (FLINT_S3CSI_QUIESCE_SECS, default 30; 0 disables the wait). See
    /// `teardown_mounter`.
    pub quiesce_secs: u64,
    /// Enforce `sizeLimitGib` on a lean tree with a loop-mounted image
    /// (FLINT_S3CSI_QUOTA, default OFF). Off ⇒ the tree is a plain
    /// directory on the node's root filesystem, at native speed, and the
    /// CR's declared ceiling is enforced by nothing. On costs speed:
    /// buffered writes 0.57x and fsync-each writes ~0.6x a plain
    /// directory, measured on EC2
    /// (docs/plans/flint-lean-loop-image-perf-2026-09-15.md).
    pub quota: bool,
    pub broker: Option<BrokerClient>,
    /// Lifetime asked of the broker per exchange.
    pub creds_lifetime_secs: u64,
    pub region: String,
    pub kubelet_root: PathBuf,
    pub plugin_root: PathBuf,
    pub comm_size: String,
    pub scratch_size: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("node_name", &self.node_name)
            .field("worker_namespace", &self.worker_namespace)
            .field("passthrough_image", &self.passthrough_image)
            .field("lean_image", &self.lean_image)
            .field("broker", &self.broker.as_ref().map(|b| b.base_url().to_string()))
            .field("creds_lifetime_secs", &self.creds_lifetime_secs)
            .field("plugin_root", &self.plugin_root)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let need = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).ok_or_else(|| format!("{k} is unset"));
        let opt = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let resources = |k: &str| -> Result<Option<ResourceRequirements>, String> {
            match opt(k) {
                Some(j) if j.trim() != "{}" => Ok(Some(
                    serde_json::from_str(&j).map_err(|e| format!("{k} is not ResourceRequirements JSON: {e}"))?,
                )),
                _ => Ok(None),
            }
        };
        let worker_resources = resources("FLINT_S3CSI_WORKER_RESOURCES")?;
        let shared_worker_resources = resources("FLINT_S3CSI_SHARED_WORKER_RESOURCES")?;
        Ok(Self {
            node_name: need("FLINT_S3CSI_NODE_NAME")?,
            node_uid: None,
            worker_namespace: opt("FLINT_S3CSI_WORKER_NAMESPACE").unwrap_or_else(|| "flint-workers".into()),
            // No default: this names the image every worker runs.
            passthrough_image: need("FLINT_S3CSI_PASSTHROUGH_IMAGE")?,
            lean_image: opt("FLINT_S3CSI_LEAN_IMAGE"),
            worker_resources,
            shared_worker_resources,
            priority_class: opt("FLINT_S3CSI_WORKER_PRIORITY_CLASS"),
            prestop_secs: opt("FLINT_S3CSI_PRESTOP_SECS").and_then(|v| v.parse().ok()),
            quiesce_secs: opt("FLINT_S3CSI_QUIESCE_SECS").and_then(|v| v.parse().ok()).unwrap_or(30),
            quota: opt("FLINT_S3CSI_QUOTA").map(|v| v == "true").unwrap_or(false),
            broker: BrokerClient::from_env()?,
            creds_lifetime_secs: opt("FLINT_S3CSI_CREDS_LIFETIME_SECS").and_then(|v| v.parse().ok()).unwrap_or(900),
            region: opt("FLINT_S3CSI_REGION").unwrap_or_else(|| "us-east-1".into()),
            kubelet_root: super::kubelet_root(),
            plugin_root: super::plugin_root(),
            comm_size: opt("FLINT_S3CSI_COMM_SIZE").unwrap_or_else(|| "16Mi".into()),
            scratch_size: opt("FLINT_S3CSI_SCRATCH_SIZE").unwrap_or_else(|| "1Gi".into()),
        })
    }
}

pub struct S3Node {
    cfg: Arc<Config>,
    client: Client,
    /// This node's workers, from one watch: the republish liveness
    /// signal at zero API cost in steady state (`worker::WorkerWatch`).
    workers: WorkerWatch,
    /// One mutating owner per volume on this node; the acquire wait is
    /// bounded below kubelet's deadline so a stuck holder surfaces as
    /// `Unavailable`, not a consumed deadline.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The broker's backend (`/v1/status`), read once per
    /// `BROKER_BACKEND_TTL` for the sharing decision: only a backend
    /// whose read grant is a function of the CR alone may be shared.
    broker_backend: tokio::sync::Mutex<Option<(Instant, String)>>,
}

/// The lock key of a shared mount's class: taken AFTER the volume's own,
/// by every join, leave, rebind and teardown of that class.
fn share_lock_key(hash: &str) -> String {
    format!("share:{hash}")
}

impl S3Node {
    /// Must run inside the tokio runtime: the worker watch is spawned here.
    pub fn new(cfg: Config, client: Client) -> Self {
        let workers = WorkerWatch::start(client.clone(), &cfg.worker_namespace, &cfg.node_name);
        Self {
            cfg: Arc::new(cfg),
            client,
            workers,
            locks: Mutex::new(HashMap::new()),
            broker_backend: tokio::sync::Mutex::new(None),
        }
    }

    /// The broker's backend name, cached for `BROKER_BACKEND_TTL`. `None`
    /// when there is no broker or its status could not be read — which
    /// the sharing decision treats as "unknown: a mounter per pod", and
    /// which is retried on the next publish rather than cached.
    async fn broker_backend(&self) -> Option<String> {
        let broker = self.cfg.broker.as_ref()?;
        let mut cache = self.broker_backend.lock().await;
        if let Some((at, backend)) = cache.as_ref() {
            if at.elapsed() < BROKER_BACKEND_TTL {
                return Some(backend.clone());
            }
        }
        match broker.status().await {
            Ok(v) => {
                let backend = v.get("backend").and_then(|b| b.as_str()).map(String::from);
                if let Some(b) = &backend {
                    *cache = Some((Instant::now(), b.clone()));
                }
                backend
            }
            Err(e) => {
                tracing::warn!("broker status (for the sharing decision): {e}");
                None
            }
        }
    }

    async fn lock(&self, vid: &str) -> Result<tokio::sync::OwnedMutexGuard<()>, Status> {
        let entry = {
            let mut m = self.locks.lock().unwrap();
            m.entry(vid.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
        };
        tokio::time::timeout(Duration::from_secs(30), entry.lock_owned())
            .await
            .map_err(|_| Status::unavailable(format!("volume {vid}: another operation holds it; retry")))
    }

    /// Re-adopt the node's live volumes after a restart: nothing to redo
    /// (the FUSE fd lives in the worker; the binds are in the host mount
    /// table), only to log and to drop state whose worker is gone.
    pub async fn adopt_existing(&self) {
        for (dir, st) in VolumeState::list(&self.cfg.plugin_root) {
            let alive = worker::is_running(&self.client, &st.worker_namespace, &st.worker_name).await.unwrap_or(false);
            tracing::info!(
                volume = %st.volume_id, phase = %st.phase, worker = %st.worker_name, worker_running = alive,
                "adopted volume state at {}", dir.display()
            );
            match adopt_action(&st) {
                AdoptAction::Keep => {}
                AdoptAction::KeepCheckingOut => tracing::info!(
                    volume = %st.volume_id,
                    "checkout in progress at startup: the syncer is its own pod and kubelet's retry resumes the wait"
                ),
                // A publish that never finished: kubelet will retry it, and
                // the retry starts clean.
                AdoptAction::Cleanup => self.cleanup(&dir, &st, "unfinished publish found at startup").await,
            }
        }
        self.adopt_shared().await;
    }

    /// The shared records, after the volumes: drop members whose volume
    /// state is gone (an unpublish that ran while the plugin was down, or
    /// a member the loop above just cleaned up), and bring down a mounter
    /// nothing holds. A mounter with members is kept whether or not it is
    /// running: a dead one strands them exactly as a per-pod dead mounter
    /// does, and the next member's publish replaces it.
    async fn adopt_shared(&self) {
        for (sdir, mut sm) in SharedMount::list(&self.cfg.plugin_root) {
            let before = sm.members.len();
            sm.members.retain(|vid| {
                VolumeState::load(&state::volume_dir(&self.cfg.plugin_root, vid))
                    .ok()
                    .flatten()
                    .is_some_and(|m| m.shared.as_deref() == Some(sm.hash.as_str()))
            });
            if sm.members.len() != before {
                tracing::info!(shared = %sm.hash, dropped = before - sm.members.len(), "shared members whose volume state is gone were dropped");
                if let Err(e) = sm.save(&sdir) {
                    tracing::warn!(shared = %sm.hash, "shared record: {e}");
                }
            }
            let alive = worker::is_running(&self.client, &sm.worker_namespace, &sm.worker_name).await.unwrap_or(false);
            tracing::info!(
                shared = %sm.hash, cr = %sm.cr, members = sm.members.len(), worker = %sm.worker_name, worker_running = alive,
                "adopted shared mount at {}", sdir.display()
            );
            if sm.members.is_empty() {
                tracing::warn!(shared = %sm.hash, "no member holds the shared mount; bringing it down");
                if let Err(e) = self.teardown_shared(&sdir, &sm).await {
                    tracing::warn!(shared = %sm.hash, "shared teardown at startup: {}", e.message());
                }
            }
        }
    }

    /// A publish that cannot finish: tear down everything it made so
    /// kubelet's retry starts clean, and hand the status back.
    async fn fail(&self, dir: &Path, st: &VolumeState, status: Status) -> Status {
        self.cleanup(dir, st, status.message()).await;
        status
    }

    async fn cleanup(&self, dir: &Path, st: &VolumeState, why: &str) {
        // A PUBLISHED lean volume's tree is the tenant's live data: an
        // agent is writing into it right now, and the only correct time
        // to remove it is NodeUnpublishVolume, after the drain. Nothing
        // should route a published volume here — but a mount-point test
        // that answered wrong once did exactly that, and the tree went
        // with it, so the refusal is written down rather than assumed.
        if st.mode == "lean" && st.phase == "published" {
            tracing::error!(
                volume = %st.volume_id, tree = %st.src,
                "REFUSING to clean up a published lean volume ({why}): its tree is the tenant's data"
            );
            return;
        }
        tracing::warn!(volume = %st.volume_id, "cleaning up: {why}");
        if let Some(hash) = st.shared.clone() {
            self.cleanup_shared_member(dir, st, &hash).await;
            return;
        }
        let _ = fuse::unmount(Path::new(&st.target_path), true);
        let _ = fuse::unmount(&fuse::ro_stage_of(Path::new(&st.src)), true);
        let _ = fuse::unmount(Path::new(&st.src), true);
        // A tree image left mounted holds the state dir busy, so the
        // remove below would fail and the volume would never be retried
        // cleanly.
        if let Some(img) = &st.tree_image {
            if let Err(e) = quota::teardown(Path::new(&st.src), Path::new(img)) {
                tracing::warn!(volume = %st.volume_id, "tree quota teardown during cleanup: {e}");
            }
        }
        let _ = worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(5)).await;
        if let Some(b) = &self.cfg.broker {
            let _ = b.deregister(&st.volume_id).await;
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A failed JOIN, or a member's unfinished publish found at startup,
    /// leaves the shared mount alone unless nothing else holds it: unbind
    /// this member's target, take it off the record, and bring the
    /// mounter down only as the last one out. A failed join must never
    /// tear down the mount it failed to join. The class lock is the
    /// caller's — every path here already holds it, or runs at startup.
    async fn cleanup_shared_member(&self, dir: &Path, st: &VolumeState, hash: &str) {
        let _ = fuse::unmount(Path::new(&st.target_path), true);
        let sdir = state::shared_dir(&self.cfg.plugin_root, hash);
        match SharedMount::load(&sdir) {
            Ok(Some(mut sm)) => {
                sm.remove_member(&st.volume_id);
                if sm.members.is_empty() {
                    tracing::info!(volume = %st.volume_id, shared = hash, "nothing else holds the shared mount; bringing it down");
                    if let Err(e) = self.teardown_shared(&sdir, &sm).await {
                        tracing::warn!(volume = %st.volume_id, shared = hash, "shared teardown during cleanup: {}", e.message());
                    }
                } else if let Err(e) = sm.save(&sdir) {
                    tracing::warn!(volume = %st.volume_id, shared = hash, "shared record: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(volume = %st.volume_id, shared = hash, "shared record: {e}; leaving the mount alone"),
        }
        if let Some(b) = &self.cfg.broker {
            let _ = b.deregister(&st.volume_id).await;
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The resources a shared mounter's pod gets: its own when the chart
    /// set them, else every worker's.
    fn shared_worker_resources(&self) -> Option<ResourceRequirements> {
        self.cfg.shared_worker_resources.clone().or_else(|| self.cfg.worker_resources.clone())
    }

    /// The mounter's memory target from the worker's limit, logged once
    /// per publish so the number is beside the volume in the log.
    fn mounter_args(
        &self,
        vid: &str,
        spec: &crate::passthrough::spec::MountSpec,
        owner: (u32, u32),
        resources: Option<&ResourceRequirements>,
    ) -> Vec<String> {
        let limit = worker_memory_limit(resources);
        let args = crate::passthrough::mounter::mounter_args_for(
            spec,
            (Some(owner.0 as i64), Some(owner.1 as i64)),
            fuse::FUSE_FD_PLACEHOLDER,
            limit.as_deref(),
        );
        match (limit.as_deref(), crate::passthrough::mounter::memory_target_mib(limit.as_deref())) {
            (Some(l), Some(mib)) => {
                let limit_mib = crate::passthrough::spec::quantity_mib(l).unwrap_or(0);
                if mib >= limit_mib {
                    tracing::warn!(
                        volume = vid, limit = l, target_mib = mib,
                        "the worker memory limit is at or below mount-s3's {} MiB floor: its buffers alone may exceed the cgroup; raise workers.resources.limits.memory",
                        crate::passthrough::mounter::MEMORY_TARGET_FLOOR_MIB
                    );
                } else {
                    tracing::info!(volume = vid, limit = l, target_mib = mib, "mounter memory target from the worker limit");
                }
            }
            (Some(l), None) => tracing::warn!(volume = vid, limit = l, "worker memory limit is not a quantity this plugin reads; mount-s3 keeps its own memory target"),
            (None, _) => tracing::info!(volume = vid, "no worker memory limit: mount-s3 keeps its own memory target (95% of the cgroup or the node)"),
        }
        args
    }

    /// A worker kubelet or its container refused, as the tenant's
    /// FailedMount. A kubelet ADMISSION refusal (`OutOfmemory`,
    /// `OutOfpods`, …) is the one case that is not the plugin's or the
    /// bucket's: the plugin places workers with `nodeName` set, so the
    /// scheduler reserved no room for one, and the tenant is already
    /// bound here — Kubernetes will not move it. Named in its own event,
    /// with what frees it.
    async fn worker_refused(&self, st: &VolumeState, what: &str, reason: &str, message: &str) -> Status {
        if !worker::admission_refused(reason) {
            return Status::failed_precondition(format!("{what} pod {}: {reason} {message}", st.worker_name));
        }
        let requests = self
            .cfg
            .worker_resources
            .as_ref()
            .and_then(|r| r.requests.as_ref())
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .unwrap_or_else(|| "none".into());
        let msg = format!(
            "{what} pod {} was not admitted by kubelet on node {}: {reason} {message} — the mount needs a worker pod on this \
             node, the scheduler reserved no room for it, and the tenant pod is bound here and will not be rescheduled. \
             kubelet keeps retrying this mount; it succeeds once capacity frees on the node (worker requests: {requests}, \
             workers.resources in the flint-s3-csi chart; one worker per mounted pod counts against the node's maxPods)",
            st.worker_name, self.cfg.node_name
        );
        self.emit_event(&st.tenant, "WorkerNotAdmitted", &msg, true).await;
        Status::failed_precondition(msg)
    }

    /// A worker that is still coming up at the deadline (an image still
    /// pulling on a fresh node, most often) is KEPT: the volume goes to
    /// phase `worker-pending` and kubelet's retry adopts the same pod
    /// (`worker::ensure` adopts a live namesake) instead of deleting it
    /// and starting the pull's accounting over under a growing backoff.
    async fn worker_pending(&self, dir: &Path, st: &mut VolumeState, what: &str, phase: &str) -> Status {
        st.phase = "worker-pending".into();
        if let Err(e) = st.save(dir) {
            return Status::internal(format!("state: {e}"));
        }
        tracing::info!(volume = %st.volume_id, worker = %st.worker_name, phase, "{what} still coming up at the deadline; kept for kubelet's retry");
        Status::unavailable(format!(
            "{what} pod {} not Running after {}s ({phase}); it is kept and the retry resumes it",
            st.worker_name,
            WORKER_RUNNING_WAIT.as_secs()
        ))
    }

    async fn emit_event(&self, tenant: &TenantRef, reason: &str, message: &str, warning: bool) {
        let api: Api<Event> = Api::namespaced(self.client.clone(), &tenant.namespace);
        let now = MicroTime(k8s_openapi::jiff::Timestamp::now());
        let ev = Event {
            metadata: ObjectMeta {
                generate_name: Some(format!("{}.s3csi.", tenant.pod)),
                namespace: Some(tenant.namespace.clone()),
                ..Default::default()
            },
            involved_object: ObjectReference {
                api_version: Some("v1".into()),
                kind: Some("Pod".into()),
                name: Some(tenant.pod.clone()),
                namespace: Some(tenant.namespace.clone()),
                uid: Some(tenant.pod_uid.clone()),
                ..Default::default()
            },
            reason: Some(reason.into()),
            message: Some(message.chars().take(1000).collect()),
            type_: Some(if warning { "Warning".into() } else { "Normal".into() }),
            source: Some(k8s_openapi::api::core::v1::EventSource { component: Some(DRIVER_NAME.into()), host: Some(self.cfg.node_name.clone()) }),
            first_timestamp: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(now.0)),
            last_timestamp: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(now.0)),
            count: Some(1),
            reporting_component: Some(DRIVER_NAME.into()),
            reporting_instance: Some(self.cfg.node_name.clone()),
            ..Default::default()
        };
        if let Err(e) = api.create(&PostParams::default(), &ev).await {
            tracing::warn!("event on {}/{}: {e}", tenant.namespace, tenant.pod);
        }
    }

    // ── publish ──────────────────────────────────────────────────────

    async fn publish(&self, req: csi::NodePublishVolumeRequest) -> Result<(), Status> {
        let vid = req.volume_id.clone();
        if vid.is_empty() || req.target_path.is_empty() {
            return Err(Status::invalid_argument("volume_id and target_path are required"));
        }
        match req.volume_capability.as_ref().and_then(|c| c.access_type.as_ref()) {
            Some(AccessType::Mount(_)) | None => {}
            Some(AccessType::Block(_)) => {
                return Err(Status::invalid_argument(format!("{DRIVER_NAME} serves filesystem volumes, not block")))
            }
        }
        let pr = attrs::parse(&req.volume_context).map_err(Status::invalid_argument)?;
        let _guard = self.lock(&vid).await?;
        let dir = super::state::volume_dir(&self.cfg.plugin_root, &vid);
        let target = PathBuf::from(&req.target_path);

        // Idempotency / republish.
        if let Ok(Some(st)) = VolumeState::load(&dir) {
            let target_mounted = fuse::is_mountpoint(&target).unwrap_or(false);
            let src_mounted = fuse::is_mountpoint(Path::new(&st.src)).unwrap_or(false);
            let tree_exists = Path::new(&st.src).is_dir();
            match published_action(&st, target_mounted, src_mounted, tree_exists) {
                PublishedAction::Republish => return self.republish(&dir, st, &pr, &req.secrets).await,
                PublishedAction::Rebind { remount } => {
                    // Published once, but the target is gone (kubelet
                    // recreated the pod dir?). The source lives: bind it
                    // again, after putting a lost loop mount back.
                    if remount {
                        quota::remount(&dir, Path::new(&st.src))
                            .map_err(|e| Status::unavailable(format!("remount tree: {e}")))?;
                    }
                    // A shared source has ONE ro stage for every member:
                    // binds of one class are serialised under its lock.
                    let _class = match &st.shared {
                        Some(h) => Some(self.lock(&share_lock_key(h)).await?),
                        None => None,
                    };
                    fuse::bind_mount(Path::new(&st.src), &target, st.read_only)
                        .map_err(|e| Status::unavailable(format!("rebind: {e}")))?;
                    return Ok(());
                }
                PublishedAction::RefuseLean => {
                    // The refusal in `cleanup` guards the same tree, but
                    // this path used to overwrite the phase before it got
                    // there. A published workspace is never started over.
                    return Err(Status::failed_precondition(format!(
                        "volume {vid} is a published lean workspace ({}) whose tree {} is gone from the node; \
                         refusing to start over, because a fresh checkout here would replace the tenant's live tree",
                        st.cr, st.src
                    )));
                }
                // The syncer is checking out between kubelet's attempts
                // (design §3.5 step 8): do not start over, wait again.
                PublishedAction::ResumeCheckout => return self.resume_lean(&dir, st, &target).await,
                // The worker from the previous attempt is still coming up:
                // nothing below it was made (no credential, no mount), so
                // the publish runs again from the top and adopts it.
                PublishedAction::ResumeWorker => {
                    tracing::info!(volume = %vid, worker = %st.worker_name, "the previous attempt's worker is still coming up; resuming rather than starting over");
                }
                // An unfinished publish: start over.
                PublishedAction::StartOver => {
                    // A shared member's cleanup edits the class record:
                    // under the class lock, released before the publish
                    // below takes it again.
                    let class = match &st.shared {
                        Some(h) => Some(self.lock(&share_lock_key(h)).await?),
                        None => None,
                    };
                    self.cleanup(&dir, &st, "retrying an unfinished publish").await;
                    drop(class);
                }
            }
        }

        let tenant = TenantRef {
            namespace: pr.pod_namespace.clone(),
            pod: pr.pod_name.clone(),
            pod_uid: pr.pod_uid.clone(),
            service_account: pr.service_account.clone(),
        };

        // Resolve + authorize. The selector names a CR in the POD'S namespace.
        let resolved = resolve::fetch(&self.client, &pr.selector, &pr.pod_namespace).await.map_err(refusal_status)?;
        let policy = resolved.policy().map_err(refusal_status)?;
        let kind = match resolved {
            Resolved::Passthrough { .. } => "FlintPassthroughMount",
            Resolved::Lean { .. } => "FlintLeanWorkspace",
        };
        // The access this pod gets: the CR's list for its SA, narrowed by
        // the volume's `csi.readOnly` and never widened by it (per-user
        // access design §4.1). Everything downstream — the bind, the
        // mounter's flag, the syncer's mode, the broker's credential —
        // follows this one value.
        let access = resolve::authorize(&policy, &pr.service_account, req.readonly, &pr.pod_namespace, kind, pr.selector.name())
            .map_err(refusal_status)?;

        match resolved {
            Resolved::Passthrough { spec } => {
                self.publish_passthrough(&dir, &vid, &target, &pr, &tenant, spec, policy.credential_mode, access, &req).await
            }
            Resolved::Lean { spec, .. } => {
                resolve::lean_read_needs_read_only_volume(access, req.readonly, &pr.service_account, &pr.pod_namespace, pr.selector.name())
                    .map_err(refusal_status)?;
                self.publish_lean(&dir, &vid, &target, &pr, &tenant, spec, policy.credential_mode, access, &req).await
            }
        }
    }

    /// The credential arm, materialized. Also registers the publish at
    /// the broker when a broker is in the loop.
    async fn credential(
        &self,
        dir: &Path,
        mode: CredentialMode,
        pr: &PublishRequest,
        st: &mut VolumeState,
        secrets: &HashMap<String, String>,
    ) -> Result<Materialized, Status> {
        if let Some(t) = &pr.token {
            // For the unpublish path, which gets no token of its own.
            if let Err(e) = state::save_token_if_changed(dir, &t.token) {
                tracing::warn!(volume = %st.volume_id, "persist token: {e}");
            }
        }
        let mut m = self.credential_arm(mode, pr, st, secrets).await?;
        // Every arm carries a region: mount-s3 takes one on its argv, but
        // the lean syncer's SDK client reads AWS_REGION and, without it,
        // fails its first request as a bare "dispatch failure" (measured:
        // the same env with AWS_REGION checks out). The static arm's
        // Secret may name its own; else the CR's (`spec.region`, carried
        // as FLINT_SYNC_REGION in the lean launch list — a bucket in the
        // wrong region 301s every request and the worker crash-loops);
        // else the node's default.
        m.env
            .entry("AWS_REGION".to_string())
            .or_insert_with(|| creds::effective_region(st.sync_env.as_ref(), &self.cfg.region));
        Ok(m)
    }

    async fn credential_arm(
        &self,
        mode: CredentialMode,
        pr: &PublishRequest,
        st: &mut VolumeState,
        secrets: &HashMap<String, String>,
    ) -> Result<Materialized, Status> {
        match mode {
            CredentialMode::Static => {
                if secrets.is_empty() {
                    return Err(Status::failed_precondition(
                        "identity.mode is static, but the pod's volume names no nodePublishSecretRef (a Secret \
                         in the pod's namespace with AWS_* keys verbatim)",
                    ));
                }
                creds::static_arm(secrets).map_err(Status::failed_precondition)
            }
            CredentialMode::Ambient => Ok(creds::ambient_arm()),
            CredentialMode::Broker | CredentialMode::WebIdentity => {
                let Some(broker) = &self.cfg.broker else {
                    return Err(Status::failed_precondition(format!(
                        "identity.mode {} needs flint-s3-broker, and this node driver has no FLINT_S3CSI_BROKER_URL",
                        mode.as_str()
                    )));
                };
                let Some(token) = &pr.token else {
                    return Err(Status::failed_precondition(format!(
                        "identity.mode {} needs a pod-bound ServiceAccount token, and kubelet delivered none — \
                         CSIDriver {DRIVER_NAME} must declare tokenRequests with audience {DRIVER_NAME}",
                        mode.as_str()
                    )));
                };
                st.token_expiration = Some(token.expiration.clone());
                broker.register(&self.registration_of(st)).await.map_err(Status::unavailable)?;
                let role_arn = creds::role_arn(&st.mode, &st.cr);
                if mode == CredentialMode::WebIdentity {
                    return Ok(creds::web_identity_arm(&role_arn, broker.base_url(), &st.nonce, &token.token, &self.cfg.region));
                }
                let c = broker
                    .exchange(&token.token, &role_arn, &st.nonce, self.cfg.creds_lifetime_secs)
                    .await
                    .map_err(|e| exchange_status(&st.cr, e))?;
                st.creds_expiration = Some(c.expiration.clone());
                let mut m = creds::door_arm(&st.nonce);
                m.files.push(creds::CommFile { name: creds::CREDS_FILE.into(), bytes: creds::creds_json(&c), mode: 0o600 });
                Ok(m)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn publish_passthrough(
        &self,
        dir: &Path,
        vid: &str,
        target: &Path,
        pr: &PublishRequest,
        tenant: &TenantRef,
        spec: crate::passthrough::spec::MountSpec,
        cred_mode: CredentialMode,
        access: Access,
        req: &csi::NodePublishVolumeRequest,
    ) -> Result<(), Status> {
        let owner_uid = pr.uid.or(spec.uid.map(|u| u as u32)).unwrap_or(DEFAULT_OWNER);
        let owner_gid = pr.gid.or(spec.gid.map(|g| g as u32)).unwrap_or(owner_uid);
        // `access` already carries the pod's `csi.readOnly`; the CR's own
        // `readOnly` narrows every consumer of this mount.
        let read_only = access.is_read() || spec.read_only;
        // A CR that shares its mounter gets the block cache by default when
        // it names none: sharing is for a dataset read many times, and
        // without the cache every read that is not concurrent goes to S3.
        // Named on the CR either way; `cache.enabled: false` opts out.
        let spec = if spec.shares_read_only() && spec.cache.is_none() {
            let spec = spec.with_default_cache(&self.cfg.scratch_size);
            match spec.cache.as_ref().and_then(|c| c.max_size_mib) {
                Some(mib) => tracing::info!(
                    volume = vid, cr = pr.selector.name(), cache_mib = mib, scratch = %self.cfg.scratch_size,
                    "sharing: block cache defaulted (spec.cache absent)"
                ),
                None => tracing::warn!(
                    volume = vid, cr = pr.selector.name(), scratch = %self.cfg.scratch_size,
                    "sharing: no default block cache — workers.scratchSize is unreadable or too small; set spec.cache on the CR"
                ),
            }
            spec
        } else {
            spec
        };
        if spec.shares_read_only() {
            let backend = if cred_mode == CredentialMode::Broker { self.broker_backend().await } else { None };
            match sharing_decision(true, read_only, cred_mode, backend.as_deref()) {
                Sharing::Shared => {
                    return self.publish_shared(dir, vid, target, pr, tenant, &spec, cred_mode, req, (owner_uid, owner_gid)).await;
                }
                // The CR asked and the answer is not known yet: a retry, not
                // a mounter of its own for the pod's whole life. With the
                // broker unreachable the credential exchange below would
                // fail the same way, so nothing is lost by waiting.
                Sharing::Retry(why) => {
                    tracing::warn!(volume = vid, cr = pr.selector.name(), "sharing cannot be decided yet: {why}; retrying");
                    return Err(Status::unavailable(format!(
                        "FlintPassthroughMount {}/{} asks for sharing and {why}; retrying",
                        pr.pod_namespace,
                        pr.selector.name()
                    )));
                }
                Sharing::Own(why) => {
                    tracing::info!(volume = vid, cr = pr.selector.name(), "own mounter, not the CR's shared one: {why}");
                    // A read-write consumer is expected to keep its own; a
                    // read-only one the knob was meant for gets told, once
                    // per publish, on the tenant pod.
                    if read_only {
                        self.emit_event(
                            tenant,
                            "SharingUnavailable",
                            &format!("{}: spec.sharing.readOnly is set but this pod gets a mounter of its own — {why}", pr.selector.name()),
                            true,
                        )
                        .await;
                    }
                }
            }
        }
        let src = dir.join("src");
        let mut st = VolumeState {
            version: STATE_VERSION,
            volume_id: vid.to_string(),
            mode: "passthrough".into(),
            cr: pr.selector.name().to_string(),
            tenant: tenant.clone(),
            target_path: target.to_string_lossy().into_owned(),
            src: src.to_string_lossy().into_owned(),
            worker_namespace: self.cfg.worker_namespace.clone(),
            worker_name: worker::worker_name(vid),
            worker_uid: None,
            phase: "publishing".into(),
            credential_mode: cred_mode.as_str().into(),
            nonce: creds::new_nonce(),
            creds_expiration: None,
            token_expiration: None,
            last_probe_ok: None,
            published_unix: None,
            read_only,
            owner_uid,
            owner_gid,
            grace_secs: None,
            tree_image: None,
            drain_started_unix: None,
            sync_env: None,
            on_behalf_of: pr.on_behalf_of.clone(),
            shared: None,
        };
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;

        // The worker pod first: its comm dir is where the credential goes.
        let (run_as, run_as_gid) = worker_owner(&st);
        let pod = worker::build_pod(&WorkerInputs {
            namespace: self.cfg.worker_namespace.clone(),
            node_name: self.cfg.node_name.clone(),
            node_uid: self.cfg.node_uid.clone(),
            image: self.cfg.passthrough_image.clone(),
            mode: "passthrough",
            volume_id: vid,
            tenant,
            cr: &st.cr,
            run_as_uid: run_as,
            run_as_gid,
            resources: self.cfg.worker_resources.clone(),
            prestop_secs: self.cfg.prestop_secs,
            env: BTreeMap::from([("FLINT_S3W_MODE".to_string(), "passthrough".to_string())]),
            lean_tree_hostpath: None,
            grace_secs: Some(30),
            priority_class: self.cfg.priority_class.clone(),
            comm_size: self.cfg.comm_size.clone(),
            scratch_size: self.cfg.scratch_size.clone(),
            shared_key: None,
        });
        if let Err(e) = worker::ensure(&self.client, &pod).await {
            return Err(self.fail(dir, &st,Status::unavailable(e)).await);
        }
        let worker_uid = match worker::wait_running(&self.client, &st.worker_namespace, &st.worker_name, WORKER_RUNNING_WAIT).await {
            Ok(WaitOutcome::Running { uid }) => uid,
            Ok(WaitOutcome::Failed { reason, message }) => {
                let status = self.worker_refused(&st, "worker", &reason, &message).await;
                return Err(self.fail(dir, &st, status).await);
            }
            // Kept, not cleaned up: the retry adopts it.
            Ok(WaitOutcome::Timeout { phase }) => return Err(self.worker_pending(dir, &mut st, "worker", &phase).await),
            Err(e) => return Err(self.fail(dir, &st,Status::unavailable(e)).await),
        };
        st.worker_uid = Some(worker_uid.clone());
        let comm = worker::comm_dir(&self.cfg.kubelet_root, &worker_uid);
        if !comm.is_dir() {
            return Err(self.fail(dir, &st,Status::unavailable(format!("worker comm dir {} not visible on the node yet", comm.display()))).await);
        }

        // Credential.
        let mat = match self.credential(dir, cred_mode, pr, &mut st, &req.secrets).await {
            Ok(m) => m,
            Err(e) => return Err(self.fail(dir, &st,e).await),
        };
        if let Err(e) = creds::write_files(&comm, &mat.files, worker_owner(&st)) {
            return Err(self.fail(dir, &st,Status::internal(format!("write comm files: {e}"))).await);
        }
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;

        // The mount: root does the syscall, the worker serves the fd. A
        // source that is still a mount point (an earlier attempt's, or a
        // predecessor plugin's dead one) is unmounted first: mounting over
        // it would stack, and the stack is what an unpublish trips on.
        if let Err(e) = unmount_all(&src) {
            return Err(self.fail(dir, &st, Status::unavailable(format!("stale source mount at {}: {e}", src.display()))).await);
        }
        let fd = match fuse::open_and_mount(&src, (owner_uid, owner_gid), read_only, "mount-s3") {
            Ok(fd) => fd,
            Err(e) => return Err(self.fail(dir, &st,Status::internal(format!("fuse mount: {e}"))).await),
        };
        let mut args = self.mounter_args(vid, &spec, (owner_uid, owner_gid), self.cfg.worker_resources.as_ref());
        if read_only && !args.iter().any(|a| a == "--read-only") {
            args.push("--read-only".into());
        }
        let launch = Launch { mode: "passthrough".into(), args, env: mat.env };
        let sock = comm.join("mount.sock");
        let reply = {
            use std::os::fd::AsRawFd;
            let raw = fd.as_raw_fd();
            let sock = sock.clone();
            let launch = launch.clone();
            tokio::task::spawn_blocking(move || fuse::send_launch(&sock, &launch, Some(raw), LAUNCH_REPLY_WAIT)).await
        };
        // Our copy of the fd goes now; the worker holds the connection.
        drop(fd);
        match reply {
            Ok(Ok(r)) if r.ok => {}
            Ok(Ok(r)) => return Err(self.fail(dir, &st,Status::unavailable(format!("worker refused the launch: {}", r.error.unwrap_or_default()))).await),
            Ok(Err(e)) => return Err(self.fail(dir, &st,Status::unavailable(format!("launch over {}: {e}", sock.display()))).await),
            Err(e) => return Err(self.fail(dir, &st,Status::internal(format!("launch task: {e}"))).await),
        }
        if let Err(e) = fuse::wait_ready(&src, FUSE_READY_WAIT).await {
            // The mounter's last words. A dead mounter is seen HERE the
            // instant its FUSE fd closes (statfs answers ENOTCONN), and
            // the supervisor writes mount.error only after it has reaped
            // the child and flushed the stderr tail — so an immediate
            // read races it and loses. On a real node an ambient mount
            // whose credential chain failed reported "mounter died
            // before serving the mount (statfs: ENOTCONN)" and nothing
            // else, while mount.error held "No signing credentials
            // available" (EC2, 2026-09-04). Wait briefly for the file.
            let detail = wait_for_mount_error(&comm, Duration::from_secs(3)).await;
            return Err(self.fail(dir, &st,Status::unavailable(format!("mounter did not serve the mount: {e}{}", if detail.is_empty() { String::new() } else { format!(" — {}", detail.trim()) }))).await);
        }
        if let Err(e) = fuse::bind_mount(&src, target, read_only) {
            return Err(self.fail(dir, &st,Status::internal(format!("bind: {e}"))).await);
        }
        st.phase = "published".into();
        st.published_unix = Some(chrono::Utc::now().timestamp() as u64);
        st.last_probe_ok = Some(true);
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
        tracing::info!(volume = vid, cr = %st.cr, tenant = %format!("{}/{}", tenant.namespace, tenant.pod), worker = %st.worker_name, "published");
        Ok(())
    }

    /// Join the CR's shared read-only mounter on this node, or create it
    /// (docs/plans/passthrough-read-only-mount-sharing.md): one worker,
    /// one FUSE source, one block cache and one connection pool per
    /// equality class (`state::share_key`), and each member's target a
    /// bind of that source.
    ///
    /// Each member keeps its own `VolumeState`, registration, nonce and
    /// token, and exchanges ITS OWN credential into the shared worker's
    /// comm dir: every member's authority is the same function of the CR
    /// — the whole security argument, and why only read-only members on a
    /// backend that scopes by CR get here (`sharing_decision`). The
    /// freshest key wins the file; the door token stays the creator's,
    /// because the mounter is reading it. The broker still logs one
    /// registration and one exchange per member.
    #[allow(clippy::too_many_arguments)]
    async fn publish_shared(
        &self,
        dir: &Path,
        vid: &str,
        target: &Path,
        pr: &PublishRequest,
        tenant: &TenantRef,
        spec: &crate::passthrough::spec::MountSpec,
        cred_mode: CredentialMode,
        req: &csi::NodePublishVolumeRequest,
        owner: (u32, u32),
    ) -> Result<(), Status> {
        let (owner_uid, owner_gid) = owner;
        let shared_resources = self.shared_worker_resources();
        let mut args = self.mounter_args(vid, spec, owner, shared_resources.as_ref());
        if !args.iter().any(|a| a == "--read-only") {
            args.push("--read-only".into());
        }
        let cr = pr.selector.name();
        let (hash, key) = state::share_key(&self.cfg.node_name, &tenant.namespace, cr, owner_uid, owner_gid, cred_mode.as_str(), &args);
        let sdir = state::shared_dir(&self.cfg.plugin_root, &hash);
        let src = sdir.join("src");
        // The class lock, after the volume's: every join, leave and
        // teardown of one class serialises here — which is also what keeps
        // two members off the class's one ro stage at once.
        let _class = self.lock(&share_lock_key(&hash)).await?;

        let mut st = VolumeState {
            version: STATE_VERSION,
            volume_id: vid.to_string(),
            mode: "passthrough".into(),
            cr: cr.to_string(),
            tenant: tenant.clone(),
            target_path: target.to_string_lossy().into_owned(),
            src: src.to_string_lossy().into_owned(),
            worker_namespace: self.cfg.worker_namespace.clone(),
            worker_name: worker::worker_name(&hash),
            worker_uid: None,
            phase: "publishing".into(),
            credential_mode: cred_mode.as_str().into(),
            nonce: creds::new_nonce(),
            creds_expiration: None,
            token_expiration: None,
            last_probe_ok: None,
            published_unix: None,
            read_only: true,
            owner_uid,
            owner_gid,
            grace_secs: None,
            tree_image: None,
            drain_started_unix: None,
            sync_env: None,
            on_behalf_of: pr.on_behalf_of.clone(),
            shared: Some(hash.clone()),
        };
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;

        let mut sm = match SharedMount::load(&sdir).map_err(|e| Status::internal(format!("shared record: {e}")))? {
            Some(sm) if sm.phase == "published" && self.shared_mount_alive(&sm).await => sm,
            // A create whose worker was never LAUNCHED (no worker uid on the
            // record: the previous attempt hit its deadline before the pod
            // ran, and kept it) and whose pod is alive — still pulling, or
            // Running by now: the CREATE branch below adopts that pod and
            // launches on it. A record WITH a worker uid was launched once
            // already, and an fd passed once cannot be passed again: replaced.
            Some(mut sm)
                if sm.phase != "published"
                    && sm.worker_uid.is_none()
                    && worker::alive(&self.client, &sm.worker_namespace, &sm.worker_name).await.unwrap_or(false) =>
            {
                tracing::info!(volume = vid, shared = %hash, worker = %sm.worker_name, "the shared worker from an earlier attempt is alive and was never launched; resuming its create on it");
                sm.phase = "publishing".into();
                sm
            }
            Some(mut sm) => {
                // A record whose mounter is not serving (dead, or a create
                // that never finished). Its members are stranded exactly as
                // a per-pod dead mounter strands its tenant — their events
                // said so — and a NEW member gets a fresh mounter under the
                // same class rather than a dead one.
                tracing::warn!(
                    volume = vid, shared = %hash, phase = %sm.phase, members = ?sm.members,
                    "the shared mounter is not serving; replacing it for the new member"
                );
                if let Err(e) = self.replace_dead_shared_mounter(&sm).await {
                    return Err(self.fail(dir, &st, e).await);
                }
                sm.phase = "publishing".into();
                sm.worker_uid = None;
                sm
            }
            None => SharedMount {
                version: STATE_VERSION,
                hash: hash.clone(),
                key: key.clone(),
                namespace: tenant.namespace.clone(),
                cr: cr.to_string(),
                src: st.src.clone(),
                worker_namespace: self.cfg.worker_namespace.clone(),
                worker_name: worker::worker_name(&hash),
                worker_uid: None,
                owner_uid,
                owner_gid,
                credential_mode: cred_mode.as_str().into(),
                phase: "publishing".into(),
                members: vec![],
                created_unix: Some(chrono::Utc::now().timestamp() as u64),
            },
        };
        // The record carries this member from here on, so a failure below
        // (`cleanup`) knows whether it was the only one.
        sm.add_member(vid);
        sm.save(&sdir).map_err(|e| Status::internal(format!("shared record: {e}")))?;

        if sm.phase != "published" {
            // CREATE: the worker first (its comm dir is where the
            // credential goes), then the credential, then the mount.
            let (run_as, run_as_gid) = worker_owner(&st);
            let pod = worker::build_pod(&WorkerInputs {
                namespace: self.cfg.worker_namespace.clone(),
                node_name: self.cfg.node_name.clone(),
                node_uid: self.cfg.node_uid.clone(),
                image: self.cfg.passthrough_image.clone(),
                mode: "passthrough",
                volume_id: &hash,
                tenant,
                cr,
                run_as_uid: run_as,
                run_as_gid,
                resources: shared_resources.clone(),
                prestop_secs: self.cfg.prestop_secs,
                env: BTreeMap::from([("FLINT_S3W_MODE".to_string(), "passthrough".to_string())]),
                lean_tree_hostpath: None,
                grace_secs: Some(30),
                priority_class: self.cfg.priority_class.clone(),
                comm_size: self.cfg.comm_size.clone(),
                scratch_size: self.cfg.scratch_size.clone(),
                shared_key: Some(&key),
            });
            if let Err(e) = worker::ensure(&self.client, &pod).await {
                return Err(self.fail(dir, &st, Status::unavailable(e)).await);
            }
            let worker_uid = match worker::wait_running(&self.client, &st.worker_namespace, &st.worker_name, WORKER_RUNNING_WAIT).await {
                Ok(WaitOutcome::Running { uid }) => uid,
                Ok(WaitOutcome::Failed { reason, message }) => {
                    let status = self.worker_refused(&st, "shared worker", &reason, &message).await;
                    return Err(self.fail(dir, &st, status).await);
                }
                // Kept, with this member on the record: the retry (this
                // member's, or another's) adopts it.
                Ok(WaitOutcome::Timeout { phase }) => return Err(self.worker_pending(dir, &mut st, "shared worker", &phase).await),
                Err(e) => return Err(self.fail(dir, &st, Status::unavailable(e)).await),
            };
            sm.worker_uid = Some(worker_uid.clone());
            sm.save(&sdir).map_err(|e| Status::internal(format!("shared record: {e}")))?;
            st.worker_uid = Some(worker_uid.clone());
            let comm = worker::comm_dir(&self.cfg.kubelet_root, &worker_uid);
            if !comm.is_dir() {
                return Err(self.fail(dir, &st, Status::unavailable(format!("shared worker comm dir {} not visible on the node yet", comm.display()))).await);
            }
            let mat = match self.credential(dir, cred_mode, pr, &mut st, &req.secrets).await {
                Ok(m) => m,
                Err(e) => return Err(self.fail(dir, &st, e).await),
            };
            if let Err(e) = creds::write_files(&comm, &mat.files, worker_owner(&st)) {
                return Err(self.fail(dir, &st, Status::internal(format!("write comm files: {e}"))).await);
            }
            st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
            if let Err(e) = unmount_all(&src) {
                return Err(self.fail(dir, &st, Status::unavailable(format!("stale shared source mount at {}: {e}", src.display()))).await);
            }
            let fd = match fuse::open_and_mount(&src, (owner_uid, owner_gid), true, "mount-s3") {
                Ok(fd) => fd,
                Err(e) => return Err(self.fail(dir, &st, Status::internal(format!("fuse mount: {e}"))).await),
            };
            let launch = Launch { mode: "passthrough".into(), args, env: mat.env };
            let sock = comm.join("mount.sock");
            let reply = {
                use std::os::fd::AsRawFd;
                let raw = fd.as_raw_fd();
                let sock = sock.clone();
                let launch = launch.clone();
                tokio::task::spawn_blocking(move || fuse::send_launch(&sock, &launch, Some(raw), LAUNCH_REPLY_WAIT)).await
            };
            drop(fd);
            match reply {
                Ok(Ok(r)) if r.ok => {}
                Ok(Ok(r)) => return Err(self.fail(dir, &st, Status::unavailable(format!("shared worker refused the launch: {}", r.error.unwrap_or_default()))).await),
                Ok(Err(e)) => return Err(self.fail(dir, &st, Status::unavailable(format!("launch over {}: {e}", sock.display()))).await),
                Err(e) => return Err(self.fail(dir, &st, Status::internal(format!("launch task: {e}"))).await),
            }
            if let Err(e) = fuse::wait_ready(&src, FUSE_READY_WAIT).await {
                let detail = wait_for_mount_error(&comm, Duration::from_secs(3)).await;
                return Err(self
                    .fail(
                        dir,
                        &st,
                        Status::unavailable(format!(
                            "shared mounter did not serve the mount: {e}{}",
                            if detail.is_empty() { String::new() } else { format!(" — {}", detail.trim()) }
                        )),
                    )
                    .await);
            }
            sm.phase = "published".into();
            sm.save(&sdir).map_err(|e| Status::internal(format!("shared record: {e}")))?;
        } else {
            // JOIN: this member's own credential into the shared comm dir
            // (identical authority; the freshest key wins) — never the
            // door token, which is the creator's and in use.
            let Some(worker_uid) = sm.worker_uid.clone() else {
                return Err(self.fail(dir, &st, Status::internal(format!("shared record {hash} is published but names no worker uid"))).await);
            };
            st.worker_uid = Some(worker_uid.clone());
            let comm = worker::comm_dir(&self.cfg.kubelet_root, &worker_uid);
            let mat = match self.credential(dir, cred_mode, pr, &mut st, &req.secrets).await {
                Ok(m) => m,
                Err(e) => return Err(self.fail(dir, &st, e).await),
            };
            let files: Vec<creds::CommFile> = mat.files.into_iter().filter(|f| f.name != creds::AUTH_TOKEN_FILE).collect();
            if let Err(e) = creds::write_files(&comm, &files, worker_owner(&st)) {
                return Err(self.fail(dir, &st, Status::internal(format!("write comm files: {e}"))).await);
            }
            st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
        }
        if let Err(e) = fuse::bind_mount(&src, target, true) {
            return Err(self.fail(dir, &st, Status::internal(format!("bind: {e}"))).await);
        }
        st.phase = "published".into();
        st.published_unix = Some(chrono::Utc::now().timestamp() as u64);
        st.last_probe_ok = Some(true);
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
        tracing::info!(
            volume = vid, cr = %st.cr, tenant = %format!("{}/{}", tenant.namespace, tenant.pod), worker = %st.worker_name,
            shared = %hash, members = sm.members.len(), "published (shared read-only mount)"
        );
        Ok(())
    }

    /// Is the shared mounter serving: its source a mount that answers
    /// statfs (a dead FUSE mount is still a mount point — `is_mountpoint`
    /// says so on purpose — hence the probe; Mountpoint answers statfs
    /// without S3, so three seconds of silence is a daemon that is not
    /// there), and its worker not DEFINITIVELY gone. An API error is not
    /// a verdict: replacing a mounter deletes its worker, which strands
    /// every current member, so only a GET that answers "not Running"
    /// counts against it — the local probe decides the rest.
    async fn shared_mount_alive(&self, sm: &SharedMount) -> bool {
        let src = Path::new(&sm.src);
        let serving = fuse::is_mountpoint(src).unwrap_or(false)
            && fuse::wait_ready_opts(src, Duration::from_secs(3), false).await.is_ok();
        if !serving {
            return false;
        }
        match worker::is_running(&self.client, &sm.worker_namespace, &sm.worker_name).await {
            Ok(running) => running,
            Err(e) => {
                tracing::warn!(shared = %sm.hash, worker = %sm.worker_name, "could not read the shared worker ({e}); its mount answers, so it is treated as alive");
                true
            }
        }
    }

    /// Clear a dead shared mounter out of the way of a new member: detach
    /// its source (its members' targets keep their own reference to the
    /// dead superblock, as they would with a dead per-pod mounter),
    /// release and delete the old worker, and wait — bounded — for the
    /// name to be free, because the new worker is created under it.
    async fn replace_dead_shared_mounter(&self, sm: &SharedMount) -> Result<(), Status> {
        let src = Path::new(&sm.src);
        unmount_all(&fuse::ro_stage_of(src)).map_err(|e| Status::unavailable(format!("unmount ro stage: {e}")))?;
        unmount_all(src).map_err(|e| Status::unavailable(format!("stale shared source mount at {}: {e}", src.display())))?;
        self.release_worker_uid(&format!("shared:{}", sm.hash), sm.worker_uid.as_deref());
        worker::delete(&self.client, &sm.worker_namespace, &sm.worker_name, Some(0)).await.map_err(Status::unavailable)?;
        let start = Instant::now();
        while !worker::is_gone(&self.client, &sm.worker_namespace, &sm.worker_name).await.map_err(Status::unavailable)? {
            if start.elapsed() > DEAD_SHARED_WORKER_WAIT {
                return Err(Status::unavailable(format!("the dead shared worker {} is still terminating; retrying", sm.worker_name)));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    }

    /// Target already mounted: refresh, probe, OK. Never an error.
    async fn republish(
        &self,
        dir: &Path,
        mut st: VolumeState,
        pr: &PublishRequest,
        secrets: &HashMap<String, String>,
    ) -> Result<(), Status> {
        let mut changed = false;
        if let Some(token) = pr.token.as_ref() {
            // Kept beside the state for the unpublish path, which gets
            // no token of its own (design §5, final-barrier row).
            match state::save_token_if_changed(dir, &token.token) {
                Ok(true) => tracing::debug!(volume = %st.volume_id, "token rotated"),
                Ok(false) => {}
                Err(e) => tracing::warn!(volume = %st.volume_id, "persist token: {e}"),
            }
        }
        if let (Some(worker_uid), Some(token)) = (st.worker_uid.clone(), pr.token.as_ref()) {
            let comm = worker::comm_dir(&self.cfg.kubelet_root, &worker_uid);
            match CredentialMode::parse(&st.credential_mode).unwrap_or(CredentialMode::Ambient) {
                CredentialMode::WebIdentity => {
                    if st.token_expiration.as_deref() != Some(token.expiration.as_str()) {
                        let f = creds::CommFile { name: creds::TOKEN_FILE.into(), bytes: token.token.as_bytes().to_vec(), mode: 0o600 };
                        if creds::write_files(&comm, &[f], worker_owner(&st)).is_ok() {
                            st.token_expiration = Some(token.expiration.clone());
                            changed = true;
                        }
                    }
                }
                CredentialMode::Broker => {
                    let left = st
                        .creds_expiration
                        .as_ref()
                        .map(|e| Creds { access_key_id: String::new(), secret_access_key: String::new(), session_token: None, expiration: e.clone() }.secs_left(chrono::Utc::now()))
                        .unwrap_or(0);
                    // Three republish periods (~90 s each) before expiry.
                    if left < 270 {
                        if let Some(broker) = &self.cfg.broker {
                            // Re-register on EVERY refresh: the broker's registry is
                            // in-memory, and a broker restart (a roll, an eviction)
                            // would otherwise refuse every later exchange — measured:
                            // S8's broker-down control left every tenant on the node
                            // with "no live publish registration" until expiry. The
                            // registration is idempotent and node-authenticated; a
                            // failure here is an outage (kept key), not a refusal.
                            let refreshed = match broker.register(&self.registration_of(&st)).await {
                                Ok(()) => broker.exchange(&token.token, &creds::role_arn(&st.mode, &st.cr), &st.nonce, self.cfg.creds_lifetime_secs).await,
                                Err(e) => Err(ExchangeError::Outage(format!("re-registration before refresh: {e}"))),
                            };
                            match refreshed {
                                Ok(c) => {
                                    let f = creds::CommFile { name: creds::CREDS_FILE.into(), bytes: creds::creds_json(&c), mode: 0o600 };
                                    if creds::write_files(&comm, &[f], worker_owner(&st)).is_ok() {
                                        st.creds_expiration = Some(c.expiration);
                                        st.token_expiration = Some(token.expiration.clone());
                                        changed = true;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(volume = %st.volume_id, "credential refresh failed: {e}");
                                    // A REFUSAL (not an outage) takes the credential away:
                                    // the door answers 503, the client fails at the old
                                    // key's expiry — revocation lands within one lifetime
                                    // (design §4.6). An outage keeps the cached key.
                                    if e.is_refusal() && revocation_removes_the_key(&st) {
                                        creds::remove_file(&comm, creds::CREDS_FILE);
                                        st.creds_expiration = None;
                                        changed = true;
                                    }
                                    let detail = if e.is_refusal() && st.shared.is_some() {
                                        // The file serves every member of the class; removing
                                        // it would only cut the others' reads until the next
                                        // member re-minted it, while this pod — bound to the
                                        // same superblock — read on regardless.
                                        format!(
                                            "{}: {e} — this pod shares its mounter with the other read-only members of its class, so the \
                                             class's credential stays for them and this pod keeps reading until it exits (per-member \
                                             revocation is not possible under spec.sharing.readOnly)",
                                            st.cr
                                        )
                                    } else {
                                        format!("{}: {e}", st.cr)
                                    };
                                    self.emit_event(&st.tenant, "CredentialRefreshFailed", &detail, true).await;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // Liveness: is the worker alive and the mount answering? The
        // source must still BE a mount: a plain directory answers statfs
        // and readdir happily, and that is what a lost mount looks like.
        // The probe is statfs only — a readdir here is a LIST per volume
        // per minute against the store, and a thread parked for the
        // mounter's whole timeout whenever the store is unreachable.
        let alive = self.workers.is_running(&st.worker_name).await.unwrap_or(true);
        let mounted = st.mode != "passthrough" || fuse::is_mountpoint(Path::new(&st.src)).unwrap_or(false);
        let ok = alive && mounted && fuse::wait_ready_opts(Path::new(&st.src), Duration::from_secs(3), false).await.is_ok();
        if st.last_probe_ok != Some(ok) {
            changed = true;
            st.last_probe_ok = Some(ok);
            if !ok {
                let detail = st
                    .worker_uid
                    .as_ref()
                    .map(|u| worker::comm_dir(&self.cfg.kubelet_root, u).join("mount.error"))
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .unwrap_or_default();
                self.emit_event(
                    &st.tenant,
                    "MounterDead",
                    &format!(
                        "the mounter serving {} at {} is not answering (worker {} running={alive}); processes with the \
                         mount open get ENOTCONN until the pod is recreated. {}",
                        st.cr, st.target_path, st.worker_name, detail.trim()
                    ),
                    true,
                )
                .await;
            }
        }
        // A lean syncer lost at the POD level — evicted, node pressure,
        // deleted by hand — leaves a tree nobody publishes for the rest
        // of the tenant's life, and the tenant sees nothing but an
        // event. The design (§6.7) has it recreated on the next
        // republish; this is that. `OnFailure` covers container exits;
        // this covers the pod. Decided by a GET, never by the cache.
        if st.mode == "lean" && !alive && st.drain_started_unix.is_none() {
            let why = match worker::phase(&self.client, &st.worker_namespace, &st.worker_name).await {
                Ok(p) => relaunch_reason(p.as_deref()),
                Err(_) => None,
            };
            if let Some(why) = why {
                match self.relaunch_lean_worker(dir, &mut st, pr, secrets).await {
                    Ok(()) => {
                        changed = true;
                        self.emit_event(
                            &st.tenant,
                            "SyncerRecreated",
                            &format!(
                                "{}: the syncer pod {why}; worker {} was started over the same tree and \
                                 self-recognises its lease",
                                st.cr, st.worker_name
                            ),
                            false,
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(volume = %st.volume_id, "syncer relaunch: {}", e.message());
                        self.emit_event(
                            &st.tenant,
                            "SyncerRecreateFailed",
                            &format!(
                                "{}: the syncer pod is gone and could not be relaunched ({}); nothing publishes this \
                                 workspace until it is — retried every republish",
                                st.cr,
                                e.message()
                            ),
                            true,
                        )
                        .await;
                    }
                }
            }
        }
        // A Running worker with NO syncer inside it: after a node reboot
        // the memory-backed comm dir is empty, so the supervisor has no
        // launch to relaunch from and sits in its accept loop (exiting
        // and restarting every accept budget) while the pod reads
        // Running and nothing publishes (audit 2026-09-03, finding 4).
        // The supervisor is listening; send the launch again.
        if st.mode == "lean" && alive && st.drain_started_unix.is_none() {
            if let Some(uid) = st.worker_uid.clone() {
                let comm = worker::comm_dir(&self.cfg.kubelet_root, &uid);
                if launch_missing(&comm) {
                    match self.resend_lean_launch(dir, &mut st, pr, secrets, &comm).await {
                        Ok(()) => {
                            changed = true;
                            self.emit_event(
                                &st.tenant,
                                "SyncerRelaunched",
                                &format!(
                                    "{}: worker {} was Running with no syncer inside it (its launch record was gone — a \
                                     node reboot); the launch was sent again and the syncer self-recognises its lease",
                                    st.cr, st.worker_name
                                ),
                                false,
                            )
                            .await;
                        }
                        Err(e) => {
                            tracing::warn!(volume = %st.volume_id, "syncer launch re-send: {}", e.message());
                            self.emit_event(
                                &st.tenant,
                                "SyncerRelaunchFailed",
                                &format!(
                                    "{}: worker {} is Running with no syncer inside it and the launch could not be sent \
                                     again ({}); nothing publishes this workspace until it is — retried every republish",
                                    st.cr,
                                    st.worker_name,
                                    e.message()
                                ),
                                true,
                            )
                            .await;
                        }
                    }
                }
            }
        }
        if changed {
            let _ = st.save(dir);
        }
        Ok(())
    }

    /// The launch, sent again to a worker whose supervisor lost it.
    async fn resend_lean_launch(
        &self,
        dir: &Path,
        st: &mut VolumeState,
        pr: &PublishRequest,
        secrets: &HashMap<String, String>,
        comm: &Path,
    ) -> Result<(), Status> {
        let Some(sync_conf) = st.sync_env.clone() else {
            return Err(Status::failed_precondition(
                "the volume was published by a plugin that kept no syncer environment; recreate the tenant pod",
            ));
        };
        let mode = CredentialMode::parse(&st.credential_mode).map_err(Status::failed_precondition)?;
        let mat = self.credential(dir, mode, pr, st, secrets).await?;
        creds::write_files(comm, &mat.files, worker_owner(st)).map_err(|e| Status::internal(format!("write comm files: {e}")))?;
        let mut env = sync_conf;
        env.extend(mat.env);
        self.send_lean_launch(st, comm, env).await
    }

    /// The one message a lean worker's supervisor waits for. The drain
    /// budget rides in it: the tenant's derived grace less a slack, so
    /// the syncer retries a failing drain for as long as the delivery
    /// actually allows rather than for three attempts.
    async fn send_lean_launch(&self, st: &VolumeState, comm: &Path, mut env: BTreeMap<String, String>) -> Result<(), Status> {
        env.insert(
            "FLINT_SYNC_DRAIN_BUDGET_SECS".to_string(),
            st.grace_secs.unwrap_or(30).saturating_sub(5).max(6).to_string(),
        );
        let launch = Launch { mode: "lean".into(), args: vec!["run".into()], env };
        let sock = comm.join("mount.sock");
        let reply = {
            let sock = sock.clone();
            tokio::task::spawn_blocking(move || fuse::send_launch(&sock, &launch, None, LAUNCH_REPLY_WAIT)).await
        };
        match reply {
            Ok(Ok(r)) if r.ok => Ok(()),
            Ok(Ok(r)) => Err(Status::unavailable(format!("syncer refused the launch: {}", r.error.unwrap_or_default()))),
            Ok(Err(e)) => Err(Status::unavailable(format!("launch over {}: {e}", sock.display()))),
            Err(e) => Err(Status::internal(format!("launch task: {e}"))),
        }
    }

    /// A lean syncer lost at the pod level: start a new worker over the
    /// SAME tree. The tree is the volume's, not the worker's, so nothing
    /// is re-materialised; the syncer finds the checkout marker and its
    /// incarnation id and self-recognises its lease (the S14 restart
    /// arm). Nothing is cleaned up on failure: the volume is published.
    async fn relaunch_lean_worker(
        &self,
        dir: &Path,
        st: &mut VolumeState,
        pr: &PublishRequest,
        secrets: &HashMap<String, String>,
    ) -> Result<(), Status> {
        let Some(image) = self.cfg.lean_image.clone() else {
            return Err(Status::failed_precondition("this node driver has no FLINT_S3CSI_LEAN_IMAGE"));
        };
        let Some(sync_conf) = st.sync_env.clone() else {
            return Err(Status::failed_precondition(
                "the volume was published by a plugin that kept no syncer environment; recreate the tenant pod",
            ));
        };
        let mode = CredentialMode::parse(&st.credential_mode).map_err(Status::failed_precondition)?;
        tracing::warn!(volume = %st.volume_id, worker = %st.worker_name, "lean syncer pod is gone; relaunching over the existing tree");
        self.launch_lean_worker(dir, st, pr, secrets, &image, mode, &sync_conf).await
    }

    // ── lean (design §3.5, §5) ────────────────────────────────────────

    /// Passthrough is a mount; lean is a PROCESS that lives with the pod.
    /// The plugin owns the tree (so its lifetime is the VOLUME's, not the
    /// worker's), hostPaths it into a syncer worker running the unchanged
    /// `flint-sync run`, waits for the checkout marker across retried
    /// publishes, and binds the tree into the tenant.
    #[allow(clippy::too_many_arguments)]
    async fn publish_lean(
        &self,
        dir: &Path,
        vid: &str,
        target: &Path,
        pr: &PublishRequest,
        tenant: &TenantRef,
        spec: crate::lean_operator::crd::FlintLeanWorkspaceSpec,
        cred_mode: CredentialMode,
        access: Access,
        req: &csi::NodePublishVolumeRequest,
    ) -> Result<(), Status> {
        let name = pr.selector.name().to_string();
        let Some(image) = self.cfg.lean_image.clone() else {
            return Err(Status::failed_precondition(
                "this node driver has no FLINT_S3CSI_LEAN_IMAGE; FlintLeanWorkspace volumes are not enabled",
            ));
        };
        let owner_uid = pr.uid.or(spec.uid.map(|u| u as u32)).ok_or_else(|| {
            Status::failed_precondition(format!(
                "FlintLeanWorkspace {}/{name} has no spec.uid — REQUIRED under the CSI delivery: the syncer runs \
                 as the app's uid so it can read every file the app writes (set spec.uid/spec.gid, or the \
                 pod's volumeAttributes {})",
                pr.pod_namespace,
                attrs::ATTR_UID
            ))
        })?;
        let owner_gid = pr.gid.or(spec.gid.map(|g| g as u32)).unwrap_or(owner_uid);
        if owner_uid == 0 {
            return Err(Status::failed_precondition("uid 0 is refused for a lean workspace: the syncer runs non-root"));
        }
        crate::lean_operator::boundary::validate_spec(&spec)
            .map_err(|r| Status::failed_precondition(format!("FlintLeanWorkspace {}/{name}: {}", pr.pod_namespace, r.message)))?;
        let tree = dir.join("tree");
        let grace = crate::lean_operator::boundary::derived_grace_secs(&spec);
        let mut st = VolumeState {
            version: STATE_VERSION,
            volume_id: vid.to_string(),
            mode: "lean".into(),
            cr: name.clone(),
            tenant: tenant.clone(),
            target_path: target.to_string_lossy().into_owned(),
            src: tree.to_string_lossy().into_owned(),
            worker_namespace: self.cfg.worker_namespace.clone(),
            worker_name: worker::worker_name(vid),
            worker_uid: None,
            phase: "publishing".into(),
            credential_mode: cred_mode.as_str().into(),
            nonce: creds::new_nonce(),
            creds_expiration: None,
            token_expiration: None,
            last_probe_ok: None,
            published_unix: None,
            // The decided access (per-user access design §4.1): the tenant
            // bind below, the syncer's mode and the broker's credential all
            // follow it.
            read_only: access.is_read(),
            owner_uid,
            owner_gid,
            grace_secs: Some(grace),
            tree_image: None,
            drain_started_unix: None,
            sync_env: None,
            on_behalf_of: pr.on_behalf_of.clone(),
            shared: None,
        };
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;

        // The tree: plugin-owned, owned by the syncer's uid, world-writable
        // with the sticky bit so an app uid the CR does not name can still
        // create files (design §3.5 step 6).
        if let Err(e) = std::fs::create_dir_all(&tree) {
            return Err(self.fail(dir, &st, Status::internal(format!("tree {}: {e}", tree.display()))).await);
        }
        // The CEILING. `sizeLimitGib` was an emptyDir sizeLimit under the
        // webhooks, where kubelet enforced it; here the tree is a
        // directory on the node's root filesystem, so the limit is a
        // loop-mounted ext4 image and an overrun is ENOSPC in the
        // tenant's own write. Refusing to publish when the ceiling cannot
        // be built is deliberate: silently serving an UNBOUNDED tree to a
        // workspace that asked for a bound is how a single agent fills a
        // node's disk and takes the kubelet with it.
        let quota_gib = if self.cfg.quota { spec.size_limit_gib } else { 0 };
        if quota_gib > 0 {
            match quota::ensure(dir, &tree, quota_gib, owner_uid, owner_gid) {
                Ok(img) => {
                    st.tree_image = Some(img.to_string_lossy().into_owned());
                    st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
                }
                Err(e) => {
                    return Err(self
                        .fail(
                            dir,
                            &st,
                            Status::internal(format!(
                                "FlintLeanWorkspace {}/{name} asks for sizeLimitGib {quota_gib} and the ceiling could \
                                 not be built: {e}. Publishing without it would hand the workspace the node's whole \
                                 root filesystem. Set sizeLimitGib: 0 to accept an unbounded tree, or install the \
                                 chart with workers.quota=false (its default)",
                                pr.pod_namespace
                            )),
                        )
                        .await);
                }
            }
        } else if let Err(e) = std::os::unix::fs::chown(&tree, Some(owner_uid), Some(owner_gid))
            .and_then(|_| std::fs::set_permissions(&tree, std::os::unix::fs::PermissionsExt::from_mode(0o1777)))
        {
            return Err(self.fail(dir, &st, Status::internal(format!("tree {}: {e}", tree.display()))).await);
        }

        // The syncer's non-secret configuration goes on the POD SPEC as
        // well as into the launch message. The child gets it either way,
        // but only the pod spec is inherited by `kubectl exec`, and
        // §3.2's operator-side recipe — `kubectl -n flint-workers exec
        // <worker> -- flint-sync status` — is the only way
        // left to run those verbs now that no tenant can. With the list
        // in the launch message alone every one of them exits 2 with
        // "FLINT_SYNC_BUCKET is required" (found by S12). Credentials are
        // NOT here: they stay in the launch message and the comm dir.
        // The list is also kept in the volume state, so a worker lost at
        // the pod level can be relaunched from a republish.
        let ws = crate::lean_operator::crd::FlintLeanWorkspace::new(&name, spec);
        let mut sync_conf: BTreeMap<String, String> =
            crate::lean_operator::sync_env::sync_env(&ws, SYNCER_ROOT).into_iter().collect();
        sync_conf.insert("FLINT_SYNC_NAMESPACE".into(), pr.pod_namespace.clone());
        lean_access_env(st.read_only, &mut sync_conf);
        st.sync_env = Some(sync_conf.clone());
        if let Err(e) = self.launch_lean_worker(dir, &mut st, pr, &req.secrets, &image, cred_mode, &sync_conf).await {
            return Err(self.fail(dir, &st, e).await);
        }
        st.phase = "checking-out".into();
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
        self.resume_lean(dir, st, target).await
    }

    /// Create (or adopt) the syncer worker, wait for it, hand it its
    /// credential and its launch. Shared by the first publish and by a
    /// relaunch over a live tree, so it never cleans up on failure: the
    /// caller decides what a failure means for the volume.
    #[allow(clippy::too_many_arguments)]
    async fn launch_lean_worker(
        &self,
        dir: &Path,
        st: &mut VolumeState,
        pr: &PublishRequest,
        secrets: &HashMap<String, String>,
        image: &str,
        cred_mode: CredentialMode,
        sync_conf: &BTreeMap<String, String>,
    ) -> Result<(), Status> {
        let (run_as, run_as_gid) = worker_owner(st);
        let mut pod_env = sync_conf.clone();
        pod_env.insert("FLINT_S3W_MODE".to_string(), "lean".to_string());
        let pod = worker::build_pod(&WorkerInputs {
            namespace: self.cfg.worker_namespace.clone(),
            node_name: self.cfg.node_name.clone(),
            node_uid: self.cfg.node_uid.clone(),
            image: image.to_string(),
            mode: "lean",
            volume_id: &st.volume_id,
            tenant: &st.tenant,
            cr: &st.cr,
            run_as_uid: run_as,
            run_as_gid,
            resources: self.cfg.worker_resources.clone(),
            prestop_secs: self.cfg.prestop_secs,
            env: pod_env,
            lean_tree_hostpath: Some(st.src.clone()),
            grace_secs: Some(st.grace_secs.unwrap_or(30) as i64),
            priority_class: self.cfg.priority_class.clone(),
            comm_size: self.cfg.comm_size.clone(),
            scratch_size: self.cfg.scratch_size.clone(),
            shared_key: None,
        });
        worker::ensure(&self.client, &pod).await.map_err(Status::unavailable)?;
        let worker_uid = match worker::wait_running(&self.client, &st.worker_namespace, &st.worker_name, WORKER_RUNNING_WAIT).await {
            Ok(WaitOutcome::Running { uid }) => uid,
            Ok(WaitOutcome::Failed { reason, message }) => return Err(self.worker_refused(st, "syncer", &reason, &message).await),
            Ok(WaitOutcome::Timeout { phase }) => {
                return Err(Status::unavailable(format!(
                    "syncer pod {} not Running after {}s ({phase}); retrying",
                    st.worker_name,
                    WORKER_RUNNING_WAIT.as_secs()
                )))
            }
            Err(e) => return Err(Status::unavailable(e)),
        };
        st.worker_uid = Some(worker_uid.clone());
        let comm = worker::comm_dir(&self.cfg.kubelet_root, &worker_uid);
        if !comm.is_dir() {
            return Err(Status::unavailable(format!("syncer comm dir {} not visible on the node yet", comm.display())));
        }
        let mat = self.credential(dir, cred_mode, pr, st, secrets).await?;
        creds::write_files(&comm, &mat.files, worker_owner(st)).map_err(|e| Status::internal(format!("write comm files: {e}")))?;

        // The syncer's environment: the same fixed list the webhook
        // stamped, with the tenant namespace as a LITERAL (design §5),
        // plus the credentials, which live only here and never on the pod.
        let mut env = sync_conf.clone();
        env.extend(mat.env);
        self.send_lean_launch(st, &comm, env).await?;
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
        Ok(())
    }

    /// Tell a worker its volume is released, so its preStop hook stops
    /// holding it open. Written BEFORE the delete on every unpublish
    /// path: on the ordinary path the hook then costs a single stat,
    /// and on a drain — where the worker is terminated by someone else
    /// entirely — its absence is what keeps the mount alive until the
    /// tenant is actually gone. Best effort by design: a worker whose
    /// comm dir has already vanished has nothing left to wait for.
    fn release_worker(&self, st: &VolumeState) {
        self.release_worker_uid(&st.volume_id, st.worker_uid.as_deref());
    }

    fn release_worker_uid(&self, label: &str, worker_uid: Option<&str>) {
        let Some(uid) = worker_uid else { return };
        let marker = worker::comm_dir(&self.cfg.kubelet_root, uid).join("released");
        match std::fs::write(&marker, b"released\n") {
            Ok(()) => tracing::debug!(volume = label, marker = %marker.display(), "released the worker's preStop hook"),
            Err(e) => tracing::warn!(volume = label, marker = %marker.display(), "could not write the release marker: {e}"),
        }
    }

    /// Bring a passthrough mounter down in the order F72 requires
    /// (docs/f72-passthrough-teardown-kills-the-mounter-before-quiescing-it.md).
    /// The tenant's bind is already gone (the caller's). Detach the ro
    /// stage and the source: the kernel aborts the FUSE connection, and
    /// mount-s3, left alone, finishes the RELEASE it may still be serving
    /// — a deferred upload, completed there when the file's last close
    /// came from a process other than its opener — and exits by itself.
    /// WAIT for that exit, bounded by `quiesce_secs`, and only then
    /// disarm the preStop hook and delete the pod: the SIGTERM those two
    /// send makes mount-s3 return without joining its threads, which is
    /// what cut the upload before. At the ceiling the old order applies
    /// (marker, delete with 10 s), so the worst case is what it always
    /// was, and the log says which happened and why it matters.
    async fn teardown_mounter(&self, label: &str, src: &Path, ns: &str, name: &str, worker_uid: Option<&str>) -> Result<(), Status> {
        unmount_all(&fuse::ro_stage_of(src)).map_err(|e| Status::internal(format!("unmount ro stage: {e}")))?;
        unmount_all(src).map_err(|e| Status::internal(format!("unmount source: {e}")))?;
        // A worker that never reached Running (a create kept for a retry
        // that never came, found at startup) has no mounter to wait for.
        let budget = if worker_uid.is_none() { Duration::ZERO } else { Duration::from_secs(self.cfg.quiesce_secs) };
        let exited = if budget.is_zero() {
            false
        } else {
            let t0 = Instant::now();
            match worker::wait_exited(&self.client, ns, name, budget).await {
                Ok(true) => {
                    tracing::info!(volume = label, worker = name, after = ?t0.elapsed(), "the mounter exited on its own once its source was detached");
                    true
                }
                Ok(false) => {
                    tracing::warn!(
                        volume = label, worker = name, budget = ?budget,
                        "the mounter is still running after its source was detached: its FUSE superblock is still \
                         referenced from another mount namespace, or an upload is still completing — terminating it; \
                         an upload it was still completing is lost (an incomplete multipart upload stays in the bucket)"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(volume = label, worker = name, "could not watch the mounter exit ({e}); terminating it");
                    false
                }
            }
        };
        self.release_worker_uid(label, worker_uid);
        worker::delete(&self.client, ns, name, Some(if exited { 0 } else { 10 })).await.map_err(Status::unavailable)?;
        Ok(())
    }

    /// The last member out: the shared mounter, then its record.
    async fn teardown_shared(&self, sdir: &Path, sm: &SharedMount) -> Result<(), Status> {
        self.teardown_mounter(&format!("shared:{}", sm.hash), Path::new(&sm.src), &sm.worker_namespace, &sm.worker_name, sm.worker_uid.as_deref())
            .await?;
        remove_state_dir(sdir)
    }

    fn marker_path(st: &VolumeState) -> PathBuf {
        Path::new(&st.src).join(".flint-sync").join("checkout-complete")
    }

    /// Wait for the checkout marker (written LAST by the syncer), then
    /// bind. Called on the first publish and on every retried one.
    async fn resume_lean(&self, dir: &Path, mut st: VolumeState, target: &Path) -> Result<(), Status> {
        let marker = Self::marker_path(&st);
        let start = std::time::Instant::now();
        loop {
            if marker.is_file() {
                break;
            }
            // Is the syncer still alive? A syncer that exited over budget
            // (Refused, maxFiles) never writes the marker.
            match worker::wait_running(&self.client, &st.worker_namespace, &st.worker_name, Duration::from_millis(1)).await {
                Ok(WaitOutcome::Failed { reason, message }) => {
                    let detail = st
                        .worker_uid
                        .as_ref()
                        .and_then(|u| std::fs::read_to_string(worker::comm_dir(&self.cfg.kubelet_root, u).join("mount.error")).ok())
                        .unwrap_or_default();
                    // A refusal names its reason on the syncer's LAST
                    // stderr line (finding 5's claim check names both
                    // projects there). The tenant reads it from the
                    // mount event, so it leads; and it is an event of
                    // its own, since kubelet may cut the mount message.
                    let status = if reason == "Refused" {
                        let why = detail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                        self.emit_event(
                            &st.tenant,
                            "SyncerRefused",
                            &format!("{}: the syncer refused and its worker was torn down, not relaunched — {why}", st.cr),
                            true,
                        )
                        .await;
                        Status::failed_precondition(format!("syncer {} refused: {why} ({message})", st.worker_name))
                    } else {
                        Status::failed_precondition(format!("syncer {}: {reason} {message} {}", st.worker_name, detail.trim()))
                    };
                    return Err(self.fail(dir, &st, status).await);
                }
                Err(e) => return Err(Status::unavailable(e)),
                Ok(WaitOutcome::Running { .. }) | Ok(WaitOutcome::Timeout { .. }) => {}
            }
            if start.elapsed() > MARKER_WAIT {
                let _ = st.save(dir);
                return Err(Status::unavailable(format!(
                    "checkout of {} in progress (no {} yet after {}s this attempt; the syncer keeps going and kubelet retries)",
                    st.cr,
                    marker.display(),
                    MARKER_WAIT.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !fuse::is_mountpoint(target).unwrap_or(false) {
            if let Err(e) = fuse::bind_mount(Path::new(&st.src), target, st.read_only) {
                return Err(self.fail(dir, &st, Status::internal(format!("bind: {e}"))).await);
            }
        }
        st.phase = "published".into();
        st.published_unix = Some(chrono::Utc::now().timestamp() as u64);
        st.last_probe_ok = Some(true);
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
        tracing::info!(volume = %st.volume_id, cr = %st.cr, tenant = %format!("{}/{}", st.tenant.namespace, st.tenant.pod), worker = %st.worker_name, "published (lean)");
        Ok(())
    }

    /// Keys for the WHOLE drain, exchanged while the pod object still
    /// exists. `NodeUnpublishVolume` carries no token; the one kept at
    /// the last republish serves. Under a normal delete it is valid
    /// until delete time + the tenant's grace + 60 s, and this runs
    /// inside the tenant's grace, so it succeeds. Under `--force` it is
    /// already dead, the drain runs on the remaining key life, and the
    /// caller's event says so. Before this the drain ran on whatever
    /// the last republish left, 270-900 s, and a hybrid workspace with
    /// a long floor could outlive its key mid-drain.
    async fn refresh_for_drain(&self, dir: &Path, st: &mut VolumeState, grace: u64) -> Result<(), String> {
        if CredentialMode::parse(&st.credential_mode).ok() != Some(CredentialMode::Broker) {
            return Ok(());
        }
        let Some(broker) = &self.cfg.broker else { return Ok(()) };
        let Some(token) = state::load_token(dir) else {
            return Err("no token kept beside the volume state (published by an earlier plugin version?)".into());
        };
        let Some(uid) = st.worker_uid.clone() else { return Err("the volume state names no worker".into()) };
        let lifetime = creds::drain_key_lifetime_secs(grace, self.cfg.creds_lifetime_secs);
        broker.register(&self.registration_of(st)).await.map_err(|e| format!("re-registration: {e}"))?;
        let c = broker
            .exchange(&token, &creds::role_arn(&st.mode, &st.cr), &st.nonce, lifetime)
            .await
            .map_err(|e| e.to_string())?;
        let comm = worker::comm_dir(&self.cfg.kubelet_root, &uid);
        let f = creds::CommFile { name: creds::CREDS_FILE.into(), bytes: creds::creds_json(&c), mode: 0o600 };
        creds::write_files(&comm, &[f], worker_owner(st)).map_err(|e| format!("write creds: {e}"))?;
        st.creds_expiration = Some(c.expiration.clone());
        tracing::info!(volume = %st.volume_id, asked = lifetime, exp = %c.expiration, "drain-length credential in place");
        Ok(())
    }

    /// An UNDRAINED tree is never removed: whatever the tenant wrote
    /// since the last boundary exists nowhere else. The volume directory
    /// is moved out of `volumes/` (adoption and retries ignore it), the
    /// quota image kept, and the tenant told where it is. Before this
    /// the same paths deleted the tree seconds after an event that
    /// pointed the operator at it.
    async fn preserve_undrained(&self, dir: &Path, st: &VolumeState, target: &Path, why: &str) -> Result<(), Status> {
        unmount_all(target).map_err(|e| Status::internal(format!("unmount target: {e}")))?;
        unmount_all(&fuse::ro_stage_of(Path::new(&st.src))).map_err(|e| Status::internal(format!("unmount ro stage: {e}")))?;
        if st.tree_image.is_some() {
            quota::unmount_tree(Path::new(&st.src)).map_err(|e| Status::unavailable(format!("unmount tree: {e}; retrying")))?;
        }
        // The pod-bound token has no use in a preserved tree (its pod is
        // gone, and TokenReview refuses it within the minute); the tree,
        // the image and the state are what an operator needs.
        let _ = std::fs::remove_file(dir.join(state::TOKEN_FILE));
        let dest = state::undrained_dir(&self.cfg.plugin_root, &st.volume_id, chrono::Utc::now().timestamp() as u64);
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p).map_err(|e| Status::internal(format!("{}: {e}", p.display())))?;
        }
        match std::fs::rename(dir, &dest) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                return Err(Status::unavailable(format!("preserve {}: {e} — a mount is still under it; retrying", dir.display())))
            }
            Err(e) => return Err(Status::internal(format!("preserve {} at {}: {e}", dir.display(), dest.display()))),
        }
        if let Some(b) = &self.cfg.broker {
            let _ = b.deregister(&st.volume_id).await;
        }
        let shape = if st.tree_image.is_some() { "an ext4 image, tree.img: mount it loop to read" } else { "the tree/ directory" };
        tracing::error!(volume = %st.volume_id, preserved = %dest.display(), "undrained lean tree preserved: the syncer {why}");
        self.emit_event(
            &st.tenant,
            "UndrainedTreePreserved",
            &format!(
                "{}: the syncer {why}. Nothing written since the last boundary was published. The tree is preserved on \
                 node {} at {} ({}); copy out what matters, then remove that directory. Objects that did upload \
                 before the failure remain in the bucket under their keys.",
                st.cr,
                self.cfg.node_name,
                dest.display(),
                shape
            ),
            true,
        )
        .await;
        Ok(())
    }

    /// The final drain (design §3.5): delete the syncer with the derived
    /// grace so its SIGTERM arm runs over a tree every tenant container
    /// has already left, wait, then unmount and remove the tree. A tree
    /// that was NOT drained — syncer gone or exited before we asked, or
    /// killed at the ceiling — is preserved, never removed.
    async fn unpublish_lean(&self, dir: &Path, mut st: VolumeState, target: &Path) -> Result<(), Status> {
        let grace = st.grace_secs.unwrap_or(30);
        let now = chrono::Utc::now().timestamp() as u64;
        let started = match st.drain_started_unix {
            Some(t) => t,
            None => {
                match worker::phase(&self.client, &st.worker_namespace, &st.worker_name).await.map_err(Status::unavailable)? {
                    None => {
                        tracing::warn!(volume = %st.volume_id, "syncer was already gone at unpublish — nothing drained");
                        self.emit_event(&st.tenant, "SyncerGoneAtUnpublish", &format!("{}: the syncer was not running when the pod's volume was unpublished, so no final drain ran; its tree is preserved (see UndrainedTreePreserved)", st.cr), true).await;
                        return self
                            .preserve_undrained(dir, &st, target, "was not running when the volume was unpublished (evicted, or removed), so no final drain ran")
                            .await;
                    }
                    Some(p) if p == "Succeeded" || p == "Failed" => {
                        // Exited on its own before anyone asked it to
                        // drain: refused, or evicted with the
                        // object left behind. Whatever it did not
                        // publish is still in the tree.
                        self.release_worker(&st);
                        let _ = worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(0)).await;
                        return self
                            .preserve_undrained(dir, &st, target, &format!("had already exited ({p}) when the volume was unpublished, so no final drain ran"))
                            .await;
                    }
                    Some(_) => {}
                }
                if let Err(e) = self.refresh_for_drain(dir, &mut st, grace).await {
                    tracing::warn!(volume = %st.volume_id, "drain credential refresh: {e}");
                    self.emit_event(
                        &st.tenant,
                        "DrainCredentialRefreshFailed",
                        &format!("{}: could not exchange for a drain-length key ({e}); the drain runs on the current key's remaining life", st.cr),
                        true,
                    )
                    .await;
                }
                self.release_worker(&st);
                worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(grace as u32)).await.map_err(Status::unavailable)?;
                tracing::info!(volume = %st.volume_id, grace, "draining: syncer deleted with the derived grace");
                st.drain_started_unix = Some(now);
                let _ = st.save(dir);
                now
            }
        };
        let start = std::time::Instant::now();
        loop {
            if worker::is_gone(&self.client, &st.worker_namespace, &st.worker_name).await.map_err(Status::unavailable)? {
                break;
            }
            let elapsed = chrono::Utc::now().timestamp() as u64 - started;
            if elapsed > grace + 30 {
                tracing::warn!(volume = %st.volume_id, elapsed, "drain past its ceiling; killing the syncer");
                self.release_worker(&st);
                worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(0)).await.map_err(Status::unavailable)?;
                self.emit_event(&st.tenant, "DrainCeilingHit", &format!("{}: the syncer did not finish its drain within {}s; it was killed and its tree is preserved (see UndrainedTreePreserved)", st.cr, grace + 30), true).await;
                return self
                    .preserve_undrained(dir, &st, target, &format!("did not finish its drain within {}s and was killed", grace + 30))
                    .await;
            }
            if start.elapsed() > DRAIN_WAIT {
                return Err(Status::unavailable(format!("draining {} ({}s of {}s); retrying", st.cr, elapsed, grace)));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        // The pod is gone. That says nothing about whether the drain
        // PUBLISHED: a drain that failed every attempt exits 1 and a
        // refused one exits EXIT_REFUSED, and both leave the pod just as gone as a
        // drain that succeeded. The syncer attests a completed drain in
        // the tree, and only that attestation lets the tree go
        // (audit 2026-09-03, finding 3).
        if !drain_attested(Path::new(&st.src), started) {
            self.emit_event(
                &st.tenant,
                "DrainNotAttested",
                &format!(
                    "{}: the syncer exited without attesting a completed drain (every attempt failed, or it was \
                     refused), so what it did not publish is still in its tree, which is preserved (see \
                     UndrainedTreePreserved)",
                    st.cr
                ),
                true,
            )
            .await;
            return self
                .preserve_undrained(
                    dir,
                    &st,
                    target,
                    "exited without attesting a completed drain (every attempt failed), so what it did not publish is still in the tree",
                )
                .await;
        }
        unmount_all(target).map_err(|e| Status::internal(format!("unmount target: {e}")))?;
        unmount_all(&fuse::ro_stage_of(Path::new(&st.src))).map_err(|e| Status::internal(format!("unmount ro stage: {e}")))?;
        // The ceiling comes down AFTER the drain and after the tenant's
        // bind is gone: it is the filesystem the tree lives on, so
        // unmounting it earlier would pull the floor out from under a
        // syncer still publishing.
        if let Some(img) = st.tree_image.clone() {
            if let Err(e) = quota::teardown(Path::new(&st.src), Path::new(&img)) {
                return Err(Status::unavailable(format!("tree quota teardown: {e}; retrying")));
            }
        }
        if let Some(b) = &self.cfg.broker {
            if let Err(e) = b.deregister(&st.volume_id).await {
                tracing::warn!(volume = %st.volume_id, "deregister: {e}");
            }
        }
        remove_state_dir(dir)?;
        tracing::info!(volume = %st.volume_id, "unpublished (lean)");
        Ok(())
    }

    // ── unpublish ────────────────────────────────────────────────────

    async fn unpublish(&self, req: csi::NodeUnpublishVolumeRequest) -> Result<(), Status> {
        let vid = req.volume_id.clone();
        if vid.is_empty() || req.target_path.is_empty() {
            return Err(Status::invalid_argument("volume_id and target_path are required"));
        }
        let _guard = self.lock(&vid).await?;
        let dir = super::state::volume_dir(&self.cfg.plugin_root, &vid);
        let target = Path::new(&req.target_path);
        let Some(st) = VolumeState::load(&dir).map_err(|e| Status::internal(format!("state: {e}")))? else {
            // Nothing of ours: make sure the target is not a mount and go.
            if fuse::is_mountpoint(target).unwrap_or(false) {
                fuse::unmount(target, true).map_err(|e| Status::internal(e.to_string()))?;
            }
            // A volume directory with no state file is a HALF-REMOVED
            // volume: a removal that unlinked the file and then failed
            // on a mount still stacked under `src` or `tree`, retried by
            // kubelet after the file was gone. Finish it here, or the
            // mount outlives the node (the shape S9 + SU found on EC2).
            if dir.is_dir() {
                for sub in [fuse::RO_STAGE, "src", "tree"] {
                    let p = dir.join(sub);
                    if let Err(e) = unmount_all(&p) {
                        return Err(Status::unavailable(format!("half-removed volume {vid}: {e}")));
                    }
                }
                remove_state_dir(&dir)?;
                tracing::warn!(volume = %vid, "finished removing a half-removed volume directory");
            }
            return Ok(());
        };
        if st.mode == "lean" {
            return self.unpublish_lean(&dir, st, target).await;
        }
        // Passthrough teardown: the tenant's bind; then the mounter, in
        // F72's order — as the owner of its own, or as the LAST member of
        // a shared one; then registration and state.
        unmount_all(target).map_err(|e| Status::internal(format!("unmount target: {e}")))?;
        if let Some(hash) = st.shared.clone() {
            let _class = self.lock(&share_lock_key(&hash)).await?;
            let sdir = state::shared_dir(&self.cfg.plugin_root, &hash);
            match SharedMount::load(&sdir).map_err(|e| Status::internal(format!("shared record: {e}")))? {
                Some(mut sm) => {
                    sm.remove_member(&vid);
                    if sm.members.is_empty() {
                        tracing::info!(volume = %vid, shared = %hash, "last member out: bringing the shared mounter down");
                        // Not saved first: a teardown that fails (retryable)
                        // is retried from a record that still names us.
                        self.teardown_shared(&sdir, &sm).await?;
                    } else {
                        sm.save(&sdir).map_err(|e| Status::internal(format!("shared record: {e}")))?;
                        tracing::info!(volume = %vid, shared = %hash, members = sm.members.len(), "left the shared mount; its mounter stays for the others");
                    }
                }
                None => tracing::warn!(volume = %vid, shared = %hash, "no shared record at {}; nothing to leave", sdir.display()),
            }
        } else {
            self.teardown_mounter(&vid, Path::new(&st.src), &st.worker_namespace, &st.worker_name, st.worker_uid.as_deref()).await?;
        }
        if let Some(b) = &self.cfg.broker {
            if let Err(e) = b.deregister(&vid).await {
                tracing::warn!(volume = %vid, "deregister: {e}");
            }
        }
        remove_state_dir(&dir)?;
        tracing::info!(volume = %vid, shared = st.shared.is_some(), "unpublished");
        Ok(())
    }
}

impl S3Node {
    /// The publish registration the broker keys its exchanges on (design
    /// §4.2): everything in it is kubelet-asserted at publish and kept in
    /// the volume state, so a refresh can re-assert it after a broker
    /// restart.
    fn registration_of(&self, st: &VolumeState) -> Registration {
        Registration {
            volume_id: st.volume_id.clone(),
            pod_uid: st.tenant.pod_uid.clone(),
            namespace: st.tenant.namespace.clone(),
            pod: st.tenant.pod.clone(),
            service_account: st.tenant.service_account.clone(),
            cr: st.cr.clone(),
            mode: st.mode.clone(),
            nonce: st.nonce.clone(),
            node: self.cfg.node_name.clone(),
            access: registered_access(st),
            on_behalf_of: st.on_behalf_of.clone(),
        }
    }
}

/// Unmount `path` until it is a plain directory (stacked mounts from a
/// retried publish need one `umount2` each), bounded.
/// Unmount `path` until it is no longer a mount point (a stack of binds
/// is possible), judged by the MOUNT TABLE and nothing else. Not
/// `path.exists()` first: a dead FUSE mount answers `stat` with
/// `ENOTCONN`, so `exists()` is false for exactly the mount that most
/// needs unmounting. With that test in front, S9's killed mounter on a
/// real node (EC2, 2026-09-04) left its source mounted for the life of
/// the node: the loop returned Ok, the state file was removed, the
/// directory removal failed on the busy mount, and every kubelet retry
/// found no state and did nothing. `is_mountpoint` reports a dead mount
/// as a mount and a missing path as not one, which is the whole answer.
/// The worker's `mount.error`, polled for up to `budget` because the
/// supervisor writes it AFTER the death the caller has already observed.
async fn wait_for_mount_error(comm: &Path, budget: Duration) -> String {
    let path = comm.join("mount.error");
    let start = std::time::Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(&path) {
            if !s.trim().is_empty() {
                return s;
            }
        }
        if start.elapsed() > budget {
            return String::new();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn unmount_all(path: &Path) -> std::io::Result<()> {
    for _ in 0..8 {
        if !fuse::is_mountpoint(path).unwrap_or(false) {
            return Ok(());
        }
        fuse::unmount(path, true)?;
    }
    Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{} is still a mount point after 8 unmounts", path.display())))
}

/// Remove the volume's state directory. `EBUSY` means a mount is still
/// stacked somewhere under it — retryable (`Unavailable`), and named,
/// rather than an `Internal` kubelet backs off on for two minutes.
fn remove_state_dir(dir: &Path) -> Result<(), Status> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => Err(Status::unavailable(format!(
            "remove state {}: {e} — a mount is still stacked under it; retrying",
            dir.display()
        ))),
        Err(e) => Err(Status::internal(format!("remove state {}: {e}", dir.display()))),
    }
}

/// The worker's memory limit as the chart wrote it (`limits.memory`),
/// the quantity the mounter's `--memory-target` is derived from.
pub fn worker_memory_limit(r: Option<&ResourceRequirements>) -> Option<String> {
    r?.limits.as_ref()?.get("memory").map(|q| q.0.clone())
}

/// Whether a REFUSED refresh takes the credential file away (design
/// §4.6: the door answers 503 and the client fails at the old key's
/// expiry). Only for a mounter of the pod's own. A shared class's file
/// serves every member, and every member could have minted the same key:
/// removing it would cut the innocent members' reads until the next
/// member's republish re-minted it (up to ~90 s, an EIO in a mount-s3
/// that refreshes inside that window) while the refused pod, bound to
/// the same superblock, read on regardless. Per-member revocation is
/// documented on the CRD field as impossible; the plugin must not
/// pretend otherwise at the others' expense.
pub fn revocation_removes_the_key(st: &VolumeState) -> bool {
    st.shared.is_none()
}

/// The uid/gid the worker RUNS as (never root: a root owner maps to
/// nobody for the process, while `--uid 0` still presents root).
fn worker_owner(st: &VolumeState) -> (u32, u32) {
    (
        if st.owner_uid == 0 { DEFAULT_OWNER } else { st.owner_uid },
        if st.owner_gid == 0 { DEFAULT_OWNER } else { st.owner_gid },
    )
}

/// A refusal is the tenant's to fix; an outage is kubelet's to retry.
fn exchange_status(cr: &str, e: ExchangeError) -> Status {
    match e {
        ExchangeError::Refused(m) => Status::permission_denied(format!("credential exchange for {cr} refused: {m}")),
        ExchangeError::Outage(m) => Status::unavailable(format!("credential exchange for {cr}: {m}; retrying")),
    }
}

/// What startup adoption does with a volume it finds on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptAction {
    /// Published: the binds are in the host mount table, the worker is
    /// its own pod; nothing to redo.
    Keep,
    /// A lean checkout in progress: the syncer is its own pod and keeps
    /// going; kubelet's next retry waits on the marker (§3.5 step 8).
    /// Cleaning this up killed the checkout on every plugin roll.
    KeepCheckingOut,
    /// A publish that never finished: kubelet retries it from clean.
    Cleanup,
}

pub fn adopt_action(st: &VolumeState) -> AdoptAction {
    if st.phase == "published" {
        return AdoptAction::Keep;
    }
    if st.mode == "lean" && st.phase == "checking-out" {
        return AdoptAction::KeepCheckingOut;
    }
    AdoptAction::Cleanup
}

/// What a `NodePublishVolume` does with a volume that already has state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishedAction {
    /// The target is mounted: refresh and probe.
    Republish,
    /// The target is gone but the source lives: bind it again. `remount`
    /// puts a lost quota loop mount back first.
    Rebind { remount: bool },
    /// A published lean workspace with no tree to bind: refuse. Never
    /// start over — that would replace the tenant's live tree.
    RefuseLean,
    /// A lean checkout between kubelet's attempts: keep waiting.
    ResumeCheckout,
    /// The worker from the previous attempt is still coming up: publish
    /// again from the top and adopt it (`worker::ensure`), rather than
    /// deleting it and starting its image pull's backoff over.
    ResumeWorker,
    /// An unfinished publish: clean up and start over.
    StartOver,
}

/// The access a publish registers at the broker, which narrows it again by
/// the CR at every exchange. A read-only volume registers `read`, so its
/// credential cannot write where the broker's backend can scope one:
/// the mount's flag and the syncer's mode are the presentation, the
/// credential is the enforcement (design D1).
pub fn registered_access(st: &VolumeState) -> Access {
    if st.read_only {
        Access::Read
    } else {
        Access::ReadWrite
    }
}

/// Whether a passthrough publish joins the CR's shared mounter on this
/// node, or gets its own. `Own` names why, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    Shared,
    Own(&'static str),
    /// The CR asked and the member qualifies, but the fact the decision
    /// turns on could not be read right now: answer `Unavailable` so
    /// kubelet retries, rather than silently giving the pod a mounter of
    /// its own for its whole life.
    Retry(&'static str),
}

/// The sharing decision (docs/plans/passthrough-read-only-mount-sharing.md
/// §5, §7): the CR must ask; the member must be READ-ONLY; and its
/// credential must be a function of the CR alone, so that every member
/// holds identical authority and sharing grants none of them anything —
/// ambient (the node's own chain), or the broker on a backend whose read
/// grant is CR-scoped: `sts` (a session policy on the CR's prefix) or
/// `static` (the one read key). The `rest` door scopes to the pod it was
/// minted for, a `static` identity is the pod's own Secret, and
/// `webIdentity` binds the worker to one pod's token: none of those may
/// share. An unknown backend is an unknown authority: a mounter per pod.
pub fn sharing_decision(cr_shares: bool, read_only: bool, mode: CredentialMode, broker_backend: Option<&str>) -> Sharing {
    if !cr_shares {
        return Sharing::Own("the CR does not ask for it (spec.sharing.readOnly)");
    }
    if !read_only {
        return Sharing::Own("a read-write consumer keeps its own mounter: only read-only members hold identical authority");
    }
    match mode {
        CredentialMode::Ambient => Sharing::Shared,
        CredentialMode::Broker => match broker_backend {
            Some("sts") | Some("static") => Sharing::Shared,
            Some("rest") => Sharing::Own("the broker's rest backend scopes a credential to the pod it was minted for, so members would not hold identical authority"),
            Some(_) => Sharing::Own("the broker reports a backend this plugin does not know"),
            None => Sharing::Retry("the broker's backend could not be read (GET /v1/status); sharing is only safe when every member's credential is known to carry the same authority, so the decision waits for the broker"),
        },
        CredentialMode::Static => Sharing::Own("identity.mode static: the key is the pod's own nodePublishSecretRef"),
        CredentialMode::WebIdentity => Sharing::Own("identity.mode webIdentity binds the worker to one pod's token"),
    }
}

/// A lean volume published `readOnly` runs its syncer with read access
/// (`FLINT_SYNC_ACCESS=read`): no fence, no barrier, `publish` answered
/// `refused-read-only`. Decided from the same flag as the tenant's bind,
/// and saved with the launch list, so a relaunched worker cannot come back
/// as a writer behind a read-only mount. The syncer's own worker view of
/// the tree stays read-write either way: a reader still writes what it
/// pulls, and the two binds are independent.
pub fn lean_access_env(read_only: bool, env: &mut BTreeMap<String, String>) {
    if read_only {
        env.insert("FLINT_SYNC_ACCESS".to_string(), "read".to_string());
    } else {
        env.remove("FLINT_SYNC_ACCESS");
    }
}

pub fn published_action(st: &VolumeState, target_mounted: bool, src_mounted: bool, tree_exists: bool) -> PublishedAction {
    let lean = st.mode == "lean";
    if st.phase == "published" {
        if target_mounted {
            return PublishedAction::Republish;
        }
        if lean {
            if src_mounted {
                return PublishedAction::Rebind { remount: false };
            }
            if st.tree_image.is_some() {
                // The image holds the tree; its mount is what is missing.
                return PublishedAction::Rebind { remount: true };
            }
            if tree_exists {
                // A quota-off tree is a plain directory: it fails the
                // mount-point test and IS the live tree.
                return PublishedAction::Rebind { remount: false };
            }
            return PublishedAction::RefuseLean;
        }
        // Passthrough: a source that is not a mount is a dead mount.
        return if src_mounted { PublishedAction::Rebind { remount: false } } else { PublishedAction::StartOver };
    }
    if lean && st.phase == "checking-out" {
        return PublishedAction::ResumeCheckout;
    }
    if st.phase == "worker-pending" {
        return PublishedAction::ResumeWorker;
    }
    PublishedAction::StartOver
}

fn refusal_status(r: Refusal) -> Status {
    match r {
        Refusal::NotFound(m) => Status::not_found(m),
        Refusal::Invalid(m) => Status::failed_precondition(m),
        Refusal::Forbidden(m) => Status::permission_denied(m),
        Refusal::Transient(m) => Status::unavailable(m),
    }
}

// ── the gRPC surface ─────────────────────────────────────────────────

#[tonic::async_trait]
impl csi::node_server::Node for S3Node {
    async fn node_publish_volume(
        &self,
        request: Request<csi::NodePublishVolumeRequest>,
    ) -> Result<Response<csi::NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        // Never log volume_context wholesale: it carries the token.
        tracing::debug!(volume = %req.volume_id, target = %req.target_path, "NodePublishVolume");
        self.publish(req).await.map_err(|s| {
            tracing::warn!("NodePublishVolume refused: {} — {}", s.code(), s.message());
            s
        })?;
        Ok(Response::new(csi::NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<csi::NodeUnpublishVolumeRequest>,
    ) -> Result<Response<csi::NodeUnpublishVolumeResponse>, Status> {
        let req = request.into_inner();
        tracing::debug!(volume = %req.volume_id, target = %req.target_path, "NodeUnpublishVolume");
        self.unpublish(req).await?;
        Ok(Response::new(csi::NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_capabilities(
        &self,
        _: Request<csi::NodeGetCapabilitiesRequest>,
    ) -> Result<Response<csi::NodeGetCapabilitiesResponse>, Status> {
        // No STAGE_UNSTAGE (kubelet then skips NodeStage), no stats in v1.
        Ok(Response::new(csi::NodeGetCapabilitiesResponse { capabilities: vec![] }))
    }

    async fn node_get_info(&self, _: Request<csi::NodeGetInfoRequest>) -> Result<Response<csi::NodeGetInfoResponse>, Status> {
        Ok(Response::new(csi::NodeGetInfoResponse {
            node_id: self.cfg.node_name.clone(),
            max_volumes_per_node: 0,
            accessible_topology: None,
        }))
    }

    async fn node_stage_volume(&self, _: Request<csi::NodeStageVolumeRequest>) -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeStageVolume: ephemeral inline volumes are not staged"))
    }
    async fn node_unstage_volume(&self, _: Request<csi::NodeUnstageVolumeRequest>) -> Result<Response<csi::NodeUnstageVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeUnstageVolume"))
    }
    async fn node_get_volume_stats(&self, _: Request<csi::NodeGetVolumeStatsRequest>) -> Result<Response<csi::NodeGetVolumeStatsResponse>, Status> {
        Err(Status::unimplemented("NodeGetVolumeStats"))
    }
    async fn node_expand_volume(&self, _: Request<csi::NodeExpandVolumeRequest>) -> Result<Response<csi::NodeExpandVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeExpandVolume"))
    }
}

pub struct S3Identity;

#[tonic::async_trait]
impl csi::identity_server::Identity for S3Identity {
    async fn get_plugin_info(&self, _: Request<csi::GetPluginInfoRequest>) -> Result<Response<csi::GetPluginInfoResponse>, Status> {
        Ok(Response::new(csi::GetPluginInfoResponse {
            name: DRIVER_NAME.into(),
            vendor_version: env!("CARGO_PKG_VERSION").into(),
            manifest: HashMap::new(),
        }))
    }
    async fn get_plugin_capabilities(
        &self,
        _: Request<csi::GetPluginCapabilitiesRequest>,
    ) -> Result<Response<csi::GetPluginCapabilitiesResponse>, Status> {
        // Node-only: no CONTROLLER_SERVICE, no topology.
        Ok(Response::new(csi::GetPluginCapabilitiesResponse { capabilities: vec![] }))
    }
    async fn probe(&self, _: Request<csi::ProbeRequest>) -> Result<Response<csi::ProbeResponse>, Status> {
        Ok(Response::new(csi::ProbeResponse { ready: Some(true) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_to_the_right_grpc_codes() {
        assert_eq!(refusal_status(Refusal::NotFound("x".into())).code(), tonic::Code::NotFound);
        assert_eq!(refusal_status(Refusal::Invalid("x".into())).code(), tonic::Code::FailedPrecondition);
        assert_eq!(refusal_status(Refusal::Forbidden("x".into())).code(), tonic::Code::PermissionDenied);
        assert_eq!(refusal_status(Refusal::Transient("x".into())).code(), tonic::Code::Unavailable);
    }

    #[test]
    fn config_requires_the_image_and_node_name() {
        std::env::remove_var("FLINT_S3CSI_NODE_NAME");
        std::env::remove_var("FLINT_S3CSI_PASSTHROUGH_IMAGE");
        assert!(Config::from_env().unwrap_err().contains("FLINT_S3CSI_NODE_NAME"));
        std::env::set_var("FLINT_S3CSI_NODE_NAME", "n");
        assert!(Config::from_env().unwrap_err().contains("FLINT_S3CSI_PASSTHROUGH_IMAGE"));
        std::env::set_var("FLINT_S3CSI_PASSTHROUGH_IMAGE", "img");
        let c = Config::from_env().unwrap();
        assert!(c.shared_worker_resources.is_none(), "unset ⇒ the shared mounter gets every worker's resources");
        std::env::set_var("FLINT_S3CSI_SHARED_WORKER_RESOURCES", r#"{"limits":{"memory":"4Gi"}}"#);
        let shared = Config::from_env().unwrap().shared_worker_resources.expect("parsed");
        assert_eq!(worker_memory_limit(Some(&shared)).as_deref(), Some("4Gi"));
        std::env::set_var("FLINT_S3CSI_SHARED_WORKER_RESOURCES", "not json");
        assert!(Config::from_env().unwrap_err().contains("FLINT_S3CSI_SHARED_WORKER_RESOURCES"));
        std::env::remove_var("FLINT_S3CSI_SHARED_WORKER_RESOURCES");
        assert_eq!(c.worker_namespace, "flint-workers");
        assert_eq!(c.creds_lifetime_secs, 900);
        assert!(c.broker.is_none());
        assert_eq!(c.quiesce_secs, 30, "F72: the mounter gets 30 s to exit on its own by default");
        std::env::remove_var("FLINT_S3CSI_NODE_NAME");
        std::env::remove_var("FLINT_S3CSI_PASSTHROUGH_IMAGE");
    }

    #[test]
    fn a_read_only_lean_volume_launches_its_syncer_with_read_access() {
        let mut env = BTreeMap::from([("FLINT_SYNC_BUCKET".to_string(), "b".to_string())]);
        lean_access_env(true, &mut env);
        assert_eq!(env.get("FLINT_SYNC_ACCESS").map(String::as_str), Some("read"));
        assert_eq!(env.get("FLINT_SYNC_BUCKET").map(String::as_str), Some("b"), "the rest of the list is kept");
        // A read-write volume carries no access line at all (the syncer's
        // default), and a stale one is not inherited.
        lean_access_env(false, &mut env);
        assert!(!env.contains_key("FLINT_SYNC_ACCESS"), "{env:?}");
        // "read" is the string flint-lean's `Access::parse` accepts; this
        // crate does not link flint-lean, so the syncer's
        // `access_parses_exactly_what_the_plugin_writes` pins the other end.
    }

    #[test]
    fn a_read_only_volume_registers_a_read_grant_on_the_wire() {
        let mut v = st("lean", "published", None);
        assert_eq!(registered_access(&v), Access::ReadWrite);
        v.read_only = true;
        assert_eq!(registered_access(&v), Access::Read);
        // The wire spelling the broker's `Registration` parses. A plugin
        // registering read-write for this volume would get a writer's key
        // from any CR that lists the SA in `serviceAccounts`.
        let reg = Registration {
            volume_id: v.volume_id.clone(),
            pod_uid: "u".into(),
            namespace: "ns".into(),
            pod: "p".into(),
            service_account: "sa".into(),
            cr: v.cr.clone(),
            mode: v.mode.clone(),
            nonce: v.nonce.clone(),
            node: "n".into(),
            access: registered_access(&v),
            on_behalf_of: Some("alice".into()),
        };
        let wire = serde_json::to_value(&reg).unwrap();
        assert_eq!(wire["access"], "read");
        assert_eq!(wire["onBehalfOf"], "alice");
        assert_eq!(serde_json::from_value::<Registration>(wire).unwrap(), reg);
    }

    fn st(mode: &str, phase: &str, tree_image: Option<&str>) -> VolumeState {
        VolumeState {
            version: STATE_VERSION,
            volume_id: "csi-1".into(),
            mode: mode.into(),
            cr: "ws".into(),
            tenant: TenantRef { namespace: "t".into(), pod: "p".into(), pod_uid: "u".into(), service_account: "s".into() },
            target_path: "/t".into(),
            src: "/s".into(),
            worker_namespace: "flint-workers".into(),
            worker_name: "s3w-x".into(),
            worker_uid: None,
            phase: phase.into(),
            credential_mode: "broker".into(),
            nonce: "n".into(),
            creds_expiration: None,
            token_expiration: None,
            last_probe_ok: None,
            published_unix: None,
            read_only: false,
            owner_uid: 1001,
            owner_gid: 1001,
            grace_secs: None,
            tree_image: tree_image.map(|s| s.to_string()),
            drain_started_unix: None,
            sync_env: None,
            on_behalf_of: None,
            shared: None,
        }
    }

    /// Only a READ-ONLY member whose credential is the CR's function may
    /// share, and only when the CR asked: every other combination is a
    /// mounter of its own, with the reason named.
    #[test]
    fn only_read_only_members_with_cr_scoped_credentials_share() {
        use CredentialMode::*;
        assert_eq!(sharing_decision(true, true, Broker, Some("sts")), Sharing::Shared);
        assert_eq!(sharing_decision(true, true, Broker, Some("static")), Sharing::Shared);
        assert_eq!(sharing_decision(true, true, Ambient, None), Sharing::Shared);
        let own = |d: Sharing, what: &str| match d {
            Sharing::Own(why) => assert!(why.contains(what), "{why:?} should name {what:?}"),
            Sharing::Shared => panic!("must not share: {what}"),
            Sharing::Retry(why) => panic!("must be a mounter of its own, not a retry: {what} ({why})"),
        };
        own(sharing_decision(false, true, Broker, Some("sts")), "does not ask");
        own(sharing_decision(false, true, Ambient, None), "does not ask");
        own(sharing_decision(true, false, Broker, Some("sts")), "read-write");
        own(sharing_decision(true, true, Broker, Some("rest")), "rest");
        own(sharing_decision(true, true, Broker, Some("other")), "does not know");
        // An UNREADABLE broker is a retry, never a silent mounter of the
        // pod's own for its whole life (the CR asked; the exchange would
        // fail on the same outage anyway).
        match sharing_decision(true, true, Broker, None) {
            Sharing::Retry(why) => assert!(why.contains("could not be read"), "{why}"),
            other => panic!("an unreadable broker must be a Retry, got {other:?}"),
        }
        // …but only when the knob applies: a read-write consumer keeps its own
        // without waiting for anything.
        own(sharing_decision(true, false, Broker, None), "read-write");
        own(sharing_decision(true, true, Static, None), "static");
        own(sharing_decision(true, true, WebIdentity, None), "webIdentity");
        assert_ne!(share_lock_key("abc"), "abc", "the class lock is its own key, never a volume's");
    }

    /// A plugin roll mid-checkout must not restart the checkout (§6.4,
    /// S17): the syncer is its own pod. Everything else unfinished is
    /// cleaned up for kubelet's retry; everything published is kept.
    #[test]
    fn adoption_keeps_a_checkout_in_progress_and_cleans_the_rest() {
        assert_eq!(adopt_action(&st("lean", "checking-out", None)), AdoptAction::KeepCheckingOut);
        assert_eq!(adopt_action(&st("lean", "published", None)), AdoptAction::Keep);
        assert_eq!(adopt_action(&st("passthrough", "published", None)), AdoptAction::Keep);
        assert_eq!(adopt_action(&st("lean", "publishing", None)), AdoptAction::Cleanup);
        assert_eq!(adopt_action(&st("passthrough", "publishing", None)), AdoptAction::Cleanup);
        // A worker kept for a retry that never came (the plugin restarted
        // instead): cleaned up like any unfinished publish.
        assert_eq!(adopt_action(&st("passthrough", "worker-pending", None)), AdoptAction::Cleanup);
    }

    /// A worker still coming up at the deadline is kept for the retry,
    /// which resumes from the top and adopts it — never StartOver, whose
    /// cleanup would delete the pod mid-pull. Only that phase: an
    /// unfinished publish past the worker (a mount that never served) is
    /// still started over.
    #[test]
    fn a_pending_worker_is_resumed_by_the_retry_not_started_over() {
        let p = st("passthrough", "worker-pending", None);
        assert_eq!(published_action(&p, false, false, false), PublishedAction::ResumeWorker);
        assert_eq!(published_action(&p, false, false, true), PublishedAction::ResumeWorker);
        assert_eq!(published_action(&st("passthrough", "publishing", None), false, false, false), PublishedAction::StartOver);
    }

    /// A refused refresh removes the key from a mounter of the pod's own
    /// (revocation within one lifetime) and NOT from a shared class, whose
    /// file serves every member.
    #[test]
    fn a_refusal_removes_the_key_only_from_an_unshared_mounter() {
        let mut v = st("passthrough", "published", None);
        assert!(revocation_removes_the_key(&v));
        v.shared = Some("abcd".into());
        assert!(!revocation_removes_the_key(&v), "a shared class's key is every member's");
    }

    /// The memory limit reaches the mounter as the chart wrote it, and
    /// only the limit: requests are for scheduling.
    #[test]
    fn the_worker_memory_limit_is_read_from_the_chart_s_resources() {
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let r: ResourceRequirements = serde_json::from_str(r#"{"requests":{"cpu":"10m","memory":"64Mi"},"limits":{"memory":"1Gi"}}"#).unwrap();
        assert_eq!(worker_memory_limit(Some(&r)).as_deref(), Some("1Gi"));
        let cpu_only = ResourceRequirements {
            limits: Some(BTreeMap::from([("cpu".to_string(), Quantity("1".into()))])),
            ..Default::default()
        };
        assert_eq!(worker_memory_limit(Some(&cpu_only)), None);
        assert_eq!(worker_memory_limit(None), None);
    }

    /// A published lean workspace is NEVER started over. The old code
    /// fell through to cleanup when neither the target nor the source
    /// was a mount point — which a quota-off tree, a plain directory,
    /// always fails — and then overwrote the phase that cleanup's own
    /// refusal keyed on.
    #[test]
    fn a_published_lean_volume_is_rebound_or_refused_never_started_over() {
        let quota_off = st("lean", "published", None);
        assert_eq!(published_action(&quota_off, true, false, true), PublishedAction::Republish);
        assert_eq!(published_action(&quota_off, false, false, true), PublishedAction::Rebind { remount: false }, "a plain-directory tree IS the live tree");
        assert_eq!(published_action(&quota_off, false, false, false), PublishedAction::RefuseLean);
        let quota_on = st("lean", "published", Some("/v/tree.img"));
        assert_eq!(published_action(&quota_on, false, true, true), PublishedAction::Rebind { remount: false });
        assert_eq!(published_action(&quota_on, false, false, true), PublishedAction::Rebind { remount: true }, "the image holds the tree; put its mount back");
        assert_eq!(published_action(&quota_on, false, false, false), PublishedAction::Rebind { remount: true });
        for tm in [true, false] {
            for sm in [true, false] {
                for te in [true, false] {
                    let a = published_action(&st("lean", "published", None), tm, sm, te);
                    assert_ne!(a, PublishedAction::StartOver, "lean published ({tm},{sm},{te}) reached StartOver");
                }
            }
        }
    }

    /// Passthrough keeps its shape: a dead source is a dead mount, and a
    /// fresh publish is the right answer.
    #[test]
    fn passthrough_and_unfinished_volumes_keep_their_paths() {
        let p = st("passthrough", "published", None);
        assert_eq!(published_action(&p, true, true, true), PublishedAction::Republish);
        assert_eq!(published_action(&p, false, true, true), PublishedAction::Rebind { remount: false });
        assert_eq!(published_action(&p, false, false, true), PublishedAction::StartOver);
        assert_eq!(published_action(&st("lean", "checking-out", None), false, false, true), PublishedAction::ResumeCheckout);
        assert_eq!(published_action(&st("lean", "publishing", None), false, false, true), PublishedAction::StartOver);
        assert_eq!(published_action(&st("passthrough", "publishing", None), false, false, false), PublishedAction::StartOver);
    }

    /// A broker that is down is an outage kubelet retries, not a
    /// refusal the tenant reads as "not allowed".
    #[test]
    fn exchange_outages_are_unavailable_and_refusals_are_denied() {
        assert_eq!(exchange_status("ws", ExchangeError::Outage("transport: connection refused".into())).code(), tonic::Code::Unavailable);
        let s = exchange_status("ws", ExchangeError::Refused("403: AccessDenied: bob is not a consumer".into()));
        assert_eq!(s.code(), tonic::Code::PermissionDenied);
        assert!(s.message().contains("bob"), "{}", s.message());
    }
}


/// Why a lean worker the watch says is not Running should be started
/// over the same tree — or `None` to leave it alone (a pod that is
/// merely Pending or Unknown is not lost).
pub(crate) fn relaunch_reason(phase: Option<&str>) -> Option<&'static str> {
    match phase {
        None => Some("was gone (evicted, or deleted)"),
        Some("Failed") => Some("had failed at the pod level"),
        // Exit 0 while the tenant is still mounted: a fence (a lost
        // renew response, a takeover) or a refusal. `OnFailure` never
        // restarts an exit 0, so without this the sole holder of the
        // workspace stayed gone for the tenant's life and nothing
        // published (audit 2026-09-03, finding 2). Relaunched, the syncer
        // self-recognises a cell that still names it and, against a live
        // successor, waits out the quiet polls it never wins.
        Some("Succeeded") => Some("had exited on its own while the tenant was still mounted"),
        Some(_) => None,
    }
}

/// A Running worker whose supervisor is waiting for a launch: the socket
/// is bound and no launch record is persisted beside it. After a node
/// reboot the memory-backed comm dir comes back empty, which is exactly
/// this shape (audit 2026-09-03, finding 4).
pub(crate) fn launch_missing(comm: &Path) -> bool {
    comm.is_dir() && comm.join("mount.sock").exists() && !comm.join("launch.json").exists()
}

/// The syncer's drain attestation (`.flint-sync/drained.json`, written
/// only after its drain returned Ok). True only when it post-dates this
/// unpublish's SIGTERM, so an attestation from an earlier life of the
/// tree cannot vouch for this drain.
pub(crate) fn drain_attested(tree: &Path, drain_started_unix: u64) -> bool {
    let p = tree.join(".flint-sync").join("drained.json");
    let Ok(bytes) = std::fs::read(&p) else { return false };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false };
    v.get("unix")
        .and_then(|u| u.as_u64())
        .map(|u| u >= drain_started_unix.saturating_sub(1))
        .unwrap_or(false)
}

#[cfg(test)]
mod audit_2026_09_03 {
    use super::*;

    #[test]
    fn a_worker_that_exited_on_its_own_is_relaunched_like_a_failed_one() {
        assert!(relaunch_reason(None).is_some(), "gone");
        assert!(relaunch_reason(Some("Failed")).is_some());
        assert!(relaunch_reason(Some("Succeeded")).is_some(), "exit 0 while mounted is a lost syncer");
        for p in ["Running", "Pending", "Unknown"] {
            assert!(relaunch_reason(Some(p)).is_none(), "{p} is not lost");
        }
    }

    #[test]
    fn a_launch_is_missing_only_when_the_supervisor_is_listening_without_a_record() {
        let d = tempfile::tempdir().unwrap();
        assert!(!launch_missing(&d.path().join("absent")), "no comm dir: nothing to send to");
        assert!(!launch_missing(d.path()), "no socket: the supervisor is not listening");
        std::fs::write(d.path().join("mount.sock"), b"").unwrap();
        assert!(launch_missing(d.path()), "socket and no record: waiting for a launch");
        std::fs::write(d.path().join("launch.json"), b"{}").unwrap();
        assert!(!launch_missing(d.path()), "a persisted launch relaunches itself");
    }

    #[test]
    fn a_drain_is_attested_only_by_a_marker_younger_than_the_sigterm() {
        let d = tempfile::tempdir().unwrap();
        let tree = d.path();
        assert!(!drain_attested(tree, 1000), "no marker: a failed or fenced drain");
        let st = tree.join(".flint-sync");
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join("drained.json"), b"not json").unwrap();
        assert!(!drain_attested(tree, 1000), "garbage is not an attestation");
        std::fs::write(st.join("drained.json"), br#"{"unix":900,"seq":3,"acks":0}"#).unwrap();
        assert!(!drain_attested(tree, 1000), "an attestation from an earlier life of the tree");
        std::fs::write(st.join("drained.json"), br#"{"unix":1000,"seq":3,"acks":0}"#).unwrap();
        assert!(drain_attested(tree, 1000));
        std::fs::write(st.join("drained.json"), br#"{"unix":999,"seq":3,"acks":0}"#).unwrap();
        assert!(drain_attested(tree, 1000), "one second of clock slack");
    }
}
