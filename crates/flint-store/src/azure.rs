//! The Azure Blob Storage backend (docs/plans/lean-azure-gcs-backends-plan-2026-11-10.md,
//! phase 1): the Blob REST API by hand, on the same hyper/rustls stack as
//! the raw read path, authorized with Shared Key, a SAS, or an Entra ID
//! token (phase 4): a workload's federated token traded by client
//! assertion, or the VM's managed identity.
//!
//! **Content ETags (plan §3, C3).** Azure's ETag names a write, not the
//! bytes, and lean compares the ETags of different objects at one path.
//! So the ETag this store hands out is a content hash: the first 128 bits
//! of the bytes' SHA-256, kept in the blob's metadata (`flint_sha256`) and
//! written with it. Azure's own ETag stays inside: it carries every
//! condition. An `If-Match` on a content ETag resolves to the Azure ETag
//! of the blob as last seen (or as one HEAD finds it), checks the stored
//! hash, and sends Azure's precondition. A 412 is re-checked by the bytes
//! before it is reported, so the answer is S3's in every case, a body that
//! changes and changes back included. A blob an outside tool wrote has no
//! hash; its ETag is `"x-<key digest>-<Azure ETag>"`, unique per write.
//!
//! **CRC-64/NVME (C7).** Azure keeps no whole-blob CRC, so the CRC this
//! client computed rides in metadata (`flint_crc64`) and comes back on
//! HEAD, GET and LIST. A caller's CRC is checked against the bytes before
//! anything is sent. Azure's per-request `x-ms-content-crc64` is the same
//! CRC, but its byte order is not documented and a wrong guess refuses
//! every request, so it waits for a real-account test.
//!
//! **The epoch cell** is a CAS on the cell's body (`cell.rs`), never a
//! native blob lease: a lease keeps one id across its renewals, and a
//! holder is judged dead when the token stops moving.
//!
//! Not here yet: server-side copies of byte ranges (`PartSource::BaseCopy`,
//! the block tier's), batch delete, presigned URLs, version and lifecycle
//! operations.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use http::{HeaderMap, Method, Request};
use http_body_util::{BodyExt, Full};
use sha2::{Digest, Sha256};

use crate::counters::{RequestCounter, RequestKind};
use crate::rest::{
    base64_decode, content_hash, encode, expires_in, foreign_token, form, https_client, jitter, presented, quoted,
    read_local, refusal, token_request, HttpsClient, TokenCache,
};
use crate::{
    base64_std, cell, crc64_nvme, crc64_to_b64, gate::ByteGate, BootstrapReport, ComposeSpec, Crc64Nvme, EpochLease,
    EpochState, GenerationStamps, ListedObject, ObjectMeta, ObjectStore, PartSource, PendingUpload, Provider,
    PutCondition, RequestCounts, StoreError, StoreResult,
};

/// The REST version every request names. One Azurite takes, and late
/// enough for every operation here.
pub const API_VERSION: &str = "2023-11-03";

/// Azure has no part floor; this is the grid lean cuts a large file on.
const MIN_PART: u64 = 4 * 1024 * 1024;
/// Most committed blocks in one blob.
const MAX_BLOCKS: usize = 50_000;
/// The content hash and the CRC, in metadata. Underscores: an Azure
/// metadata name is a C# identifier.
const META_HASH: &str = "flint_sha256";
const META_CRC: &str = "flint_crc64";
/// Blobs whose Azure ETag the store remembers, before it forgets them all.
const TOKENS_MAX: usize = 4096;

/// Entra ID's public cloud; `AZURE_AUTHORITY_HOST` names another.
pub const ENTRA_AUTHORITY: &str = "https://login.microsoftonline.com";
/// The instance metadata service, where a VM's managed identity answers.
pub const IMDS: &str = "http://169.254.169.254";
/// The resource an Entra token for Blob Storage is for.
const STORAGE_RESOURCE: &str = "https://storage.azure.com";

enum Auth {
    SharedKey(Vec<u8>),
    /// A SAS query string, as issued (already percent-encoded).
    Sas(String),
    /// A bearer token for Azure Storage, from `source`, kept in `cache`.
    Entra { source: EntraSource, cache: TokenCache },
}

/// Where an Entra ID token for Blob Storage comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntraSource {
    /// Workload identity federation: a token Kubernetes issued the
    /// workload (`token_file`, re-read at every refresh, since the kubelet
    /// rotates it), traded at `authority` for an Entra token by client
    /// assertion, as the app registration or managed identity `client_id`
    /// in `tenant` whose federated credential trusts it. The variables the
    /// AKS workload identity webhook sets name all four.
    Federated { tenant: String, client_id: String, token_file: std::path::PathBuf, authority: String },
    /// The VM's managed identity, from the instance metadata service at
    /// `endpoint`: the system-assigned one, or the user-assigned
    /// `client_id`.
    ManagedIdentity { client_id: Option<String>, endpoint: String },
}

/// Timeouts and retries; the raw read path's numbers.
#[derive(Debug, Clone)]
pub struct AzureOptions {
    pub connect_timeout: Duration,
    /// Wait for a response's first byte, plus [`Self::per_mib_sent`] for
    /// every MiB of request body still being sent.
    pub first_byte_timeout: Duration,
    pub per_mib_sent: Duration,
    pub body_stall_timeout: Duration,
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
}

impl Default for AzureOptions {
    fn default() -> Self {
        AzureOptions {
            connect_timeout: Duration::from_secs(3),
            first_byte_timeout: Duration::from_secs(10),
            per_mib_sent: Duration::from_secs(1),
            body_stall_timeout: Duration::from_secs(30),
            max_attempts: 3,
            backoff_base: Duration::from_millis(100),
            backoff_cap: Duration::from_secs(2),
        }
    }
}

pub struct AzureStore {
    client: HttpsClient,
    account: String,
    auth: Auth,
    /// `scheme://host[:port]` of the blob endpoint.
    origin: String,
    /// The endpoint's own path: empty for the service, `/devstoreaccount1`
    /// for Azurite.
    base_path: String,
    container: String,
    opts: AzureOptions,
    requests: Arc<RequestCounter>,
    upload_gate: Option<Arc<ByteGate>>,
    /// key -> (content ETag, Azure ETag), as last read or written.
    tokens: Mutex<HashMap<String, (String, String)>>,
}

