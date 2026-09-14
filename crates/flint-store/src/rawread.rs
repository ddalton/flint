//! The raw read path: `GET` (whole, ranged, by version) and `HEAD` over
//! pooled HTTP/1.1 connections, SigV4-signed with the SDK's own
//! credentials, and nothing else in between. Writes stay on the SDK.
//!
//! Why it exists. On the 2026-09-12 loopback rig the SDK's per-request
//! machinery cost ~165 µs of CPU per 8 KiB object against ~50 µs for a
//! bare keep-alive client, and on the cluster the syncer's 350-520 µs
//! per file was the whole client cost once the fan-out was driven from
//! several threads. The cost is diffuse — interceptors, the config bag,
//! runtime components, `Arc` traffic, allocation — and there is no knob
//! for it, so the read side is done by hand here. Reads are the hot
//! path (a checkout is one GET per file); publishes are a fraction of
//! the request count and keep the SDK's checksum and MPU machinery.
//!
//! What it does NOT do. It validates no wire checksum: the manifest's
//! CRC-64, which every reader verifies before a byte becomes visible
//! (`d1539fb9`, `328bbf5d`), is the integrity check on every backend,
//! and it is computed by the client that moved the bytes rather than
//! read from a header. This path returns the same `ObjectMeta` the SDK
//! path does — etag, size, user metadata, version, checksum header when
//! the backend sends one — so nothing above the store can tell them
//! apart except by cost.
//!
//! Parity with the SDK path, by construction: the same timeouts
//! (3 s connect, 10 s to first byte), the same retry budget (three
//! attempts, full-jitter exponential backoff, on 5xx/429/transport
//! errors — S3's `SlowDown` under a hot prefix is exactly this), the
//! same error table (`s3::classify`), the same `x-amz-checksum-mode`
//! on whole and versioned reads, the same virtual-hosted addressing on
//! real S3 and path-style on an endpoint override.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
use http::header::{HeaderName, HeaderValue, HOST};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    sign, PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest,
    SignatureLocation, SigningSettings, UriPathNormalizationMode,
};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;

use crate::{ObjectMeta, StoreError, StoreResult};

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>;

/// Retry budget and timeouts. Public so a rig can shorten them; the
/// defaults are the SDK path's.
#[derive(Debug, Clone)]
pub struct RawReadOptions {
    pub connect_timeout: Duration,
    /// Time to FIRST BYTE of the response, per attempt — the SDK's
    /// `read_timeout`. The body has its own stall bound below.
    pub first_byte_timeout: Duration,
    /// Longest gap tolerated between two body frames.
    pub body_stall_timeout: Duration,
    /// Attempts in total (1 = no retry).
    pub max_attempts: u32,
    /// Backoff base for attempt n is `base * 2^(n-1)`, jittered over
    /// `[0, that]`, capped at `backoff_cap`.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
}

impl Default for RawReadOptions {
    fn default() -> Self {
        RawReadOptions {
            connect_timeout: Duration::from_secs(3),
            first_byte_timeout: Duration::from_secs(10),
            body_stall_timeout: Duration::from_secs(30),
            max_attempts: 3,
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(20),
        }
    }
}

/// The credentials the reader last resolved, and when. The SDK's
/// `SdkConfig::credentials_provider()` is the BARE chain — its caching
/// lives in the SDK's identity layer, which this path bypasses — and
/// on EC2 the chain is the instance metadata service. Asking it per
/// request cost an IMDS round trip per GET on the first cluster run
/// (170-690 files/s against the SDK's 5,000-8,500, three threads and
/// 0.02 s of CPU: the process was WAITING), and twenty thousand calls
/// got IMDS throttled hard enough that the SDK arm's own startup then
/// failed with "dispatch failure" and the chain reported "the
/// credential provider was not enabled". So: resolve once, share,
/// refresh single-flight ahead of expiry.
struct CredCache {
    creds: Option<Credentials>,
    fetched: Instant,
}

impl CredCache {
    /// Refresh when the expiry is within five minutes (but not more
    /// often than every ten seconds, so a short-lived credential does
    /// not turn back into a per-request call), or every fifteen
    /// minutes for a credential that names no expiry.
    fn stale(&self, now: Instant) -> bool {
        let Some(c) = &self.creds else { return true };
        let age = now.saturating_duration_since(self.fetched);
        match c.expiry() {
            Some(exp) => {
                age >= Duration::from_secs(10)
                    && SystemTime::now() + Duration::from_secs(300) >= exp
            }
            None => age >= Duration::from_secs(900),
        }
    }
}

