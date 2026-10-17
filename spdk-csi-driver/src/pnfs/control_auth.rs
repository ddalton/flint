//! Who may call the pNFS control path (pNFS Kerberos plan, Phase 5b).
//!
//! Phase 5a made the control path TLS, but who the CLIENT is rested on one
//! shared Bearer token: every holder could register a data server (and
//! attract stripe writes), create and delete volumes, or tell a DS to
//! truncate. Here each caller presents its own pod-bound ServiceAccount
//! token, projected with the audience of the server it calls
//! ([`MDS_AUDIENCE`], [`DS_AUDIENCE`]), and the server reviews it
//! (`TokenReview`, cached) and checks the role table:
//!
//! | server | RPCs | caller |
//! |---|---|---|
//! | MDS | Register/Heartbeat/UpdateCapacity/UnregisterDataServer | `flint-pnfs-ds`, as its own pod (`device_id` == pod name) |
//! | MDS | AttachBlockNode, BlockSessionUp | `flint-csi-node` for its own node; `flint-csi-controller` |
//! | MDS | every other MdsControl RPC | `flint-csi-controller` |
//! | DS | TruncateStripeFile, ReadStripe, WriteStripe | `flint-pnfs-mds` |
//!
//! All in the server's own namespace. Anything not in the table is
//! refused. A review the apiserver could not answer is UNAVAILABLE (the
//! caller retries), never a refusal of the token and never a pass.
//!
//! On with `FLINT_PNFS_CONTROL_AUTH=serviceaccount` on the servers; the
//! clients send the token file `FLINT_PNFS_MDS_TOKEN_FILE` /
//! `FLINT_PNFS_DS_TOKEN_FILE` (re-read as the kubelet rotates it). Off,
//! the shared token (`FLINT_PNFS_CONTROL_TOKEN`) is checked as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// The MDS's token audience.
pub const MDS_AUDIENCE: &str = "flint-pnfs-mds";
/// A data server's token audience.
pub const DS_AUDIENCE: &str = "flint-pnfs-ds";
/// Server: `serviceaccount` turns the checks on.
pub const MODE_ENV: &str = "FLINT_PNFS_CONTROL_AUTH";
/// Client: the projected token for calls to the MDS.
pub const MDS_TOKEN_FILE_ENV: &str = "FLINT_PNFS_MDS_TOKEN_FILE";
/// Client: the projected token for calls to a data server.
pub const DS_TOKEN_FILE_ENV: &str = "FLINT_PNFS_DS_TOKEN_FILE";

pub const DS_SA: &str = "flint-pnfs-ds";
pub const MDS_SA: &str = "flint-pnfs-mds";
pub const CONTROLLER_SA: &str = "flint-csi-controller";
pub const NODE_SA: &str = "flint-csi-node";

/// How long a verdict is reused. Also how long a deleted pod's token keeps
/// working after its pod is gone (E5: the apiserver itself caches briefly).
const CACHE_TTL: Duration = Duration::from_secs(60);

/// A verified caller, put into the request's extensions for the handlers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub service_account: String,
    pub pod_name: Option<String>,
    pub node_name: Option<String>,
}

/// Which ServiceAccounts may call `path` (`/pnfs.control.<Service>/<Method>`).
pub fn allowed(path: &str) -> &'static [&'static str] {
    let Some((service, method)) = path.strip_prefix('/').and_then(|p| p.split_once('/')) else {
        return &[];
    };
    match (service, method) {
        ("pnfs.control.MdsControl", "RegisterDataServer" | "Heartbeat" | "UpdateCapacity" | "UnregisterDataServer") => {
            &[DS_SA]
        }
        ("pnfs.control.MdsControl", "AttachBlockNode" | "BlockSessionUp") => &[CONTROLLER_SA, NODE_SA],
        (
            "pnfs.control.MdsControl",
            "CreateVolume" | "DeleteVolume" | "ExpandVolume" | "FenceBlockClient" | "UnfenceBlockClient"
            | "DetachBlockNode" | "BlockExportStatus" | "HostBlockLeg" | "DropBlockLeg",
        ) => &[CONTROLLER_SA],
        ("pnfs.control.DsControl", "TruncateStripeFile" | "ReadStripe" | "WriteStripe") => &[MDS_SA],
        _ => &[],
    }
}