/// One request, before it is signed.
struct Req {
    ctx: &'static str,
    kind: RequestKind,
    method: Method,
    /// `None`: the container itself.
    key: Option<String>,
    /// Unencoded names and values.
    query: Vec<(&'static str, String)>,
    /// Lowercase names.
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl Req {
    fn new(ctx: &'static str, kind: RequestKind, method: Method, key: Option<&str>) -> Req {
        Req { ctx, kind, method, key: key.map(str::to_string), query: vec![], headers: vec![], body: Bytes::new() }
    }
    fn header(mut self, name: &str, value: impl Into<String>) -> Req {
        self.headers.push((name.to_string(), value.into()));
        self
    }
    fn query(mut self, name: &'static str, value: impl Into<String>) -> Req {
        self.query.push((name, value.into()));
        self
    }
    fn body(mut self, body: Bytes) -> Req {
        self.body = body;
        self
    }
}

struct Resp {
    headers: HeaderMap,
    body: Bytes,
}

enum Failure {
    Retry(String),
    Final(StoreError),
}

/// A blob as HEAD, GET or a write saw it.
struct Seen {
    meta: ObjectMeta,
    /// Azure's ETag, quoted.
    token: String,
}

impl AzureStore {
    /// Authorize with the account's Shared Key (base64, as the portal
    /// shows it). `endpoint` is the blob endpoint, path included for
    /// Azurite (`http://127.0.0.1:10000/devstoreaccount1`); `None` is
    /// `https://<account>.blob.core.windows.net`.
    pub fn with_shared_key(
        account: impl Into<String>,
        key_b64: &str,
        container: impl Into<String>,
        endpoint: Option<String>,
    ) -> StoreResult<Self> {
        let key = base64_decode(key_b64.trim())
            .ok_or_else(|| StoreError::Other("azure: the account key is not base64".into()))?;
        Self::build(account.into(), Auth::SharedKey(key), container.into(), endpoint)
    }

    /// Authorize with a SAS token (the query string, with or without `?`).
    pub fn with_sas(
        account: impl Into<String>,
        sas: &str,
        container: impl Into<String>,
        endpoint: Option<String>,
    ) -> StoreResult<Self> {
        let sas = sas.trim().trim_start_matches('?').to_string();
        if sas.is_empty() {
            return Err(StoreError::Other("azure: the SAS token is empty".into()));
        }
        Self::build(account.into(), Auth::Sas(sas), container.into(), endpoint)
    }

    /// Authorize with an Entra ID token from `source`. The account only
    /// names the default endpoint: a token is not signed with it.
    pub fn with_entra(
        account: impl Into<String>,
        source: EntraSource,
        container: impl Into<String>,
        endpoint: Option<String>,
    ) -> StoreResult<Self> {
        Self::build(account.into(), Auth::Entra { source, cache: TokenCache::default() }, container.into(), endpoint)
    }

    /// From the environment the Azure tools use, in the Azure SDK's order:
    /// - `AZURE_STORAGE_KEY`, then `AZURE_STORAGE_SAS_TOKEN`, each with
    ///   `AZURE_STORAGE_ACCOUNT`;
    /// - workload identity: `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_CLIENT_ID`
    ///   and `AZURE_TENANT_ID` (and `AZURE_AUTHORITY_HOST`), as the AKS
    ///   webhook sets them;
    /// - otherwise the VM's managed identity (`AZURE_CLIENT_ID` picks a
    ///   user-assigned one; `AZURE_POD_IDENTITY_AUTHORITY_HOST` moves the
    ///   metadata endpoint), asked at the first request.
    ///
    /// A token names no account, so with one `AZURE_STORAGE_ACCOUNT` may be
    /// left out when `endpoint` names it.
    pub fn from_env(container: impl Into<String>, endpoint: Option<String>) -> StoreResult<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let named = var("AZURE_STORAGE_ACCOUNT");
        let account = || named.clone().ok_or_else(|| StoreError::Other("azure: AZURE_STORAGE_ACCOUNT is not set".into()));
        if let Some(key) = var("AZURE_STORAGE_KEY") {
            return Self::with_shared_key(account()?, &key, container, endpoint);
        }
        if let Some(sas) = var("AZURE_STORAGE_SAS_TOKEN") {
            return Self::with_sas(account()?, &sas, container, endpoint);
        }
        let account = match named.or_else(|| endpoint.as_deref().and_then(account_of_endpoint)) {
            Some(a) => a,
            None => {
                return Err(StoreError::Other(
                    "azure: AZURE_STORAGE_ACCOUNT is not set, and no endpoint names the account".into(),
                ))
            }
        };
        let source = match (var("AZURE_FEDERATED_TOKEN_FILE"), var("AZURE_CLIENT_ID"), var("AZURE_TENANT_ID")) {
            (Some(file), Some(client_id), Some(tenant)) => EntraSource::Federated {
                tenant,
                client_id,
                token_file: file.into(),
                authority: var("AZURE_AUTHORITY_HOST").unwrap_or_else(|| ENTRA_AUTHORITY.into()),
            },
            (Some(_), _, _) => {
                return Err(StoreError::Other(
                    "azure: AZURE_FEDERATED_TOKEN_FILE is set, but AZURE_CLIENT_ID or AZURE_TENANT_ID is not".into(),
                ))
            }
            (None, client_id, _) => EntraSource::ManagedIdentity {
                client_id,
                endpoint: var("AZURE_POD_IDENTITY_AUTHORITY_HOST").unwrap_or_else(|| IMDS.into()),
            },
        };
        Self::with_entra(account, source, container, endpoint)
    }

    fn build(account: String, auth: Auth, container: String, endpoint: Option<String>) -> StoreResult<Self> {
        if account.is_empty() || container.is_empty() {
            return Err(StoreError::Other("azure: the account or the container is empty".into()));
        }
        let endpoint = endpoint.unwrap_or_else(|| format!("https://{account}.blob.core.windows.net"));
        let uri: http::Uri =
            endpoint.parse().map_err(|e| StoreError::Other(format!("azure: endpoint {endpoint:?}: {e}")))?;
        let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
            return Err(StoreError::Other(format!("azure: endpoint {endpoint:?} needs a scheme and a host")));
        };
        let opts = AzureOptions::default();
        Ok(AzureStore {
            client: https_client(opts.connect_timeout, "azure")?,
            account,
            auth,
            origin: format!("{scheme}://{authority}"),
            base_path: uri.path().trim_end_matches('/').to_string(),
            container,
            opts,
            requests: Default::default(),
            upload_gate: None,
            tokens: Mutex::new(HashMap::new()),
        })
    }

    /// Bound the bytes the upload path holds at once (`gate.rs`). `0` = no gate.
    pub fn with_upload_inflight_max_bytes(mut self, n: u64) -> Self {
        self.upload_gate = (n > 0).then(|| Arc::new(ByteGate::new(n)));
        self
    }

    pub fn container(&self) -> &str {
        &self.container
    }

    /// Create the container when it is missing. For rigs and tests: a
    /// deployment's container is the operator's to make.
    pub async fn ensure_container(&self) -> StoreResult<()> {
        let req = Req::new("create_container", RequestKind::Put, Method::PUT, None).query("restype", "container");
        match self.send(req).await {
            Ok(_) => Ok(()),
            Err(StoreError::Conflict(m)) if m.contains("ContainerAlreadyExists") => Ok(()),
            Err(e) => Err(e),
        }
    }

    // ── requests ─────────────────────────────────────────────────────

    /// `/<base>/<container>[/<key>]`, percent-encoded.
    fn url_path(&self, key: Option<&str>) -> String {
        let mut p = format!("{}/{}", self.base_path, encode(&self.container, true));
        if let Some(k) = key {
            p.push('/');
            p.push_str(&encode(k, true));
        }
        p
    }

    /// The URL of a blob, as another request names it (a copy's source).
    fn blob_url(&self, key: &str) -> String {
        let mut u = format!("{}{}", self.origin, self.url_path(Some(key)));
        if let Auth::Sas(sas) = &self.auth {
            u.push('?');
            u.push_str(sas);
        }
        u
    }

    /// The bearer token, when this store is authorized by one.
    async fn bearer(&self) -> StoreResult<Option<String>> {
        let Auth::Entra { source, cache } = &self.auth else { return Ok(None) };
        cache.get(|| self.entra_token(source)).await.map(Some)
    }

