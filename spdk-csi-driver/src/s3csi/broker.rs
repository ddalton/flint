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
//!    request field), must list the SA in `spec.consumers`.
//! 4. The backend mints: `static` (rig / a proxy that hands out one
//!    key per project), `sts` (forward the pod token to an STS that
//!    trusts the cluster issuer — MinIO/RGW/AWS, the K0 arm), or `rest`
//!    (POST the pod token as a bearer to the application's REST API and
//!    take the keys it returns — the customer's JWT-enforcing door).
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
use super::policy::Consumers;
use super::resolve;
use super::DRIVER_NAME;

#[derive(Debug, Clone)]
pub enum Backend {
    /// One fixed key set; `Expiration` is synthetic so clients refresh.
    Static { access_key_id: String, secret_access_key: String, session_token: Option<String> },
    /// Forward `AssumeRoleWithWebIdentity` (with the POD's token) to a
    /// real STS that trusts the cluster issuer.
    Sts { url: String, role_arn: Option<String> },
    /// `POST <url>` with `Authorization: Bearer <pod token>` and a JSON
    /// body naming the project; expects JSON keys back.
    Rest { url: String, extra_headers: BTreeMap<String, String> },
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
        })
    }
}

/// What `TokenReview` said about a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub username: String,
    pub namespace: String,
    pub service_account: String,
    pub pod_uid: Option<String>,
    pub pod_name: Option<String>,
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
    })
}

/// The pure decision, given what the cluster said. Every refusal names
/// the reason — it is the tenant's error message.
pub fn decide(
    id: &Identity,
    role_arn: &str,
    session_name: &str,
    registration: Option<&Registration>,
    require_registration: bool,
    consumers: Option<&Consumers>,
) -> Result<(String, String), String> {
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
    if !consumers.allows(&id.service_account) {
        return Err(format!(
            "ServiceAccount {}/{} is not in spec.consumers.serviceAccounts of {cr}",
            id.namespace, id.service_account
        ));
    }
    Ok((mode, cr))
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

    async fn consumers_of(&self, mode: &str, ns: &str, cr: &str) -> Result<Option<Consumers>, String> {
        let sel = match mode {
            "passthrough" => super::attrs::Selector::Mount(cr.to_string()),
            "lean" => super::attrs::Selector::Workspace(cr.to_string()),
            other => return Err(format!("unknown mode {other}")),
        };
        match resolve::fetch(&self.client, &sel, ns).await {
            Ok(r) => Ok(Some(r.policy().map_err(|e| e.message().to_string())?.consumers)),
            Err(resolve::Refusal::NotFound(_)) => Ok(None),
            Err(e) => Err(e.message().to_string()),
        }
    }

    async fn mint(&self, id: &Identity, mode: &str, cr: &str, pod_token: &str, session: &str, lifetime: u64) -> Result<Creds, String> {
        let expiration = (chrono::Utc::now() + chrono::Duration::seconds(lifetime as i64)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match &self.cfg.backend {
            Backend::Static { access_key_id, secret_access_key, session_token } => Ok(Creds {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
                expiration,
            }),
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
        let consumers = match self.consumers_of(&mode, &id.namespace, &cr).await {
            Ok(c) => c,
            Err(e) => return sts_error(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", &e),
        };
        if let Err(e) = decide(&id, role_arn, &session, reg.as_ref(), self.cfg.require_registration, consumers.as_ref()) {
            self.refused.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(ns = %id.namespace, sa = %id.service_account, pod_uid = ?id.pod_uid, cr = %cr, "exchange refused (AccessDenied): {e}");
            return sts_error(StatusCode::FORBIDDEN, "AccessDenied", &e);
        }
        let lifetime = f.duration_seconds.unwrap_or(self.cfg.default_lifetime_secs).clamp(60, self.cfg.max_lifetime_secs);
        match self.mint(&id, &mode, &cr, token, &session, lifetime).await {
            Ok(c) => {
                self.issued.fetch_add(1, Ordering::Relaxed);
                tracing::info!(ns = %id.namespace, sa = %id.service_account, pod_uid = ?id.pod_uid, cr = %cr, mode = %mode, exp = %c.expiration, "issued");
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
        tracing::info!(volume = %reg.volume_id, ns = %reg.namespace, sa = %reg.service_account, cr = %reg.cr, node = %reg.node, "registered");
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
        })
    }

    /// Serve. Blocks.
    pub async fn serve(self: Arc<Self>) {
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
        Identity { username: "system:serviceaccount:team-a:trainer".into(), namespace: "team-a".into(), service_account: "trainer".into(), pod_uid: Some("p1".into()), pod_name: None }
    }

    fn reg(nonce: &str) -> Registration {
        Registration { volume_id: "v".into(), pod_uid: "p1".into(), namespace: "team-a".into(), pod: "agent".into(), service_account: "trainer".into(), cr: "datasets".into(), mode: "passthrough".into(), nonce: nonce.into(), node: "n".into() }
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
        let allow = Consumers { service_accounts: vec!["trainer".into()] };
        let arn = creds::role_arn("passthrough", "datasets");
        assert_eq!(decide(&id(), &arn, "n1", Some(&reg("n1")), true, Some(&allow)).unwrap(), ("passthrough".into(), "datasets".into()));
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
        let deny = Consumers { service_accounts: vec!["bob".into()] };
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
        std::env::set_var("FLINT_S3B_BACKEND", "bogus");
        assert!(BrokerConfig::from_env().unwrap_err().contains("bogus"));
        std::env::remove_var("FLINT_S3B_BACKEND");
        std::env::remove_var("FLINT_S3B_REST_URL");
        std::env::remove_var("FLINT_S3B_REST_HEADERS");
    }
}
