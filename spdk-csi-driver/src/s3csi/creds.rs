//! How a worker gets its S3 credential (design §4.4, §4.5), and the
//! plugin's client to `flint-s3-broker`.
//!
//! Four arms behind one seam:
//!
//! - `broker` (default): the PLUGIN exchanges the pod-bound ServiceAccount
//!   token at the broker for short-lived keys and writes them host-side
//!   into the worker's memory-backed `comm` dir as `creds.json`; the
//!   worker's PID 1 serves them on the loopback container-credentials
//!   door, which mount-s3 (CRT) and the Rust SDK consume unchanged and
//!   re-fetch before `Expiration`. Republish re-exchanges when the keys
//!   are within three periods of expiry.
//! - `webIdentity`: the WORKER calls the broker's STS façade itself with
//!   the token file the plugin keeps fresh. Needs the broker's TLS
//!   trusted by the mounter image (the CRT's web-identity provider is
//!   HTTPS-only), which is why it is not the default.
//! - `static`: the pod's `nodePublishSecretRef` — kubelet fetched it with
//!   kubelet's credentials and delivered it in `secrets`; the node SA
//!   needs no Secrets RBAC. Today's trust level; the interim arm.
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
    ) -> Result<Creds, String> {
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
            .map_err(|e| format!("broker exchange: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "broker refused the exchange ({status}): {}",
                sts_error_message(&body).unwrap_or_else(|| body.chars().take(300).collect())
            ));
        }
        parse_sts_xml(&body)
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
