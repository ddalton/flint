//! `flint-s3-broker` — the STS-shaped identity exchange (design §4.2).
//!
//! One Deployment, one job: turn a kubelet-minted, pod-bound
//! ServiceAccount token into short-lived, proxy-scoped S3 keys. It is
//! the only component in the design with a standing credential, and it
//! holds it for every project, so it says so: every issuance is a
//! `TokenReview`-verified, registration-bound, consumer-checked grant
//! with an audit line `(ns, sa, pod-uid, cr, expiry)`.
//!
//! Wire: `POST /` with the `AssumeRoleWithWebIdentity` form the AWS
//! clients (and the node plugin) send; XML back. Plus the node plugin's
//! registration verbs (`POST /v1/volumes`, `DELETE /v1/volumes/{id}`),
//! authenticated with the PLUGIN'S own token.
//!
//! The chain, per exchange:
//!
//! 1. `TokenReview{token, audiences: [s3.csi.chert.us]}` — online, not
//!    offline JWKS: a deleted pod's token is refused within 60 s of its
//!    `deletionTimestamp`; offline verification would honour it to `exp`.
//! 2. The `RoleSessionName` must equal the nonce of a LIVE registration
//!    the node plugin made for this pod-uid and CR — the one binding a
//!    pod cannot self-mint (§2.4 T2).
//! 3. The CR named by `RoleArn`, in the TOKEN'S namespace (never a
//!    request field), must list the SA in `spec.consumers`, and decides
//!    its ACCESS: the registration's (the pod's `csi.readOnly`, as the
//!    plugin decided it) narrowed by the CR's lists, never widened.
//! 4. The backend mints: `static` (rig / a proxy that hands out one
//!    key per project), `sts` (forward the pod token to an STS that
//!    trusts the cluster issuer — MinIO/RGW/AWS, the K0 arm), or `rest`
//!    (POST the pod token as a bearer to the application's REST API and
//!    take the keys it returns — the customer's JWT-enforcing door).
//!
//! A READ grant is minted as a credential that cannot write where the
//! backend can express one (per-user access design §4.3): `sts` attaches
//! a session policy of reads on the CR's bucket and prefix, which can only
//! narrow the role; `rest` tells the door `"access": "read"` and the door
//! scopes; `static` hands out its read key set when one is configured.
//! A `static` backend with no read key hands out its one key, and its
//! `issued` line and `/v1/status` say `cooperative`: the mount and the
//! syncer keep that pod read-only, the bucket does not.
//!
//! What it never does: read tenant Secrets, hold a bucket key of its
//! own in `sts`/`rest` mode, or accept a `RoleArn` it did not shape.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::api::{Api, PostParams};
use kube::Client;
use serde::Deserialize;
use warp::http::StatusCode;
use warp::{Filter, Rejection};

use super::creds::{self, Creds, Registration};
use super::policy::{Access, MountConsumers};
use super::resolve;
use super::DRIVER_NAME;

#[derive(Clone, PartialEq, Eq)]
pub struct StaticKeys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// An access key id, shortened the way `Creds` prints one: enough to tell
/// two key sets apart in a log, never a secret.
fn akid(k: &str) -> String {
    format!("{}…", k.chars().take(4).collect::<String>())
}

impl std::fmt::Debug for StaticKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "StaticKeys {{ access_key_id: {}, secret: <redacted>, session_token: {} }}",
            akid(&self.access_key_id),
            if self.session_token.is_some() { "<redacted>" } else { "none" }
        )
    }
}

#[derive(Clone)]
pub enum Backend {
    /// One fixed key set; `Expiration` is synthetic so clients refresh.
    /// `read`, when set, is what a READ grant gets instead: a key set the
    /// operator scoped to reads in the store's own IAM.
    Static { access_key_id: String, secret_access_key: String, session_token: Option<String>, read: Option<StaticKeys> },
    /// Forward `AssumeRoleWithWebIdentity` (with the POD's token) to a
    /// real STS that trusts the cluster issuer.
    Sts { url: String, role_arn: Option<String> },
    /// `POST <url>` with `Authorization: Bearer <pod token>` and a JSON
    /// body naming the project; expects JSON keys back.
    Rest { url: String, extra_headers: BTreeMap<String, String> },
}

/// Written by hand, never derived: the broker's start-up line prints its
/// backend, and a derived `Debug` printed the static backend's secret
/// access key at INFO into every broker pod's log from `fcac038f` to the
/// access drill that found it (2026-09-15). Header VALUES are redacted
/// too: a `rest` door's extra header is where a bearer would go.
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Static { access_key_id, session_token, read, .. } => write!(
                f,
                "Static {{ access_key_id: {}, secret: <redacted>, session_token: {}, read: {:?} }}",
                akid(access_key_id),
                if session_token.is_some() { "<redacted>" } else { "none" },
                read
            ),
            Backend::Sts { url, role_arn } => write!(f, "Sts {{ url: {url:?}, role_arn: {role_arn:?} }}"),
            Backend::Rest { url, extra_headers } => write!(
                f,
                "Rest {{ url: {url:?}, extra_headers: {:?} (values redacted) }}",
                extra_headers.keys().collect::<Vec<_>>()
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub listen: SocketAddr,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub backend: Backend,
    pub audience: String,
    /// `system:serviceaccount:<ns>:<name>` of the node plugin — the only
    /// principal allowed to register publishes.
    pub node_principal: String,
    pub max_lifetime_secs: u64,
    pub default_lifetime_secs: u64,
    /// `false` only for rigs that exercise the exchange without a plugin.
    pub require_registration: bool,
    /// The ARN partition of a read grant's session policy (`aws`,
    /// `aws-cn`, `aws-us-gov`). S3-compatible STS servers take `aws`.
    pub arn_partition: String,
}