/// Why a call was turned away.
#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    /// No token, or one the apiserver refused.
    Unauthenticated(String),
    /// A valid token of a caller not allowed this RPC.
    Forbidden(String),
    /// The apiserver did not answer: nothing was judged.
    Unavailable(String),
}

impl Denied {
    fn status(&self) -> tonic::Status {
        match self {
            Denied::Unauthenticated(m) => tonic::Status::unauthenticated(m.clone()),
            Denied::Forbidden(m) => tonic::Status::permission_denied(m.clone()),
            Denied::Unavailable(m) => tonic::Status::unavailable(m.clone()),
        }
    }
}

/// What the apiserver said about a token: the caller, a refusal, or no
/// answer.
pub type Review = Result<Caller, crate::s3csi::broker::ReviewError>;

/// Reviews a token (the seam tests replace).
#[async_trait::async_trait]
pub trait Reviewer: Send + Sync {
    async fn review(&self, token: &str) -> Review;
}

/// `TokenReview` against the apiserver, for one audience, from one
/// namespace.
pub struct KubeReviewer {
    pub client: kube::Client,
    pub audience: &'static str,
    pub namespace: String,
}

#[async_trait::async_trait]
impl Reviewer for KubeReviewer {
    async fn review(&self, token: &str) -> Review {
        use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
        let api: kube::Api<TokenReview> = kube::Api::all(self.client.clone());
        let tr = TokenReview {
            spec: TokenReviewSpec { token: Some(token.to_string()), audiences: Some(vec![self.audience.to_string()]) },
            ..Default::default()
        };
        let created = api.create(&kube::api::PostParams::default(), &tr).await;
        let node = created
            .as_ref()
            .ok()
            .and_then(|t| t.status.as_ref())
            .and_then(|s| s.user.as_ref())
            .and_then(|u| u.extra.as_ref())
            .and_then(|e| e.get("authentication.kubernetes.io/node-name"))
            .and_then(|v| v.first().cloned());
        let id = crate::s3csi::broker::review_outcome(created, self.audience)?;
        if id.namespace != self.namespace {
            return Err(crate::s3csi::broker::ReviewError::Refused(format!(
                "{} is not of namespace {}",
                id.username, self.namespace
            )));
        }
        Ok(Caller { service_account: id.service_account, pod_name: id.pod_name, node_name: node })
    }
}

/// A server's policy: who may call what, by reviewing their tokens.
pub struct Policy {
    reviewer: Arc<dyn Reviewer>,
    cache: Mutex<HashMap<[u8; 32], (Instant, Review)>>,
}

impl Policy {
    pub fn new(reviewer: Arc<dyn Reviewer>) -> Arc<Self> {
        Arc::new(Self { reviewer, cache: Mutex::new(HashMap::new()) })
    }

    async fn review(&self, token: &str) -> Review {
        use sha2::Digest;
        // The cache key is a hash, never the token.
        let key: [u8; 32] = sha2::Sha256::digest(token.as_bytes()).into();
        if let Some((at, v)) = self.cache.lock().unwrap().get(&key) {
            if at.elapsed() < CACHE_TTL {
                return v.clone();
            }
        }
        let v = self.reviewer.review(token).await;
        // An unanswered review is not cached: the next call asks again.
        if !matches!(v, Err(crate::s3csi::broker::ReviewError::Unavailable(_))) {
            let mut c = self.cache.lock().unwrap();
            if c.len() > 4096 {
                c.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            }
            c.insert(key, (Instant::now(), v.clone()));
        }
        v
    }

