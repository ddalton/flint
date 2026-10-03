//! How a worker gets its S3 credential (design §4.4, §4.5), and the
//! plugin's client to `flint-s3-broker`.
//!
//! Five arms behind one seam:
//!
//! - `broker` (default): the PLUGIN exchanges the pod-bound ServiceAccount
//!   token at the broker for short-lived keys and writes them host-side
//!   into the worker's memory-backed `comm` dir as `creds.json`; the
//!   worker's PID 1 serves them on the loopback container-credentials
//!   door, which mount-s3 (CRT) and the Rust SDK consume unchanged and
//!   re-fetch before `Expiration`. Republish re-exchanges when the keys
//!   are within 420 s of expiry (`node::BROKER_REFRESH_SECS`: the
//!   mounter asks the door once, `node::MOUNTER_ASKS_SECS_BEFORE_EXPIRY`
//!   before, and a republish can be `node::REPUBLISH_MAX_SECS` late).
//! - `webIdentity`: the WORKER calls the broker's STS façade itself with
//!   the token file the plugin keeps fresh. Needs the broker's TLS
//!   trusted by the mounter image (the CRT's web-identity provider is
//!   HTTPS-only), which is why it is not the default.
//! - `static`: the pod's `nodePublishSecretRef` — kubelet fetched it with
//!   kubelet's credentials and delivered it in `secrets`; the node SA
//!   needs no Secrets RBAC. Today's trust level; the interim arm.
//! - `stsSecret`: the pod's `nodePublishSecretRef` again, but an EXPIRING
//!   credential a controller keeps fresh (AWS_* plus
//!   `AWS_CREDENTIAL_EXPIRATION` and a `generation`), served over the
//!   door like the broker's keys and replaced in place on republish under
//!   the generation rules (`sts_replace_decision`). Nothing sensitive in
//!   the child's env.
//! - `ambient`: nothing; the worker's own chain.
//!
//! Nothing here ever lands in a pod spec: env for the child goes over the
//! launch socket, files go into the emptyDir host-side.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the worker mounts its `comm` emptyDir.
pub const COMM_MOUNT: &str = "/comm";
/// The worker's loopback door.
pub const DOOR_ADDR: &str = "127.0.0.1:9911";
pub const CREDS_FILE: &str = "creds.json";
pub const AUTH_TOKEN_FILE: &str = "auth.token";
pub const TOKEN_FILE: &str = "token";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
    /// RFC 3339, as STS returns it.
    pub expiration: String,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Creds(akid={}…, exp={})", &self.access_key_id.chars().take(4).collect::<String>(), self.expiration)
    }
}

impl Creds {
    /// Seconds until expiry (0 if past or unparseable).
    pub fn secs_left(&self, now: chrono::DateTime<chrono::Utc>) -> i64 {
        chrono::DateTime::parse_from_rfc3339(&self.expiration)
            .map(|e| (e.with_timezone(&chrono::Utc) - now).num_seconds().max(0))
            .unwrap_or(0)
    }
}

/// `{AccessKeyId, SecretAccessKey, Token, Expiration}` — the container-
/// credentials JSON both AWS clients parse.
///
/// `Token` is ALWAYS present, empty when the arm has no session token.
/// The AWS Rust SDK's JSON credentials parser treats it as required for
/// the refreshable form and rejects the document without it; the CRT
/// (mount-s3) tolerates its absence. Measured on kind: passthrough
/// mounted happily from a Token-less document while the lean syncer,
/// which uses the Rust SDK, failed its first request as a bare
/// "dispatch failure".
pub fn creds_json(c: &Creds) -> Vec<u8> {
    let v = serde_json::json!({
        "AccessKeyId": c.access_key_id,
        "SecretAccessKey": c.secret_access_key,
        "Token": c.session_token.clone().unwrap_or_default(),
        "Expiration": c.expiration,
    });
    serde_json::to_vec(&v).expect("json")
}

pub struct CommFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub mode: u32,
}

impl std::fmt::Debug for CommFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CommFile({}, {} bytes, {:o})", self.name, self.bytes.len(), self.mode)
    }
}

/// What an arm hands the worker: child env (over the socket) and files
/// (into the comm dir, host-side).
#[derive(Default)]
pub struct Materialized {
    pub env: BTreeMap<String, String>,
    pub files: Vec<CommFile>,
}

impl std::fmt::Debug for Materialized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Env VALUES may be secrets (the static arm); print keys only.
        write!(f, "Materialized(env keys={:?}, files={:?})", self.env.keys().collect::<Vec<_>>(), self.files)
    }
}

fn base_env() -> BTreeMap<String, String> {
    BTreeMap::from([("AWS_EC2_METADATA_DISABLED".to_string(), "true".to_string())])
}