impl BrokerConfig {
    pub fn from_env() -> Result<Self, String> {
        let opt = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let need = |k: &str| opt(k).ok_or_else(|| format!("{k} is unset"));
        let backend = match opt("FLINT_S3B_BACKEND").as_deref().unwrap_or("static") {
            "static" => Backend::Static {
                access_key_id: need("FLINT_S3B_STATIC_ACCESS_KEY_ID")?,
                secret_access_key: need("FLINT_S3B_STATIC_SECRET_ACCESS_KEY")?,
                session_token: opt("FLINT_S3B_STATIC_SESSION_TOKEN"),
                read: match (opt("FLINT_S3B_STATIC_READ_ACCESS_KEY_ID"), opt("FLINT_S3B_STATIC_READ_SECRET_ACCESS_KEY")) {
                    (Some(access_key_id), Some(secret_access_key)) => Some(StaticKeys {
                        access_key_id,
                        secret_access_key,
                        session_token: opt("FLINT_S3B_STATIC_READ_SESSION_TOKEN"),
                    }),
                    (None, None) => None,
                    // Half a key set would hand every reader a credential
                    // that fails at its first request, or — read the other
                    // way — silently fall back to the write key.
                    _ => {
                        return Err("FLINT_S3B_STATIC_READ_ACCESS_KEY_ID and FLINT_S3B_STATIC_READ_SECRET_ACCESS_KEY \
                                    are set together or not at all"
                            .into())
                    }
                },
            },
            "sts" => Backend::Sts { url: need("FLINT_S3B_STS_URL")?, role_arn: opt("FLINT_S3B_STS_ROLE_ARN") },
            "rest" => {
                let mut extra_headers = BTreeMap::new();
                if let Some(h) = opt("FLINT_S3B_REST_HEADERS") {
                    for kv in h.split(';') {
                        if let Some((k, v)) = kv.split_once('=') {
                            extra_headers.insert(k.trim().to_string(), v.trim().to_string());
                        }
                    }
                }
                Backend::Rest { url: need("FLINT_S3B_REST_URL")?, extra_headers }
            }
            other => return Err(format!("FLINT_S3B_BACKEND {other:?} is not static | sts | rest")),
        };
        Ok(Self {
            listen: opt("FLINT_S3B_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".into()).parse().map_err(|e| format!("FLINT_S3B_LISTEN: {e}"))?,
            tls_cert: opt("FLINT_S3B_TLS_CERT"),
            tls_key: opt("FLINT_S3B_TLS_KEY"),
            backend,
            audience: opt("FLINT_S3B_AUDIENCE").unwrap_or_else(|| DRIVER_NAME.into()),
            node_principal: opt("FLINT_S3B_NODE_PRINCIPAL").unwrap_or_else(|| "system:serviceaccount:flint-system:flint-s3-csi-node".into()),
            max_lifetime_secs: opt("FLINT_S3B_MAX_LIFETIME_SECS").and_then(|v| v.parse().ok()).unwrap_or(3600),
            default_lifetime_secs: opt("FLINT_S3B_DEFAULT_LIFETIME_SECS").and_then(|v| v.parse().ok()).unwrap_or(900),
            require_registration: opt("FLINT_S3B_REQUIRE_REGISTRATION").map(|v| v != "false").unwrap_or(true),
            arn_partition: opt("FLINT_S3B_ARN_PARTITION").unwrap_or_else(|| "aws".into()),
        })
    }
}

impl Backend {
    /// How a READ grant is held to reads — the `enforcement` of its
    /// `issued` line and `/v1/status`.
    pub fn read_enforcement(&self) -> &'static str {
        match self {
            Backend::Sts { .. } => "sessionPolicy",
            Backend::Rest { .. } => "restDoor",
            Backend::Static { read: Some(_), .. } => "readKey",
            Backend::Static { read: None, .. } => "cooperative",
        }
    }
}

/// The session policy a READ grant carries on the `sts` backend: object
/// reads under the CR's prefix, and listing only that prefix.
///
/// Every action here is a read. `s3:ListBucket` is bounded by the
/// `s3:prefix` condition (the only way to scope it; its resource is the
/// bucket), covering both the prefix itself and everything under it. A
/// session policy intersects with the role's own, so a broker configured
/// with a too-wide role still hands a reader keys that cannot write, and
/// cannot read another prefix. A read-write grant carries no session policy:
/// its scope is the role's, as before this field existed.
///
/// No `s3:GetObjectAttributes`: nothing flint runs calls it, and Ceph RGW
/// Squid's policy parser does not know the action, so a policy naming it is
/// refused whole (`ERR_MALFORMED_DOC`) and every read grant with it.
pub fn read_session_policy(partition: &str, bucket: &str, prefix: &str) -> String {
    let prefix = prefix.trim_matches('/');
    let objects = if prefix.is_empty() {
        format!("arn:{partition}:s3:::{bucket}/*")
    } else {
        format!("arn:{partition}:s3:::{bucket}/{prefix}/*")
    };
    let mut list = serde_json::json!({
        "Effect": "Allow",
        "Action": ["s3:ListBucket", "s3:ListBucketVersions"],
        "Resource": format!("arn:{partition}:s3:::{bucket}"),
    });
    if !prefix.is_empty() {
        list["Condition"] = serde_json::json!({ "StringLike": { "s3:prefix": [prefix, format!("{prefix}/*")] } });
    }
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:GetObjectVersion"],
                "Resource": objects,
            },
            list,
        ],
    })
    .to_string()
}

/// The CR's say in a mint: who may, and where the bucket and prefix are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub consumers: MountConsumers,
    pub bucket: String,
    pub prefix: String,
}

/// What one exchange is for, once decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub mode: String,
    pub cr: String,
    pub access: Access,
}

/// Who vouched for an identity, and therefore what KIND of principal it
/// is.
///
/// An explicit discriminator rather than inferring "a person has no
/// ServiceAccount" from an empty or absent field. The authorization
/// matcher turns on this — a person must never match a bare
/// ServiceAccount entry by happening to have the right `username`, and
/// a pod must never match a `jwt:user:` one — and a safety property
/// that rests on a type cannot be lost by a future construction site
/// forgetting the convention. That is the same lesson as
/// `ReviewError`: the cache used to infer "transport failure" from an
/// error's text and was right for exactly one implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vouched {
    /// The apiserver, via `TokenReview`. `namespace` and
    /// `service_account` are meaningful.
    Kubernetes,
    /// An external issuer, via a signature the door verified. The
    /// principal is a PERSON: `username` is the issuer's `sub`, and the
    /// ServiceAccount fields are empty and must not be read.
    Issuer,
}