    /// May the bearer of `authorization` call `path`?
    pub async fn authorize(&self, path: &str, authorization: Option<&str>) -> Result<Caller, Denied> {
        let allowed = allowed(path);
        if allowed.is_empty() {
            return Err(Denied::Forbidden(format!("{path}: no caller may use it")));
        }
        let token = authorization
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Denied::Unauthenticated("no bearer token".into()))?;
        let caller = match self.review(token).await {
            Ok(c) => c,
            Err(crate::s3csi::broker::ReviewError::Refused(m)) => return Err(Denied::Unauthenticated(m)),
            Err(crate::s3csi::broker::ReviewError::Unavailable(m)) => return Err(Denied::Unavailable(m)),
        };
        if !allowed.contains(&caller.service_account.as_str()) {
            return Err(Denied::Forbidden(format!(
                "{}: ServiceAccount {} may not call it",
                path, caller.service_account
            )));
        }
        Ok(caller)
    }
}

/// Whether the servers check ServiceAccount tokens ([`MODE_ENV`]); a typo
/// is an error.
pub fn mode_from_env() -> Result<bool, String> {
    match std::env::var(MODE_ENV).ok().map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("token") => Ok(false),
        Some("serviceaccount") => Ok(true),
        Some(other) => Err(format!("{MODE_ENV}={other:?}: expected serviceaccount or token")),
    }
}

/// The policy for a server with `audience`, when [`MODE_ENV`] asks for it:
/// a kube client and this pod's namespace (`POD_NAMESPACE`, else the
/// service account's). `Ok(None)` when off.
pub async fn policy_from_env(audience: &'static str) -> Result<Option<Arc<Policy>>, String> {
    if !mode_from_env()? {
        return Ok(None);
    }
    crate::install_crypto_provider();
    let namespace = std::env::var("POD_NAMESPACE")
        .ok()
        .filter(|n| !n.is_empty())
        .or_else(|| std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").ok())
        .map(|n| n.trim().to_string())
        .ok_or_else(|| format!("{MODE_ENV}=serviceaccount needs POD_NAMESPACE"))?;
    let client = kube::Client::try_default().await.map_err(|e| format!("{MODE_ENV}: no Kubernetes API: {e}"))?;
    Ok(Some(Policy::new(Arc::new(KubeReviewer { client, audience, namespace }))))
}

/// A tower layer checking every call with a [`Policy`].
#[derive(Clone)]
pub struct AuthLayer {
    policy: Arc<Policy>,
}

impl AuthLayer {
    pub fn new(policy: Arc<Policy>) -> Self {
        Self { policy }
    }
}

impl<S> tower::Layer<S> for AuthLayer {
    type Service = AuthService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        AuthService { inner, policy: self.policy.clone() }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    policy: Arc<Policy>,
}

impl<S, B> tower::Service<tonic::codegen::http::Request<B>> for AuthService<S>
where
    S: tower::Service<tonic::codegen::http::Request<B>, Response = tonic::codegen::http::Response<tonic::body::BoxBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: tonic::codegen::http::Request<B>) -> Self::Future {
        // The inner service that was polled ready is the one to call.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let policy = self.policy.clone();
        Box::pin(async move {
            let path = req.uri().path().to_string();
            let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
            match policy.authorize(&path, auth.as_deref()).await {
                Ok(caller) => {
                    req.extensions_mut().insert(caller);
                    inner.call(req).await
                }
                Err(denied) => {
                    tracing::warn!("🔒 control call {path} refused: {denied:?}");
                    Ok(denied.status().to_http())
                }
            }
        })
    }
}

/// For a handler: the verified caller of `req`, if the server checks
/// ServiceAccounts.
pub fn caller_of<T>(req: &tonic::Request<T>) -> Option<&Caller> {
    req.extensions().get::<Caller>()
}