    /// One Entra ID token for Blob Storage, and its lifetime.
    async fn entra_token(&self, source: &EntraSource) -> StoreResult<(String, Duration)> {
        let wait = self.opts.first_byte_timeout;
        let (status, v) = match source {
            EntraSource::Federated { tenant, client_id, token_file, authority } => {
                // Re-read every time: the kubelet rotates the file.
                let assertion = tokio::fs::read_to_string(token_file)
                    .await
                    .map_err(|e| StoreError::Auth(format!("azure: the federated token {}: {e}", token_file.display())))?;
                let scope = format!("{STORAGE_RESOURCE}/.default");
                let body = form(&[
                    ("grant_type", "client_credentials"),
                    ("client_id", client_id),
                    ("scope", &scope),
                    ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"),
                    ("client_assertion", assertion.trim()),
                ]);
                let url = format!("{}/{}/oauth2/v2.0/token", authority.trim_end_matches('/'), encode(tenant, false));
                let req = Request::builder()
                    .method(Method::POST)
                    .uri(&url)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("content-length", body.len().to_string())
                    .body(Full::new(Bytes::from(body)))
                    .map_err(|e| StoreError::Other(format!("azure token: build request: {e}")))?;
                let (status, v) = token_request(&self.client, req, wait, "azure token").await?;
                if status != 200 {
                    return Err(StoreError::Auth(format!(
                        "azure: Entra ID refused the federated token for client {client_id}: {status} {}",
                        refusal(&v)
                    )));
                }
                (status, v)
            }
            EntraSource::ManagedIdentity { client_id, endpoint } => {
                let mut url = format!(
                    "{}/metadata/identity/oauth2/token?api-version=2018-02-01&resource={}",
                    endpoint.trim_end_matches('/'),
                    encode(&format!("{STORAGE_RESOURCE}/"), false)
                );
                if let Some(c) = client_id {
                    url.push_str(&format!("&client_id={}", encode(c, false)));
                }
                let req = Request::builder()
                    .method(Method::GET)
                    .uri(&url)
                    .header("metadata", "true")
                    .body(Full::new(Bytes::new()))
                    .map_err(|e| StoreError::Other(format!("azure token: build request: {e}")))?;
                let (status, v) = token_request(&self.client, req, wait, "azure token").await.map_err(|e| {
                    StoreError::Auth(format!(
                        "azure: no credentials: AZURE_STORAGE_KEY, AZURE_STORAGE_SAS_TOKEN and \
                         AZURE_FEDERATED_TOKEN_FILE are unset, and the managed identity endpoint did not answer: {e}"
                    ))
                })?;
                if status != 200 {
                    return Err(StoreError::Auth(format!("azure: the managed identity endpoint refused: {status} {}", refusal(&v))));
                }
                (status, v)
            }
        };
        let token = v["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| StoreError::Auth(format!("azure: the token endpoint answered {status} with no access_token")))?;
        Ok((token.to_string(), expires_in(&v)))
    }

    /// Sign `req` and give back the request hyper sends. `bearer`: the
    /// Entra token, when that is how this store is authorized.
    fn signed(&self, req: &Req, bearer: Option<&str>) -> StoreResult<Request<Full<Bytes>>> {
        let path = self.url_path(req.key.as_deref());
        let mut headers = req.headers.clone();
        headers.push(("x-ms-date".into(), httpdate::fmt_http_date(SystemTime::now())));
        headers.push(("x-ms-version".into(), API_VERSION.into()));
        let mut query: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={}", encode(v, false))).collect();
        match &self.auth {
            Auth::SharedKey(key) => {
                let sts = string_to_sign(&req.method, req.body.len(), &headers, &self.account, &path, &req.query);
                let sig = base64_std(&hmac_sha256(key, sts.as_bytes()));
                headers.push(("authorization".into(), format!("SharedKey {}:{sig}", self.account)));
            }
            Auth::Sas(sas) => query.push(sas.clone()),
            Auth::Entra { .. } => {
                let t = bearer.ok_or_else(|| StoreError::Other(format!("{}: no Entra token to send", req.ctx)))?;
                headers.push(("authorization".into(), format!("Bearer {t}")));
            }
        }
        let mut uri = format!("{}{path}", self.origin);
        if !query.is_empty() {
            uri.push('?');
            uri.push_str(&query.join("&"));
        }
        let mut b = Request::builder().method(req.method.clone()).uri(uri);
        for (k, v) in &headers {
            b = b.header(k.as_str(), v.as_str());
        }
        if !req.body.is_empty() || req.method == Method::PUT {
            b = b.header("content-length", req.body.len().to_string());
        }
        b.body(Full::new(req.body.clone())).map_err(|e| StoreError::Other(format!("{}: build request: {e}", req.ctx)))
    }

    /// Send with the retry budget.
    async fn send(&self, req: Req) -> StoreResult<Resp> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            self.requests.bump(req.kind);
            match self.attempt(&req).await {
                Ok(r) => return Ok(r),
                Err(Failure::Final(e)) => return Err(e),
                Err(Failure::Retry(why)) => {
                    if attempt >= self.opts.max_attempts {
                        return Err(StoreError::Other(format!("{}: {why} (after {attempt} attempts)", req.ctx)));
                    }
                    let cap = self.opts.backoff_base.saturating_mul(1u32 << (attempt - 1).min(16)).min(self.opts.backoff_cap);
                    tokio::time::sleep(jitter(cap)).await;
                }
            }
        }
    }

    async fn attempt(&self, req: &Req) -> Result<Resp, Failure> {
        let bearer = self.bearer().await.map_err(Failure::Final)?;
        let http_req = self.signed(req, bearer.as_deref()).map_err(Failure::Final)?;
        let mib = (req.body.len() as u64).div_ceil(1 << 20) as u32;
        let wait = self.opts.first_byte_timeout + self.opts.per_mib_sent * mib;
        let resp = match tokio::time::timeout(wait, self.client.request(http_req)).await {
            Err(_) => return Err(Failure::Retry(format!("no response within {wait:?}"))),
            Ok(Err(e)) => return Err(Failure::Retry(format!("transport: {e}"))),
            Ok(Ok(r)) => r,
        };
        let status = resp.status();
        let (parts, mut body) = resp.into_parts();
        let mut all = BytesMut::new();
        if req.method != Method::HEAD {
            loop {
                match tokio::time::timeout(self.opts.body_stall_timeout, body.frame()).await {
                    Err(_) => return Err(Failure::Retry(format!("body stalled for {:?}", self.opts.body_stall_timeout))),
                    Ok(None) => break,
                    Ok(Some(Err(e))) => return Err(Failure::Retry(format!("body: {e}"))),
                    Ok(Some(Ok(frame))) => {
                        if let Ok(data) = frame.into_data() {
                            all.put_slice(&data);
                        }
                    }
                }
            }
        }
        if status.is_success() {
            return Ok(Resp { headers: parts.headers, body: all.freeze() });
        }
        let code = parts
            .headers
            .get("x-ms-error-code")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| tag_body(&String::from_utf8_lossy(&all), "Code").map(xml_unescape))
            .unwrap_or_default();
        let msg = format!("{}: {} {code}", req.ctx, status.as_u16());
        if status.as_u16() == 401 {
            // The service refused the token itself (expired early,
            // revoked): the next request fetches another.
            if let Auth::Entra { cache, .. } = &self.auth {
                cache.clear().await;
            }
        }
        classify(status.as_u16(), &code, msg)
    }

    /// Send a write under `cond`. `If-Match` on a content ETag resolves to
    /// Azure's, and a 412 is re-checked by the bytes before it is reported.
    async fn send_conditional(
        &self,
        key: &str,
        cond: &PutCondition,
        build: &(dyn Fn(Option<(&'static str, String)>) -> Req + Send + Sync),
    ) -> StoreResult<Resp> {
        let want = match cond {
            PutCondition::Unconditional => return self.send(build(None)).await,
            PutCondition::IfNoneMatchAny => return self.send(build(Some(("if-none-match", "*".into())))).await,
            PutCondition::IfMatch(want) => want,
        };
        let mut fresh = false;
        for _ in 0..3 {
            let token = self.token_for(key, want, fresh).await?;
            match self.send(build(Some(("if-match", token)))).await {
                // Ours moved under us, or the cached token was stale: ask
                // the bytes. A source condition is not ours to re-check.
                Err(StoreError::PreconditionFailed(m)) if !m.contains("SourceConditionNotMet") => fresh = true,
                r => return r,
            }
        }
        Err(StoreError::PreconditionFailed(format!("{key}: still moving after three re-checks")))
    }

    /// The Azure ETag behind `want`, a content ETag or an outside writer's.
    async fn token_for(&self, key: &str, want: &str, fresh: bool) -> StoreResult<String> {
        let want = quoted(want);
        if let Some(t) = foreign_token(key, &want)? {
            return Ok(quoted(&t));
        }
        if !fresh {
            if let Some((c, t)) = self.tokens.lock().unwrap().get(key) {
                if *c == want {
                    return Ok(t.clone());
                }
            }
        }
        let seen = self.head_seen(key).await?;
        if seen.meta.etag != want {
            return Err(StoreError::PreconditionFailed(format!("{key}: is {} not {want}", seen.meta.etag)));
        }
        Ok(seen.token)
    }

    fn remember(&self, key: &str, etag: &str, token: &str) {
        let mut t = self.tokens.lock().unwrap();
        if t.len() >= TOKENS_MAX {
            t.clear();
        }
        t.insert(key.to_string(), (etag.to_string(), token.to_string()));
    }

    fn forget(&self, key: &str) {
        self.tokens.lock().unwrap().remove(key);
    }

    async fn head_seen(&self, key: &str) -> StoreResult<Seen> {
        let resp = self.send(Req::new("get_blob_properties", RequestKind::Head, Method::HEAD, Some(key))).await?;
        let size = header(&resp.headers, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let seen = seen(key, &resp.headers, size)?;
        self.remember(key, &seen.meta.etag, &seen.token);
        Ok(seen)
    }

    /// GET, optionally a range, checked against `if_match` by the ETag the
    /// response carries: body and metadata come from one version.
    async fn get(&self, key: &str, range: Option<(u64, u64)>, if_match: Option<&str>) -> StoreResult<(Seen, Bytes)> {
        let mut req = Req::new("get_blob", RequestKind::Get, Method::GET, Some(key));
        if let Some((off, len)) = range {
            req = req.header("x-ms-range", format!("bytes={off}-{}", off + len - 1));
        }
        if let Some(want) = if_match {
            if let Some(token) = foreign_token(key, &quoted(want))? {
                req = req.header("if-match", quoted(&token));
            }
        }
        let resp = self.send(req).await?;
        let size = match range {
            Some(_) => header(&resp.headers, "content-range")
                .and_then(|v| v.rsplit('/').next().and_then(|t| t.parse().ok()))
                .unwrap_or(resp.body.len() as u64),
            None => resp.body.len() as u64,
        };
        let seen = seen(key, &resp.headers, size)?;
        if let Some(want) = if_match {
            if seen.meta.etag != quoted(want) {
                return Err(StoreError::PreconditionFailed(format!("{key}: is {} not {}", seen.meta.etag, quoted(want))));
            }
        }
        self.remember(key, &seen.meta.etag, &seen.token);
        Ok((seen, resp.body))
    }
}

/// Map an Azure error. `Retry` for what the next attempt may not meet.
fn classify(status: u16, code: &str, msg: String) -> Result<Resp, Failure> {
    let transient = matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
        || matches!(code, "ServerBusy" | "OperationTimedOut" | "InternalError");
    if transient {
        return Err(Failure::Retry(msg));
    }
    Err(Failure::Final(match (status, code) {
        (412, _) => StoreError::PreconditionFailed(msg),
        // `If-None-Match: *` over a blob: S3 answers 412, Azure 409.
        (409, "BlobAlreadyExists") => StoreError::PreconditionFailed(msg),
        (409, _) => StoreError::Conflict(msg),
        (404, _) => StoreError::NotFound(msg),
        (401 | 403, _) => StoreError::Auth(msg),
        (400, "Md5Mismatch" | "Crc64Mismatch" | "InvalidMd5" | "InvalidCrc64") => StoreError::ChecksumMismatch(msg),
        _ => StoreError::Other(msg),
    }))
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// A blob's metadata, by the names Azure stored (`x-ms-meta-` stripped).
fn raw_meta(h: &HeaderMap) -> HashMap<String, String> {
    h.iter()
        .filter_map(|(k, v)| {
            let name = k.as_str().strip_prefix("x-ms-meta-")?;
            Some((name.to_string(), v.to_str().ok()?.to_string()))
        })
        .collect()
}

/// The metadata a caller sees: flint's own names back in their hyphenated
/// form (`flint_gen` -> `flint-gen`), the hash and the CRC left out.
fn public_meta(raw: &HashMap<String, String>) -> HashMap<String, String> {
    raw.iter()
        .filter(|(k, _)| k.as_str() != META_HASH && k.as_str() != META_CRC)
        .map(|(k, v)| {
            let k = if k.starts_with("flint_") { k.replace('_', "-") } else { k.clone() };
            (k, v.clone())
        })
        .collect()
}

/// `x-ms-meta-*` headers for a write: the stamps, renamed to identifiers,
/// then the hash and the CRC.
fn meta_headers(stamps: &GenerationStamps, hash: &str, crc_b64: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> =
        stamps.to_meta().into_iter().map(|(k, val)| (format!("x-ms-meta-{}", k.replace('-', "_")), val)).collect();
    v.push((format!("x-ms-meta-{META_HASH}"), hash.to_string()));
    v.push((format!("x-ms-meta-{META_CRC}"), crc_b64.to_string()));
    v
}

fn seen(key: &str, h: &HeaderMap, size: u64) -> StoreResult<Seen> {
    let token = header(h, "etag").map(quoted).ok_or_else(|| StoreError::Other(format!("{key}: no ETag in the response")))?;
    let raw = raw_meta(h);
    let tier = header(h, "x-ms-access-tier").filter(|t| !t.eq_ignore_ascii_case("hot"));
    Ok(Seen {
        meta: ObjectMeta {
            etag: presented(key, raw.get(META_HASH).map(String::as_str), &token),
            size,
            crc64_b64: raw.get(META_CRC).cloned(),
            meta: public_meta(&raw),
            last_modified_unix: header(h, "last-modified").and_then(unix_of),
            storage_class: tier.map(str::to_string),
            version_id: header(h, "x-ms-version-id").map(str::to_string),
        },
        token,
    })
}

fn unix_of(http_date: &str) -> Option<u64> {
    httpdate::parse_http_date(http_date).ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The account an endpoint names: the first label of
/// `<account>.blob.core.windows.net` (any cloud's), or the first path
/// segment of an emulator's `http://127.0.0.1:10000/<account>`.
fn account_of_endpoint(endpoint: &str) -> Option<String> {
    let uri: http::Uri = endpoint.parse().ok()?;
    let host = uri.host()?;
    if let Some((first, rest)) = host.split_once('.') {
        if rest.starts_with("blob.") && !first.is_empty() {
            return Some(first.to_string());
        }
    }
    uri.path().trim_matches('/').split('/').next().filter(|s| !s.is_empty()).map(str::to_string)
}

/// A block id: one per compose (so two composes of one key never mix
/// blocks), then the part's index. Every id of a blob has one length.
fn block_id(nonce: &str, i: usize) -> String {
    base64_std(format!("{nonce}{i:08}").as_bytes())
}

// ── Shared Key ───────────────────────────────────────────────────────

/// The Blob service's Shared Key string to sign (version 2015-02-21 and
/// later: a zero Content-Length is the empty string). Ranges travel as
/// `x-ms-range`, so `Range` is always empty here.
fn string_to_sign(
    method: &Method,
    content_length: usize,
    headers: &[(String, String)],
    account: &str,
    path: &str,
    query: &[(&'static str, String)],
) -> String {
    let get = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("");
    let len = if content_length == 0 { String::new() } else { content_length.to_string() };
    let mut ms: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| k.starts_with("x-ms-"))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    ms.sort();
    let mut s = format!(
        "{}\n{}\n{}\n{len}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        method.as_str(),
        get("content-encoding"),
        get("content-language"),
        get("content-md5"),
        get("content-type"),
        get("date"),
        get("if-modified-since"),
        get("if-match"),
        get("if-none-match"),
        get("if-unmodified-since"),
        get("range"),
    );
    for (k, v) in &ms {
        s.push_str(k);
        s.push(':');
        s.push_str(v);
        s.push('\n');
    }
    s.push('/');
    s.push_str(account);
    s.push_str(path);
    let mut q: Vec<(String, &str)> = query.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.as_str())).collect();
    q.sort();
    for (k, v) in q {
        s.push('\n');
        s.push_str(&k);
        s.push(':');
        s.push_str(v);
    }
    s
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let (mut ipad, mut opad) = ([0x36u8; 64], [0x5cu8; 64]);
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new().chain_update(ipad).chain_update(msg).finalize();
    Sha256::new().chain_update(opad).chain_update(inner).finalize().into()
}

// ── XML, just enough ─────────────────────────────────────────────────

/// The inner text of the first `<tag>` (attributes allowed) in `s`;
/// `Some("")` for `<tag/>`.
fn tag_body<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    tag_bodies(s, tag).into_iter().next().map(|(_, body)| body)
}

/// Every `<tag ...>body</tag>` in `s`: (the opening tag's attributes, the body).
fn tag_bodies<'a>(s: &'a str, tag: &str) -> Vec<(&'a str, &'a str)> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = vec![];
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        match after.as_bytes().first() {
            Some(b'>') | Some(b' ') | Some(b'/') => {}
            _ => {
                rest = after;
                continue;
            }
        }
        let Some(gt) = after.find('>') else { break };
        let attrs = &after[..gt];
        if attrs.ends_with('/') {
            out.push((attrs, ""));
            rest = &after[gt + 1..];
            continue;
        }
        let inner = &after[gt + 1..];
        let Some(end) = inner.find(&close) else { break };
        out.push((attrs, &inner[..end]));
        rest = &inner[end + close.len()..];
    }
    out
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let ent = &after[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=semi]),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// One page of List Blobs: (key, size, Azure ETag, last-modified, hash), and the next marker.
type Listed = (String, u64, String, Option<u64>, Option<String>);