/// What a verifier said about a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub username: String,
    pub namespace: String,
    pub service_account: String,
    pub pod_uid: Option<String>,
    pub pod_name: Option<String>,
    /// Who vouched. Everything below `username` is meaningful only for
    /// [`Vouched::Kubernetes`].
    pub vouched: Vouched,
}

impl Identity {
    /// A person, as an external issuer named them.
    pub fn person(sub: impl Into<String>) -> Self {
        Identity {
            username: sub.into(),
            namespace: String::new(),
            service_account: String::new(),
            pod_uid: None,
            pod_name: None,
            vouched: Vouched::Issuer,
        }
    }
}

pub fn identity_from_review(tr: &TokenReview, audience: &str) -> Result<Identity, String> {
    let st = tr.status.as_ref().ok_or("TokenReview returned no status")?;
    if let Some(e) = &st.error {
        return Err(format!("token review: {e}"));
    }
    if st.authenticated != Some(true) {
        return Err("token is not authenticated (expired, revoked, or its pod is gone)".into());
    }
    if let Some(auds) = &st.audiences {
        if !auds.iter().any(|a| a == audience) {
            return Err(format!("token audience is not {audience}"));
        }
    }
    let user = st.user.as_ref().ok_or("token review carries no user")?;
    let username = user.username.clone().unwrap_or_default();
    let rest = username
        .strip_prefix("system:serviceaccount:")
        .ok_or_else(|| format!("{username} is not a ServiceAccount"))?;
    let (ns, sa) = rest.split_once(':').ok_or_else(|| format!("{username} is malformed"))?;
    let extra = |k: &str| user.extra.as_ref().and_then(|e| e.get(k)).and_then(|v| v.first().cloned());
    Ok(Identity {
        username: username.clone(),
        namespace: ns.to_string(),
        service_account: sa.to_string(),
        pod_uid: extra("authentication.kubernetes.io/pod-uid"),
        pod_name: extra("authentication.kubernetes.io/pod-name"),
        vouched: Vouched::Kubernetes,
    })
}

/// The pure decision, given what the cluster said. Every refusal names
/// the reason — it is the tenant's error message.
///
/// The access is the registration's narrowed by the CR, never widened: a
/// plugin that registered `read` gets read whatever the CR allows, and an
/// SA the CR lists as read-only gets read whatever the registration asked.
/// A rig that waives registration gets the CR's access for the SA.
pub fn decide(
    id: &Identity,
    role_arn: &str,
    session_name: &str,
    registration: Option<&Registration>,
    require_registration: bool,
    consumers: Option<&MountConsumers>,
) -> Result<Grant, String> {
    let (mode, cr) = creds::parse_role_arn(role_arn)
        .ok_or_else(|| format!("RoleArn {role_arn:?} is not arn:flint:iam::<mode>:role/<cr>"))?;
    if require_registration {
        let reg = registration.ok_or_else(|| {
            format!("no live publish registration for RoleSessionName {session_name:?} — only the node plugin's publish path may mint for a pod")
        })?;
        if reg.namespace != id.namespace || reg.service_account != id.service_account {
            return Err("the registration belongs to another identity".into());
        }
        if reg.cr != cr || reg.mode != mode {
            return Err(format!("the registration is for {}/{}, not {mode}/{cr}", reg.mode, reg.cr));
        }
        if let Some(uid) = &id.pod_uid {
            if &reg.pod_uid != uid {
                return Err("the token's pod is not the registered pod".into());
            }
        }
    }
    let consumers = consumers.ok_or_else(|| format!("{mode} CR {}/{cr} does not exist", id.namespace))?;
    let registered_read = registration.map(|r| r.access.is_read()).unwrap_or(false);
    let Some(access) = consumers.access(&id.service_account, registered_read) else {
        return Err(format!(
            "ServiceAccount {}/{} is in neither spec.consumers.serviceAccounts nor \
             spec.consumers.readOnlyServiceAccounts of {cr}",
            id.namespace, id.service_account
        ));
    };
    Ok(Grant { mode, cr, access })
}