/// The keys a `nodePublishSecretRef` Secret carries, AWS_* verbatim.
pub fn static_arm(secrets: &HashMap<String, String>) -> Result<Materialized, String> {
    let mut env = base_env();
    let need = |k: &str| -> Result<String, String> {
        secrets
            .get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("nodePublishSecretRef Secret has no {k} — keys must be AWS_* verbatim"))
    };
    env.insert("AWS_ACCESS_KEY_ID".into(), need("AWS_ACCESS_KEY_ID")?);
    env.insert("AWS_SECRET_ACCESS_KEY".into(), need("AWS_SECRET_ACCESS_KEY")?);
    for k in ["AWS_SESSION_TOKEN", "AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Some(v) = secrets.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()) {
            env.insert(k.into(), v.into());
        }
    }
    Ok(Materialized { env, files: vec![] })
}

// ── stsSecret: a controller-fed, expiring credential ─────────────────
//
// The Secret a controller (awc-docs PR #136's receiver, or anything that
// mints sessions) keeps fresh in the pod's namespace, named by the pod's
// nodePublishSecretRef and re-delivered by kubelet on every republish.
// Served over the door like the broker's keys, so the mounter re-fetches
// before `Expiration` and nothing sensitive sits in its env; replaced in
// place under `sts_replace_decision`.

/// The keys an `identity.mode: stsSecret` Secret carries. PROVISIONAL:
/// the envelope PR #136 specifies is not settled, so this is flint's own
/// closed schema until it is. A key outside it is REFUSED by name, not
/// ignored — a misspelt envelope field must not pass as an absent one.
pub const STS_KEY_EXPIRATION: &str = "AWS_CREDENTIAL_EXPIRATION";
pub const STS_KEY_GENERATION: &str = "generation";
pub const STS_KEY_NAMESPACE: &str = "namespace";
pub const STS_KEY_SERVICE_ACCOUNT: &str = "serviceAccount";
pub const STS_KEY_MOUNT: &str = "mount";
const STS_REQUIRED: [&str; 4] = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", STS_KEY_EXPIRATION, STS_KEY_GENERATION];
const STS_OPTIONAL: [&str; 4] = ["AWS_SESSION_TOKEN", STS_KEY_NAMESPACE, STS_KEY_SERVICE_ACCOUNT, STS_KEY_MOUNT];
/// A candidate with less than this left is refused: by the time the
/// mounter re-fetched it, it would be as good as expired, and installing
/// it would displace a credential that still works. Kubelet's republish
/// is every ~60-90 s, so a controller should offer the next generation
/// well before this — the broker arm refreshes at 420 s left for the
/// same reason (`node::BROKER_REFRESH_SECS`).
pub const STS_MIN_SECS_LEFT: i64 = 120;

/// What an `stsSecret` Secret said: the credential, its generation, and
/// the optional envelope naming who it was minted for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StsSecret {
    pub creds: Creds,
    pub generation: u64,
    pub namespace: Option<String>,
    pub service_account: Option<String>,
    pub mount: Option<String>,
}

/// Parse the Secret. `ignore` names keys that are not the Secret's own —
/// the kubelet token key when `serviceAccountTokenInSecrets` is on.
pub fn parse_sts_secret(secrets: &HashMap<String, String>, ignore: &[&str]) -> Result<StsSecret, String> {
    let mut unknown: Vec<&str> = secrets
        .keys()
        .map(String::as_str)
        .filter(|k| !STS_REQUIRED.contains(k) && !STS_OPTIONAL.contains(k) && !ignore.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(format!(
            "nodePublishSecretRef Secret carries keys identity.mode stsSecret does not accept: {} — it takes {} and the optional {}",
            unknown.join(", "),
            STS_REQUIRED.join(", "),
            STS_OPTIONAL.join(", ")
        ));
    }
    let get = |k: &str| secrets.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()).map(str::to_string);
    let need = |k: &str| {
        get(k).ok_or_else(|| format!("nodePublishSecretRef Secret has no {k} — identity.mode stsSecret needs {}", STS_REQUIRED.join(", ")))
    };
    let expiration = need(STS_KEY_EXPIRATION)?;
    chrono::DateTime::parse_from_rfc3339(&expiration).map_err(|e| format!("{STS_KEY_EXPIRATION} {expiration:?} is not RFC 3339: {e}"))?;
    let generation = need(STS_KEY_GENERATION)?;
    let generation = generation.parse::<u64>().map_err(|e| format!("{STS_KEY_GENERATION} {generation:?} is not a whole number: {e}"))?;
    Ok(StsSecret {
        creds: Creds {
            access_key_id: need("AWS_ACCESS_KEY_ID")?,
            secret_access_key: need("AWS_SECRET_ACCESS_KEY")?,
            session_token: get("AWS_SESSION_TOKEN"),
            expiration,
        },
        generation,
        namespace: get(STS_KEY_NAMESPACE),
        service_account: get(STS_KEY_SERVICE_ACCOUNT),
        mount: get(STS_KEY_MOUNT),
    })
}

