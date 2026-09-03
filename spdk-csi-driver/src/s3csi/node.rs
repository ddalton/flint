//! The CSI Identity + Node services of `s3.chert.us` (design §3.4, §3.5,
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
use std::time::Duration;

use k8s_openapi::api::core::v1::{Event, ObjectReference, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::api::{Api, PostParams};
use kube::Client;
use tonic::{Request, Response, Status};

use crate::csi;
use crate::csi::volume_capability::AccessType;

use super::attrs::{self, PublishRequest};
use super::creds::{self, BrokerClient, Creds, Materialized, Registration};
use super::fuse::{self, Launch};
use super::policy::CredentialMode;
use super::resolve::{self, Refusal, Resolved};
use super::state::{TenantRef, VolumeState, STATE_VERSION};
use super::worker::{self, WaitOutcome, WorkerInputs};
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

pub struct Config {
    pub node_name: String,
    pub node_uid: Option<String>,
    pub worker_namespace: String,
    pub passthrough_image: String,
    pub lean_image: Option<String>,
    pub worker_resources: Option<ResourceRequirements>,
    pub priority_class: Option<String>,
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
        let worker_resources = match opt("FLINT_S3CSI_WORKER_RESOURCES") {
            Some(j) if j.trim() != "{}" => Some(
                serde_json::from_str(&j).map_err(|e| format!("FLINT_S3CSI_WORKER_RESOURCES is not ResourceRequirements JSON: {e}"))?,
            ),
            _ => None,
        };
        Ok(Self {
            node_name: need("FLINT_S3CSI_NODE_NAME")?,
            node_uid: None,
            worker_namespace: opt("FLINT_S3CSI_WORKER_NAMESPACE").unwrap_or_else(|| "flint-workers".into()),
            // No default: this names the image every worker runs.
            passthrough_image: need("FLINT_S3CSI_PASSTHROUGH_IMAGE")?,
            lean_image: opt("FLINT_S3CSI_LEAN_IMAGE"),
            worker_resources,
            priority_class: opt("FLINT_S3CSI_WORKER_PRIORITY_CLASS"),
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
    /// One mutating owner per volume on this node; the acquire wait is
    /// bounded below kubelet's deadline so a stuck holder surfaces as
    /// `Unavailable`, not a consumed deadline.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl S3Node {
    pub fn new(cfg: Config, client: Client) -> Self {
        Self { cfg: Arc::new(cfg), client, locks: Mutex::new(HashMap::new()) }
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
            if st.phase != "published" {
                // A publish that never finished: kubelet will retry it, and
                // the retry starts clean.
                self.cleanup(&dir, &st, "unfinished publish found at startup").await;
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
        let _ = fuse::unmount(Path::new(&st.target_path), true);
        let _ = fuse::unmount(Path::new(&st.src), true);
        let _ = worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(5)).await;
        if let Some(b) = &self.cfg.broker {
            let _ = b.deregister(&st.volume_id).await;
        }
        let _ = std::fs::remove_dir_all(dir);
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
            if st.phase == "published" {
                if fuse::is_mountpoint(&target).unwrap_or(false) {
                    return self.republish(&dir, st, &pr).await;
                }
                // Published once, but the target is gone (kubelet
                // recreated the pod dir?). Rebind if the source lives.
                if fuse::is_mountpoint(Path::new(&st.src)).unwrap_or(false) {
                    fuse::bind_mount(Path::new(&st.src), &target, st.read_only)
                        .map_err(|e| Status::unavailable(format!("rebind: {e}")))?;
                    return Ok(());
                }
            }
            if st.mode == "lean" && st.phase == "checking-out" {
                // The syncer is checking out between kubelet's attempts
                // (design §3.5 step 8): do not start over, wait again.
                return self.resume_lean(&dir, st, &target).await;
            }
            // An unfinished publish: start over.
            self.cleanup(&dir, &st, "retrying an unfinished publish").await;
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
        resolve::authorize(&policy, &pr.service_account, &pr.pod_namespace, kind, pr.selector.name()).map_err(refusal_status)?;

        match resolved {
            Resolved::Passthrough { spec } => {
                self.publish_passthrough(&dir, &vid, &target, &pr, &tenant, spec, policy.credential_mode, &req).await
            }
            Resolved::Lean { spec, .. } => {
                self.publish_lean(&dir, &vid, &target, &pr, &tenant, spec, policy.credential_mode, &req).await
            }
        }
    }

    /// The credential arm, materialized. Also registers the publish at
    /// the broker when a broker is in the loop.
    async fn credential(
        &self,
        mode: CredentialMode,
        pr: &PublishRequest,
        st: &mut VolumeState,
        secrets: &HashMap<String, String>,
    ) -> Result<Materialized, Status> {
        let mut m = self.credential_arm(mode, pr, st, secrets).await?;
        // Every arm carries a region: mount-s3 takes one on its argv, but
        // the lean syncer's SDK client reads AWS_REGION and, without it,
        // fails its first request as a bare "dispatch failure" (measured:
        // the same env with AWS_REGION checks out). The static arm's
        // Secret may name its own; the node's default fills the rest.
        m.env.entry("AWS_REGION".to_string()).or_insert_with(|| self.cfg.region.clone());
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
                    .map_err(|e| Status::permission_denied(format!("credential exchange for {}: {e}", st.cr)))?;
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
        req: &csi::NodePublishVolumeRequest,
    ) -> Result<(), Status> {
        let owner_uid = pr.uid.or(spec.uid.map(|u| u as u32)).unwrap_or(DEFAULT_OWNER);
        let owner_gid = pr.gid.or(spec.gid.map(|g| g as u32)).unwrap_or(owner_uid);
        let read_only = req.readonly || spec.read_only;
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
            env: BTreeMap::from([("FLINT_S3W_MODE".to_string(), "passthrough".to_string())]),
            lean_tree_hostpath: None,
            grace_secs: Some(30),
            priority_class: self.cfg.priority_class.clone(),
            comm_size: self.cfg.comm_size.clone(),
            scratch_size: self.cfg.scratch_size.clone(),
        });
        if let Err(e) = worker::ensure(&self.client, &pod).await {
            return Err(self.fail(dir, &st,Status::unavailable(e)).await);
        }
        let worker_uid = match worker::wait_running(&self.client, &st.worker_namespace, &st.worker_name, WORKER_RUNNING_WAIT).await {
            Ok(WaitOutcome::Running { uid }) => uid,
            Ok(WaitOutcome::Failed { reason, message }) => {
                return Err(self.fail(dir, &st,Status::failed_precondition(format!("worker pod {}: {reason} {message}", st.worker_name))).await)
            }
            Ok(WaitOutcome::Timeout { phase }) => {
                return Err(self.fail(dir, &st,Status::unavailable(format!("worker pod {} not Running after {}s ({phase}); retrying", st.worker_name, WORKER_RUNNING_WAIT.as_secs()))).await)
            }
            Err(e) => return Err(self.fail(dir, &st,Status::unavailable(e)).await),
        };
        st.worker_uid = Some(worker_uid.clone());
        let comm = worker::comm_dir(&self.cfg.kubelet_root, &worker_uid);
        if !comm.is_dir() {
            return Err(self.fail(dir, &st,Status::unavailable(format!("worker comm dir {} not visible on the node yet", comm.display()))).await);
        }

        // Credential.
        let mat = match self.credential(cred_mode, pr, &mut st, &req.secrets).await {
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
        let mut args = crate::passthrough::mounter::mounter_args_for(
            &spec,
            (Some(owner_uid as i64), Some(owner_gid as i64)),
            fuse::FUSE_FD_PLACEHOLDER,
        );
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
            let detail = std::fs::read_to_string(comm.join("mount.error")).unwrap_or_default();
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

    /// Target already mounted: refresh, probe, OK. Never an error.
    async fn republish(&self, dir: &Path, mut st: VolumeState, pr: &PublishRequest) -> Result<(), Status> {
        let mut changed = false;
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
                                Err(e) => Err(format!("re-registration before refresh: {e}")),
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
                                    if e.contains("refused the exchange") {
                                        creds::remove_file(&comm, creds::CREDS_FILE);
                                        st.creds_expiration = None;
                                        changed = true;
                                    }
                                    self.emit_event(&st.tenant, "CredentialRefreshFailed", &format!("{}: {e}", st.cr), true).await;
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
        let alive = worker::is_running(&self.client, &st.worker_namespace, &st.worker_name).await.unwrap_or(true);
        let mounted = st.mode != "passthrough" || fuse::is_mountpoint(Path::new(&st.src)).unwrap_or(false);
        let ok = alive && mounted && fuse::wait_ready(Path::new(&st.src), Duration::from_secs(3)).await.is_ok();
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
        if changed {
            let _ = st.save(dir);
        }
        Ok(())
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
            read_only: false,
            owner_uid,
            owner_gid,
            grace_secs: Some(grace),
            tree_image: None,
            drain_started_unix: None,
        };
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;

        // The tree: plugin-owned, owned by the syncer's uid, world-writable
        // with the sticky bit so an app uid the CR does not name can still
        // create files (design §3.5 step 6). A loop-image quota is a
        // follow-up; today the CR's byte budget is the syncer's refusal.
        if let Err(e) = std::fs::create_dir_all(&tree)
            .and_then(|_| std::os::unix::fs::chown(&tree, Some(owner_uid), Some(owner_gid)))
            .and_then(|_| std::fs::set_permissions(&tree, std::os::unix::fs::PermissionsExt::from_mode(0o1777)))
        {
            return Err(self.fail(dir, &st, Status::internal(format!("tree {}: {e}", tree.display()))).await);
        }

        let (run_as, run_as_gid) = worker_owner(&st);
        let pod = worker::build_pod(&WorkerInputs {
            namespace: self.cfg.worker_namespace.clone(),
            node_name: self.cfg.node_name.clone(),
            node_uid: self.cfg.node_uid.clone(),
            image,
            mode: "lean",
            volume_id: vid,
            tenant,
            cr: &name,
            run_as_uid: run_as,
            run_as_gid,
            resources: self.cfg.worker_resources.clone(),
            env: BTreeMap::from([("FLINT_S3W_MODE".to_string(), "lean".to_string())]),
            lean_tree_hostpath: Some(st.src.clone()),
            grace_secs: Some(grace as i64),
            priority_class: self.cfg.priority_class.clone(),
            comm_size: self.cfg.comm_size.clone(),
            scratch_size: self.cfg.scratch_size.clone(),
        });
        if let Err(e) = worker::ensure(&self.client, &pod).await {
            return Err(self.fail(dir, &st, Status::unavailable(e)).await);
        }
        let worker_uid = match worker::wait_running(&self.client, &st.worker_namespace, &st.worker_name, WORKER_RUNNING_WAIT).await {
            Ok(WaitOutcome::Running { uid }) => uid,
            Ok(WaitOutcome::Failed { reason, message }) => {
                return Err(self.fail(dir, &st, Status::failed_precondition(format!("syncer pod {}: {reason} {message}", st.worker_name))).await)
            }
            Ok(WaitOutcome::Timeout { phase }) => {
                return Err(self.fail(dir, &st, Status::unavailable(format!("syncer pod {} not Running after {}s ({phase}); retrying", st.worker_name, WORKER_RUNNING_WAIT.as_secs()))).await)
            }
            Err(e) => return Err(self.fail(dir, &st, Status::unavailable(e)).await),
        };
        st.worker_uid = Some(worker_uid.clone());
        let comm = worker::comm_dir(&self.cfg.kubelet_root, &worker_uid);
        if !comm.is_dir() {
            return Err(self.fail(dir, &st, Status::unavailable(format!("syncer comm dir {} not visible on the node yet", comm.display()))).await);
        }
        let mat = match self.credential(cred_mode, pr, &mut st, &req.secrets).await {
            Ok(m) => m,
            Err(e) => return Err(self.fail(dir, &st, e).await),
        };
        if let Err(e) = creds::write_files(&comm, &mat.files, worker_owner(&st)) {
            return Err(self.fail(dir, &st, Status::internal(format!("write comm files: {e}"))).await);
        }

        // The syncer's environment: the same fixed list the webhook
        // stamped, with the tenant namespace as a LITERAL (design §5).
        let ws = crate::lean_operator::crd::FlintLeanWorkspace::new(&name, spec);
        let mut env: BTreeMap<String, String> = crate::lean_operator::sync_env::sync_env(&ws, SYNCER_ROOT).into_iter().collect();
        env.insert("FLINT_SYNC_NAMESPACE".into(), pr.pod_namespace.clone());
        env.extend(mat.env);
        let launch = Launch { mode: "lean".into(), args: vec!["run".into()], env };
        let sock = comm.join("mount.sock");
        let reply = {
            let sock = sock.clone();
            tokio::task::spawn_blocking(move || fuse::send_launch(&sock, &launch, None, LAUNCH_REPLY_WAIT)).await
        };
        match reply {
            Ok(Ok(r)) if r.ok => {}
            Ok(Ok(r)) => return Err(self.fail(dir, &st, Status::unavailable(format!("syncer refused the launch: {}", r.error.unwrap_or_default()))).await),
            Ok(Err(e)) => return Err(self.fail(dir, &st, Status::unavailable(format!("launch over {}: {e}", sock.display()))).await),
            Err(e) => return Err(self.fail(dir, &st, Status::internal(format!("launch task: {e}"))).await),
        }
        st.phase = "checking-out".into();
        st.save(dir).map_err(|e| Status::internal(format!("state: {e}")))?;
        self.resume_lean(dir, st, target).await
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
            // (Refused, Fenced, maxFiles) never writes the marker.
            match worker::wait_running(&self.client, &st.worker_namespace, &st.worker_name, Duration::from_millis(1)).await {
                Ok(WaitOutcome::Failed { reason, message }) => {
                    let detail = st
                        .worker_uid
                        .as_ref()
                        .and_then(|u| std::fs::read_to_string(worker::comm_dir(&self.cfg.kubelet_root, u).join("mount.error")).ok())
                        .unwrap_or_default();
                    return Err(self.fail(dir, &st, Status::failed_precondition(format!("syncer {}: {reason} {message} {}", st.worker_name, detail.trim()))).await);
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
            if let Err(e) = fuse::bind_mount(Path::new(&st.src), target, false) {
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

    /// The final drain (design §3.5): delete the syncer with the derived
    /// grace so its SIGTERM arm runs over a tree every tenant container
    /// has already left, wait, then unmount and remove the tree.
    async fn unpublish_lean(&self, dir: &Path, mut st: VolumeState, target: &Path) -> Result<(), Status> {
        let grace = st.grace_secs.unwrap_or(30);
        let now = chrono::Utc::now().timestamp() as u64;
        let started = match st.drain_started_unix {
            Some(t) => t,
            None => {
                if worker::is_gone(&self.client, &st.worker_namespace, &st.worker_name).await.map_err(Status::unavailable)? {
                    tracing::warn!(volume = %st.volume_id, "syncer was already gone at unpublish — nothing drained; recover-staged applies");
                    self.emit_event(&st.tenant, "SyncerGoneAtUnpublish", &format!("{}: the syncer was not running when the pod's volume was unpublished, so no final drain ran; run recover-staged on the workspace", st.cr), true).await;
                } else {
                    worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(grace as u32)).await.map_err(Status::unavailable)?;
                    tracing::info!(volume = %st.volume_id, grace, "draining: syncer deleted with the derived grace");
                }
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
                worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(0)).await.map_err(Status::unavailable)?;
                self.emit_event(&st.tenant, "DrainCeilingHit", &format!("{}: the syncer did not finish its drain within {}s; it was killed and staged work may need recover-staged", st.cr, grace + 30), true).await;
                break;
            }
            if start.elapsed() > DRAIN_WAIT {
                return Err(Status::unavailable(format!("draining {} ({}s of {}s); retrying", st.cr, elapsed, grace)));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        unmount_all(target).map_err(|e| Status::internal(format!("unmount target: {e}")))?;
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
            return Ok(());
        };
        if st.mode == "lean" {
            return self.unpublish_lean(&dir, st, target).await;
        }
        // Passthrough teardown: target, source, worker, registration, state.
        unmount_all(target).map_err(|e| Status::internal(format!("unmount target: {e}")))?;
        unmount_all(Path::new(&st.src)).map_err(|e| Status::internal(format!("unmount source: {e}")))?;
        worker::delete(&self.client, &st.worker_namespace, &st.worker_name, Some(10)).await.map_err(Status::unavailable)?;
        if let Some(b) = &self.cfg.broker {
            if let Err(e) = b.deregister(&vid).await {
                tracing::warn!(volume = %vid, "deregister: {e}");
            }
        }
        remove_state_dir(&dir)?;
        tracing::info!(volume = %vid, "unpublished");
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
        }
    }
}

/// Unmount `path` until it is a plain directory (stacked mounts from a
/// retried publish need one `umount2` each), bounded.
fn unmount_all(path: &Path) -> std::io::Result<()> {
    for _ in 0..8 {
        if !path.exists() || !fuse::is_mountpoint(path).unwrap_or(false) {
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

/// The uid/gid the worker RUNS as (never root: a root owner maps to
/// nobody for the process, while `--uid 0` still presents root).
fn worker_owner(st: &VolumeState) -> (u32, u32) {
    (
        if st.owner_uid == 0 { DEFAULT_OWNER } else { st.owner_uid },
        if st.owner_gid == 0 { DEFAULT_OWNER } else { st.owner_gid },
    )
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
        assert_eq!(c.worker_namespace, "flint-workers");
        assert_eq!(c.creds_lifetime_secs, 900);
        assert!(c.broker.is_none());
        std::env::remove_var("FLINT_S3CSI_NODE_NAME");
        std::env::remove_var("FLINT_S3CSI_PASSTHROUGH_IMAGE");
    }
}