pub struct Broker {
    cfg: BrokerConfig,
    client: Client,
    http: reqwest::Client,
    registrations: Mutex<HashMap<String, Registration>>,
    issued: AtomicU64,
    refused: AtomicU64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StsForm {
    action: Option<String>,
    role_arn: Option<String>,
    role_session_name: Option<String>,
    web_identity_token: Option<String>,
    duration_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestCreds {
    access_key_id: String,
    secret_access_key: String,
    #[serde(default)]
    session_token: Option<String>,
    #[serde(default)]
    expiration: Option<String>,
}

impl Broker {
    pub fn new(cfg: BrokerConfig, client: Client) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            client,
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build().expect("http client"),
            registrations: Mutex::new(HashMap::new()),
            issued: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        })
    }

    async fn review(&self, token: &str) -> Result<Identity, String> {
        let api: Api<TokenReview> = Api::all(self.client.clone());
        let tr = TokenReview {
            spec: TokenReviewSpec { token: Some(token.to_string()), audiences: Some(vec![self.cfg.audience.clone()]) },
            ..Default::default()
        };
        let out = api.create(&PostParams::default(), &tr).await.map_err(|e| format!("TokenReview: {e}"))?;
        identity_from_review(&out, &self.cfg.audience)
    }

    async fn target_of(&self, mode: &str, ns: &str, cr: &str) -> Result<Option<Target>, String> {
        let sel = match mode {
            "passthrough" => super::attrs::Selector::Mount(cr.to_string()),
            "lean" => super::attrs::Selector::Workspace(cr.to_string()),
            other => return Err(format!("unknown mode {other}")),
        };
        match resolve::fetch(&self.client, &sel, ns).await {
            Ok(r) => {
                let consumers = r.policy().map_err(|e| e.message().to_string())?.consumers;
                let (bucket, prefix) = match &r {
                    resolve::Resolved::Passthrough { spec } => (spec.bucket.clone(), spec.key_prefix.clone().unwrap_or_default()),
                    resolve::Resolved::Lean { spec, .. } => (spec.bucket.clone(), spec.key_prefix.clone()),
                };
                Ok(Some(Target { consumers, bucket, prefix }))
            }
            Err(resolve::Refusal::NotFound(_)) => Ok(None),
            Err(e) => Err(e.message().to_string()),
        }
    }

    /// Mint for a decided grant, from the CR it was decided against.
    pub async fn mint(
        &self,
        id: &Identity,
        grant: &Grant,
        target: &Target,
        on_behalf_of: Option<&str>,
        pod_token: &str,
        session: &str,
        lifetime: u64,
    ) -> Result<Creds, String> {
        let (mode, cr) = (grant.mode.as_str(), grant.cr.as_str());
        let expiration = (chrono::Utc::now() + chrono::Duration::seconds(lifetime as i64)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match &self.cfg.backend {
            Backend::Static { access_key_id, secret_access_key, session_token, read } => {
                let keys = match (grant.access, read) {
                    (Access::Read, Some(r)) => r.clone(),
                    _ => StaticKeys {
                        access_key_id: access_key_id.clone(),
                        secret_access_key: secret_access_key.clone(),
                        session_token: session_token.clone(),
                    },
                };
                Ok(Creds {
                    access_key_id: keys.access_key_id,
                    secret_access_key: keys.secret_access_key,
                    session_token: keys.session_token,
                    expiration,
                })
            }
            Backend::Sts { url, role_arn } => {
                let mut form = vec![
                    ("Action", "AssumeRoleWithWebIdentity".to_string()),
                    ("Version", "2011-06-15".to_string()),
                    ("WebIdentityToken", pod_token.to_string()),
                    ("RoleSessionName", session.to_string()),
                    ("DurationSeconds", lifetime.max(900).to_string()),
                ];
                if let Some(r) = role_arn {
                    form.push(("RoleArn", r.clone()));
                }
                if grant.access.is_read() {
                    form.push(("Policy", read_session_policy(&self.cfg.arn_partition, &target.bucket, &target.prefix)));
                }
                let resp = self.http.post(url).form(&form).send().await.map_err(|e| format!("upstream STS: {e}"))?;
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(format!(
                        "upstream STS refused ({status}): {}",
                        creds::sts_error_message(&body).unwrap_or_else(|| body.chars().take(300).collect())
                    ));
                }
                creds::parse_sts_xml(&body)
            }
            Backend::Rest { url, extra_headers } => {
                let mut req = self.http.post(url).bearer_auth(pod_token).json(&serde_json::json!({
                    "namespace": id.namespace,
                    "serviceAccount": id.service_account,
                    "podUid": id.pod_uid,
                    "cr": cr,
                    "mode": mode,
                    "durationSeconds": lifetime,
                    // The door decides and scopes: `read` asks it for keys
                    // that cannot write (per-user access design §4.3).
                    "access": grant.access.as_str(),
                    "onBehalfOf": on_behalf_of,
                }));
                for (k, v) in extra_headers {
                    req = req.header(k, v);
                }
                let resp = req.send().await.map_err(|e| format!("application REST: {e}"))?;
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(format!("application REST refused ({status}): {}", body.chars().take(300).collect::<String>()));
                }
                let r: RestCreds = serde_json::from_str(&body).map_err(|e| format!("application REST body: {e}"))?;
                Ok(Creds {
                    access_key_id: r.access_key_id,
                    secret_access_key: r.secret_access_key,
                    session_token: r.session_token,
                    expiration: r.expiration.unwrap_or(expiration),
                })
            }
        }
    }

    /// `POST /` — the exchange.
    pub async fn assume(&self, form: HashMap<String, String>) -> (StatusCode, String) {
        let f: StsForm = match serde_urlencoded::from_str(&serde_urlencoded::to_string(&form).unwrap_or_default()) {
            Ok(f) => f,
            Err(e) => return sts_error(StatusCode::BAD_REQUEST, "InvalidInput", &e.to_string()),
        };
        if f.action.as_deref() != Some("AssumeRoleWithWebIdentity") {
            return sts_error(StatusCode::BAD_REQUEST, "InvalidAction", "only AssumeRoleWithWebIdentity is served");
        }
        let (Some(role_arn), Some(token)) = (f.role_arn.as_deref(), f.web_identity_token.as_deref()) else {
            return sts_error(StatusCode::BAD_REQUEST, "MissingParameter", "RoleArn and WebIdentityToken are required");
        };
        let session = f.role_session_name.clone().unwrap_or_default();
        let id = match self.review(token).await {
            Ok(id) => id,
            Err(e) => {
                self.refused.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("exchange refused (InvalidIdentityToken) at TokenReview: {e}");
                // 400, as AWS STS answers it: a client can tell "not a
                // valid token" from "a valid token with no entitlement".
                return sts_error(StatusCode::BAD_REQUEST, "InvalidIdentityToken", &e);
            }
        };
        let reg = self.registrations.lock().unwrap().values().find(|r| r.nonce == session).cloned();
        let (mode, cr) = match creds::parse_role_arn(role_arn) {
            Some(x) => x,
            None => return sts_error(StatusCode::BAD_REQUEST, "InvalidParameterValue", &format!("RoleArn {role_arn:?}")),
        };
        let target = match self.target_of(&mode, &id.namespace, &cr).await {
            Ok(t) => t,
            Err(e) => return sts_error(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", &e),
        };
        let grant = match decide(&id, role_arn, &session, reg.as_ref(), self.cfg.require_registration, target.as_ref().map(|t| &t.consumers)) {
            Ok(g) => g,
            Err(e) => {
                self.refused.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(ns = %id.namespace, sa = %id.service_account, pod_uid = ?id.pod_uid, cr = %cr, "exchange refused (AccessDenied): {e}");
                return sts_error(StatusCode::FORBIDDEN, "AccessDenied", &e);
            }
        };
        // `decide` refuses a CR that does not exist, so a grant has one.
        let Some(target) = target else {
            return sts_error(StatusCode::FORBIDDEN, "AccessDenied", &format!("{mode} CR {}/{cr} does not exist", id.namespace));
        };
        let on_behalf_of = reg.as_ref().and_then(|r| r.on_behalf_of.clone());
        let lifetime = f.duration_seconds.unwrap_or(self.cfg.default_lifetime_secs).clamp(60, self.cfg.max_lifetime_secs);
        let enforcement = if grant.access.is_read() { self.cfg.backend.read_enforcement() } else { "none" };
        match self.mint(&id, &grant, &target, on_behalf_of.as_deref(), token, &session, lifetime).await {
            Ok(c) => {
                self.issued.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    ns = %id.namespace,
                    sa = %id.service_account,
                    pod_uid = ?id.pod_uid,
                    cr = %cr,
                    mode = %mode,
                    access = grant.access.as_str(),
                    enforcement,
                    on_behalf_of = ?on_behalf_of,
                    exp = %c.expiration,
                    "issued"
                );
                (StatusCode::OK, sts_success(&id, &cr, &session, &c, &self.cfg.audience))
            }
            Err(e) => {
                self.refused.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(ns = %id.namespace, sa = %id.service_account, cr = %cr, "backend failed: {e}");
                sts_error(StatusCode::BAD_GATEWAY, "IDPCommunicationError", &e)
            }
        }
    }

    async fn node_authenticated(&self, bearer: Option<String>) -> Result<(), String> {
        let token = bearer
            .as_deref()
            .and_then(|b| b.strip_prefix("Bearer "))
            .ok_or("missing bearer")?;
        let id = self.review(token).await?;
        if id.username != self.cfg.node_principal {
            return Err(format!("{} may not register publishes", id.username));
        }
        Ok(())
    }

    pub async fn register(&self, bearer: Option<String>, reg: Registration) -> (StatusCode, String) {
        if let Err(e) = self.node_authenticated(bearer).await {
            return (StatusCode::FORBIDDEN, e);
        }
        tracing::info!(
            volume = %reg.volume_id,
            ns = %reg.namespace,
            sa = %reg.service_account,
            cr = %reg.cr,
            node = %reg.node,
            access = reg.access.as_str(),
            on_behalf_of = ?reg.on_behalf_of,
            "registered"
        );
        self.registrations.lock().unwrap().insert(reg.volume_id.clone(), reg);
        (StatusCode::NO_CONTENT, String::new())
    }

    pub async fn deregister(&self, bearer: Option<String>, volume_id: String) -> (StatusCode, String) {
        if let Err(e) = self.node_authenticated(bearer).await {
            return (StatusCode::FORBIDDEN, e);
        }
        let removed = self.registrations.lock().unwrap().remove(&volume_id).is_some();
        tracing::info!(volume = %volume_id, removed, "deregistered");
        (if removed { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }, String::new())
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "registrations": self.registrations.lock().unwrap().len(),
            "issued": self.issued.load(Ordering::Relaxed),
            "refused": self.refused.load(Ordering::Relaxed),
            "backend": match &self.cfg.backend { Backend::Static{..} => "static", Backend::Sts{..} => "sts", Backend::Rest{..} => "rest" },
            // How a read-only pod is held to reads by THIS broker:
            // `cooperative` means its key could write.
            "readEnforcement": self.cfg.backend.read_enforcement(),
        })
    }

    /// Serve. Blocks.
    pub async fn serve(self: Arc<Self>) {
        if self.cfg.backend.read_enforcement() == "cooperative" {
            tracing::warn!(
                "backend static has no read key (FLINT_S3B_STATIC_READ_*): a read-only pod gets the one key, \
                 which can write — its mount and its syncer keep it read-only, the bucket does not"
            );
        }
        let b = self.clone();
        let assume = warp::post()
            .and(warp::path::end())
            .and(warp::body::form::<HashMap<String, String>>())
            .and_then(move |form| {
                let b = b.clone();
                async move {
                    let (code, body) = b.assume(form).await;
                    Ok::<_, Rejection>(warp::reply::with_status(warp::reply::with_header(body, "content-type", "text/xml"), code))
                }
            });
        let b = self.clone();
        let register = warp::post()
            .and(warp::path!("v1" / "volumes"))
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::body::json::<Registration>())
            .and_then(move |auth, reg| {
                let b = b.clone();
                async move {
                    let (code, body) = b.register(auth, reg).await;
                    Ok::<_, Rejection>(warp::reply::with_status(body, code))
                }
            });
        let b = self.clone();
        let deregister = warp::delete()
            .and(warp::path!("v1" / "volumes" / String))
            .and(warp::header::optional::<String>("authorization"))
            .and_then(move |vid, auth| {
                let b = b.clone();
                async move {
                    let (code, body) = b.deregister(auth, vid).await;
                    Ok::<_, Rejection>(warp::reply::with_status(body, code))
                }
            });
        let b = self.clone();
        let status = warp::get().and(warp::path!("v1" / "status")).map(move || warp::reply::json(&b.status()));
        let healthz = warp::get().and(warp::path("healthz")).map(|| "ok");
        let routes = assume.or(register).or(deregister).or(status).or(healthz).with(warp::log("flint_s3_broker"));
        match (&self.cfg.tls_cert, &self.cfg.tls_key) {
            (Some(c), Some(k)) => {
                tracing::info!("flint-s3-broker serving https on {}", self.cfg.listen);
                warp::serve(routes).tls().cert_path(c).key_path(k).run(self.cfg.listen).await
            }
            _ => {
                tracing::info!("flint-s3-broker serving http on {}", self.cfg.listen);
                warp::serve(routes).run(self.cfg.listen).await
            }
        }
    }
}