/// A data server's RPC (register, heartbeat, capacity, unregister) must be
/// about the caller's own pod: `device_id` is the pod name in the chart.
pub fn check_device<T>(req: &tonic::Request<T>, device_id: &str) -> Result<(), tonic::Status> {
    match caller_of(req) {
        Some(c) if c.service_account == DS_SA && c.pod_name.as_deref() != Some(device_id) => {
            Err(tonic::Status::permission_denied(format!(
                "pod {} may not speak for device {device_id}",
                c.pod_name.as_deref().unwrap_or("?")
            )))
        }
        _ => Ok(()),
    }
}

/// A node plugin's block-session RPC must be about its own node.
pub fn check_node<T>(req: &tonic::Request<T>, node_name: &str) -> Result<(), tonic::Status> {
    match caller_of(req) {
        Some(c) if c.service_account == NODE_SA && c.node_name.as_deref() != Some(node_name) => {
            Err(tonic::Status::permission_denied(format!(
                "node plugin on {} may not act for node {node_name}",
                c.node_name.as_deref().unwrap_or("?")
            )))
        }
        _ => Ok(()),
    }
}

/// A client's token from `file`, re-read when the kubelet rotates it
/// (checked at most every 30 s).
pub struct TokenFile {
    path: std::path::PathBuf,
    cached: Mutex<Option<(Instant, String)>>,
}