/// The envelope: each field that is PRESENT against what kubelet asserted
/// (the pod's namespace and ServiceAccount) and what the pod named (the
/// CR). Absent fields check nothing — the schema is the controller's to
/// fill in, and the CR's consumer list already gates the mount.
pub fn sts_envelope_check(s: &StsSecret, namespace: &str, service_account: &str, mount: &str) -> Result<(), String> {
    for (key, got, want) in [
        (STS_KEY_NAMESPACE, s.namespace.as_deref(), namespace),
        (STS_KEY_SERVICE_ACCOUNT, s.service_account.as_deref(), service_account),
        (STS_KEY_MOUNT, s.mount.as_deref(), mount),
    ] {
        if let Some(got) = got {
            if got != want {
                return Err(format!(
                    "the Secret's envelope says {key} {got:?} but this mount is for {want:?} — the credential was not minted for this pod"
                ));
            }
        }
    }
    Ok(())
}

/// What a republish does with the Secret it was handed.
#[derive(Debug, PartialEq, Eq)]
pub enum StsReplace {
    /// Write it: a first credential, or a higher generation with life left.
    Install,
    /// The installed generation, re-offered unchanged: nothing to do, nothing to say.
    Idempotent,
    /// Leave the installed credential alone, and say why.
    Refuse(String),
}

/// The generation rules (the AWC fuse-node spec's N3, made flint's): never
/// roll back, never install the same generation twice with different
/// contents, never install what is about to expire — and never discard a
/// still-valid installed credential: every refusal leaves the file as it
/// was. `installed` is the generation and expiration in the worker's
/// `creds.json` now; the state keeps no key material, so "unchanged" is
/// judged on those two.
pub fn sts_replace_decision(installed: Option<(u64, &str)>, candidate: &StsSecret, now: chrono::DateTime<chrono::Utc>) -> StsReplace {
    let g = candidate.generation;
    match installed {
        Some((have, exp)) if g == have && candidate.creds.expiration == exp => return StsReplace::Idempotent,
        Some((have, _)) if g < have => {
            return StsReplace::Refuse(format!("generation {g} is lower than the installed {have}; a credential never rolls back"))
        }
        Some((have, exp)) if g == have => {
            return StsReplace::Refuse(format!(
                "generation {have} is already installed (expires {exp}) and the Secret offers the same generation with expiration {} — a replacement must carry a HIGHER generation",
                candidate.creds.expiration
            ))
        }
        _ => {}
    }
    if candidate.creds.secs_left(now) < STS_MIN_SECS_LEFT {
        return StsReplace::Refuse(format!(
            "generation {g} expires at {}, under {STS_MIN_SECS_LEFT} s away; the installed credential is kept",
            candidate.creds.expiration
        ));
    }
    StsReplace::Install
}

/// The loopback door: the worker checks `auth.token`, serves `creds.json`.
pub fn door_arm(auth_token: &str) -> Materialized {
    let mut env = base_env();
    env.insert("AWS_CONTAINER_CREDENTIALS_FULL_URI".into(), format!("http://{DOOR_ADDR}/v1/creds"));
    env.insert("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE".into(), format!("{COMM_MOUNT}/{AUTH_TOKEN_FILE}"));
    Materialized {
        env,
        files: vec![CommFile { name: AUTH_TOKEN_FILE.into(), bytes: auth_token.as_bytes().to_vec(), mode: 0o600 }],
    }
}

/// The worker calls the STS façade itself.
/// The region a worker runs against when its credential arm named none:
/// the CR's own (`spec.region`, carried as `FLINT_SYNC_REGION` in the lean
/// launch list) ahead of the node-wide default (`FLINT_S3CSI_REGION`). A
/// bucket in the wrong region answers every request 301 and the worker
/// crash-loops (runcu 2026-09-12), so the CR must be able to say.
pub fn effective_region(launch: Option<&std::collections::BTreeMap<String, String>>, node_default: &str) -> String {
    launch
        .and_then(|l| l.get("FLINT_SYNC_REGION"))
        .filter(|r| !r.is_empty())
        .cloned()
        .unwrap_or_else(|| node_default.to_string())
}

pub fn web_identity_arm(role_arn: &str, sts_url: &str, session_name: &str, token: &str, region: &str) -> Materialized {
    let mut env = base_env();
    env.insert("AWS_ROLE_ARN".into(), role_arn.into());
    env.insert("AWS_WEB_IDENTITY_TOKEN_FILE".into(), format!("{COMM_MOUNT}/{TOKEN_FILE}"));
    env.insert("AWS_ROLE_SESSION_NAME".into(), session_name.into());
    env.insert("AWS_ENDPOINT_URL_STS".into(), sts_url.into());
    env.insert("AWS_REGION".into(), region.into());
    Materialized {
        env,
        files: vec![CommFile { name: TOKEN_FILE.into(), bytes: token.as_bytes().to_vec(), mode: 0o600 }],
    }
}