fn sts_error(code: StatusCode, sts_code: &str, message: &str) -> (StatusCode, String) {
    (
        code,
        format!(
            "<ErrorResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><Error><Type>Sender</Type><Code>{}</Code><Message>{}</Message></Error><RequestId>{}</RequestId></ErrorResponse>",
            creds::xml_escape(sts_code),
            creds::xml_escape(message),
            uuid::Uuid::new_v4()
        ),
    )
}

pub fn sts_success(id: &Identity, cr: &str, session: &str, c: &Creds, audience: &str) -> String {
    let e = creds::xml_escape;
    format!(
        "<AssumeRoleWithWebIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
<AssumeRoleWithWebIdentityResult>\
<SubjectFromWebIdentityToken>{}</SubjectFromWebIdentityToken>\
<Audience>{}</Audience>\
<AssumedRoleUser><Arn>arn:flint:sts::{}:assumed-role/{}/{}</Arn><AssumedRoleId>{}:{}</AssumedRoleId></AssumedRoleUser>\
<Credentials><AccessKeyId>{}</AccessKeyId><SecretAccessKey>{}</SecretAccessKey><SessionToken>{}</SessionToken><Expiration>{}</Expiration></Credentials>\
<Provider>flint-s3-broker</Provider>\
</AssumeRoleWithWebIdentityResult>\
<ResponseMetadata><RequestId>{}</RequestId></ResponseMetadata>\
</AssumeRoleWithWebIdentityResponse>",
        e(&id.username),
        e(audience),
        e(&id.namespace),
        e(cr),
        e(session),
        e(cr),
        e(session),
        e(&c.access_key_id),
        e(&c.secret_access_key),
        e(c.session_token.as_deref().unwrap_or("")),
        e(&c.expiration),
        uuid::Uuid::new_v4()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};

    fn review(authenticated: bool, user: &str, pod_uid: Option<&str>, auds: Vec<&str>) -> TokenReview {
        TokenReview {
            status: Some(TokenReviewStatus {
                authenticated: Some(authenticated),
                audiences: Some(auds.into_iter().map(String::from).collect()),
                user: Some(UserInfo {
                    username: Some(user.into()),
                    extra: pod_uid.map(|u| BTreeMap::from([("authentication.kubernetes.io/pod-uid".to_string(), vec![u.to_string()])])),
                    ..Default::default()
                }),
                error: None,
            }),
            ..Default::default()
        }
    }

    fn id() -> Identity {
        Identity { username: "system:serviceaccount:team-a:trainer".into(), namespace: "team-a".into(), service_account: "trainer".into(), pod_uid: Some("p1".into()), pod_name: None, vouched: Vouched::Kubernetes }
    }

    fn reg(nonce: &str) -> Registration {
        Registration {
            volume_id: "v".into(),
            pod_uid: "p1".into(),
            namespace: "team-a".into(),
            pod: "agent".into(),
            service_account: "trainer".into(),
            cr: "datasets".into(),
            mode: "passthrough".into(),
            nonce: nonce.into(),
            node: "n".into(),
            access: Access::ReadWrite,
            on_behalf_of: None,
        }
    }

    fn rw(list: &[&str]) -> MountConsumers {
        MountConsumers { service_accounts: list.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn token_review_identity_needs_authenticated_sa_and_audience() {
        let ok = identity_from_review(&review(true, "system:serviceaccount:team-a:trainer", Some("p1"), vec!["s3.csi.chert.us"]), "s3.csi.chert.us").unwrap();
        assert_eq!(ok, id());
        assert!(identity_from_review(&review(false, "system:serviceaccount:team-a:trainer", None, vec!["s3.csi.chert.us"]), "s3.csi.chert.us").is_err());
        assert!(identity_from_review(&review(true, "system:serviceaccount:team-a:trainer", None, vec!["other"]), "s3.csi.chert.us").unwrap_err().contains("audience"));
        assert!(identity_from_review(&review(true, "alice", None, vec!["s3.csi.chert.us"]), "s3.csi.chert.us").unwrap_err().contains("not a ServiceAccount"));
    }

    #[test]
    fn decide_refuses_each_break_in_the_chain_by_name() {
        let allow = rw(&["trainer"]);
        let arn = creds::role_arn("passthrough", "datasets");
        assert_eq!(
            decide(&id(), &arn, "n1", Some(&reg("n1")), true, Some(&allow)).unwrap(),
            Grant { mode: "passthrough".into(), cr: "datasets".into(), access: Access::ReadWrite }
        );
        // No registration.
        assert!(decide(&id(), &arn, "n1", None, true, Some(&allow)).unwrap_err().contains("registration"));
        // Wrong pod.
        let mut other = reg("n1");
        other.pod_uid = "p2".into();
        assert!(decide(&id(), &arn, "n1", Some(&other), true, Some(&allow)).unwrap_err().contains("not the registered pod"));
        // Registration for another CR: a token for project A presented for project B.
        let arn_b = creds::role_arn("passthrough", "other");
        assert!(decide(&id(), &arn_b, "n1", Some(&reg("n1")), true, Some(&allow)).unwrap_err().contains("not passthrough/other"));
        // Not a consumer.
        let deny = rw(&["bob"]);
        assert!(decide(&id(), &arn, "n1", Some(&reg("n1")), true, Some(&deny)).unwrap_err().contains("spec.consumers"));
        // CR gone.
        assert!(decide(&id(), &arn, "n1", Some(&reg("n1")), true, None).unwrap_err().contains("does not exist"));
        // Bad ARN.
        assert!(decide(&id(), "arn:aws:iam::1:role/x", "n1", Some(&reg("n1")), true, Some(&allow)).unwrap_err().contains("RoleArn"));
        // Rigs may waive registration; consumers still apply.
        assert!(decide(&id(), &arn, "", None, false, Some(&allow)).is_ok());
        assert!(decide(&id(), &arn, "", None, false, Some(&deny)).is_err());
    }

    #[test]
    fn a_grant_is_the_registration_narrowed_by_the_cr_never_widened() {
        let arn = creds::role_arn("lean", "datasets");
        let mut r = reg("n1");
        r.mode = "lean".into();
        let ro = MountConsumers { read_only_service_accounts: vec!["trainer".into()], ..Default::default() };
        let access = |reg: Option<&Registration>, required: bool, c: &MountConsumers| {
            decide(&id(), &arn, "n1", reg, required, Some(c)).unwrap().access
        };
        // The plugin registered read-write; the CR says read-only: read.
        assert_eq!(access(Some(&r), true, &ro), Access::Read);
        // The plugin registered read (the pod's csi.readOnly); the CR would
        // allow writing: still read.
        r.access = Access::Read;
        assert_eq!(access(Some(&r), true, &rw(&["trainer"])), Access::Read);
        // Control: both read-write.
        r.access = Access::ReadWrite;
        assert_eq!(access(Some(&r), true, &rw(&["trainer"])), Access::ReadWrite);
        // A rig without registration gets the CR's access for the SA.
        assert_eq!(access(None, false, &ro), Access::Read);
        assert_eq!(access(None, false, &rw(&["trainer"])), Access::ReadWrite);
    }

    #[test]
    fn the_read_session_policy_reads_the_prefix_and_nothing_else() {
        let got: serde_json::Value = serde_json::from_str(&read_session_policy("aws", "b", "/ws/proj1/")).unwrap();
        let want = serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [
                { "Effect": "Allow",
                  "Action": ["s3:GetObject", "s3:GetObjectVersion"],
                  "Resource": "arn:aws:s3:::b/ws/proj1/*" },
                { "Effect": "Allow",
                  "Action": ["s3:ListBucket", "s3:ListBucketVersions"],
                  "Resource": "arn:aws:s3:::b",
                  "Condition": { "StringLike": { "s3:prefix": ["ws/proj1", "ws/proj1/*"] } } },
            ],
        });
        assert_eq!(got, want);
        // No action in it writes: a future edit that adds one fails here.
        for st in got["Statement"].as_array().unwrap() {
            for a in st["Action"].as_array().unwrap() {
                let a = a.as_str().unwrap();
                assert!(a.starts_with("s3:Get") || a.starts_with("s3:List"), "{a} is not a read");
                assert_ne!(a, "s3:GetObjectAttributes", "RGW Squid refuses a policy naming it, and with it every read grant");
            }
        }
        // A workspace at the bucket root: every object, and no condition.
        let root: serde_json::Value = serde_json::from_str(&read_session_policy("aws-us-gov", "b", "")).unwrap();
        assert_eq!(root["Statement"][0]["Resource"], "arn:aws-us-gov:s3:::b/*");
        assert!(root["Statement"][1].get("Condition").is_none());
        // Written for the live check (lean/e2e/access/read-grant-minio.sh).
        if let Ok(out) = std::env::var("FLINT_S3B_WRITE_READ_POLICY") {
            let (bucket, prefix) = std::env::var("FLINT_S3B_READ_POLICY_TARGET")
                .ok()
                .and_then(|t| t.split_once('/').map(|(b, p)| (b.to_string(), p.to_string())))
                .unwrap_or(("b".into(), "ws/proj1".into()));
            std::fs::write(out, read_session_policy("aws", &bucket, &prefix)).unwrap();
        }
    }

    #[test]
    fn the_start_up_line_never_prints_a_secret() {
        let backends = [
            Backend::Static {
                access_key_id: "AKIAWRITEKEY".into(),
                secret_access_key: "SECRET-WRITE".into(),
                session_token: Some("TOKEN-WRITE".into()),
                read: Some(StaticKeys {
                    access_key_id: "AKIAREADKEY".into(),
                    secret_access_key: "SECRET-READ".into(),
                    session_token: Some("TOKEN-READ".into()),
                }),
            },
            Backend::Rest {
                url: "https://door".into(),
                extra_headers: BTreeMap::from([("Authorization".to_string(), "Bearer HEADER-SECRET".to_string())]),
            },
        ];
        for b in backends {
            // As the binary prints it: the whole config, at `?`.
            let cfg = BrokerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                tls_cert: None,
                tls_key: None,
                backend: b,
                audience: DRIVER_NAME.into(),
                node_principal: "system:serviceaccount:flint-system:node".into(),
                max_lifetime_secs: 3600,
                default_lifetime_secs: 900,
                require_registration: true,
                arn_partition: "aws".into(),
            };
            let line = format!("{cfg:?}");
            for secret in ["SECRET-WRITE", "TOKEN-WRITE", "SECRET-READ", "TOKEN-READ", "HEADER-SECRET"] {
                assert!(!line.contains(secret), "{secret} printed: {line}");
            }
            assert!(line.contains("AKIA…") || line.contains("Authorization"), "{line}");
        }
    }

    #[test]
    fn read_enforcement_names_how_each_backend_holds_a_reader() {
        let keys = StaticKeys { access_key_id: "R".into(), secret_access_key: "r".into(), session_token: None };
        let st = |read| Backend::Static { access_key_id: "W".into(), secret_access_key: "w".into(), session_token: None, read };
        assert_eq!(st(None).read_enforcement(), "cooperative");
        assert_eq!(st(Some(keys)).read_enforcement(), "readKey");
        assert_eq!(Backend::Sts { url: "u".into(), role_arn: None }.read_enforcement(), "sessionPolicy");
        assert_eq!(Backend::Rest { url: "u".into(), extra_headers: BTreeMap::new() }.read_enforcement(), "restDoor");
    }

    /// A broker over `backend`, for `mint`. Its kube client points at
    /// nothing: `mint` never calls the apiserver.
    fn broker(backend: Backend) -> Arc<Broker> {
        let cfg = BrokerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls_cert: None,
            tls_key: None,
            backend,
            audience: DRIVER_NAME.into(),
            node_principal: "system:serviceaccount:flint-system:node".into(),
            max_lifetime_secs: 3600,
            default_lifetime_secs: 900,
            require_registration: true,
            arn_partition: "aws".into(),
        };
        crate::install_crypto_provider();
        let client = Client::try_from(kube::Config::new("http://127.0.0.1:9".parse().unwrap())).unwrap();
        Broker::new(cfg, client)
    }

    fn target() -> Target {
        Target { consumers: MountConsumers::default(), bucket: "b".into(), prefix: "ws/proj1".into() }
    }

    fn grant(access: Access) -> Grant {
        Grant { mode: "lean".into(), cr: "proj1".into(), access }
    }

    /// One request at a time, captured, answered with `reply`.
    async fn capture(reply: String) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let route = warp::post().and(warp::body::bytes()).map(move |b: bytes::Bytes| {
            s2.lock().unwrap().push(String::from_utf8_lossy(&b).into_owned());
            reply.clone()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);
        (format!("http://{addr}/"), seen)
    }

    #[tokio::test]
    async fn sts_attaches_the_read_policy_to_a_read_grant_and_nothing_to_a_write_grant() {
        let c = Creds { access_key_id: "AK".into(), secret_access_key: "SK".into(), session_token: Some("t".into()), expiration: "2026-09-02T00:15:00Z".into() };
        let (url, seen) = capture(sts_success(&id(), "proj1", "n1", &c, DRIVER_NAME)).await;
        let b = broker(Backend::Sts { url, role_arn: Some("arn:aws:iam::1:role/agents".into()) });
        let form = |i: usize| -> HashMap<String, String> { serde_urlencoded::from_str(&seen.lock().unwrap()[i]).unwrap() };

        assert_eq!(b.mint(&id(), &grant(Access::Read), &target(), Some("alice"), "tok", "n1", 900).await.unwrap(), c);
        let f = form(0);
        assert_eq!(f.get("Policy"), Some(&read_session_policy("aws", "b", "ws/proj1")), "{f:?}");
        assert_eq!(f.get("RoleArn").map(String::as_str), Some("arn:aws:iam::1:role/agents"));

        b.mint(&id(), &grant(Access::ReadWrite), &target(), None, "tok", "n1", 900).await.unwrap();
        assert_eq!(form(1).get("Policy"), None, "a write grant keeps the role's own scope");
    }

    #[tokio::test]
    async fn rest_tells_the_door_the_access_and_who_the_pod_acts_for() {
        let (url, seen) = capture(r#"{"accessKeyId":"AK","secretAccessKey":"SK"}"#.into()).await;
        let b = broker(Backend::Rest { url, extra_headers: BTreeMap::new() });
        b.mint(&id(), &grant(Access::Read), &target(), Some("alice@example.com"), "tok", "n1", 900).await.unwrap();
        b.mint(&id(), &grant(Access::ReadWrite), &target(), None, "tok", "n1", 900).await.unwrap();
        let body = |i: usize| -> serde_json::Value { serde_json::from_str(&seen.lock().unwrap()[i]).unwrap() };
        assert_eq!(body(0)["access"], "read");
        assert_eq!(body(0)["onBehalfOf"], "alice@example.com");
        assert_eq!(body(0)["cr"], "proj1");
        assert_eq!(body(1)["access"], "readWrite");
        assert!(body(1)["onBehalfOf"].is_null());
    }

    #[tokio::test]
    async fn static_hands_a_read_grant_its_read_key_when_it_has_one() {
        let read = StaticKeys { access_key_id: "READ".into(), secret_access_key: "r".into(), session_token: Some("rt".into()) };
        let with = broker(Backend::Static { access_key_id: "WRITE".into(), secret_access_key: "w".into(), session_token: None, read: Some(read) });
        let r = with.mint(&id(), &grant(Access::Read), &target(), None, "tok", "n1", 900).await.unwrap();
        assert_eq!((r.access_key_id.as_str(), r.session_token.as_deref()), ("READ", Some("rt")));
        let w = with.mint(&id(), &grant(Access::ReadWrite), &target(), None, "tok", "n1", 900).await.unwrap();
        assert_eq!(w.access_key_id, "WRITE");
        // Without one, the reader gets the one key (and the status says cooperative).
        let without = broker(Backend::Static { access_key_id: "WRITE".into(), secret_access_key: "w".into(), session_token: None, read: None });
        assert_eq!(without.mint(&id(), &grant(Access::Read), &target(), None, "tok", "n1", 900).await.unwrap().access_key_id, "WRITE");
        assert_eq!(without.status()["readEnforcement"], "cooperative");
        assert_eq!(with.status()["readEnforcement"], "readKey");
    }

    #[test]
    fn sts_xml_round_trips_through_the_client_parser() {
        let c = Creds { access_key_id: "AK".into(), secret_access_key: "s<k".into(), session_token: Some("t".into()), expiration: "2026-09-02T00:15:00Z".into() };
        let body = sts_success(&id(), "datasets", "n1", &c, "s3.csi.chert.us");
        assert_eq!(creds::parse_sts_xml(&body).unwrap(), c);
        let (code, err) = sts_error(StatusCode::FORBIDDEN, "AccessDenied", "bob & co");
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(creds::sts_error_message(&err).unwrap(), "AccessDenied: bob & co");
    }

    #[test]
    fn config_backends_parse() {
        for k in ["FLINT_S3B_BACKEND", "FLINT_S3B_STATIC_ACCESS_KEY_ID", "FLINT_S3B_STATIC_SECRET_ACCESS_KEY", "FLINT_S3B_REST_URL", "FLINT_S3B_STS_URL"] {
            std::env::remove_var(k);
        }
        assert!(BrokerConfig::from_env().unwrap_err().contains("FLINT_S3B_STATIC_ACCESS_KEY_ID"));
        std::env::set_var("FLINT_S3B_BACKEND", "rest");
        assert!(BrokerConfig::from_env().unwrap_err().contains("FLINT_S3B_REST_URL"));
        std::env::set_var("FLINT_S3B_REST_URL", "http://app/creds");
        std::env::set_var("FLINT_S3B_REST_HEADERS", "X-A=1; X-B = 2");
        let c = BrokerConfig::from_env().unwrap();
        match c.backend {
            Backend::Rest { url, extra_headers } => {
                assert_eq!(url, "http://app/creds");
                assert_eq!(extra_headers.get("X-B").map(String::as_str), Some("2"));
            }
            _ => panic!(),
        }
        assert!(c.require_registration);
        assert_eq!(c.arn_partition, "aws");
        // Static: a read key set is both halves or neither.
        std::env::set_var("FLINT_S3B_BACKEND", "static");
        std::env::set_var("FLINT_S3B_STATIC_ACCESS_KEY_ID", "W");
        std::env::set_var("FLINT_S3B_STATIC_SECRET_ACCESS_KEY", "w");
        assert!(matches!(BrokerConfig::from_env().unwrap().backend, Backend::Static { read: None, .. }));
        std::env::set_var("FLINT_S3B_STATIC_READ_ACCESS_KEY_ID", "R");
        assert!(BrokerConfig::from_env().unwrap_err().contains("together"));
        std::env::set_var("FLINT_S3B_STATIC_READ_SECRET_ACCESS_KEY", "r");
        match BrokerConfig::from_env().unwrap().backend {
            Backend::Static { read: Some(k), .. } => assert_eq!(k.access_key_id, "R"),
            other => panic!("{other:?}"),
        }
        for k in ["FLINT_S3B_STATIC_ACCESS_KEY_ID", "FLINT_S3B_STATIC_SECRET_ACCESS_KEY", "FLINT_S3B_STATIC_READ_ACCESS_KEY_ID", "FLINT_S3B_STATIC_READ_SECRET_ACCESS_KEY"] {
            std::env::remove_var(k);
        }
        std::env::set_var("FLINT_S3B_BACKEND", "bogus");
        assert!(BrokerConfig::from_env().unwrap_err().contains("bogus"));
        std::env::remove_var("FLINT_S3B_BACKEND");
        std::env::remove_var("FLINT_S3B_REST_URL");
        std::env::remove_var("FLINT_S3B_REST_HEADERS");
    }
}
