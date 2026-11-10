//! The Google Cloud Storage backend (docs/plans/lean-azure-gcs-backends-plan-2026-11-10.md,
//! phase 3): the JSON API by hand, on the same hyper/rustls stack as the
//! Azure backend, authorized by a service account's key (an OAuth2 token
//! from a signed JWT), a bearer token, or nothing for an emulator.
//!
//! **Content ETags (plan §3, C3).** A generation names a write of one
//! object name, not the bytes, and lean compares the ETags of different
//! objects at one path. So the ETag this store hands out is a content
//! hash: the first 128 bits of the bytes' SHA-256, kept in the object's
//! metadata (`flint-sha256`) and written with it. The generation stays
//! inside and carries every condition (`ifGenerationMatch`, `0` for
//! create-only). An `If-Match` on a content ETag resolves to the
//! generation as last seen (or as one metadata GET finds it), and a 412
//! is re-checked by the bytes before it is reported, so the answer is
//! S3's in every case, a body that changes and changes back included. An
//! object an outside tool wrote has no hash; its ETag is
//! `"x-<key digest>-<generation>"`.
//!
//! **Checksums (C7).** GCS checks the CRC32C of every upload, which this
//! client computes and sends. The CRC-64/NVME lean cites rides in metadata
//! (`flint-crc64`), checked against the bytes before anything is sent.
//!
//! **Large files (C5).** Each part goes up as its own object under
//! `<scratch root>.flint/lean/parts/<nonce>/`, create-only and
//! CRC32C-checked. Then one compose (32 sources a call, nested above that)
//! lands the object at its key under the caller's condition, with the
//! hash, the CRC and the stamps as metadata, and the composite's CRC32C is
//! checked against the one computed over the parts. Then the parts go.
//! No lean sweep lists that namespace; `list_uploads` reports a crashed
//! compose's parts by age, and the operator's upload sweep deletes them.
//!
//! **Copies** are a one-source compose, not `objects.rewrite`: a compose
//! takes the destination's metadata as given, so the copy carries its own
//! stamps and never the source's (C6), and it is one request at any size.
//! (storage-testbench merges the source's metadata into a rewrite's.)
//!
//! **Reads** are guarded with `ifGenerationMatch`, never `generation=`: a
//! generation names a version, which a bucket that keeps old versions
//! (and the emulator) still serves after the object moved on.
//!
//! **The epoch cell** is a CAS on the cell's body (`cell.rs`). GCS takes
//! about one write a second per object name; a 429 is retried with
//! backoff.
//!
//! Not here yet: server-side copies of byte ranges (`PartSource::BaseCopy`),
//! batch delete, presigned URLs, version and lifecycle operations,
//! workload identity (phase 4), a count of 429s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use futures::stream::{FuturesUnordered, StreamExt};
use http::{HeaderMap, Method, Request};
use http_body_util::{BodyExt, Full};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::counters::{RequestCounter, RequestKind};
use crate::rest::{
    base64_decode, content_hash, encode, foreign_token, https_client, jitter, presented, quoted, read_local,
    HttpsClient,
};
use crate::{
    base64_std, cell, crc64_nvme, crc64_to_b64, gate::ByteGate, BootstrapReport, ComposeSpec, Crc64Nvme, EpochLease,
    EpochState, GenerationStamps, ListedObject, ObjectMeta, ObjectStore, PartSource, PendingUpload, Provider,
    PutCondition, RequestCounts, StoreError, StoreResult,
};

/// The service, when no endpoint is given.
pub const SERVICE: &str = "https://storage.googleapis.com";

/// Where a compose keeps its parts, under the scratch root: a namespace
/// no lean sweep lists.
pub const PARTS_DIR: &str = ".flint/lean/parts/";

/// GCS has no part floor; this is the grid lean cuts a large file on.
const MIN_PART: u64 = 8 * 1024 * 1024;
/// Parts of one object. Two levels of 32-source composes reach it, and it
/// stays within the 1,024 components a composite object once had as its
/// limit.
const MAX_PARTS: usize = 1024;
/// Sources in one compose request.
const COMPOSE_MAX: usize = 32;
/// The content hash and the CRC, in metadata.
const META_HASH: &str = "flint-sha256";
const META_CRC: &str = "flint-crc64";
/// Objects whose generation the store remembers, before it forgets them all.
const TOKENS_MAX: usize = 4096;
/// Part deletes in flight at once.
const DELETE_PARALLELISM: usize = 16;
/// Read and write objects; nothing on the bucket itself.
const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

enum Auth {
    /// An emulator: no Authorization header at all.
    None,
    Bearer(String),
    ServiceAccount(ServiceAccount),
}

struct ServiceAccount {
    email: String,
    key: Arc<dyn rustls::sign::SigningKey>,
    token_uri: String,
    /// The access token, and when to stop using it.
    cached: tokio::sync::Mutex<Option<(String, Instant)>>,
}