pub fn ambient_arm() -> Materialized {
    Materialized::default()
}

/// 32 hex chars from the OS RNG.
pub fn new_nonce() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `arn:flint:iam::<mode>:role/<cr>` — what the worker presents as
/// `RoleArn`; the namespace comes from the token, never from here.
pub fn role_arn(mode: &str, cr: &str) -> String {
    format!("arn:flint:iam::{mode}:role/{cr}")
}

pub fn parse_role_arn(arn: &str) -> Option<(String, String)> {
    let rest = arn.strip_prefix("arn:flint:iam::")?;
    let (mode, role) = rest.split_once(':')?;
    let cr = role.strip_prefix("role/")?;
    if mode.is_empty() || cr.is_empty() {
        return None;
    }
    Some((mode.to_string(), cr.to_string()))
}

/// Write the arm's files into the worker's comm dir (host path),
/// atomically, OWNED BY THE WORKER'S uid/gid: this process is root and
/// the worker is not, and a 0600 file root wrote is exactly the file
/// the worker's door must read.
pub fn write_files(comm_dir: &Path, files: &[CommFile], owner: (u32, u32)) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(comm_dir)?;
    for f in files {
        let tmp = comm_dir.join(format!("{}.tmp", f.name));
        std::fs::write(&tmp, &f.bytes)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(f.mode))?;
        // chown only where we can (root on the node); a rig running the
        // unit tests as a user keeps its own ownership.
        if let Err(e) = std::os::unix::fs::chown(&tmp, Some(owner.0), Some(owner.1)) {
            if nix::unistd::geteuid().is_root() {
                return Err(e);
            }
        }
        std::fs::rename(&tmp, comm_dir.join(&f.name))?;
    }
    Ok(())
}

/// Remove a comm file (a refused refresh takes the credential away so
/// the door answers 503 and the client fails at expiry, design §4.6).
pub fn remove_file(comm_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(comm_dir.join(name));
}

/// Why an exchange yielded no keys. `Refused` is the broker's verdict
/// on THIS identity — an STS 4xx: not a token, not a consumer, no live
/// registration — and is the tenant's to fix. `Outage` is everything
/// else — transport, a 5xx from the broker or its backend, an
/// unparseable body — and is kubelet's to retry; a cached key is kept
/// through it. The two were one string before, and a broker that was
/// merely unreachable reported to the tenant as `PermissionDenied`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeError {
    Refused(String),
    Outage(String),
}

impl ExchangeError {
    pub fn is_refusal(&self) -> bool {
        matches!(self, ExchangeError::Refused(_))
    }
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExchangeError::Refused(m) => write!(f, "broker refused the exchange: {m}"),
            ExchangeError::Outage(m) => write!(f, "broker exchange outage: {m}"),
        }
    }
}

/// An STS-shaped answer, sorted: 4xx is a refusal, anything else an
/// outage. The broker answers `InvalidIdentityToken` and
/// `InvalidParameterValue` with 400 and `AccessDenied` with 403 (all
/// final for this identity), `ServiceUnavailable` with 503 and
/// `IDPCommunicationError` with 502 (both transient).
pub fn classify_exchange(status: u16, detail: String) -> ExchangeError {
    if (400..500).contains(&status) {
        ExchangeError::Refused(format!("{status}: {detail}"))
    } else {
        ExchangeError::Outage(format!("{status}: {detail}"))
    }
}

/// Slack on top of the drain ceiling for the key exchanged at unpublish.
pub const DRAIN_KEY_MARGIN_SECS: u64 = 300;

/// The lifetime asked for the final drain's key: the derived grace plus
/// the plugin's 30 s kill ceiling plus a margin, never shorter than the
/// ordinary lifetime. The broker clamps to its own maximum; a drain
/// longer than that still runs on what it gets, and the event says so.
pub fn drain_key_lifetime_secs(grace_secs: u64, ordinary_secs: u64) -> u64 {
    ordinary_secs.max(grace_secs + 30 + DRAIN_KEY_MARGIN_SECS)
}

// ── the broker client ────────────────────────────────────────────────