pub struct RawReader {
    client: HttpsClient,
    creds: SharedCredentialsProvider,
    /// Read-locked on the hot path (a few nanoseconds, never held
    /// across an await); `cred_refresh` makes the refresh single-flight.
    cred_cache: std::sync::RwLock<CredCache>,
    cred_refresh: tokio::sync::Mutex<()>,
    /// Calls made to the credential provider. A rig reads it; the
    /// reader's test pins it at one for a burst of reads.
    cred_fetches: AtomicU64,
    region: String,
    /// `https://bucket.s3.region.amazonaws.com` or the endpoint as
    /// given, without a trailing slash.
    origin: String,
    /// Empty for virtual-hosted addressing; `/bucket` for path-style.
    path_prefix: String,
    host: HeaderValue,
    opts: RawReadOptions,
    /// Requests actually sent, retries included. A rig reads it.
    attempts: AtomicU64,
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn other(ctx: &str, e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(format!("{ctx}: {e}"))
}

/// S3's own key encoding: RFC 3986 unreserved characters and `/` pass,
/// everything else is `%XX` — one pass, never re-encoded, which is
/// also what SigV4's canonical URI wants for S3.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 8);
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn encode_query_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `<Code>` of an S3 error document, when the body is one.
fn s3_error_code(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    match (s.find("<Code>"), s.find("</Code>")) {
        (Some(a), Some(b)) if b > a + 6 => s[a + 6..b].to_string(),
        _ => String::new(),
    }
}

/// Cheap jitter without a new dependency: the low bits of a v4 UUID
/// are 122 random bits from the OS.
fn jitter_ms(cap_ms: u64) -> u64 {
    if cap_ms == 0 {
        return 0;
    }
    (uuid::Uuid::new_v4().as_u128() as u64) % (cap_ms + 1)
}

enum Failure {
    /// Try again if the budget allows: transport, timeout, 5xx, 429.
    Retry(String),
    /// Final: a mapped store error.
    Final(StoreError),
}

impl RawReader {
    /// `endpoint`: `None` is real S3 (virtual-hosted,
    /// `https://<bucket>.s3.<region>.amazonaws.com`; a bucket with a
    /// dot in its name takes the path-style regional endpoint, as the
    /// SDK does, because the wildcard certificate cannot cover it).
    /// `Some(url)` is an override (MinIO, Ozone, a rig), always
    /// path-style, `http` or `https` as given.
    pub fn new(
        bucket: &str,
        region: &str,
        endpoint: Option<&str>,
        creds: SharedCredentialsProvider,
        opts: RawReadOptions,
    ) -> StoreResult<Self> {
        let (origin, path_prefix) = match endpoint {
            Some(ep) => (ep.trim_end_matches('/').to_string(), format!("/{bucket}")),
            None if bucket.contains('.') => {
                (format!("https://s3.{region}.amazonaws.com"), format!("/{bucket}"))
            }
            None => (format!("https://{bucket}.s3.{region}.amazonaws.com"), String::new()),
        };
        let uri: http::Uri = origin
            .parse()
            .map_err(|e| StoreError::Other(format!("raw reads: endpoint {origin:?}: {e}")))?;
        let authority = uri
            .authority()
            .ok_or_else(|| StoreError::Other(format!("raw reads: endpoint {origin:?} has no host")))?;
        let host = HeaderValue::from_str(authority.as_str())
            .map_err(|e| StoreError::Other(format!("raw reads: host header: {e}")))?;
        let client = Self::build_client(&opts)?;
        Ok(RawReader {
            client,
            creds,
            cred_cache: std::sync::RwLock::new(CredCache { creds: None, fetched: Instant::now() }),
            cred_refresh: tokio::sync::Mutex::new(()),
            cred_fetches: AtomicU64::new(0),
            region: region.to_string(),
            origin,
            path_prefix,
            host,
            opts,
            attempts: AtomicU64::new(0),
        })
    }