/// Timeouts and retries. More attempts than Azure's: GCS answers a second
/// write to one object name within a second with 429, and the lease cell
/// is written that often.
#[derive(Debug, Clone)]
pub struct GcsOptions {
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

impl Default for GcsOptions {
    fn default() -> Self {
        GcsOptions {
            connect_timeout: Duration::from_secs(3),
            first_byte_timeout: Duration::from_secs(10),
            per_mib_sent: Duration::from_secs(1),
            body_stall_timeout: Duration::from_secs(30),
            max_attempts: 5,
            backoff_base: Duration::from_millis(200),
            backoff_cap: Duration::from_secs(4),
        }
    }
}

pub struct GcsStore {
    client: HttpsClient,
    auth: Auth,
    /// `scheme://host[:port]` of the JSON API.
    origin: String,
    bucket: String,
    opts: GcsOptions,
    requests: Arc<RequestCounter>,
    upload_gate: Option<Arc<ByteGate>>,
    part_parallelism: usize,
    /// Where compose parts go: `<scratch root>.flint/lean/parts/...`.
    scratch_root: String,
    /// key -> the object as last read or written.
    seen: Mutex<HashMap<String, Seen>>,
}

/// One request, before it is authorized.
struct Req {
    ctx: &'static str,
    kind: RequestKind,
    method: Method,
    /// Percent-encoded, from the origin.
    path: String,
    /// Unencoded names and values.
    query: Vec<(&'static str, String)>,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl Req {
    fn new(ctx: &'static str, kind: RequestKind, method: Method, path: String) -> Req {
        Req { ctx, kind, method, path, query: vec![], headers: vec![], body: Bytes::new() }
    }
    fn query(mut self, name: &'static str, value: impl Into<String>) -> Req {
        self.query.push((name, value.into()));
        self
    }
    fn header(mut self, name: &str, value: impl Into<String>) -> Req {
        self.headers.push((name.to_string(), value.into()));
        self
    }
    fn json(mut self, v: &Value) -> Req {
        self.headers.push(("content-type".into(), "application/json".into()));
        self.body = Bytes::from(v.to_string());
        self
    }
    /// `ifGenerationMatch`, when there is one.
    fn generation(self, g: Option<String>) -> Req {
        match g {
            Some(g) => self.query("ifGenerationMatch", g),
            None => self,
        }
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

/// An object as its metadata, a read or a write saw it.
#[derive(Clone)]
struct Seen {
    meta: ObjectMeta,
    generation: String,
    metageneration: String,
    /// The service's own CRC32C of the bytes, wire form.
    crc32c: Option<String>,
}

impl GcsStore {
    /// Authorize as a service account, from its JSON key (as the console
    /// downloads it). `endpoint` `None` is the service.
    pub fn with_service_account_json(json_key: &str, bucket: impl Into<String>, endpoint: Option<String>) -> StoreResult<Self> {
        let sa = service_account(json_key)?;
        Self::build(Auth::ServiceAccount(sa), bucket.into(), endpoint)
    }

    /// Authorize with an OAuth2 access token as issued (`gcloud auth
    /// print-access-token`). It is not refreshed: for a rig or a check.
    pub fn with_token(token: &str, bucket: impl Into<String>, endpoint: Option<String>) -> StoreResult<Self> {
        let token = token.trim();
        if token.is_empty() {
            return Err(StoreError::Other("gcs: the access token is empty".into()));
        }
        Self::build(Auth::Bearer(token.to_string()), bucket.into(), endpoint)
    }

    /// No authorization at all: an emulator (storage-testbench,
    /// fake-gcs-server) at `endpoint`.
    pub fn emulator(endpoint: impl Into<String>, bucket: impl Into<String>) -> StoreResult<Self> {
        Self::build(Auth::None, bucket.into(), Some(endpoint.into()))
    }

    /// From the environment, first match wins:
    /// - `FLINT_GCS_ACCESS_TOKEN`: a bearer token;
    /// - `FLINT_GCS_SERVICE_ACCOUNT_JSON`: a service account's JSON key,
    ///   inline (a Secret delivered as a variable);
    /// - `GOOGLE_APPLICATION_CREDENTIALS`: the path of one;
    /// - `STORAGE_EMULATOR_HOST` (Google's own convention): an emulator,
    ///   unauthorized; `endpoint` wins over it when both are set.
    pub fn from_env(bucket: impl Into<String>, endpoint: Option<String>) -> StoreResult<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let bucket = bucket.into();
        if let Some(t) = var("FLINT_GCS_ACCESS_TOKEN") {
            return Self::with_token(&t, bucket, endpoint);
        }
        if let Some(j) = var("FLINT_GCS_SERVICE_ACCOUNT_JSON") {
            return Self::with_service_account_json(&j, bucket, endpoint);
        }
        if let Some(path) = var("GOOGLE_APPLICATION_CREDENTIALS") {
            let j = std::fs::read_to_string(&path)
                .map_err(|e| StoreError::Other(format!("gcs: GOOGLE_APPLICATION_CREDENTIALS {path}: {e}")))?;
            return Self::with_service_account_json(&j, bucket, endpoint);
        }
        if let Some(host) = var("STORAGE_EMULATOR_HOST") {
            let ep = endpoint.unwrap_or_else(|| {
                if host.contains("://") {
                    host.clone()
                } else {
                    format!("http://{host}")
                }
            });
            return Self::emulator(ep, bucket);
        }
        Err(StoreError::Other(
            "gcs: no credentials: set FLINT_GCS_SERVICE_ACCOUNT_JSON, GOOGLE_APPLICATION_CREDENTIALS or \
             FLINT_GCS_ACCESS_TOKEN (STORAGE_EMULATOR_HOST for an emulator)"
                .into(),
        ))
    }

    fn build(auth: Auth, bucket: String, endpoint: Option<String>) -> StoreResult<Self> {
        if bucket.is_empty() {
            return Err(StoreError::Other("gcs: the bucket is empty".into()));
        }
        let endpoint = endpoint.unwrap_or_else(|| SERVICE.to_string());
        let uri: http::Uri =
            endpoint.parse().map_err(|e| StoreError::Other(format!("gcs: endpoint {endpoint:?}: {e}")))?;
        let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
            return Err(StoreError::Other(format!("gcs: endpoint {endpoint:?} needs a scheme and a host")));
        };
        let opts = GcsOptions::default();
        Ok(GcsStore {
            client: https_client(opts.connect_timeout, "gcs")?,
            auth,
            origin: format!("{scheme}://{authority}"),
            bucket,
            opts,
            requests: Default::default(),
            upload_gate: None,
            part_parallelism: 1,
            scratch_root: String::new(),
            seen: Mutex::new(HashMap::new()),
        })
    }

    /// Bound the bytes the upload path holds at once (`gate.rs`). `0` = no gate.
    pub fn with_upload_inflight_max_bytes(mut self, n: u64) -> Self {
        self.upload_gate = (n > 0).then(|| Arc::new(ByteGate::new(n)));
        self
    }

    /// Parts of one compose uploaded concurrently. 1 = sequential.
    pub fn with_part_parallelism(mut self, n: usize) -> Self {
        self.part_parallelism = n.max(1);
        self
    }

    /// Where compose parts go: lean's workspace prefix, with its `/`. The
    /// default is the bucket's root.
    pub fn with_scratch_root(mut self, root: impl Into<String>) -> Self {
        self.scratch_root = root.into();
        self
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Create the bucket when it is missing. For rigs and tests against an
    /// emulator: a deployment's bucket is the operator's to make.
    pub async fn ensure_bucket(&self, project: &str) -> StoreResult<()> {
        let req = Req::new("create_bucket", RequestKind::Put, Method::POST, "/storage/v1/b".into())
            .query("project", project)
            .json(&json!({ "name": self.bucket }));
        match self.send(req).await {
            Ok(_) | Err(StoreError::Conflict(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    // ── paths ────────────────────────────────────────────────────────

    fn object_path(&self, key: &str) -> String {
        format!("/storage/v1/b/{}/o/{}", encode(&self.bucket, false), encode(key, false))
    }

    fn objects_path(&self) -> String {
        format!("/storage/v1/b/{}/o", encode(&self.bucket, false))
    }

    fn upload_path(&self) -> String {
        format!("/upload/storage/v1/b/{}/o", encode(&self.bucket, false))
    }

    // ── requests ─────────────────────────────────────────────────────

    async fn authorization(&self) -> StoreResult<Option<String>> {
        match &self.auth {
            Auth::None => Ok(None),
            Auth::Bearer(t) => Ok(Some(format!("Bearer {t}"))),
            Auth::ServiceAccount(sa) => {
                let mut cached = sa.cached.lock().await;
                if let Some((t, until)) = &*cached {
                    if Instant::now() < *until {
                        return Ok(Some(format!("Bearer {t}")));
                    }
                }
                let (t, ttl) = self.fetch_token(sa).await?;
                // A minute early, so no request carries a token that
                // expires on the way.
                *cached = Some((t.clone(), Instant::now() + ttl.saturating_sub(Duration::from_secs(60))));
                Ok(Some(format!("Bearer {t}")))
            }
        }
    }

    /// The OAuth2 JWT-bearer grant (RFC 7523): a JWT the key signs, traded
    /// for an access token.
    async fn fetch_token(&self, sa: &ServiceAccount) -> StoreResult<(String, Duration)> {
        let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let assertion = jwt(sa, now)?;
        let form = format!(
            "grant_type={}&assertion={}",
            encode("urn:ietf:params:oauth:grant-type:jwt-bearer", false),
            encode(&assertion, false)
        );
        let req = Request::builder()
            .method(Method::POST)
            .uri(&sa.token_uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("content-length", form.len().to_string())
            .body(Full::new(Bytes::from(form)))
            .map_err(|e| StoreError::Other(format!("gcs token: build request: {e}")))?;
        let resp = tokio::time::timeout(self.opts.first_byte_timeout, self.client.request(req))
            .await
            .map_err(|_| StoreError::Other(format!("gcs token: no response from {}", sa.token_uri)))?
            .map_err(|e| StoreError::Other(format!("gcs token: {}: {e}", sa.token_uri)))?;
        let status = resp.status();
        let body = tokio::time::timeout(self.opts.body_stall_timeout, resp.into_body().collect())
            .await
            .map_err(|_| StoreError::Other("gcs token: body stalled".into()))?
            .map_err(|e| StoreError::Other(format!("gcs token: body: {e}")))?
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if !status.is_success() {
            let why = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("");
            return Err(StoreError::Auth(format!("gcs token: {} for {}: {why}", status.as_u16(), sa.email)));
        }
        let token = v["access_token"]
            .as_str()
            .ok_or_else(|| StoreError::Auth(format!("gcs token: no access_token for {}", sa.email)))?;
        let ttl = Duration::from_secs(v["expires_in"].as_u64().unwrap_or(3600));
        Ok((token.to_string(), ttl))
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
        let mut uri = format!("{}{}", self.origin, req.path);
        if !req.query.is_empty() {
            uri.push('?');
            let q: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={}", encode(v, false))).collect();
            uri.push_str(&q.join("&"));
        }
        let mut b = Request::builder().method(req.method.clone()).uri(uri);
        for (k, v) in &req.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        if let Some(a) = self.authorization().await.map_err(Failure::Final)? {
            b = b.header("authorization", a);
        }
        if !req.body.is_empty() || matches!(req.method, Method::POST | Method::PUT) {
            b = b.header("content-length", req.body.len().to_string());
        }
        let http_req = b
            .body(Full::new(req.body.clone()))
            .map_err(|e| Failure::Final(StoreError::Other(format!("{}: build request: {e}", req.ctx))))?;
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
        if status.is_success() {
            return Ok(Resp { headers: parts.headers, body: all.freeze() });
        }
        classify(status.as_u16(), &String::from_utf8_lossy(&all), req.ctx)
    }

    /// Send under `cond`, which `build` turns into `ifGenerationMatch`.
    /// `If-Match` on a content ETag resolves to a generation, and a 412 is
    /// re-checked by the bytes before it is reported.
    async fn send_conditional(
        &self,
        key: &str,
        cond: &PutCondition,
        build: &(dyn Fn(Option<String>) -> Req + Send + Sync),
    ) -> StoreResult<Resp> {
        let want = match cond {
            PutCondition::Unconditional => return self.send(build(None)).await,
            PutCondition::IfNoneMatchAny => return self.send(build(Some("0".into()))).await,
            PutCondition::IfMatch(want) => want,
        };
        let mut fresh = false;
        for _ in 0..3 {
            let generation = self.generation_for(key, want, fresh).await?;
            match self.send(build(Some(generation))).await {
                // Ours moved under us, or the remembered generation was
                // stale: ask the bytes.
                Err(StoreError::PreconditionFailed(_)) => fresh = true,
                r => return r,
            }
        }
        Err(StoreError::PreconditionFailed(format!("{key}: still moving after three re-checks")))
    }

    /// The generation behind `want`, a content ETag or an outside writer's.
    async fn generation_for(&self, key: &str, want: &str, fresh: bool) -> StoreResult<String> {
        let want = quoted(want);
        if let Some(g) = foreign_token(key, &want)? {
            return Ok(g);
        }
        if !fresh {
            if let Some(s) = self.seen.lock().unwrap().get(key) {
                if s.meta.etag == want {
                    return Ok(s.generation.clone());
                }
            }
        }
        let s = self.head_seen(key).await?;
        if s.meta.etag != want {
            return Err(StoreError::PreconditionFailed(format!("{key}: is {} not {want}", s.meta.etag)));
        }
        Ok(s.generation)
    }

    fn remember(&self, key: &str, s: &Seen) {
        let mut m = self.seen.lock().unwrap();
        if m.len() >= TOKENS_MAX {
            m.clear();
        }
        m.insert(key.to_string(), s.clone());
    }

    fn forget(&self, key: &str) {
        self.seen.lock().unwrap().remove(key);
    }

    async fn delete_generation(&self, key: &str, generation: &str) -> StoreResult<()> {
        self.forget(key);
        let req = Req::new("delete_object", RequestKind::Delete, Method::DELETE, self.object_path(key))
            .generation(Some(generation.to_string()));
        self.send(req).await.map(|_| ())
    }

    /// The object's metadata, optionally only while it is `generation`.
    async fn metadata(&self, key: &str, generation: Option<&str>) -> StoreResult<Seen> {
        let req = Req::new("get_object", RequestKind::Head, Method::GET, self.object_path(key))
            .generation(generation.map(str::to_string));
        let resp = self.send(req).await?;
        let seen = seen_of(key, &parse_json(&resp.body, "get_object")?)?;
        self.remember(key, &seen);
        Ok(seen)
    }

    async fn head_seen(&self, key: &str) -> StoreResult<Seen> {
        self.metadata(key, None).await
    }

    /// The bytes (or a range of them), optionally only while the object is
    /// `generation`. Returns the generation and metageneration served.
    async fn media(&self, key: &str, range: Option<(u64, u64)>, generation: Option<String>) -> StoreResult<(String, String, Bytes)> {
        let mut req = Req::new("get_media", RequestKind::Get, Method::GET, self.object_path(key))
            .query("alt", "media")
            .generation(generation);
        if let Some((off, len)) = range {
            req = req.header("range", format!("bytes={off}-{}", off + len - 1));
        }
        let resp = self.send(req).await?;
        let g = header(&resp.headers, "x-goog-generation")
            .ok_or_else(|| StoreError::Other(format!("{key}: no x-goog-generation in the response")))?
            .to_string();
        let mg = header(&resp.headers, "x-goog-metageneration").unwrap_or("1").to_string();
        Ok((g, mg, resp.body))
    }

    /// The metadata of exactly the version a media GET served: from what
    /// the store remembers when it is that generation and metageneration,
    /// or one GET guarded on the generation. `None`: it moved on between.
    async fn metadata_of(&self, key: &str, generation: &str, metageneration: &str) -> StoreResult<Option<Seen>> {
        if let Some(s) = self.seen.lock().unwrap().get(key) {
            if s.generation == generation && s.metageneration == metageneration {
                return Ok(Some(s.clone()));
            }
        }
        match self.metadata(key, Some(generation)).await {
            Ok(s) => Ok(Some(s)),
            Err(StoreError::PreconditionFailed(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// GET the whole object, checked against `if_match`: body and metadata
    /// come from one version.
    async fn get(&self, key: &str, if_match: Option<&str>) -> StoreResult<(ObjectMeta, Bytes)> {
        let mut fresh = false;
        for _ in 0..3 {
            let guard = match if_match {
                Some(want) => Some(self.generation_for(key, want, fresh).await?),
                None => None,
            };
            let (g, mg, body) = match self.media(key, None, guard).await {
                Err(StoreError::PreconditionFailed(_)) if if_match.is_some() => {
                    fresh = true;
                    continue;
                }
                r => r?,
            };
            let Some(s) = self.metadata_of(key, &g, &mg).await? else { continue };
            if let Some(want) = if_match {
                if s.meta.etag != quoted(want) {
                    return Err(StoreError::PreconditionFailed(format!("{key}: is {} not {}", s.meta.etag, quoted(want))));
                }
            }
            return Ok((s.meta, body));
        }
        Err(StoreError::PreconditionFailed(format!("{key}: still moving after three reads")))
    }

    /// One object upload (multipart: the resource, then the bytes), its
    /// CRC32C checked by the service.
    fn upload_req(&self, key: &str, body: &Bytes, metadata: &Value, generation: Option<String>) -> Req {
        let resource = json!({
            "name": key,
            "contentType": "application/octet-stream",
            "crc32c": crc32c_b64(crc32c(0, body)),
            "metadata": metadata,
        });
        let boundary = format!("flint-{}", uuid::Uuid::new_v4().simple());
        let mut b = BytesMut::with_capacity(body.len() + 512);
        b.put_slice(format!("--{boundary}\r\ncontent-type: application/json; charset=UTF-8\r\n\r\n{resource}\r\n").as_bytes());
        b.put_slice(format!("--{boundary}\r\ncontent-type: application/octet-stream\r\n\r\n").as_bytes());
        b.put_slice(body);
        b.put_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let mut r = Req::new("upload", RequestKind::Put, Method::POST, self.upload_path())
            .query("uploadType", "multipart")
            .header("content-type", format!("multipart/related; boundary={boundary}"))
            .generation(generation);
        r.body = b.freeze();
        r
    }

    /// The compose that lands `dst`, from `sources` (name, generation).
    fn compose_req(&self, dst: &str, sources: &[(String, String)], metadata: &Value, generation: Option<String>, kind: RequestKind) -> Req {
        let src: Vec<Value> = sources.iter().map(|(n, g)| json!({ "name": n, "generation": g })).collect();
        let body = json!({
            "sourceObjects": src,
            "destination": { "contentType": "application/octet-stream", "metadata": metadata },
        });
        Req::new("compose", kind, Method::POST, format!("{}/compose", self.object_path(dst))).json(&body).generation(generation)
    }

    /// A part, create-only. Its generation pins it in the compose.
    async fn put_part(&self, key: &str, body: Bytes) -> StoreResult<String> {
        let resp = self.send(self.upload_req(key, &body, &json!({}), Some("0".into()))).await?;
        Ok(seen_of(key, &parse_json(&resp.body, "upload")?)?.generation)
    }

    /// Best effort: whatever is left, the upload sweep finds by age.
    async fn delete_all(&self, keys: Vec<String>) {
        futures::stream::iter(keys)
            .map(|k| async move {
                let _ = self.delete(&k).await;
            })
            .buffer_unordered(DELETE_PARALLELISM)
            .collect::<Vec<()>>()
            .await;
    }

    /// Fold `sources` to at most 32 with intermediate composes under `dir`,
    /// pushing each into `made` for the cleanup.
    async fn fold(&self, dir: &str, mut sources: Vec<(String, String)>, made: &mut Vec<String>) -> StoreResult<Vec<(String, String)>> {
        let mut level = 0;
        while sources.len() > COMPOSE_MAX {
            let mut next = Vec::with_capacity(sources.len().div_ceil(COMPOSE_MAX));
            for (j, group) in sources.chunks(COMPOSE_MAX).enumerate() {
                let name = format!("{dir}c{level}-{j:04}");
                made.push(name.clone());
                let req = self.compose_req(&name, group, &json!({}), Some("0".into()), RequestKind::Multipart);
                let resp = self.send(req).await?;
                next.push((name.clone(), seen_of(&name, &parse_json(&resp.body, "compose")?)?.generation));
            }
            sources = next;
            level += 1;
        }
        Ok(sources)
    }

    /// Upload the `Local` parts of `spec` under `dir`, in order of the
    /// object, `part_parallelism` at a time, hashing each as it is read.
    async fn upload_parts(
        &self,
        spec: &ComposeSpec<'_>,
        dir: &str,
        started: &mut Vec<String>,
        sums: &mut Sums,
    ) -> StoreResult<Vec<(String, String)>> {
        let file = spec
            .local
            .as_ref()
            .ok_or_else(|| StoreError::Other(format!("{}: a Local part with no descriptor", spec.key)))?;
        let mut done: Vec<Option<String>> = vec![None; spec.parts.len()];
        let mut inflight = FuturesUnordered::new();
        let mut failed: Option<StoreError> = None;
        for (i, part) in spec.parts.iter().enumerate() {
            let (offset, len) = match part {
                PartSource::Local { offset, len } => (*offset, *len),
                PartSource::BaseCopy { .. } => unreachable!("refused before any part moves"),
            };
            let permit = match &self.upload_gate {
                Some(g) => Some(g.acquire(len).await),
                None => None,
            };
            let bytes = match read_local(file, offset, len).await {
                Ok(b) => b,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            };
            sums.update(&bytes);
            let name = format!("{dir}{i:05}");
            started.push(name.clone());
            inflight.push(async move {
                let r = self.put_part(&name, bytes).await;
                drop(permit);
                (i, len, r)
            });
            while inflight.len() >= self.part_parallelism {
                let (i, len, r) = inflight.next().await.expect("one in flight");
                match r {
                    Ok(g) => {
                        done[i] = Some(g);
                        spec.note_progress(len);
                    }
                    Err(e) => {
                        failed.get_or_insert(e);
                    }
                }
            }
            if failed.is_some() {
                break;
            }
        }
        // Every upload started is waited for, so the cleanup that follows a
        // failure sees every part that landed.
        while let Some((i, len, r)) = inflight.next().await {
            match r {
                Ok(g) => {
                    done[i] = Some(g);
                    spec.note_progress(len);
                }
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(started.iter().cloned().zip(done.into_iter().map(|g| g.expect("every part landed"))).collect())
    }
}

/// The three sums a compose takes in its one pass over the parts.
struct Sums {
    crc64: Crc64Nvme,
    sha: Sha256,
    crc32c: u32,
    len: u64,
}

impl Sums {
    fn update(&mut self, b: &[u8]) {
        self.crc64.update(b);
        self.sha.update(b);
        self.crc32c = crc32c(self.crc32c, b);
        self.len += b.len() as u64;
    }
}

/// Map a GCS error. `Retry` for what the next attempt may not meet.
fn classify(status: u16, body: &str, ctx: &str) -> Result<Resp, Failure> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let reason = v["error"]["errors"][0]["reason"].as_str().unwrap_or("");
    let message: String = v["error"]["message"].as_str().unwrap_or(body).chars().take(240).collect();
    let msg = format!("{ctx}: {status} {reason} {message}");
    if matches!(status, 408 | 429 | 500 | 502 | 503 | 504) {
        return Err(Failure::Retry(msg));
    }
    // The service answers 400 to a CRC32C that disagrees with the bytes,
    // the emulator 412: either way it is the bytes, not a condition.
    if matches!(status, 400 | 412) && body.to_ascii_lowercase().contains("crc32c") {
        return Err(Failure::Final(StoreError::ChecksumMismatch(msg)));
    }
    Err(Failure::Final(match status {
        412 => StoreError::PreconditionFailed(msg),
        409 => StoreError::Conflict(msg),
        404 => StoreError::NotFound(msg),
        401 | 403 => StoreError::Auth(msg),
        _ => StoreError::Other(msg),
    }))
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn parse_json(body: &[u8], ctx: &str) -> StoreResult<Value> {
    serde_json::from_slice(body).map_err(|e| StoreError::Other(format!("{ctx}: the response is not JSON: {e}")))
}

/// A number the JSON API sends as a string (int64), or as a number.
fn num_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The stamps, the hash and the CRC, as one metadata object.
fn metadata_of_write(stamps: &GenerationStamps, hash: &str, crc_b64: &str) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in stamps.to_meta() {
        m.insert(k, Value::String(v));
    }
    m.insert(META_HASH.into(), Value::String(hash.into()));
    m.insert(META_CRC.into(), Value::String(crc_b64.into()));
    Value::Object(m)
}

/// An object resource, as the JSON API returns it.
fn seen_of(key: &str, v: &Value) -> StoreResult<Seen> {
    let generation =
        num_str(&v["generation"]).ok_or_else(|| StoreError::Other(format!("{key}: no generation in the response")))?;
    let name = v["name"].as_str().unwrap_or(key);
    let raw: HashMap<String, String> = v["metadata"]
        .as_object()
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let meta: HashMap<String, String> =
        raw.iter().filter(|(k, _)| k.as_str() != META_HASH && k.as_str() != META_CRC).map(|(k, v)| (k.clone(), v.clone())).collect();
    let time = v["timeCreated"].as_str().or(v["updated"].as_str());
    Ok(Seen {
        meta: ObjectMeta {
            etag: presented(name, raw.get(META_HASH).map(String::as_str), &generation),
            size: num_str(&v["size"]).and_then(|s| s.parse().ok()).unwrap_or(0),
            crc64_b64: raw.get(META_CRC).cloned(),
            meta,
            last_modified_unix: time.and_then(unix_of_rfc3339),
            storage_class: v["storageClass"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
            version_id: None,
        },
        generation,
        metageneration: num_str(&v["metageneration"]).unwrap_or_else(|| "1".into()),
        crc32c: v["crc32c"].as_str().map(str::to_string),
    })
}

/// `2026-11-10T22:15:13.258656Z` (or with an offset) as Unix seconds.
fn unix_of_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let tz_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (clock, tz) = rest.split_at(tz_at);
    let clock = clock.split('.').next()?;
    let mut c = clock.splitn(3, ':').map(|x| x.parse::<i64>());
    let (hh, mm, ss) = (c.next()?.ok()?, c.next()?.ok()?, c.next()?.ok()?);
    let offset = match tz.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(sign) => {
            let (oh, om) = tz[1..].split_once(':')?;
            let o = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60;
            if *sign == b'-' {
                -o
            } else {
                o
            }
        }
    };
    // Days from the civil date (H. Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let t = days * 86400 + hh * 3600 + mm * 60 + ss - offset;
    u64::try_from(t).ok()
}

// ── service-account tokens ───────────────────────────────────────────

fn service_account(json_key: &str) -> StoreResult<ServiceAccount> {
    let v: Value =
        serde_json::from_str(json_key).map_err(|e| StoreError::Other(format!("gcs: the service account key is not JSON: {e}")))?;
    let field = |k: &str| {
        v[k].as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| StoreError::Other(format!("gcs: the service account key has no {k}")))
    };
    if v["type"].as_str() != Some("service_account") {
        return Err(StoreError::Other(format!(
            "gcs: the key's type is {:?}, not service_account",
            v["type"].as_str().unwrap_or("")
        )));
    }
    let der = pem_body(field("private_key")?)
        .ok_or_else(|| StoreError::Other("gcs: the service account's private_key is not a PEM private key".into()))?;
    let key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&rustls::pki_types::PrivateKeyDer::Pkcs8(der.into()))
        .map_err(|e| StoreError::Other(format!("gcs: the service account's private_key: {e}")))?;
    Ok(ServiceAccount {
        email: field("client_email")?.to_string(),
        key,
        token_uri: v["token_uri"].as_str().filter(|s| !s.is_empty()).unwrap_or(TOKEN_URI).to_string(),
        cached: tokio::sync::Mutex::new(None),
    })
}

/// The DER inside `-----BEGIN PRIVATE KEY-----` (PKCS #8, as Google
/// issues it).
fn pem_body(pem: &str) -> Option<Vec<u8>> {
    let start = pem.find("-----BEGIN PRIVATE KEY-----")? + "-----BEGIN PRIVATE KEY-----".len();
    let end = pem[start..].find("-----END PRIVATE KEY-----")? + start;
    let b64: String = pem[start..end].chars().filter(|c| !c.is_whitespace()).collect();
    base64_decode(&b64).filter(|d| !d.is_empty())
}

fn b64url(b: &[u8]) -> String {
    base64_std(b).trim_end_matches('=').replace('+', "-").replace('/', "_")
}

/// The signed JWT the token endpoint takes: RS256, an hour long.
fn jwt(sa: &ServiceAccount, now: u64) -> StoreResult<String> {
    let header = json!({ "alg": "RS256", "typ": "JWT" });
    let claims = json!({
        "iss": sa.email,
        "scope": SCOPE,
        "aud": sa.token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    let signing_input = format!("{}.{}", b64url(header.to_string().as_bytes()), b64url(claims.to_string().as_bytes()));
    let signer = sa
        .key
        .choose_scheme(&[rustls::SignatureScheme::RSA_PKCS1_SHA256])
        .ok_or_else(|| StoreError::Other("gcs: the service account's key cannot sign RS256".into()))?;
    let sig = signer.sign(signing_input.as_bytes()).map_err(|e| StoreError::Other(format!("gcs: signing the JWT: {e}")))?;
    Ok(format!("{signing_input}.{}", b64url(&sig)))
}

// ── CRC32C ───────────────────────────────────────────────────────────

/// CRC-32C (Castagnoli) of `data`, continuing from `crc`, the CRC of what
/// came before (0 to start): the checksum GCS keeps of every object.
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: the CPU has SSE4.2, checked on the line above.
        return !unsafe { crc32c_sse42(!crc, data) };
    }
    !crc32c_table(!crc, data)
}

fn crc32c_tables8() -> &'static [[u32; 256]; 8] {
    static T: std::sync::OnceLock<[[u32; 256]; 8]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            }
            t[0][i as usize] = c;
        }
        for i in 0..256 {
            for k in 1..8 {
                let prev = t[k - 1][i];
                t[k][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            }
        }
        t
    })
}

/// Slice-by-8 over the inverted register.
fn crc32c_table(mut c: u32, data: &[u8]) -> u32 {
    let t = crc32c_tables8();
    let (chunks, rest) = data.as_chunks::<8>();
    for ch in chunks {
        let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
        let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][((hi >> 8) & 0xff) as usize]
            ^ t[1][((hi >> 16) & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in rest {
        c = (c >> 8) ^ t[0][((c ^ b as u32) & 0xff) as usize];
    }
    c
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(c: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    let mut c64 = c as u64;
    let (chunks, rest) = data.as_chunks::<8>();
    for ch in chunks {
        c64 = _mm_crc32_u64(c64, u64::from_le_bytes(*ch));
    }
    let mut c = c64 as u32;
    for &b in rest {
        c = _mm_crc32_u8(c, b);
    }
    c
}

/// The wire form: four bytes, big-endian, base64.
fn crc32c_b64(crc: u32) -> String {
    base64_std(&crc.to_be_bytes())
}

#[async_trait]
impl ObjectStore for GcsStore {
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        condition: &PutCondition,
        stamps: &GenerationStamps,
        crc64: u64,
    ) -> StoreResult<ObjectMeta> {
        // The caller's CRC is a claim about these bytes: check it here,
        // before anything leaves (GCS keeps a CRC32C, not this one).
        let actual = crc64_nvme(&body);
        if actual != crc64 {
            return Err(StoreError::ChecksumMismatch(format!(
                "{key}: the bytes' CRC-64 is {} and the caller said {}",
                crc64_to_b64(actual),
                crc64_to_b64(crc64)
            )));
        }
        let hash = content_hash(&Sha256::digest(&body));
        let metadata = metadata_of_write(stamps, &hash, &crc64_to_b64(crc64));
        let build = |g: Option<String>| self.upload_req(key, &body, &metadata, g);
        let resp = self.send_conditional(key, condition, &build).await?;
        let seen = seen_of(key, &parse_json(&resp.body, "upload")?)?;
        self.remember(key, &seen);
        Ok(seen.meta)
    }

    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> StoreResult<ObjectMeta> {
        let mut fresh_dst = false;
        for _ in 0..3 {
            let src = self.head_seen(src_key).await?;
            if let Some(want) = src_if_match {
                if src.meta.etag != quoted(want) {
                    return Err(StoreError::PreconditionFailed(format!("{src_key}: is {} not {}", src.meta.etag, quoted(want))));
                }
            }
            // The copy's hash and CRC are the source's (C6). An outside
            // writer's object carries neither: read it once, guarded on
            // the generation just seen.
            let (hash, crc_b64) = match (src.meta.etag.trim_matches('"').starts_with("x-"), &src.meta.crc64_b64) {
                (false, Some(c)) => (src.meta.etag.trim_matches('"').to_string(), c.clone()),
                _ => {
                    let (_, _, body) = self.media(src_key, None, Some(src.generation.clone())).await?;
                    (content_hash(&Sha256::digest(&body)), crc64_to_b64(crc64_nvme(&body)))
                }
            };
            let metadata = metadata_of_write(stamps, &hash, &crc_b64);
            let generation = match condition {
                PutCondition::Unconditional => None,
                PutCondition::IfNoneMatchAny => Some("0".to_string()),
                PutCondition::IfMatch(want) => Some(self.generation_for(dst_key, want, fresh_dst).await?),
            };
            let sources = [(src_key.to_string(), src.generation.clone())];
            let req = self.compose_req(dst_key, &sources, &metadata, generation, RequestKind::Copy);
            match self.send(req).await {
                Ok(resp) => {
                    let seen = seen_of(dst_key, &parse_json(&resp.body, "compose")?)?;
                    self.remember(dst_key, &seen);
                    return Ok(seen.meta);
                }
                // The source moved (its pinned generation is gone), or the
                // destination's condition failed. Ask both again; a
                // create-only destination that exists is final.
                Err(StoreError::NotFound(_)) => continue,
                Err(StoreError::PreconditionFailed(m)) => match condition {
                    PutCondition::IfMatch(_) => fresh_dst = true,
                    _ => return Err(StoreError::PreconditionFailed(m)),
                },
                Err(e) => return Err(e),
            }
        }
        Err(StoreError::PreconditionFailed(format!("{src_key} -> {dst_key}: still moving after three re-checks")))
    }

    async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta> {
        if spec.parts.iter().any(|p| matches!(p, PartSource::BaseCopy { .. })) {
            return Err(StoreError::Other(format!(
                "{}: the gcs backend does not copy byte ranges yet (PartSource::BaseCopy)",
                spec.key
            )));
        }
        if spec.parts.len() > MAX_PARTS {
            return Err(StoreError::Other(format!("{}: {} parts, this store takes {MAX_PARTS}", spec.key, spec.parts.len())));
        }
        if spec.parts.is_empty() {
            // A compose takes at least one source: an empty object is a put.
            let empty = crc64_nvme(&[]);
            if spec.crc64.is_some_and(|c| c != empty) {
                return Err(StoreError::ChecksumMismatch(format!("{}: no parts, and the caller's CRC-64 is not the empty one", spec.key)));
            }
            return self.put_whole(spec.key, Bytes::new(), &spec.condition, &spec.stamps, empty).await;
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
        let dir = format!("{}{PARTS_DIR}{nonce}/", self.scratch_root);
        let mut made = Vec::new();
        let mut sums = Sums { crc64: Crc64Nvme::new(), sha: Sha256::new(), crc32c: 0, len: 0 };
        let landed = async {
            let parts = self.upload_parts(spec, &dir, &mut made, &mut sums).await?;
            let crc_v = sums.crc64.clone().finalize();
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
            let sources = self.fold(&dir, parts, &mut made).await?;
            let hash = content_hash(&sums.sha.clone().finalize());
            let metadata = metadata_of_write(&spec.stamps, &hash, &crc64_to_b64(crc_v));
            let build = |g: Option<String>| self.compose_req(spec.key, &sources, &metadata, g, RequestKind::Multipart);
            let resp = self.send_conditional(spec.key, &spec.condition, &build).await?;
            seen_of(spec.key, &parse_json(&resp.body, "compose")?)
        }
        .await;
        // The parts go whatever happened: a refused compose leaves nothing
        // behind (A9).
        self.delete_all(made).await;
        let seen = landed?;
        // The service's CRC32C of what it assembled, against the one taken
        // over the bytes as they were read.
        let want = crc32c_b64(sums.crc32c);
        let got = seen.crc32c.clone();
        if got.as_deref() != Some(want.as_str()) || seen.meta.size != sums.len {
            let _ = self.delete_generation(spec.key, &seen.generation).await;
            return Err(StoreError::ChecksumMismatch(format!(
                "{}: composed {} bytes with CRC32C {got:?}, the parts were {} bytes with {want}",
                spec.key, seen.meta.size, sums.len
            )));
        }
        self.remember(spec.key, &seen);
        Ok(seen.meta)
    }

    async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
        Ok(self.head_seen(key).await?.meta)
    }

    async fn get_whole(&self, key: &str, if_match: Option<&str>) -> StoreResult<(ObjectMeta, Bytes)> {
        self.get(key, if_match).await
    }

    /// Every range is guarded on the generation behind the content ETag.
    /// The ETag is the bytes' hash, so ranges taken from two generations
    /// that carry it are ranges of one set of bytes.
    async fn get_range(&self, key: &str, offset: u64, len: u64, if_match: &str) -> StoreResult<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let mut fresh = false;
        for _ in 0..3 {
            let g = self.generation_for(key, if_match, fresh).await?;
            match self.media(key, Some((offset, len)), Some(g)).await {
                Err(StoreError::PreconditionFailed(_)) => fresh = true,
                r => return r.map(|(_, _, b)| b),
            }
        }
        Err(StoreError::PreconditionFailed(format!("{key}: still moving after three re-checks")))
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>> {
        let mut out = vec![];
        let mut page: Option<String> = None;
        loop {
            let mut req = Req::new("list_objects", RequestKind::List, Method::GET, self.objects_path()).query("prefix", prefix);
            if let Some(t) = &page {
                req = req.query("pageToken", t.clone());
            }
            let resp = self.send(req).await?;
            let v = parse_json(&resp.body, "list_objects")?;
            for item in v["items"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let Some(key) = item["name"].as_str() else { continue };
                let seen = seen_of(key, item)?;
                self.remember(key, &seen);
                out.push(ListedObject {
                    key: key.to_string(),
                    size: seen.meta.size,
                    etag: seen.meta.etag,
                    last_modified_unix: seen.meta.last_modified_unix,
                });
            }
            match v["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
                Some(t) => page = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        self.forget(key);
        let req = Req::new("delete_object", RequestKind::Delete, Method::DELETE, self.object_path(key));
        match self.send(req).await {
            Ok(_) | Err(StoreError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> StoreResult<()> {
        let path = self.object_path(key);
        let build = |g: Option<String>| Req::new("delete_object", RequestKind::Delete, Method::DELETE, path.clone()).generation(g);
        self.send_conditional(key, &PutCondition::IfMatch(etag.to_string()), &build).await?;
        self.forget(key);
        Ok(())
    }

    /// A compose that never finished leaves its parts: one entry per
    /// parts directory under `prefix`, dated by its oldest part.
    async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>> {
        let root = format!("{prefix}{PARTS_DIR}");
        let mut by_dir: std::collections::BTreeMap<String, Option<u64>> = Default::default();
        for o in self.list(&root).await? {
            let Some((nonce, _)) = o.key[root.len()..].split_once('/') else { continue };
            let oldest = by_dir.entry(nonce.to_string()).or_insert(o.last_modified_unix);
            *oldest = match (*oldest, o.last_modified_unix) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        Ok(by_dir
            .into_iter()
            .map(|(nonce, t)| PendingUpload { key: format!("{root}{nonce}/"), upload_id: nonce, initiated_unix: t })
            .collect())
    }

    async fn abort_upload(&self, key: &str, upload_id: &str) -> StoreResult<()> {
        if upload_id.is_empty() || upload_id.contains('/') || !key.ends_with(&format!("{PARTS_DIR}{upload_id}/")) {
            return Err(StoreError::Other(format!("gcs: {key} is not a compose's parts directory")));
        }
        let keys = self.list(key).await?.into_iter().map(|o| o.key).collect();
        self.delete_all(keys).await;
        Ok(())
    }

    async fn bootstrap(&self, prefix: &str) -> StoreResult<BootstrapReport> {
        let mut report = BootstrapReport::default();
        let req = Req::new("get_bucket", RequestKind::Head, Method::GET, format!("/storage/v1/b/{}", encode(&self.bucket, false)));
        match self.send(req).await {
            Err(e) => {
                report.errors.push(format!("bucket {} unreachable: {e}", self.bucket));
                return Ok(report);
            }
            Ok(resp) => {
                let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                let versioning = v["versioning"]["enabled"].as_bool().unwrap_or(false);
                let rules = v["lifecycle"]["rule"].as_array().map_or(0, Vec::len);
                let uniform = v["iamConfiguration"]["uniformBucketLevelAccess"]["enabled"].as_bool().unwrap_or(false);
                report.notes.push(format!(
                    "bucket {}: versioning {}, {rules} lifecycle rule(s), uniform bucket-level access {}",
                    self.bucket,
                    if versioning { "on" } else { "off" },
                    if uniform { "on" } else { "off" }
                ));
            }
        }
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
        MAX_PARTS
    }

    fn upload_gate(&self) -> Option<Arc<ByteGate>> {
        self.upload_gate.clone()
    }

    fn request_counts(&self) -> Option<RequestCounts> {
        Some(self.requests.snapshot())
    }

    fn provider(&self) -> Provider {
        Provider::Gcs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_meets_its_check_value_and_both_paths_agree() {
        assert_eq!(crc32c(0, b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(0, b""), 0);
        // Continued across a split, as the compose pass feeds it.
        assert_eq!(crc32c(crc32c(0, b"hello "), b"world"), crc32c(0, b"hello world"));
        // The emulator's CRC32C of "hello world", as it reported it.
        assert_eq!(crc32c_b64(crc32c(0, b"hello world")), "yZRlqg==");
        let data: Vec<u8> = (0..100_003u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for cut in [0, 1, 7, 8, 9, 4096, 100_003] {
            let whole = !crc32c_table(!0, &data[..cut]);
            assert_eq!(crc32c(0, &data[..cut]), whole, "cut {cut}");
        }
    }

    #[test]
    fn rfc3339_times_read_as_unix_seconds() {
        assert_eq!(unix_of_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_of_rfc3339("2026-11-10T22:15:13.258656Z"), Some(1_794_348_913));
        assert_eq!(unix_of_rfc3339("2026-11-10T23:15:13+01:00"), Some(1_794_348_913));
        assert_eq!(unix_of_rfc3339("2026-11-10T21:15:13.5-01:00"), Some(1_794_348_913));
        assert_eq!(unix_of_rfc3339("yesterday"), None);
    }

    #[test]
    fn errors_map_as_the_contract_needs() {
        let f = |s, body: &str| match classify(s, body, "t") {
            Err(Failure::Final(e)) => Some(e),
            _ => None,
        };
        let pre = r#"{"error":{"code":412,"message":"At least one of the pre-conditions you specified did not hold.","errors":[{"reason":"conditionNotMet"}]}}"#;
        assert!(matches!(f(412, pre), Some(StoreError::PreconditionFailed(_))));
        assert!(matches!(f(412, "ifGenerationMatch validation failed. Expected = 0"), Some(StoreError::PreconditionFailed(_))));
        // The service's 400 and the emulator's 412 for bytes that are not
        // what the CRC32C says.
        assert!(matches!(f(400, r#"{"error":{"message":"Provided CRC32C \"AAAAAA==\" doesn't match calculated CRC32C"}}"#), Some(StoreError::ChecksumMismatch(_))));
        assert!(matches!(f(412, "crc32c validation failed. Expected = 1 vs Actual = 2"), Some(StoreError::ChecksumMismatch(_))));
        assert!(matches!(f(404, "{}"), Some(StoreError::NotFound(_))));
        assert!(matches!(f(409, "{}"), Some(StoreError::Conflict(_))));
        assert!(matches!(f(401, "{}"), Some(StoreError::Auth(_))));
        assert!(matches!(f(403, "{}"), Some(StoreError::Auth(_))));
        assert!(f(429, "{}").is_none() && f(503, "{}").is_none(), "throttling and outages retry");
    }

    #[test]
    fn a_resource_reads_with_its_content_etag_and_hides_the_internal_metadata() {
        let stamps = GenerationStamps { generation: 3, epoch: 2, flush_uuid: "f".into(), boundary_source: Some("cadence".into()), posix: None };
        let resource = json!({
            "name": "p/a", "generation": "1794348872532", "metageneration": "1", "size": "6",
            "crc32c": "ev2UKA==", "timeCreated": "2026-11-10T22:15:13.258656Z", "storageClass": "STANDARD",
            "metadata": metadata_of_write(&stamps, "ab12", "AAAAAAAAAAA="),
        });
        let s = seen_of("p/a", &resource).unwrap();
        assert_eq!(s.meta.etag, "\"ab12\"");
        assert_eq!((s.meta.size, s.generation.as_str(), s.crc32c.as_deref()), (6, "1794348872532", Some("ev2UKA==")));
        assert_eq!(s.meta.crc64_b64.as_deref(), Some("AAAAAAAAAAA="));
        assert_eq!(s.meta.last_modified_unix, Some(1_794_348_913));
        assert!(s.meta.copy_source_allowed());
        assert_eq!(GenerationStamps::from_meta(&s.meta.meta), Some(stamps));
        assert!(!s.meta.meta.contains_key(META_HASH) && !s.meta.meta.contains_key(META_CRC), "{:?}", s.meta.meta);
        // An outside writer's object: no hash, so its ETag names the
        // generation, for this key only.
        let foreign = seen_of("p/b", &json!({ "name": "p/b", "generation": 7, "size": "1" })).unwrap();
        assert_eq!(foreign_token("p/b", &foreign.meta.etag).unwrap(), Some("7".into()));
    }

    #[test]
    fn a_key_that_is_not_a_service_accounts_is_refused() {
        let refusal = |j: &str| match service_account(j) {
            Err(StoreError::Other(m)) => m,
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("accepted {j}"),
        };
        assert!(refusal("not json").contains("not JSON"));
        assert!(refusal(r#"{"type":"authorized_user"}"#).contains("authorized_user"));
        assert!(refusal(r#"{"type":"service_account","client_email":"a@b","private_key":"nope"}"#).contains("PEM"));
        // Built, not written out, so no scanner takes a fixture for a key.
        let pem = |body: &str| format!("-----BEGIN {k}-----\n{body}\n-----END {k}-----\n", k = "PRIVATE KEY");
        let garbage = pem("AAAA");
        let j = json!({ "type": "service_account", "client_email": "a@b", "private_key": garbage }).to_string();
        assert!(refusal(&j).contains("private_key"));
        assert_eq!(pem_body(&format!("x{}", pem("AQID\nBA=="))), Some(vec![1, 2, 3, 4]));
    }

    /// Signs with SHA-256 alone, so the test can see what was signed. A
    /// real key's RS256 is checked only against the service (D4).
    #[derive(Debug)]
    struct DigestKey;

    #[derive(Debug)]
    struct DigestSigner;

    impl rustls::sign::SigningKey for DigestKey {
        fn choose_scheme(&self, offered: &[rustls::SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
            offered
                .contains(&rustls::SignatureScheme::RSA_PKCS1_SHA256)
                .then(|| Box::new(DigestSigner) as Box<dyn rustls::sign::Signer>)
        }
        fn algorithm(&self) -> rustls::SignatureAlgorithm {
            rustls::SignatureAlgorithm::RSA
        }
    }

    impl rustls::sign::Signer for DigestSigner {
        fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
            Ok(Sha256::digest(message).to_vec())
        }
        fn scheme(&self) -> rustls::SignatureScheme {
            rustls::SignatureScheme::RSA_PKCS1_SHA256
        }
    }

    fn digest_account(token_uri: &str) -> ServiceAccount {
        ServiceAccount {
            email: "lean@p.iam.gserviceaccount.com".into(),
            key: Arc::new(DigestKey),
            token_uri: token_uri.into(),
            cached: tokio::sync::Mutex::new(None),
        }
    }

    fn b64url_decode(s: &str) -> Vec<u8> {
        base64_decode(&s.replace('-', "+").replace('_', "/")).unwrap()
    }

    #[test]
    fn the_jwt_is_rs256_over_its_header_and_claims() {
        let sa = digest_account(TOKEN_URI);
        let t = jwt(&sa, 1_000).unwrap();
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3, "{t}");
        assert!(!t.contains('=') && !t.contains('+') && !t.contains('/'), "base64url without padding: {t}");
        let header: Value = serde_json::from_slice(&b64url_decode(parts[0])).unwrap();
        assert_eq!(header, json!({ "alg": "RS256", "typ": "JWT" }));
        let claims: Value = serde_json::from_slice(&b64url_decode(parts[1])).unwrap();
        assert_eq!(claims["iss"], "lean@p.iam.gserviceaccount.com");
        assert_eq!((claims["aud"].as_str(), claims["scope"].as_str()), (Some(TOKEN_URI), Some(SCOPE)));
        assert_eq!((claims["iat"].as_u64(), claims["exp"].as_u64()), (Some(1_000), Some(4_600)));
        let signed = format!("{}.{}", parts[0], parts[1]);
        assert_eq!(b64url_decode(parts[2]), Sha256::digest(signed.as_bytes()).to_vec(), "the signature covers header.claims");
    }

    /// The grant goes to the key's token URI as a form, and its token is
    /// kept until a minute before it expires.
    #[tokio::test]
    async fn a_service_account_trades_one_jwt_for_a_token_and_keeps_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let grants = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_body = Arc::new(Mutex::new(String::new()));
        let (g, b) = (grants.clone(), seen_body.clone());
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 64 * 1024];
                let mut n = 0;
                loop {
                    n += sock.read(&mut buf[n..]).await.unwrap();
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        if body.len() >= len {
                            *b.lock().unwrap() = format!("{}\n{body}", head.lines().next().unwrap());
                            break;
                        }
                    }
                }
                g.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let reply = r#"{"access_token":"granted-token","expires_in":3600,"token_type":"Bearer"}"#;
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len());
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        let uri = format!("http://{addr}/token");
        let store = GcsStore::build(Auth::ServiceAccount(digest_account(&uri)), "b".into(), None).unwrap();
        assert_eq!(store.authorization().await.unwrap().as_deref(), Some("Bearer granted-token"));
        assert_eq!(store.authorization().await.unwrap().as_deref(), Some("Bearer granted-token"));
        assert_eq!(grants.load(std::sync::atomic::Ordering::SeqCst), 1, "the token was not kept");
        let body = seen_body.lock().unwrap().clone();
        assert!(body.starts_with("POST /token "), "{body}");
        assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion="), "{body}");
    }

    #[tokio::test]
    async fn a_refused_grant_is_an_auth_failure() {
        // The control for the test above: what the endpoint refuses comes
        // back as Auth, never as a token.
        {
            use tokio::io::AsyncWriteExt;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let reply = r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#;
                let resp = format!("HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len());
                sock.write_all(resp.as_bytes()).await.unwrap();
            });
            let store = GcsStore::build(Auth::ServiceAccount(digest_account(&format!("http://{addr}/token"))), "b".into(), None).unwrap();
            match store.authorization().await {
                Err(StoreError::Auth(m)) => assert!(m.contains("Invalid JWT Signature"), "{m}"),
                other => panic!("{other:?}"),
            }
        }
    }
}