/// One publish, registered at the broker (design §4.2): the binding a
/// pod cannot self-mint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub volume_id: String,
    pub pod_uid: String,
    pub namespace: String,
    pub pod: String,
    pub service_account: String,
    pub cr: String,
    pub mode: String,
    pub nonce: String,
    pub node: String,
    /// What the plugin decided for this publish (per-user access design
    /// §4.2). The broker narrows it again by the CR at every exchange, so
    /// a registration can only ever ask for less. Absent from a plugin
    /// that predates the field: `readWrite`, which the CR still narrows.
    #[serde(default)]
    pub access: super::policy::Access,
    /// `chert.us/on-behalf-of`: audit only, never an input to a decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
}

pub struct BrokerClient {
    base: String,
    http: reqwest::Client,
    node_token_file: PathBuf,
}

impl BrokerClient {
    /// `FLINT_S3CSI_BROKER_URL` (unset ⇒ no broker: only `static` and
    /// `ambient` can publish), `FLINT_S3CSI_BROKER_CA` (PEM path, for an
    /// https broker with a private CA), `FLINT_S3CSI_NODE_TOKEN_FILE`
    /// (the plugin's own projected token, audience `s3.csi.chert.us`).
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(base) = std::env::var("FLINT_S3CSI_BROKER_URL").ok().filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20));
        if let Some(ca) = std::env::var("FLINT_S3CSI_BROKER_CA").ok().filter(|s| !s.is_empty()) {
            let pem = std::fs::read(&ca).map_err(|e| format!("read {ca}: {e}"))?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| format!("parse {ca}: {e}"))?;
            b = b.add_root_certificate(cert);
        }
        let http = b.build().map_err(|e| e.to_string())?;
        let node_token_file = std::env::var("FLINT_S3CSI_NODE_TOKEN_FILE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/run/secrets/flint-s3/token"));
        Ok(Some(Self { base: base.trim_end_matches('/').to_string(), http, node_token_file }))
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// `GET /v1/status` — the broker's backend and its read enforcement.
    /// The node plugin reads it to decide whether a read grant's authority
    /// is a function of the CR alone (sts, static: shareable between pods)
    /// or of the pod it was minted for (rest: not).
    pub async fn status(&self) -> Result<serde_json::Value, String> {
        let resp = self
            .http
            .get(format!("{}/v1/status", self.base))
            .send()
            .await
            .map_err(|e| format!("broker status: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("broker status: {}", resp.status()));
        }
        resp.json().await.map_err(|e| format!("broker status: {e}"))
    }

    fn node_token(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.node_token_file)
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("node token {}: {e}", self.node_token_file.display()))
    }

    pub async fn register(&self, r: &Registration) -> Result<(), String> {
        let resp = self
            .http
            .post(format!("{}/v1/volumes", self.base))
            .bearer_auth(self.node_token()?)
            .json(r)
            .send()
            .await
            .map_err(|e| format!("broker register: {e}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("broker register: {st}: {}", body.chars().take(300).collect::<String>()));
        }
        Ok(())
    }

    pub async fn deregister(&self, volume_id: &str) -> Result<(), String> {
        let resp = self
            .http
            .delete(format!("{}/v1/volumes/{volume_id}", self.base))
            .bearer_auth(self.node_token()?)
            .send()
            .await
            .map_err(|e| format!("broker deregister: {e}"))?;
        if !resp.status().is_success() && resp.status().as_u16() != 404 {
            return Err(format!("broker deregister: {}", resp.status()));
        }
        Ok(())
    }

    /// `AssumeRoleWithWebIdentity` at the façade, exactly as the AWS
    /// clients would call it.
    pub async fn exchange(
        &self,
        web_identity_token: &str,
        role_arn: &str,
        session_name: &str,
        duration_secs: u64,
    ) -> Result<Creds, ExchangeError> {
        let form = [
            ("Action", "AssumeRoleWithWebIdentity"),
            ("Version", "2011-06-15"),
            ("RoleArn", role_arn),
            ("RoleSessionName", session_name),
            ("WebIdentityToken", web_identity_token),
            ("DurationSeconds", &duration_secs.to_string()),
        ];
        let resp = self
            .http
            .post(format!("{}/", self.base))
            .form(&form)
            .send()
            .await
            .map_err(|e| ExchangeError::Outage(format!("transport: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(classify_exchange(
                status.as_u16(),
                sts_error_message(&body).unwrap_or_else(|| body.chars().take(300).collect()),
            ));
        }
        parse_sts_xml(&body).map_err(ExchangeError::Outage)
    }
}