fn parse_list(xml: &str) -> (Vec<Listed>, Option<String>) {
    let mut out = vec![];
    for (_, blob) in tag_bodies(xml, "Blob") {
        let Some((attrs, name)) = tag_bodies(blob, "Name").into_iter().next() else { continue };
        let name = if attrs.contains("Encoded=\"true\"") { percent_decode(name) } else { xml_unescape(name) };
        let props = tag_body(blob, "Properties").unwrap_or("");
        let size = tag_body(props, "Content-Length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        let etag = tag_body(props, "Etag").map(|v| quoted(&xml_unescape(v))).unwrap_or_default();
        let modified = tag_body(props, "Last-Modified").and_then(|v| unix_of(v.trim()));
        let hash = tag_body(blob, "Metadata").and_then(|m| tag_body(m, META_HASH)).map(|h| xml_unescape(h).trim().to_string());
        out.push((name, size, etag, modified, hash));
    }
    let next = tag_body(xml, "NextMarker").map(xml_unescape).filter(|m| !m.trim().is_empty());
    (out, next)
}

#[async_trait]
impl ObjectStore for AzureStore {
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        condition: &PutCondition,
        stamps: &GenerationStamps,
        crc64: u64,
    ) -> StoreResult<ObjectMeta> {
        // The caller's CRC is a claim about these bytes: check it here,
        // before anything leaves (Azure keeps no CRC of its own to check).
        let actual = crc64_nvme(&body);
        if actual != crc64 {
            return Err(StoreError::ChecksumMismatch(format!(
                "{key}: the bytes' CRC-64 is {} and the caller said {}",
                crc64_to_b64(actual),
                crc64_to_b64(crc64)
            )));
        }
        let hash = content_hash(&Sha256::digest(&body));
        let crc_b64 = crc64_to_b64(crc64);
        let metas = meta_headers(stamps, &hash, &crc_b64);
        let build = |cond: Option<(&'static str, String)>| {
            let mut r = Req::new("put_blob", RequestKind::Put, Method::PUT, Some(key))
                .header("x-ms-blob-type", "BlockBlob")
                .body(body.clone());
            r.headers.extend(metas.iter().cloned());
            if let Some((k, v)) = cond {
                r = r.header(k, v);
            }
            r
        };
        let resp = self.send_conditional(key, condition, &build).await?;
        let token = header(&resp.headers, "etag").map(quoted).unwrap_or_default();
        let etag = presented(key, Some(&hash), &token);
        self.remember(key, &etag, &token);
        Ok(ObjectMeta {
            etag,
            size: body.len() as u64,
            crc64_b64: Some(crc_b64),
            meta: stamps.to_meta().into_iter().collect(),
            last_modified_unix: header(&resp.headers, "last-modified").and_then(unix_of),
            storage_class: None,
            version_id: header(&resp.headers, "x-ms-version-id").map(str::to_string),
        })
    }

    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> StoreResult<ObjectMeta> {
        let src = self.head_seen(src_key).await?;
        if let Some(want) = src_if_match {
            if src.meta.etag != quoted(want) {
                return Err(StoreError::PreconditionFailed(format!("{src_key}: is {} not {}", src.meta.etag, quoted(want))));
            }
        }
        // The copy's hash and CRC are the source's (C6). An outside
        // writer's blob carries neither: read it once, pinned to the
        // version just seen.
        let (hash, crc_b64) = match (src.meta.etag.trim_matches('"').starts_with("x-"), &src.meta.crc64_b64) {
            (false, Some(c)) => (src.meta.etag.trim_matches('"').to_string(), c.clone()),
            _ => {
                let (_, body) = self.get(src_key, None, Some(&src.meta.etag)).await?;
                (content_hash(&Sha256::digest(&body)), crc64_to_b64(crc64_nvme(&body)))
            }
        };
        let metas = meta_headers(stamps, &hash, &crc_b64);
        let source = self.blob_url(src_key);
        let src_token = src.token.clone();
        let build = |cond: Option<(&'static str, String)>| {
            let mut r = Req::new("copy_blob", RequestKind::Copy, Method::PUT, Some(dst_key))
                .header("x-ms-copy-source", source.clone())
                .header("x-ms-source-if-match", src_token.clone());
            r.headers.extend(metas.iter().cloned());
            if let Some((k, v)) = cond {
                r = r.header(k, v);
            }
            r
        };
        let resp = self.send_conditional(dst_key, condition, &build).await?;
        let mut token = header(&resp.headers, "etag").map(quoted).unwrap_or_default();
        let mut status = header(&resp.headers, "x-ms-copy-status").unwrap_or("success").to_string();
        // Within one account a copy usually completes in the request; when
        // it does not, wait for it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        while status == "pending" {
            if tokio::time::Instant::now() > deadline {
                return Err(StoreError::Other(format!("{dst_key}: the copy from {src_key} is still pending")));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            let r = self.send(Req::new("copy_status", RequestKind::Head, Method::HEAD, Some(dst_key))).await?;
            status = header(&r.headers, "x-ms-copy-status").unwrap_or("success").to_string();
            token = header(&r.headers, "etag").map(quoted).unwrap_or_default();
        }
        if status != "success" {
            return Err(StoreError::Other(format!("{dst_key}: the copy from {src_key} ended {status}")));
        }
        let etag = presented(dst_key, Some(&hash), &token);
        self.remember(dst_key, &etag, &token);
        Ok(ObjectMeta {
            etag,
            size: src.meta.size,
            crc64_b64: Some(crc_b64),
            meta: stamps.to_meta().into_iter().collect(),
            last_modified_unix: header(&resp.headers, "last-modified").and_then(unix_of),
            storage_class: None,
            version_id: header(&resp.headers, "x-ms-version-id").map(str::to_string),
        })
    }

    async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta> {
        if spec.parts.iter().any(|p| matches!(p, PartSource::BaseCopy { .. })) {
            return Err(StoreError::Other(format!(
                "{}: the azure backend does not copy byte ranges yet (PartSource::BaseCopy)",
                spec.key
            )));
        }
        if spec.parts.len() > MAX_BLOCKS {
            return Err(StoreError::Other(format!("{}: {} parts, Azure takes {MAX_BLOCKS}", spec.key, spec.parts.len())));
        }
        let file = spec
            .local
            .as_ref()
            .ok_or_else(|| StoreError::Other(format!("{}: a Local part with no descriptor", spec.key)))?;
        let nonce = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
        let mut crc = Crc64Nvme::new();
        let mut sha = Sha256::new();
        let mut ids = Vec::with_capacity(spec.parts.len());
        let mut total = 0u64;
        for (i, part) in spec.parts.iter().enumerate() {
            let (offset, len) = match part {
                PartSource::Local { offset, len } => (*offset, *len),
                PartSource::BaseCopy { .. } => unreachable!("refused above"),
            };
            let _permit = match &self.upload_gate {
                Some(g) => Some(g.acquire(len).await),
                None => None,
            };
            let bytes = read_local(file, offset, len).await?;
            crc.update(&bytes);
            sha.update(&bytes);
            let id = block_id(&nonce, i);
            let req = Req::new("put_block", RequestKind::Put, Method::PUT, Some(spec.key))
                .query("comp", "block")
                .query("blockid", id.clone())
                .body(bytes);
            self.send(req).await?;
            ids.push(id);
            total += len;
            spec.note_progress(len);
        }
        let crc_v = crc.finalize();
        if let Some(want) = spec.crc64 {
            if want != crc_v {
                return Err(StoreError::ChecksumMismatch(format!(
                    "{}: the parts' CRC-64 is {} and the caller said {}",
                    spec.key,
                    crc64_to_b64(crc_v),
                    crc64_to_b64(want)
                )));
            }
        }
        let hash = content_hash(&sha.finalize());
        let crc_b64 = crc64_to_b64(crc_v);
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for id in &ids {
            xml.push_str("<Latest>");
            xml.push_str(id);
            xml.push_str("</Latest>");
        }
        xml.push_str("</BlockList>");
        let xml = Bytes::from(xml);
        let metas = meta_headers(&spec.stamps, &hash, &crc_b64);
        let build = |cond: Option<(&'static str, String)>| {
            let mut r = Req::new("put_block_list", RequestKind::Multipart, Method::PUT, Some(spec.key))
                .query("comp", "blocklist")
                .body(xml.clone());
            r.headers.extend(metas.iter().cloned());
            if let Some((k, v)) = cond {
                r = r.header(k, v);
            }
            r
        };
        // A refused commit leaves only uncommitted blocks, which no read
        // sees and Azure discards within a week: nothing to abort (A9).
        let resp = self.send_conditional(spec.key, &spec.condition, &build).await?;
        let token = header(&resp.headers, "etag").map(quoted).unwrap_or_default();
        let etag = presented(spec.key, Some(&hash), &token);
        self.remember(spec.key, &etag, &token);
        Ok(ObjectMeta {
            etag,
            size: total,
            crc64_b64: Some(crc_b64),
            meta: spec.stamps.to_meta().into_iter().collect(),
            last_modified_unix: header(&resp.headers, "last-modified").and_then(unix_of),
            storage_class: None,
            version_id: header(&resp.headers, "x-ms-version-id").map(str::to_string),
        })
    }

    async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
        Ok(self.head_seen(key).await?.meta)
    }

    async fn get_whole(&self, key: &str, if_match: Option<&str>) -> StoreResult<(ObjectMeta, Bytes)> {
        let (seen, body) = self.get(key, None, if_match).await?;
        Ok((seen.meta, body))
    }

    /// Every range is checked against the content ETag on its own
    /// response. The ETag is the bytes' hash, so ranges taken from two
    /// versions that carry it are ranges of one set of bytes.
    async fn get_range(&self, key: &str, offset: u64, len: u64, if_match: &str) -> StoreResult<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let (_, body) = self.get(key, Some((offset, len)), Some(if_match)).await?;
        Ok(body)
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>> {
        let mut out = vec![];
        let mut marker: Option<String> = None;
        loop {
            let mut req = Req::new("list_blobs", RequestKind::List, Method::GET, None)
                .query("restype", "container")
                .query("comp", "list")
                .query("include", "metadata")
                .query("prefix", prefix);
            if let Some(m) = &marker {
                req = req.query("marker", m.clone());
            }
            let resp = self.send(req).await?;
            let (page, next) = parse_list(&String::from_utf8_lossy(&resp.body));
            for (key, size, token, modified, hash) in page {
                let etag = presented(&key, hash.as_deref(), &token);
                self.remember(&key, &etag, &token);
                out.push(ListedObject { key, size, etag, last_modified_unix: modified });
            }
            match next {
                Some(m) => marker = Some(m),
                None => break,
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        let req = Req::new("delete_blob", RequestKind::Delete, Method::DELETE, Some(key))
            .header("x-ms-delete-snapshots", "include");
        self.forget(key);
        match self.send(req).await {
            Ok(_) | Err(StoreError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> StoreResult<()> {
        let build = |cond: Option<(&'static str, String)>| {
            let mut r = Req::new("delete_blob", RequestKind::Delete, Method::DELETE, Some(key))
                .header("x-ms-delete-snapshots", "include");
            if let Some((k, v)) = cond {
                r = r.header(k, v);
            }
            r
        };
        self.send_conditional(key, &PutCondition::IfMatch(etag.to_string()), &build).await?;
        self.forget(key);
        Ok(())
    }

    /// Nothing to list: uncommitted blocks are invisible and expire on
    /// their own.
    async fn list_uploads(&self, _prefix: &str) -> StoreResult<Vec<PendingUpload>> {
        Ok(vec![])
    }

    async fn abort_upload(&self, _key: &str, _upload_id: &str) -> StoreResult<()> {
        Ok(())
    }

    async fn bootstrap(&self, prefix: &str) -> StoreResult<BootstrapReport> {
        let mut report = BootstrapReport::default();
        let props = Req::new("get_container_properties", RequestKind::Head, Method::HEAD, None).query("restype", "container");
        if let Err(e) = self.send(props).await {
            report.errors.push(format!("container {} unreachable: {e}", self.container));
            return Ok(report);
        }
        report.notes.push(
            "versioning, soft delete and lifecycle are storage-account settings (the management plane): not checked".into(),
        );
        let probe = format!("{prefix}flint-bootstrap-probe");
        let body = Bytes::from_static(b"flint bootstrap probe");
        let stamps = GenerationStamps { generation: 0, epoch: 0, flush_uuid: "bootstrap".into(), boundary_source: None, posix: None };
        match self.put_whole(&probe, body.clone(), &PutCondition::Unconditional, &stamps, crc64_nvme(&body)).await {
            Ok(_) => {
                if let Err(e) = self.delete(&probe).await {
                    report.warnings.push(format!("probe {probe}: written but not deleted: {e}"));
                }
            }
            Err(e) => report.errors.push(format!("probe {probe}: cannot write: {e}")),
        }
        Ok(report)
    }

    async fn epoch_read(&self, key: &str) -> StoreResult<Option<EpochState>> {
        cell::read(self, key).await
    }

    async fn epoch_acquire(&self, key: &str, holder_id: &str, supersede: Option<&EpochState>) -> StoreResult<EpochLease> {
        cell::acquire(self, key, holder_id, supersede).await
    }

    async fn epoch_renew(&self, key: &str, lease: &EpochLease, echo: Option<&str>) -> StoreResult<EpochLease> {
        cell::renew(self, key, lease, echo).await
    }

    async fn epoch_release(&self, key: &str, lease: &EpochLease) -> StoreResult<()> {
        cell::release(self, key, lease).await
    }

    async fn epoch_enqueue(&self, key: &str, observed: &EpochState, holder_id: &str) -> StoreResult<EpochState> {
        cell::enqueue(self, key, observed, holder_id).await
    }

    async fn epoch_handoff(&self, key: &str, lease: &EpochLease, echo: Option<&str>) -> StoreResult<()> {
        cell::handoff(self, key, lease, echo).await
    }

    fn min_part_size(&self) -> u64 {
        MIN_PART
    }

    fn max_parts(&self) -> usize {
        MAX_BLOCKS
    }

    fn upload_gate(&self) -> Option<Arc<ByteGate>> {
        self.upload_gate.clone()
    }

    fn request_counts(&self) -> Option<RequestCounts> {
        Some(self.requests.snapshot())
    }

    fn provider(&self) -> Provider {
        Provider::Azure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::hex;

    #[test]
    fn hmac_sha256_meets_rfc_4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(hex(&mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // A key longer than the block is hashed first (case 6).
        let mac = hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(hex(&mac), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    /// The layout Microsoft documents: verb, eleven standard headers, the
    /// x-ms headers sorted, then `/account/path` and the query, sorted.
    #[test]
    fn the_string_to_sign_has_the_documented_layout() {
        let headers = vec![
            ("x-ms-version".to_string(), API_VERSION.to_string()),
            ("x-ms-date".to_string(), "Tue, 10 Nov 2026 16:00:00 GMT".to_string()),
            ("if-match".to_string(), "\"0x8D\"".to_string()),
            ("x-ms-blob-type".to_string(), "BlockBlob".to_string()),
        ];
        let sts = string_to_sign(
            &Method::PUT,
            11,
            &headers,
            "acct",
            "/acct/c/k",
            &[("comp", "block".into()), ("blockid", "QQ==".into())],
        );
        let want = "PUT\n\n\n11\n\n\n\n\n\"0x8D\"\n\n\n\n\
                    x-ms-blob-type:BlockBlob\nx-ms-date:Tue, 10 Nov 2026 16:00:00 GMT\nx-ms-version:2023-11-03\n\
                    /acct/acct/c/k\nblockid:QQ==\ncomp:block";
        assert_eq!(sts, want);
        // A zero length is the empty string.
        let get = string_to_sign(&Method::GET, 0, &[], "a", "/c", &[]);
        assert!(get.starts_with("GET\n\n\n\n"), "{get:?}");
    }

    #[test]
    fn flint_names_round_trip_through_azure_identifiers_and_the_internal_ones_stay_hidden() {
        let stamps = GenerationStamps { generation: 3, epoch: 2, flush_uuid: "f".into(), boundary_source: Some("cadence".into()), posix: None };
        let headers = meta_headers(&stamps, "h", "c");
        for (name, _) in &headers {
            let id = name.strip_prefix("x-ms-meta-").unwrap();
            assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "{id} is not an identifier");
        }
        let raw: HashMap<String, String> =
            headers.into_iter().map(|(k, v)| (k.strip_prefix("x-ms-meta-").unwrap().to_string(), v)).collect();
        let public = public_meta(&raw);
        assert_eq!(GenerationStamps::from_meta(&public), Some(stamps));
        assert!(!public.keys().any(|k| k.contains("sha256") || k.contains("crc64")), "{public:?}");
    }

    #[test]
    fn block_ids_have_one_length_and_differ_by_compose() {
        let a: Vec<String> = (0..3).map(|i| block_id("0123456789abcdef", i)).collect();
        let b = block_id("fedcba9876543210", 0);
        assert!(a.iter().all(|id| id.len() == a[0].len()) && b.len() == a[0].len());
        assert_ne!(a[0], b);
        assert!(!a[0].ends_with('='), "24 bytes encode without padding");
    }

    #[test]
    fn an_endpoint_names_its_account() {
        assert_eq!(account_of_endpoint("https://acct.blob.core.windows.net").as_deref(), Some("acct"));
        assert_eq!(account_of_endpoint("https://acct.blob.core.chinacloudapi.cn/").as_deref(), Some("acct"));
        assert_eq!(account_of_endpoint("http://127.0.0.1:10000/devstoreaccount1").as_deref(), Some("devstoreaccount1"));
        assert_eq!(account_of_endpoint("https://proxy.example.com"), None);
        assert_eq!(account_of_endpoint("not a url"), None);
    }

    fn entra(source: EntraSource) -> AzureStore {
        AzureStore::with_entra("acct", source, "c", Some("http://127.0.0.1:1/acct".into())).unwrap()
    }

    /// The client-assertion grant: the workload's token, re-read from its
    /// file, traded at the tenant's token endpoint for a storage token,
    /// which is kept, and sent as a bearer with no Shared Key.
    #[tokio::test]
    async fn a_federated_token_is_traded_by_client_assertion_and_kept() {
        let idp = crate::rest::mock::start(|_| (200, r#"{"access_token":"entra-1","expires_in":3599,"token_type":"Bearer"}"#.into())).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "pod-token-1\n").unwrap();
        let s = entra(EntraSource::Federated {
            tenant: "tenant-a".into(),
            client_id: "client-b".into(),
            token_file: file.clone(),
            authority: idp.base.clone(),
        });
        assert_eq!(s.bearer().await.unwrap().as_deref(), Some("entra-1"));
        assert_eq!(s.bearer().await.unwrap().as_deref(), Some("entra-1"));
        let seen = idp.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "the token was not kept");
        assert_eq!((seen[0].method.as_str(), seen[0].target.as_str()), ("POST", "/tenant-a/oauth2/v2.0/token"));
        let body = &seen[0].body;
        for want in [
            "grant_type=client_credentials",
            "client_id=client-b",
            "scope=https%3A%2F%2Fstorage.azure.com%2F.default",
            "client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer",
            "client_assertion=pod-token-1",
        ] {
            assert!(body.split('&').any(|kv| kv == want), "{want} not in {body}");
        }
        let req = Req::new("t", RequestKind::Get, Method::GET, Some("k"));
        let signed = s.signed(&req, Some("entra-1")).unwrap();
        assert_eq!(signed.headers().get("authorization").unwrap(), "Bearer entra-1");
        assert!(signed.uri().query().is_none(), "no SAS on a bearer request");
        // A rotated file is what the next grant sends.
        std::fs::write(&file, "pod-token-2").unwrap();
        if let Auth::Entra { cache, .. } = &s.auth {
            cache.clear().await;
        }
        s.bearer().await.unwrap();
        assert!(idp.seen.lock().unwrap()[1].body.contains("client_assertion=pod-token-2"));
    }

    #[tokio::test]
    async fn a_refused_assertion_is_an_auth_failure_naming_the_client() {
        let idp = crate::rest::mock::start(|_| {
            (400, r#"{"error":"invalid_client","error_description":"AADSTS700213: No matching federated identity record found"}"#.into())
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t"), "x").unwrap();
        let s = entra(EntraSource::Federated {
            tenant: "t".into(),
            client_id: "client-b".into(),
            token_file: dir.path().join("t"),
            authority: idp.base.clone(),
        });
        match s.bearer().await {
            Err(StoreError::Auth(m)) => assert!(m.contains("client-b") && m.contains("AADSTS700213"), "{m}"),
            other => panic!("{other:?}"),
        }
        // A missing token file is named, not sent.
        let gone = entra(EntraSource::Federated {
            tenant: "t".into(),
            client_id: "c".into(),
            token_file: dir.path().join("absent"),
            authority: idp.base.clone(),
        });
        assert!(matches!(gone.bearer().await, Err(StoreError::Auth(m)) if m.contains("absent")));
    }

    /// The managed identity: the metadata service, asked with
    /// `Metadata: true` for a storage token, its lifetime a string.
    #[tokio::test]
    async fn a_managed_identity_asks_the_metadata_service() {
        let imds = crate::rest::mock::start(|r| {
            if r.header("metadata") != Some("true") {
                return (400, r#"{"error":"invalid_request","error_description":"Required metadata header not specified"}"#.into());
            }
            (200, r#"{"access_token":"mi-1","expires_in":"86399","expires_on":"1794400000","resource":"https://storage.azure.com/","token_type":"Bearer"}"#.into())
        })
        .await;
        let s = entra(EntraSource::ManagedIdentity { client_id: Some("uami-1".into()), endpoint: imds.base.clone() });
        assert_eq!(s.bearer().await.unwrap().as_deref(), Some("mi-1"));
        let seen = imds.seen.lock().unwrap().clone();
        assert_eq!(seen[0].method, "GET");
        assert_eq!(
            seen[0].target,
            "/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https%3A%2F%2Fstorage.azure.com%2F&client_id=uami-1"
        );
        // Nobody answering is a failure that names the whole chain.
        let none = entra(EntraSource::ManagedIdentity { client_id: None, endpoint: "http://127.0.0.1:1".into() });
        match none.bearer().await {
            Err(StoreError::Auth(m)) => assert!(m.contains("azure: no credentials") && m.contains("AZURE_FEDERATED_TOKEN_FILE"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_encoded_blob_name_decodes() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("p/%C3%A9%"), "p/é%");
    }

    #[test]
    fn a_list_page_parses_names_metadata_and_the_marker() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><EnumerationResults ContainerName="c"><Prefix>p/</Prefix><Blobs>
            <Blob><Name>p/a&amp;b</Name><Properties><Last-Modified>Tue, 10 Nov 2026 16:00:00 GMT</Last-Modified><Etag>0x8DC1</Etag><Content-Length>5</Content-Length></Properties><Metadata><flint_sha256>ab12</flint_sha256><flint_gen>1</flint_gen></Metadata></Blob>
            <Blob><Name Encoded="true">p/x%0Ay</Name><Properties><Etag>0x8DC2</Etag><Content-Length>0</Content-Length></Properties><Metadata /></Blob>
            </Blobs><NextMarker>2!abc</NextMarker></EnumerationResults>"#;
        let (page, next) = parse_list(xml);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].0, "p/a&b");
        assert_eq!((page[0].1, page[0].2.as_str(), page[0].4.as_deref()), (5, "\"0x8DC1\"", Some("ab12")));
        assert!(page[0].3.is_some());
        assert_eq!(page[1].0, "p/x\ny");
        assert_eq!(page[1].4, None);
        assert_eq!(next.as_deref(), Some("2!abc"));
        let (_, last) = parse_list("<EnumerationResults><Blobs></Blobs><NextMarker /></EnumerationResults>");
        assert_eq!(last, None);
    }

    #[test]
    fn errors_map_as_the_contract_needs() {
        let f = |s, c: &str| match classify(s, c, format!("{s} {c}")) {
            Err(Failure::Final(e)) => Some(e),
            _ => None,
        };
        assert!(matches!(f(412, "ConditionNotMet"), Some(StoreError::PreconditionFailed(_))));
        assert!(matches!(f(409, "BlobAlreadyExists"), Some(StoreError::PreconditionFailed(_))));
        assert!(matches!(f(409, "LeaseIdMissing"), Some(StoreError::Conflict(_))));
        assert!(matches!(f(404, "BlobNotFound"), Some(StoreError::NotFound(_))));
        assert!(matches!(f(403, "AuthenticationFailed"), Some(StoreError::Auth(_))));
        assert!(matches!(f(400, "Crc64Mismatch"), Some(StoreError::ChecksumMismatch(_))));
        assert!(f(503, "ServerBusy").is_none() && f(500, "InternalError").is_none(), "transient errors retry");
    }

    /// What the content ETag costs a large upload (plan §3.5): SHA-256 and
    /// CRC-64/NVME over 1 GiB, one core, as the compose pass runs them.
    /// `cargo test --release --features azure --lib hash_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn hash_cost() {
        let buf: Vec<u8> = (0..64 << 20).map(|i: usize| (i * 31 % 251) as u8).collect();
        let rounds = 16;
        let t = std::time::Instant::now();
        let mut sha = Sha256::new();
        for _ in 0..rounds {
            sha.update(&buf);
        }
        let d = sha.finalize();
        let sha_s = t.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        let mut crc = Crc64Nvme::new();
        for _ in 0..rounds {
            crc.update(&buf);
        }
        let c = crc.finalize();
        let crc_s = t.elapsed().as_secs_f64();
        let gib = (buf.len() * rounds) as f64 / (1u64 << 30) as f64;
        #[cfg(target_arch = "x86_64")]
        let sha_ni = std::arch::is_x86_feature_detected!("sha");
        #[cfg(not(target_arch = "x86_64"))]
        let sha_ni = "n/a (not x86_64)";
        eprintln!(
            "hash_cost: {gib:.1} GiB  sha256 {:.2} GiB/s  crc64-nvme {:.2} GiB/s  sha_ni={}  ({} {c:x})",
            gib / sha_s,
            gib / crc_s,
            sha_ni,
            content_hash(&d)
        );
    }

    #[test]
    fn xml_unescape_handles_named_and_numeric_entities() {
        assert_eq!(xml_unescape("a&amp;b&lt;c&gt;&quot;&apos;&#65;&#x42;&bogus;"), "a&b<c>\"'AB&bogus;");
    }
}