    fn build_client(opts: &RawReadOptions) -> StoreResult<HttpsClient> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(opts.connect_timeout));
        // The SAME crypto provider the SDK's client uses in this
        // process (aws-lc-rs): two providers in one binary is the
        // 1.26/1.27 regression, and rustls refuses to guess.
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(rustls::crypto::aws_lc_rs::default_provider())
            .map_err(|e| StoreError::Other(format!("raw reads: TLS roots: {e}")))?
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Ok(Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(4096)
            .pool_timer(TokioTimer::new())
            .build(https))
    }

    /// Requests sent so far, retries included.
    pub fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::Relaxed)
    }

    /// Credential-provider calls so far.
    pub fn credential_fetches(&self) -> u64 {
        self.cred_fetches.load(Ordering::Relaxed)
    }

    /// The cached credentials, refreshed single-flight when stale: one
    /// caller resolves, the others wait for its answer rather than each
    /// asking the chain.
    async fn credentials(&self) -> StoreResult<Credentials> {
        let now = Instant::now();
        let fresh = |g: &CredCache| if g.stale(now) { None } else { g.creds.clone() };
        if let Some(c) = fresh(&self.cred_cache.read().unwrap_or_else(|p| p.into_inner())) {
            return Ok(c);
        }
        let _flight = self.cred_refresh.lock().await;
        // Someone may have refreshed while we waited for the flight.
        if let Some(c) = fresh(&self.cred_cache.read().unwrap_or_else(|p| p.into_inner())) {
            return Ok(c);
        }
        self.cred_fetches.fetch_add(1, Ordering::Relaxed);
        let c = self
            .creds
            .provide_credentials()
            .await
            .map_err(|e| StoreError::Auth(format!("raw reads: credentials: {e}")))?;
        let mut g = self.cred_cache.write().unwrap_or_else(|p| p.into_inner());
        g.creds = Some(c.clone());
        g.fetched = now;
        Ok(c)
    }

    fn path_and_query(&self, key: &str, version_id: Option<&str>) -> String {
        let mut p = format!("{}/{}", self.path_prefix, encode_key(key));
        if let Some(v) = version_id {
            p.push_str("?versionId=");
            p.push_str(&encode_query_value(v));
        }
        p
    }

    /// One signed request. Signing happens per ATTEMPT: `x-amz-date`
    /// must be fresh, and S3 refuses a signature older than 15 minutes
    /// — a retry after backoff would otherwise carry a stale one.
    async fn signed(
        &self,
        method: &Method,
        path_and_query: &str,
        headers: &[(HeaderName, HeaderValue)],
    ) -> StoreResult<Request<Empty<Bytes>>> {
        let identity: Identity = self.credentials().await?.into();
        let mut settings = SigningSettings::default();
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.signature_location = SignatureLocation::Headers;
        let params: aws_sigv4::http_request::SigningParams<'_> = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|e| other("raw reads: signing params", e))?
            .into();
        let url = format!("{}{}", self.origin, path_and_query);
        let host_str = self.host.to_str().map_err(|e| other("raw reads: host", e))?;
        let mut signable_headers: Vec<(&str, &str)> = Vec::with_capacity(headers.len() + 1);
        signable_headers.push((HOST.as_str(), host_str));
        for (k, v) in headers {
            signable_headers.push((k.as_str(), v.to_str().map_err(|e| other("raw reads: header", e))?));
        }
        let signable = SignableRequest::new(
            method.as_str(),
            url.as_str(),
            signable_headers.into_iter(),
            SignableBody::Precomputed(EMPTY_SHA256.into()),
        )
        .map_err(|e| other("raw reads: signable request", e))?;
        let (instructions, _sig) =
            sign(signable, &params).map_err(|e| other("raw reads: sign", e))?.into_parts();
        let mut req = Request::builder()
            .method(method.clone())
            .uri(url.as_str())
            .header(HOST, self.host.clone());
        for (k, v) in headers {
            req = req.header(k.clone(), v.clone());
        }
        let mut req = req
            .body(Empty::<Bytes>::new())
            .map_err(|e| other("raw reads: build request", e))?;
        instructions.apply_to_request_http1x(&mut req);
        Ok(req)
    }

    /// Send with the retry budget; returns the response head and the
    /// body as the frames it arrived in (empty for HEAD).
    async fn send(
        &self,
        ctx: &'static str,
        method: Method,
        path_and_query: String,
        headers: Vec<(HeaderName, HeaderValue)>,
    ) -> StoreResult<(http::response::Parts, Vec<Bytes>)> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            self.attempts.fetch_add(1, Ordering::Relaxed);
            match self.attempt(ctx, &method, &path_and_query, &headers).await {
                Ok(ok) => return Ok(ok),
                Err(Failure::Final(e)) => return Err(e),
                Err(Failure::Retry(why)) => {
                    if attempt >= self.opts.max_attempts {
                        return Err(StoreError::Other(format!(
                            "{ctx}: {why} (after {attempt} attempts)"
                        )));
                    }
                    let cap = self
                        .opts
                        .backoff_base
                        .saturating_mul(1u32 << (attempt - 1).min(16))
                        .min(self.opts.backoff_cap);
                    tokio::time::sleep(Duration::from_millis(jitter_ms(cap.as_millis() as u64)))
                        .await;
                }
            }
        }
    }

    async fn attempt(
        &self,
        ctx: &'static str,
        method: &Method,
        path_and_query: &str,
        headers: &[(HeaderName, HeaderValue)],
    ) -> Result<(http::response::Parts, Vec<Bytes>), Failure> {
        let req = self.signed(method, path_and_query, headers).await.map_err(Failure::Final)?;
        let resp = match tokio::time::timeout(self.opts.first_byte_timeout, self.client.request(req))
            .await
        {
            Err(_) => {
                return Err(Failure::Retry(format!(
                    "no response within {:?}",
                    self.opts.first_byte_timeout
                )))
            }
            Ok(Err(e)) => return Err(Failure::Retry(format!("transport: {e}"))),
            Ok(Ok(r)) => r,
        };
        let status = resp.status();
        let (parts, body) = resp.into_parts();
        // The body, frame by frame, with a stall bound. An error
        // document is small and read the same way.
        let mut segs: Vec<Bytes> = Vec::new();
        if *method != Method::HEAD {
            let mut body = body;
            loop {
                match tokio::time::timeout(self.opts.body_stall_timeout, body.frame()).await {
                    Err(_) => {
                        return Err(Failure::Retry(format!(
                            "body stalled for {:?}",
                            self.opts.body_stall_timeout
                        )))
                    }
                    Ok(None) => break,
                    Ok(Some(Err(e))) => return Err(Failure::Retry(format!("body: {e}"))),
                    Ok(Some(Ok(frame))) => {
                        if let Ok(data) = frame.into_data() {
                            if !data.is_empty() {
                                segs.push(data);
                            }
                        }
                    }
                }
            }
        }
        if status.is_success() {
            return Ok((parts, segs));
        }
        let code = {
            let mut all = BytesMut::new();
            for s in &segs {
                all.put_slice(s);
            }
            s3_error_code(&all)
        };
        let msg = format!("{ctx}: {} {code}", status.as_u16());
        // The SDK's retry classification: 5xx and 429 are transient,
        // `RequestTimeout` (a 400) too; everything else is the error
        // table's business.
        if status.is_server_error()
            || status == StatusCode::TOO_MANY_REQUESTS
            || code == "RequestTimeout"
        {
            return Err(Failure::Retry(msg));
        }
        Err(Failure::Final(
            crate::s3::classify(status.as_u16(), &code, msg.clone())
                .unwrap_or(StoreError::Other(msg)),
        ))
    }

    fn meta_of(parts: &http::response::Parts) -> ObjectMeta {
        let h = &parts.headers;
        let text = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let mut meta: HashMap<String, String> = HashMap::new();
        for (k, v) in h.iter() {
            if let Some(bare) = k.as_str().strip_prefix("x-amz-meta-") {
                if let Ok(v) = v.to_str() {
                    meta.insert(bare.to_string(), v.to_string());
                }
            }
        }
        ObjectMeta {
            etag: text("etag").unwrap_or_default(),
            size: text("content-length").and_then(|s| s.parse().ok()).unwrap_or(0),
            crc64_b64: text("x-amz-checksum-crc64nvme"),
            meta,
            last_modified_unix: text("last-modified")
                .and_then(|s| httpdate::parse_http_date(&s).ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            storage_class: text("x-amz-storage-class"),
            version_id: text("x-amz-version-id"),
        }
    }

    fn hv(s: &str) -> StoreResult<HeaderValue> {
        HeaderValue::from_str(s).map_err(|e| other("raw reads: header value", e))
    }

    pub async fn head(&self, key: &str, version_id: Option<&str>) -> StoreResult<ObjectMeta> {
        let headers = vec![(
            HeaderName::from_static("x-amz-checksum-mode"),
            HeaderValue::from_static("ENABLED"),
        )];
        let (parts, _) = self
            .send(
                if version_id.is_some() { "head_version" } else { "head" },
                Method::HEAD,
                self.path_and_query(key, version_id),
                headers,
            )
            .await?;
        Ok(Self::meta_of(&parts))
    }

    pub async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
        version_id: Option<&str>,
    ) -> StoreResult<(ObjectMeta, Bytes)> {
        let mut headers = vec![(
            HeaderName::from_static("x-amz-checksum-mode"),
            HeaderValue::from_static("ENABLED"),
        )];
        if let Some(etag) = if_match {
            headers.push((http::header::IF_MATCH, Self::hv(etag)?));
        }
        let (parts, segs) = self
            .send(
                if version_id.is_some() { "get_version" } else { "get_whole" },
                Method::GET,
                self.path_and_query(key, version_id),
                headers,
            )
            .await?;
        let meta = Self::meta_of(&parts);
        let body = match segs.len() {
            0 => Bytes::new(),
            1 => segs.into_iter().next().unwrap_or_default(),
            _ => {
                let mut all = BytesMut::with_capacity(segs.iter().map(|s| s.len()).sum());
                for s in &segs {
                    all.put_slice(s);
                }
                all.freeze()
            }
        };
        Ok((meta, body))
    }

    /// A ranged read, guarded by `If-Match`, returned as the frames it
    /// arrived in (no flattening — see `ObjectStore::get_range_segments`).
    pub async fn get_range_segments(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        if_match: &str,
    ) -> StoreResult<Vec<Bytes>> {
        let headers = vec![
            (http::header::RANGE, Self::hv(&format!("bytes={}-{}", offset, offset + len - 1))?),
            (http::header::IF_MATCH, Self::hv(if_match)?),
        ];
        let (parts, segs) = self
            .send("get_range", Method::GET, self.path_and_query(key, None), headers)
            .await?;
        // A 200 to a ranged GET is the whole object: a proxy that
        // dropped the header. Refuse rather than write the wrong bytes
        // at the wrong offset.
        if parts.status != StatusCode::PARTIAL_CONTENT {
            return Err(StoreError::Other(format!(
                "get_range: {} answered {} to a Range request — the range was ignored",
                key,
                parts.status.as_u16()
            )));
        }
        Ok(segs)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Against an in-process hyper server that records every request
    //! and answers from a script: the request shape S3 will sign-check,
    //! the metadata parsed back, the status table, the retry budget,
    //! the timeouts, and connection reuse. Signatures are cross-checked
    //! by re-signing the recorded request with the same inputs and the
    //! `x-amz-date` it carried; S3 itself is the final arbiter and is
    //! exercised by the cluster drill.
    //!
    //! `serve` is `pub(crate)` so the SDK-path tests in `s3.rs` can point
    //! a store at the same recorder.
    use super::*;
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    use aws_credential_types::Credentials;
    use http::{HeaderMap, Response};
    use http_body_util::{combinators::BoxBody, Full, StreamBody};
    use hyper::body::Frame;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    #[derive(Clone, Debug)]
    pub(crate) struct Seen {
        pub(crate) method: String,
        pub(crate) path: String,
        pub(crate) headers: HeaderMap,
    }

    /// One scripted answer: status, headers, body frames.
    pub(crate) type Answer = (u16, Vec<(&'static str, String)>, Vec<Bytes>);

    pub(crate) struct Server {
        pub(crate) url: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        script: Arc<Mutex<VecDeque<Answer>>>,
        conns: Arc<AtomicUsize>,
    }

    impl Server {
        pub(crate) fn push(&self, a: Answer) {
            self.script.lock().unwrap().push_back(a);
        }
        pub(crate) fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    pub(crate) async fn serve(delay: Option<Duration>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Seen>>> = Default::default();
        let script: Arc<Mutex<VecDeque<Answer>>> = Default::default();
        let conns = Arc::new(AtomicUsize::new(0));
        let (seen2, script2, conns2) = (seen.clone(), script.clone(), conns.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                conns2.fetch_add(1, Ordering::SeqCst);
                let (seen, script) = (seen2.clone(), script2.clone());
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let (seen, script) = (seen.clone(), script.clone());
                        async move {
                            seen.lock().unwrap().push(Seen {
                                method: req.method().to_string(),
                                path: req
                                    .uri()
                                    .path_and_query()
                                    .map(|p| p.to_string())
                                    .unwrap_or_default(),
                                headers: req.headers().clone(),
                            });
                            if let Some(d) = delay {
                                tokio::time::sleep(d).await;
                            }
                            let (status, headers, frames) =
                                script.lock().unwrap().pop_front().unwrap_or((
                                    200,
                                    vec![("etag", "\"default\"".into())],
                                    vec![Bytes::from_static(b"default body")],
                                ));
                            let mut b = Response::builder().status(status);
                            for (k, v) in headers {
                                b = b.header(k, v);
                            }
                            let is_head = req.method() == Method::HEAD;
                            let total: usize = frames.iter().map(|f| f.len()).sum();
                            let body: BoxBody<Bytes, Infallible> = if is_head {
                                Full::new(Bytes::new()).boxed()
                            } else if frames.len() == 1 {
                                Full::new(frames.into_iter().next().unwrap()).boxed()
                            } else {
                                StreamBody::new(futures::stream::iter(
                                    frames.into_iter().map(|f| Ok(Frame::data(f))),
                                ))
                                .boxed()
                            };
                            let _ = total;
                            Ok::<_, Infallible>(b.body(body).unwrap())
                        }
                    });
                    let _ = http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        Server { url, seen, script, conns }
    }

    fn reader(url: &str, opts: RawReadOptions) -> RawReader {
        let creds = Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI", Some("tok".into()), None, "test");
        RawReader::new("bkt", "us-west-1", Some(url), SharedCredentialsProvider::new(creds), opts)
            .unwrap()
    }

    /// A credential chain that counts how often it is asked — what the
    /// instance metadata service is on EC2, minus the round trip.
    #[derive(Debug)]
    struct Counting(Arc<AtomicUsize>, Option<SystemTime>);
    impl ProvideCredentials for Counting {
        fn provide_credentials<'a>(
            &'a self,
        ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            aws_credential_types::provider::future::ProvideCredentials::ready(Ok(
                Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI", None, self.1, "counting"),
            ))
        }
    }

    #[tokio::test]
    async fn the_credential_chain_is_asked_once_for_a_burst_of_reads() {
        let srv = serve(None).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = SharedCredentialsProvider::new(Counting(calls.clone(), None));
        let r = RawReader::new("bkt", "us-west-1", Some(&srv.url), provider, fast()).unwrap();
        for _ in 0..50 {
            srv.push((200, vec![("etag", "\"x\"".into())], vec![Bytes::from_static(b"x")]));
            r.get_whole("k", None, None).await.unwrap();
        }
        assert_eq!(srv.seen().len(), 50);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one resolve for fifty GETs — on EC2 each extra one is an IMDS round trip");
        assert_eq!(r.credential_fetches(), 1);
    }

    #[tokio::test]
    async fn a_credential_near_expiry_is_refreshed_but_not_per_request() {
        let srv = serve(None).await;
        let calls = Arc::new(AtomicUsize::new(0));
        // Expires in two minutes: inside the five-minute refresh window
        // from the first call on.
        let exp = SystemTime::now() + Duration::from_secs(120);
        let provider = SharedCredentialsProvider::new(Counting(calls.clone(), Some(exp)));
        let r = RawReader::new("bkt", "us-west-1", Some(&srv.url), provider, fast()).unwrap();
        for _ in 0..30 {
            srv.push((200, vec![], vec![Bytes::from_static(b"x")]));
            r.get_whole("k", None, None).await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a near-expiry credential is not re-resolved within ten seconds of the last resolve");
        // Age the cache past the ten-second floor: now it refreshes once.
        r.cred_cache.write().unwrap().fetched = Instant::now() - Duration::from_secs(11);
        srv.push((200, vec![], vec![Bytes::from_static(b"x")]));
        r.get_whole("k", None, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn fast() -> RawReadOptions {
        RawReadOptions {
            max_attempts: 3,
            backoff_base: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(2),
            ..RawReadOptions::default()
        }
    }

    fn h<'a>(s: &'a Seen, name: &str) -> &'a str {
        s.headers.get(name).map(|v| v.to_str().unwrap()).unwrap_or("")
    }

    #[tokio::test]
    async fn a_whole_get_is_signed_the_way_s3_checks_it_and_parses_what_comes_back() {
        let srv = serve(None).await;
        srv.push((
            200,
            vec![
                ("etag", "\"abc123\"".into()),
                ("content-length", "5".into()),
                ("x-amz-meta-flint-gen", "7".into()),
                ("x-amz-meta-flint-flush-uuid", "u-1".into()),
                ("x-amz-version-id", "v9".into()),
                ("x-amz-checksum-crc64nvme", "AAAAAAAAAAA=".into()),
                ("x-amz-storage-class", "STANDARD_IA".into()),
                ("last-modified", "Sat, 12 Sep 2026 22:18:13 GMT".into()),
            ],
            vec![Bytes::from_static(b"hello")],
        ));
        let r = reader(&srv.url, fast());
        let (meta, body) = r.get_whole("dir/a b+c.txt", Some("\"e1\""), None).await.unwrap();
        assert_eq!(&body[..], b"hello");
        assert_eq!(meta.etag, "\"abc123\"");
        assert_eq!(meta.size, 5);
        assert_eq!(meta.meta.get("flint-gen").map(String::as_str), Some("7"));
        assert_eq!(meta.meta.get("flint-flush-uuid").map(String::as_str), Some("u-1"));
        assert_eq!(meta.version_id.as_deref(), Some("v9"));
        assert_eq!(meta.crc64_b64.as_deref(), Some("AAAAAAAAAAA="));
        assert_eq!(meta.storage_class.as_deref(), Some("STANDARD_IA"));
        assert_eq!(meta.last_modified_unix, Some(1789251493));

        let s = &srv.seen()[0];
        assert_eq!(s.method, "GET");
        assert_eq!(s.path, "/bkt/dir/a%20b%2Bc.txt", "S3 key encoding, one pass, '/' kept");
        assert_eq!(h(s, "host"), srv.url.trim_start_matches("http://"));
        assert_eq!(h(s, "if-match"), "\"e1\"");
        assert_eq!(h(s, "x-amz-checksum-mode"), "ENABLED");
        assert_eq!(h(s, "x-amz-content-sha256"), EMPTY_SHA256);
        assert_eq!(h(s, "x-amz-security-token"), "tok");
        let date = h(s, "x-amz-date").to_string();
        assert!(date.len() == 16 && date.ends_with('Z') && &date[8..9] == "T", "{date}");
        let auth = h(s, "authorization").to_string();
        assert!(
            auth.starts_with(&format!(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/{}/us-west-1/s3/aws4_request, SignedHeaders=",
                &date[..8]
            )),
            "{auth}"
        );
        let signed = auth.split("SignedHeaders=").nth(1).unwrap().split(',').next().unwrap();
        for must in ["host", "if-match", "x-amz-checksum-mode", "x-amz-content-sha256", "x-amz-date", "x-amz-security-token"] {
            assert!(signed.split(';').any(|x| x == must), "{must} not in SignedHeaders={signed}");
        }
        let sig = auth.split("Signature=").nth(1).unwrap();
        assert!(sig.len() == 64 && sig.chars().all(|c| c.is_ascii_hexdigit()), "{sig}");

        // Cross-check: re-sign the recorded request with the same
        // inputs and the `x-amz-date` it carried; the Authorization
        // header must come out identical. This pins that nothing was
        // added or altered after signing (a header hyper adds behind
        // the signature is the classic SignatureDoesNotMatch).
        let t = httpdate_from_amz(&date);
        let identity: Identity = Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI", Some("tok".into()), None, "test").into();
        let mut settings = SigningSettings::default();
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        let params: aws_sigv4::http_request::SigningParams<'_> = v4::SigningParams::builder()
            .identity(&identity)
            .region("us-west-1")
            .name("s3")
            .time(t)
            .settings(settings)
            .build()
            .unwrap()
            .into();
        let host = h(s, "host").to_string();
        let url = format!("{}{}", srv.url, s.path);
        let hdrs = [("host", host.as_str()), ("if-match", "\"e1\""), ("x-amz-checksum-mode", "ENABLED")];
        let signable = SignableRequest::new("GET", url.as_str(), hdrs.into_iter(), SignableBody::Precomputed(EMPTY_SHA256.into())).unwrap();
        let (instr, _) = sign(signable, &params).unwrap().into_parts();
        let mut expect = Request::builder().method("GET").uri(url.as_str()).body(()).unwrap();
        instr.apply_to_request_http1x(&mut expect);
        assert_eq!(expect.headers().get("authorization").unwrap().to_str().unwrap(), auth);
    }

    fn httpdate_from_amz(d: &str) -> SystemTime {
        // YYYYMMDDTHHMMSSZ → SystemTime, via a civil-date fold.
        let n = |a: usize, b: usize| d[a..b].parse::<i64>().unwrap();
        let (y, mo, da, hh, mm, ss) = (n(0, 4), n(4, 6), n(6, 8), n(9, 11), n(11, 13), n(13, 15));
        let (y2, m2) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
        let era = y2.div_euclid(400);
        let yoe = y2 - era * 400;
        let doy = (153 * (m2 - 3) + 2) / 5 + da - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + doe - 719468;
        UNIX_EPOCH + Duration::from_secs((days * 86400 + hh * 3600 + mm * 60 + ss) as u64)
    }

    #[tokio::test]
    async fn head_and_versioned_reads_put_the_version_in_the_query() {
        let srv = serve(None).await;
        srv.push((200, vec![("etag", "\"h\"".into()), ("content-length", "7".into()), ("x-amz-version-id", "v.1_x".into())], vec![]));
        let r = reader(&srv.url, fast());
        let m = r.head("k/1", Some("v.1_x")).await.unwrap();
        assert_eq!((m.etag.as_str(), m.size, m.version_id.as_deref()), ("\"h\"", 7, Some("v.1_x")));
        let s = &srv.seen()[0];
        assert_eq!((s.method.as_str(), s.path.as_str()), ("HEAD", "/bkt/k/1?versionId=v.1_x"));
        assert_eq!(h(s, "x-amz-checksum-mode"), "ENABLED");
        assert!(!s.headers.contains_key("if-match"));

        srv.push((200, vec![("etag", "\"g\"".into())], vec![Bytes::from_static(b"vbody")]));
        let (m, b) = r.get_whole("k/1", None, Some("a/b c")).await.unwrap();
        assert_eq!((m.etag.as_str(), &b[..]), ("\"g\"", &b"vbody"[..]));
        assert_eq!(srv.seen()[1].path, "/bkt/k/1?versionId=a%2Fb%20c");
    }

    #[tokio::test]
    async fn a_ranged_read_returns_the_frames_and_refuses_a_200() {
        let srv = serve(None).await;
        srv.push((206, vec![("etag", "\"r\"".into())], vec![Bytes::from_static(b"01234"), Bytes::from_static(b"56789")]));
        let r = reader(&srv.url, fast());
        let segs = r.get_range_segments("big.bin", 10, 10, "\"r\"").await.unwrap();
        let mut all = Vec::new();
        for s in &segs {
            all.extend_from_slice(s);
        }
        assert_eq!(all, b"0123456789");
        assert!(!segs.is_empty());
        let s = &srv.seen()[0];
        assert_eq!(h(s, "range"), "bytes=10-19");
        assert_eq!(h(s, "if-match"), "\"r\"");
        assert!(!s.headers.contains_key("x-amz-checksum-mode"), "the SDK path sends none on ranges");

        srv.push((200, vec![], vec![Bytes::from_static(b"the whole object")]));
        let err = r.get_range_segments("big.bin", 10, 10, "\"r\"").await.unwrap_err();
        assert!(err.to_string().contains("range was ignored"), "{err}");
    }

    #[tokio::test]
    async fn the_status_table_is_the_sdk_paths() {
        let srv = serve(None).await;
        let r = reader(&srv.url, fast());
        srv.push((412, vec![], vec![Bytes::from_static(b"<Error><Code>PreconditionFailed</Code></Error>")]));
        assert!(matches!(r.get_whole("k", Some("\"x\""), None).await, Err(StoreError::PreconditionFailed(_))));
        srv.push((404, vec![], vec![Bytes::from_static(b"<Error><Code>NoSuchKey</Code></Error>")]));
        assert!(matches!(r.get_whole("k", None, None).await, Err(StoreError::NotFound(_))));
        srv.push((403, vec![], vec![Bytes::from_static(b"<Error><Code>AccessDenied</Code><Message>x</Message></Error>")]));
        match r.get_whole("k", None, None).await {
            Err(StoreError::Auth(m)) => assert!(m.contains("403 AccessDenied"), "{m}"),
            other => panic!("{other:?}"),
        }
        srv.push((400, vec![], vec![Bytes::from_static(b"<Error><Code>ExpiredToken</Code></Error>")]));
        assert!(matches!(r.get_whole("k", None, None).await, Err(StoreError::Auth(_))));
        // HEAD carries no document: status alone decides.
        srv.push((404, vec![], vec![]));
        assert!(matches!(r.head("k", None).await, Err(StoreError::NotFound(_))));
        srv.push((412, vec![], vec![]));
        assert!(matches!(r.head("k", None).await, Err(StoreError::PreconditionFailed(_))));
        assert_eq!(srv.seen().len(), 6, "4xx is final: no retries");
    }

    #[tokio::test]
    async fn a_5xx_is_retried_within_the_budget_and_then_given_up() {
        let srv = serve(None).await;
        let r = reader(&srv.url, fast());
        let slow = || (503u16, vec![], vec![Bytes::from_static(b"<Error><Code>SlowDown</Code></Error>")]);
        srv.push(slow());
        srv.push(slow());
        srv.push((200, vec![("etag", "\"ok\"".into())], vec![Bytes::from_static(b"eventually")]));
        let (m, b) = r.get_whole("k", None, None).await.unwrap();
        assert_eq!((m.etag.as_str(), &b[..]), ("\"ok\"", &b"eventually"[..]));
        assert_eq!(srv.seen().len(), 3);
        assert_eq!(r.attempts(), 3);
        // Each attempt is signed afresh: three distinct Authorization
        // values would be a coincidence of the clock, but the DATE must
        // be present on every one and the counter must agree.
        for s in srv.seen() {
            assert!(!h(&s, "x-amz-date").is_empty());
        }

        srv.push(slow());
        srv.push(slow());
        srv.push(slow());
        let err = r.get_whole("k", None, None).await.unwrap_err();
        let m = err.to_string();
        assert!(m.contains("503 SlowDown") && m.contains("3 attempts"), "{m}");
        assert_eq!(srv.seen().len(), 6);
    }

    #[tokio::test]
    async fn a_response_that_never_starts_is_retried_and_then_given_up() {
        let srv = serve(Some(Duration::from_millis(400))).await;
        let r = reader(
            &srv.url,
            RawReadOptions { first_byte_timeout: Duration::from_millis(60), max_attempts: 2, ..fast() },
        );
        let err = r.get_whole("k", None, None).await.unwrap_err();
        assert!(err.to_string().contains("no response within"), "{err}");
        assert_eq!(r.attempts(), 2);
    }

    #[tokio::test]
    async fn a_transport_error_is_retried_and_then_given_up() {
        // A port nothing listens on.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let r = reader(&url, fast());
        let err = r.head("k", None).await.unwrap_err();
        let m = err.to_string();
        assert!(m.contains("transport") && m.contains("3 attempts"), "{m}");
        assert_eq!(r.attempts(), 3);
    }

    #[tokio::test]
    async fn sequential_reads_reuse_one_connection() {
        let srv = serve(None).await;
        let r = reader(&srv.url, fast());
        for i in 0..20 {
            srv.push((200, vec![("etag", format!("\"{i}\""))], vec![Bytes::from_static(b"x")]));
            r.get_whole("k", None, None).await.unwrap();
        }
        assert_eq!(srv.seen().len(), 20);
        assert_eq!(srv.conns.load(Ordering::SeqCst), 1, "keep-alive: one connection for twenty GETs");
    }

    #[test]
    fn key_encoding_is_s3s_single_pass() {
        assert_eq!(encode_key("a/b c+d%e~f-g_h.i"), "a/b%20c%2Bd%25e~f-g_h.i");
        assert_eq!(encode_key("ünï/€"), "%C3%BCn%C3%AF/%E2%82%AC");
        assert_eq!(encode_query_value("a/b c"), "a%2Fb%20c");
        assert_eq!(s3_error_code(b"<Error><Code>SlowDown</Code><Message>m</Message></Error>"), "SlowDown");
        assert_eq!(s3_error_code(b"not xml"), "");
    }

    #[test]
    fn addressing_is_virtual_hosted_on_s3_and_path_style_elsewhere() {
        let creds = SharedCredentialsProvider::new(Credentials::for_tests());
        let real = RawReader::new("bkt", "us-west-1", None, creds.clone(), fast()).unwrap();
        assert_eq!(real.origin, "https://bkt.s3.us-west-1.amazonaws.com");
        assert_eq!(real.path_prefix, "");
        assert_eq!(real.path_and_query("k", None), "/k");
        let dotted = RawReader::new("my.bkt", "us-west-1", None, creds.clone(), fast()).unwrap();
        assert_eq!(dotted.origin, "https://s3.us-west-1.amazonaws.com");
        assert_eq!(dotted.path_and_query("k", None), "/my.bkt/k");
        let ep = RawReader::new("bkt", "us-east-1", Some("http://localhost:9878/"), creds, fast()).unwrap();
        assert_eq!(ep.origin, "http://localhost:9878");
        assert_eq!(ep.path_and_query("k", None), "/bkt/k");
        assert_eq!(ep.host.to_str().unwrap(), "localhost:9878");
    }
}