fn tag(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let s = body.find(&open)? + open.len();
    let e = body[s..].find(&close)? + s;
    Some(xml_unescape(body[s..e].trim()))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn sts_error_message(body: &str) -> Option<String> {
    let code = tag(body, "Code")?;
    let msg = tag(body, "Message").unwrap_or_default();
    Some(format!("{code}: {msg}"))
}

/// The four tags every STS client reads.
pub fn parse_sts_xml(body: &str) -> Result<Creds, String> {
    let creds = |n: &str| tag(body, n).ok_or_else(|| format!("STS response has no <{n}>"));
    Ok(Creds {
        access_key_id: creds("AccessKeyId")?,
        secret_access_key: creds("SecretAccessKey")?,
        session_token: tag(body, "SessionToken").filter(|s| !s.is_empty()),
        expiration: creds("Expiration")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The door document must ALWAYS carry Token — the Rust SDK's parser
    /// requires it and answers a missing field with an error the caller
    /// only ever sees as "dispatch failure".
    #[test]
    fn creds_json_always_carries_a_token_field() {
        let no_session = Creds {
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            session_token: None,
            expiration: "2030-01-01T00:00:00Z".into(),
        };
        let v: serde_json::Value = serde_json::from_slice(&creds_json(&no_session)).unwrap();
        assert_eq!(v["Token"], "", "absent session token must serialize as an EMPTY Token, not a missing key");
        assert_eq!(v["AccessKeyId"], "AK");
        let with_session = Creds { session_token: Some("ST".into()), ..no_session };
        let v: serde_json::Value = serde_json::from_slice(&creds_json(&with_session)).unwrap();
        assert_eq!(v["Token"], "ST");
    }



    fn sts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn full_sts() -> HashMap<String, String> {
        sts(&[
            ("AWS_ACCESS_KEY_ID", "AK"),
            ("AWS_SECRET_ACCESS_KEY", "SK"),
            ("AWS_SESSION_TOKEN", "ST"),
            (STS_KEY_EXPIRATION, "2030-01-01T00:00:00Z"),
            (STS_KEY_GENERATION, "7"),
        ])
    }
    const TOKEN_KEY: &str = "csi.storage.k8s.io/serviceAccount.tokens";

    #[test]
    fn sts_secret_parses_the_tuple_and_its_envelope() {
        let mut m = full_sts();
        m.insert(STS_KEY_NAMESPACE.into(), "team-a".into());
        m.insert(STS_KEY_SERVICE_ACCOUNT.into(), "trainer".into());
        m.insert(STS_KEY_MOUNT.into(), "datasets".into());
        m.insert(TOKEN_KEY.into(), "{}".into());
        let s = parse_sts_secret(&m, &[TOKEN_KEY]).unwrap();
        assert_eq!(s.generation, 7);
        assert_eq!(s.creds.session_token.as_deref(), Some("ST"));
        assert_eq!(s.creds.expiration, "2030-01-01T00:00:00Z");
        assert_eq!(
            (s.namespace.as_deref(), s.service_account.as_deref(), s.mount.as_deref()),
            (Some("team-a"), Some("trainer"), Some("datasets"))
        );
        // The session token is the session's business: an expiring static key still parses.
        let mut no_token = full_sts();
        no_token.remove("AWS_SESSION_TOKEN");
        assert!(parse_sts_secret(&no_token, &[]).unwrap().creds.session_token.is_none());
    }

    #[test]
    fn sts_secret_refuses_each_missing_or_malformed_field_by_name() {
        for k in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", STS_KEY_EXPIRATION, STS_KEY_GENERATION] {
            let mut m = full_sts();
            m.remove(k);
            let e = parse_sts_secret(&m, &[]).unwrap_err();
            assert!(e.contains(k), "missing {k}: {e}");
            let mut m = full_sts();
            m.insert(k.into(), "  ".into());
            assert!(parse_sts_secret(&m, &[]).unwrap_err().contains(k), "blank {k}");
        }
        let mut m = full_sts();
        m.insert(STS_KEY_EXPIRATION.into(), "tomorrow".into());
        assert!(parse_sts_secret(&m, &[]).unwrap_err().contains("RFC 3339"));
        let mut m = full_sts();
        m.insert(STS_KEY_GENERATION.into(), "-1".into());
        assert!(parse_sts_secret(&m, &[]).unwrap_err().contains(STS_KEY_GENERATION));
    }

    /// A misspelt envelope key must not pass as an absent one.
    #[test]
    fn sts_secret_refuses_unknown_keys_by_name_but_ignores_the_token_key() {
        let mut m = full_sts();
        m.insert("serviceaccount".into(), "x".into());
        m.insert("AWS_REGION".into(), "us-east-1".into());
        let e = parse_sts_secret(&m, &[TOKEN_KEY]).unwrap_err();
        assert!(e.contains("AWS_REGION") && e.contains("serviceaccount"), "{e}");
        let mut m = full_sts();
        m.insert(TOKEN_KEY.into(), "{}".into());
        assert!(parse_sts_secret(&m, &[TOKEN_KEY]).is_ok());
        assert!(parse_sts_secret(&m, &[]).is_err(), "the ignore list is the caller's, not built in");
    }

    #[test]
    fn sts_envelope_mismatch_is_refused_per_field_and_absence_passes() {
        let s = parse_sts_secret(&full_sts(), &[]).unwrap();
        assert!(sts_envelope_check(&s, "team-a", "trainer", "datasets").is_ok(), "no envelope: nothing to disagree");
        for (k, right) in [(STS_KEY_NAMESPACE, "team-a"), (STS_KEY_SERVICE_ACCOUNT, "trainer"), (STS_KEY_MOUNT, "datasets")] {
            let mut m = full_sts();
            m.insert(k.into(), "other".into());
            let e = sts_envelope_check(&parse_sts_secret(&m, &[]).unwrap(), "team-a", "trainer", "datasets").unwrap_err();
            assert!(e.contains(k) && e.contains("other"), "{k}: {e}");
            m.insert(k.into(), right.into());
            assert!(sts_envelope_check(&parse_sts_secret(&m, &[]).unwrap(), "team-a", "trainer", "datasets").is_ok(), "{k}");
        }
    }

    #[test]
    fn sts_replace_decision_table() {
        let now = chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let cand = |g: u64, exp: &str| StsSecret {
            creds: Creds { access_key_id: "AK".into(), secret_access_key: "SK".into(), session_token: None, expiration: exp.into() },
            generation: g,
            namespace: None,
            service_account: None,
            mount: None,
        };
        let refused = |v: StsReplace, word: &str| match v {
            StsReplace::Refuse(m) => assert!(m.contains(word), "{m}"),
            v => panic!("wanted a refusal naming {word}, got {v:?}"),
        };
        let far = "2030-01-01T01:00:00Z"; // 3600 s left
        assert_eq!(sts_replace_decision(None, &cand(1, far), now), StsReplace::Install, "first install");
        assert_eq!(sts_replace_decision(Some((1, far)), &cand(2, far), now), StsReplace::Install, "higher generation");
        assert_eq!(sts_replace_decision(Some((2, far)), &cand(2, far), now), StsReplace::Idempotent, "same generation, same expiration");
        refused(sts_replace_decision(Some((2, far)), &cand(1, far), now), "lower");
        refused(sts_replace_decision(Some((2, far)), &cand(2, "2030-01-01T02:00:00Z"), now), "HIGHER");
        // The floor: under 120 s left is refused whether or not anything is installed...
        let soon = "2030-01-01T00:01:59Z"; // 119 s
        refused(sts_replace_decision(None, &cand(1, soon), now), "120");
        refused(sts_replace_decision(Some((1, far)), &cand(2, soon), now), "120");
        assert_eq!(sts_replace_decision(Some((1, far)), &cand(2, "2030-01-01T00:02:00Z"), now), StsReplace::Install, "exactly 120 s installs");
        // ...but the installed generation re-offered unchanged is idempotent even near its end: nothing to say.
        assert_eq!(sts_replace_decision(Some((2, soon)), &cand(2, soon), now), StsReplace::Idempotent);
        // A refused generation is retryable: gen 3 refused under the floor, then gen 3 with life installs.
        refused(sts_replace_decision(Some((2, far)), &cand(3, soon), now), "120");
        assert_eq!(sts_replace_decision(Some((2, far)), &cand(3, "2030-01-01T03:00:00Z"), now), StsReplace::Install);
    }

    /// The CR's region beats the node's default and an empty stamp does not.



    #[test]



    fn effective_region_prefers_the_cr_then_the_node() {



        let mut l = std::collections::BTreeMap::new();



        assert_eq!(effective_region(None, "us-east-1"), "us-east-1");



        assert_eq!(effective_region(Some(&l), "us-east-1"), "us-east-1");



        l.insert("FLINT_SYNC_REGION".to_string(), String::new());



        assert_eq!(effective_region(Some(&l), "us-east-1"), "us-east-1");



        l.insert("FLINT_SYNC_REGION".to_string(), "us-west-1".to_string());



        assert_eq!(effective_region(Some(&l), "us-east-1"), "us-west-1");



    }




    #[test]
    fn static_arm_needs_both_keys_and_passes_region() {
        let e = static_arm(&HashMap::from([("AWS_ACCESS_KEY_ID".into(), "a".into())])).unwrap_err();
        assert!(e.contains("AWS_SECRET_ACCESS_KEY"), "{e}");
        let m = static_arm(&HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "a".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "s\n".into()),
            ("AWS_REGION".into(), "r".into()),
        ]))
        .unwrap();
        assert_eq!(m.env["AWS_SECRET_ACCESS_KEY"], "s");
        assert_eq!(m.env["AWS_REGION"], "r");
        assert_eq!(m.env["AWS_EC2_METADATA_DISABLED"], "true");
        assert!(m.files.is_empty());
    }

    #[test]
    fn door_arm_points_at_loopback_with_a_token_file() {
        let m = door_arm("n0nce");
        assert_eq!(m.env["AWS_CONTAINER_CREDENTIALS_FULL_URI"], "http://127.0.0.1:9911/v1/creds");
        assert_eq!(m.env["AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"], "/comm/auth.token");
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].name, "auth.token");
        assert_eq!(m.files[0].bytes, b"n0nce");
        assert_eq!(m.files[0].mode, 0o600);
    }

    #[test]
    fn creds_json_is_the_container_credentials_shape() {
        let c = Creds { access_key_id: "A".into(), secret_access_key: "S".into(), session_token: Some("T".into()), expiration: "2026-09-02T01:00:00Z".into() };
        let v: serde_json::Value = serde_json::from_slice(&creds_json(&c)).unwrap();
        assert_eq!(v["AccessKeyId"], "A");
        assert_eq!(v["Token"], "T");
        assert_eq!(v["Expiration"], "2026-09-02T01:00:00Z");
        assert!(!format!("{c:?}").contains('S'), "Debug must not print the secret");
    }

    #[test]
    fn role_arn_round_trips() {
        let a = role_arn("passthrough", "datasets");
        assert_eq!(parse_role_arn(&a), Some(("passthrough".into(), "datasets".into())));
        assert_eq!(parse_role_arn("arn:aws:iam::123:role/x"), None);
        assert_eq!(parse_role_arn("arn:flint:iam:::role/"), None);
    }

    #[test]
    fn sts_xml_parses_and_errors_are_named() {
        let body = "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials>\
            <AccessKeyId>AK</AccessKeyId><SecretAccessKey>a&amp;b</SecretAccessKey>\
            <SessionToken>tok</SessionToken><Expiration>2026-09-02T00:15:00Z</Expiration>\
            </Credentials></AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>";
        let c = parse_sts_xml(body).unwrap();
        assert_eq!(c.access_key_id, "AK");
        assert_eq!(c.secret_access_key, "a&b");
        assert_eq!(c.session_token.as_deref(), Some("tok"));
        assert!(parse_sts_xml("<x/>").unwrap_err().contains("AccessKeyId"));
        let err = "<ErrorResponse><Error><Code>AccessDenied</Code><Message>bob is not a consumer</Message></Error></ErrorResponse>";
        assert_eq!(sts_error_message(err).unwrap(), "AccessDenied: bob is not a consumer");
    }

    #[test]
    fn secs_left_counts_down_and_floors_at_zero() {
        let c = Creds { access_key_id: "".into(), secret_access_key: "".into(), session_token: None, expiration: "2026-09-02T00:15:00Z".into() };
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-02T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        assert_eq!(c.secs_left(now), 900);
        let later = chrono::DateTime::parse_from_rfc3339("2026-09-02T01:00:00Z").unwrap().with_timezone(&chrono::Utc);
        assert_eq!(c.secs_left(later), 0);
    }

    /// The tenant's event must say "refused" only when the broker
    /// refused THIS identity. A broker that is down, or whose backend
    /// is, is an outage: kubelet retries it and a cached key survives it.
    #[test]
    fn exchange_errors_sort_refusals_from_outages() {
        assert!(classify_exchange(403, "AccessDenied: bob is not a consumer".into()).is_refusal());
        assert!(classify_exchange(400, "InvalidIdentityToken: pod is gone".into()).is_refusal());
        assert!(!classify_exchange(502, "IDPCommunicationError".into()).is_refusal());
        assert!(!classify_exchange(503, "ServiceUnavailable".into()).is_refusal());
        assert!(!classify_exchange(500, "".into()).is_refusal());
        let e = ExchangeError::Outage("transport: connection refused".into());
        assert!(e.to_string().contains("outage"), "{e}");
        assert!(!e.to_string().contains("refused the exchange"), "{e}");
        let r = classify_exchange(403, "AccessDenied".into());
        assert!(r.to_string().contains("refused the exchange"), "{r}");
    }

    /// The drain key covers the ceiling the plugin enforces, and is
    /// never shorter than the ordinary key.
    #[test]
    fn drain_key_covers_the_ceiling_and_never_shrinks() {
        assert_eq!(drain_key_lifetime_secs(120, 900), 900, "a gated drain fits inside the ordinary key");
        assert_eq!(drain_key_lifetime_secs(3621, 900), 3621 + 30 + DRAIN_KEY_MARGIN_SECS, "a 1-hour floor asks for more");
        assert!(drain_key_lifetime_secs(0, 900) >= 900);
    }

    #[test]
    fn nonce_is_32_hex_and_unique() {
        let a = new_nonce();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, new_nonce());
    }

    #[test]
    fn files_are_written_with_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let me = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
        write_files(d.path(), &door_arm("x").files, me).unwrap();
        let p = d.path().join("auth.token");
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