impl TokenFile {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into(), cached: Mutex::new(None) }
    }

    pub fn from_env(var: &str) -> Option<Self> {
        std::env::var(var).ok().filter(|p| !p.trim().is_empty()).map(|p| Self::new(p.trim()))
    }

    /// `Bearer <token>`, or `None` when the file cannot be read.
    pub fn header(&self) -> Option<String> {
        let mut c = self.cached.lock().unwrap();
        if let Some((at, t)) = c.as_ref() {
            if at.elapsed() < Duration::from_secs(30) {
                return Some(format!("Bearer {t}"));
            }
        }
        match std::fs::read_to_string(&self.path) {
            Ok(t) if !t.trim().is_empty() => {
                let t = t.trim().to_string();
                *c = Some((Instant::now(), t.clone()));
                Some(format!("Bearer {t}"))
            }
            _ => {
                tracing::warn!("control token file {} unreadable", self.path.display());
                c.as_ref().map(|(_, t)| format!("Bearer {t}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::s3csi::broker::ReviewError;

    struct Fixed(HashMap<&'static str, Review>, std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl Reviewer for Fixed {
        async fn review(&self, token: &str) -> Review {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0.get(token).cloned().unwrap_or(Err(ReviewError::Refused("unknown token".into())))
        }
    }

    fn caller(sa: &str, pod: Option<&str>, node: Option<&str>) -> Review {
        Ok(Caller { service_account: sa.into(), pod_name: pod.map(Into::into), node_name: node.map(Into::into) })
    }

    fn policy() -> (Arc<Policy>, Arc<Fixed>) {
        let f = Arc::new(Fixed(
            HashMap::from([
                ("ds0", caller(DS_SA, Some("flint-pnfs-ds-0"), Some("w1"))),
                ("ctl", caller(CONTROLLER_SA, Some("flint-csi-controller-x"), Some("cp"))),
                ("node", caller(NODE_SA, Some("flint-csi-node-a"), Some("w1"))),
                ("mds", caller(MDS_SA, Some("flint-pnfs-mds-0-x"), Some("w2"))),
                ("down", Err(ReviewError::Unavailable("apiserver unreachable".into()))),
            ]),
            Default::default(),
        ));
        (Policy::new(f.clone()), f)
    }

    const REG: &str = "/pnfs.control.MdsControl/RegisterDataServer";
    const CREATE: &str = "/pnfs.control.MdsControl/CreateVolume";
    const ATTACH: &str = "/pnfs.control.MdsControl/AttachBlockNode";
    const TRUNC: &str = "/pnfs.control.DsControl/TruncateStripeFile";

    #[tokio::test]
    async fn each_caller_reaches_its_own_rpcs_and_nothing_else() {
        let (p, _) = policy();
        let ok = |r: &Result<Caller, Denied>| r.is_ok();
        let forbidden = |r: &Result<Caller, Denied>| matches!(r, Err(Denied::Forbidden(_)));
        assert!(ok(&p.authorize(REG, Some("Bearer ds0")).await));
        assert!(forbidden(&p.authorize(CREATE, Some("Bearer ds0")).await), "a DS creates no volumes");
        assert!(forbidden(&p.authorize(REG, Some("Bearer ctl")).await), "the controller registers no DS");
        assert!(ok(&p.authorize(CREATE, Some("Bearer ctl")).await));
        assert!(ok(&p.authorize(ATTACH, Some("Bearer node")).await));
        assert!(forbidden(&p.authorize(CREATE, Some("Bearer node")).await), "a node creates no volumes");
        assert!(ok(&p.authorize(TRUNC, Some("Bearer mds")).await));
        assert!(forbidden(&p.authorize(TRUNC, Some("Bearer ds0")).await), "a DS does not command DSes");
        assert!(forbidden(&p.authorize("/pnfs.control.MdsControl/Unknown", Some("Bearer ctl")).await));
    }

    #[tokio::test]
    async fn no_token_a_refused_token_and_no_answer_are_three_different_refusals() {
        let (p, f) = policy();
        assert!(matches!(p.authorize(REG, None).await, Err(Denied::Unauthenticated(_))));
        assert!(matches!(p.authorize(REG, Some("Basic x")).await, Err(Denied::Unauthenticated(_))));
        assert!(matches!(p.authorize(REG, Some("Bearer stolen")).await, Err(Denied::Unauthenticated(_))));
        assert!(matches!(p.authorize(REG, Some("Bearer down")).await, Err(Denied::Unavailable(_))));
        // Verdicts are cached, an unanswered review is asked again.
        let before = f.1.load(std::sync::atomic::Ordering::SeqCst);
        let _ = p.authorize(REG, Some("Bearer ds0")).await;
        let _ = p.authorize(REG, Some("Bearer ds0")).await;
        let _ = p.authorize(REG, Some("Bearer down")).await;
        let _ = p.authorize(REG, Some("Bearer down")).await;
        assert_eq!(f.1.load(std::sync::atomic::Ordering::SeqCst) - before, 1 + 2, "ds0 once (cached), down twice");
    }

    #[test]
    fn a_data_server_speaks_only_for_its_own_pod_and_a_node_for_its_own_node() {
        let mut r = tonic::Request::new(());
        r.extensions_mut().insert(Caller {
            service_account: DS_SA.into(),
            pod_name: Some("flint-pnfs-ds-0".into()),
            node_name: Some("w1".into()),
        });
        assert!(check_device(&r, "flint-pnfs-ds-0").is_ok());
        assert!(check_device(&r, "flint-pnfs-ds-1").is_err(), "another DS's device id");
        let mut n = tonic::Request::new(());
        n.extensions_mut().insert(Caller {
            service_account: NODE_SA.into(),
            pod_name: Some("flint-csi-node-a".into()),
            node_name: Some("w1".into()),
        });
        assert!(check_node(&n, "w1").is_ok());
        assert!(check_node(&n, "w2").is_err(), "another node's session");
        // The controller acts for any node; without the checks on, no caller.
        let mut c = tonic::Request::new(());
        c.extensions_mut().insert(Caller { service_account: CONTROLLER_SA.into(), pod_name: None, node_name: None });
        assert!(check_node(&c, "w2").is_ok());
        assert!(check_device(&tonic::Request::new(()), "anything").is_ok());
    }

    #[test]
    fn malformed_paths_reach_nothing() {
        assert_eq!(allowed("/pnfs.control.MdsControl/Heartbeat"), &[DS_SA]);
        assert!(allowed("garbage").is_empty());
        assert!(allowed("/pnfs.control.MdsControl").is_empty());
        assert!(allowed("/other.Service/CreateVolume").is_empty());
    }
}
