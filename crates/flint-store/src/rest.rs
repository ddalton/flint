//! What the hand-written REST backends share (Azure Blob, GCS): the
//! HTTPS client, the content ETag's digits, and the encodings both speak.
//! Neither carries a provider SDK (docs/plans/lean-azure-gcs-backends-plan-2026-11-10.md, D1).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::Request;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use sha2::{Digest, Sha256};

use crate::gate::ByteGate;
use crate::{ComposeSpec, DeleteManyReport, PartSource, StoreError, StoreResult};

pub(crate) type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// One pooled HTTP/1 client, HTTPS or plain HTTP (an emulator).
pub(crate) fn https_client(connect_timeout: Duration, ctx: &str) -> StoreResult<HttpsClient> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(connect_timeout));
    // aws-lc-rs, the provider the rest of this crate uses: two in one
    // binary and rustls refuses to guess.
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(rustls::crypto::aws_lc_rs::default_provider())
        .map_err(|e| StoreError::Other(format!("{ctx}: TLS roots: {e}")))?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(1024)
        .pool_timer(TokioTimer::new())
        .build(https))
}

pub(crate) fn quoted(etag: &str) -> String {
    format!("\"{}\"", etag.trim().trim_matches('"'))
}

/// The content ETag's digits: 128 bits of SHA-256, as lean's chunk
/// addresses are.
pub(crate) fn content_hash(digest: &[u8]) -> String {
    hex(&digest[..16])
}

/// The ETag lean sees for an object: its content hash, or, for one an
/// outside tool wrote (no hash), `"x-<key digest>-<token>"`: unique per
/// write, and never equal to another object's ETag.
pub(crate) fn presented(key: &str, hash: Option<&str>, token: &str) -> String {
    match hash {
        Some(h) => format!("\"{h}\""),
        None => format!("\"x-{}-{}\"", key_digest(key), token.trim().trim_matches('"')),
    }
}

/// The provider token inside an outside writer's ETag, for THIS key; a
/// form minted for another key is a precondition failure, not a token.
/// `None`: a content ETag.
pub(crate) fn foreign_token(key: &str, etag: &str) -> StoreResult<Option<String>> {
    let Some(rest) = etag.trim_matches('"').strip_prefix("x-") else { return Ok(None) };
    match rest.split_once('-') {
        Some((digest, token)) if digest == key_digest(key) => Ok(Some(token.to_string())),
        _ => Err(StoreError::PreconditionFailed(format!("{key}: {etag} names another object"))),
    }
}

pub(crate) fn key_digest(key: &str) -> String {
    hex(&Sha256::digest(key.as_bytes())[..8])
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn jitter(cap: Duration) -> Duration {
    let ms = cap.as_millis() as u64;
    if ms == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis((uuid::Uuid::new_v4().as_u128() % (ms as u128 + 1)) as u64)
}

/// Read one part from the descriptor the caller opened (never a path:
/// see `ComposeSpec::local`).
pub(crate) async fn read_local(file: &Arc<std::fs::File>, offset: u64, len: u64) -> StoreResult<Bytes> {
    let f = file.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<Bytes> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; len as usize];
        f.read_exact_at(&mut buf, offset)?;
        Ok(Bytes::from(buf))
    })
    .await
    .map_err(|e| StoreError::Other(format!("local read join: {e}")))?
    .map_err(|e| StoreError::Other(format!("local read [{offset}, +{len}): {e}")))
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Some(out)
}

/// Percent-encode everything but the unreserved characters, and `/` when
/// `path` (an object name's separators stay separators).
pub(crate) fn encode(s: &str, path: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || (path && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ── bulk paths ────────────────────────────────────────────────────────

/// Deletes in flight at once in a `delete_many`: neither store has S3's
/// one-request DeleteObjects, and lean's collector renews its lease only
/// between batches of a thousand keys (`DELETE_RENEW_BATCH`), so a
/// thousand deletes one after another (tens of seconds) would reach the
/// ~60 s at which a waiter judges the holder dead.
pub(crate) const DELETE_PARALLELISM: usize = 32;

/// Delete every key, `parallelism` at a time, with `delete`. A key already
/// gone counts as deleted, as S3's DeleteObjects counts it.
pub(crate) async fn delete_each<'a, F, Fut>(keys: &'a [String], parallelism: usize, delete: F) -> DeleteManyReport
where
    F: Fn(&'a str) -> Fut,
    Fut: std::future::Future<Output = StoreResult<()>> + 'a,
{
    use futures::stream::StreamExt;
    // The futures are made before anything awaits, so no closure is held
    // across an await (a stream that maps keys through one is not `Send`
    // enough for an `async_trait` method).
    let deletes: Vec<_> = keys
        .iter()
        .map(|k| {
            let f = delete(k);
            async move { (k, f.await) }
        })
        .collect();
    let results: Vec<(&String, StoreResult<()>)> =
        futures::stream::iter(deletes).buffer_unordered(parallelism.max(1)).collect().await;
    let mut report = DeleteManyReport::default();
    for (k, r) in results {
        match r {
            Ok(()) | Err(StoreError::NotFound(_)) => report.deleted += 1,
            Err(e) => report.failed.push((k.clone(), e.to_string())),
        }
    }
    report
}

/// Upload the `Local` parts of `spec` `parallelism` at a time, read in the
/// object's order from the caller's descriptor, each handed to `seen` as
/// it is read (the streaming sums, which need that order) and then to
/// `put(index, bytes)`. Every upload started is waited for, so the cleanup
/// that follows a failure sees every part that landed. The results come
/// back in the object's order.
pub(crate) async fn upload_parts<T, F, Fut>(
    spec: &ComposeSpec<'_>,
    gate: Option<&Arc<ByteGate>>,
    parallelism: usize,
    mut seen: impl FnMut(&[u8]),
    put: F,
) -> StoreResult<Vec<T>>
where
    F: Fn(usize, Bytes) -> Fut,
    Fut: std::future::Future<Output = StoreResult<T>>,
{
    use futures::stream::{FuturesUnordered, StreamExt};
    let file = spec
        .local
        .as_ref()
        .ok_or_else(|| StoreError::Other(format!("{}: a Local part with no descriptor", spec.key)))?;
    let mut done: Vec<Option<T>> = (0..spec.parts.len()).map(|_| None).collect();
    let mut inflight = FuturesUnordered::new();
    let mut failed: Option<StoreError> = None;
    let mut land = |(i, len, r): (usize, u64, StoreResult<T>), failed: &mut Option<StoreError>| match r {
        Ok(t) => {
            done[i] = Some(t);
            spec.note_progress(len);
        }
        Err(e) => {
            failed.get_or_insert(e);
        }
    };
    for (i, part) in spec.parts.iter().enumerate() {
        let (offset, len) = match part {
            PartSource::Local { offset, len } => (*offset, *len),
            PartSource::BaseCopy { .. } => {
                failed = Some(StoreError::Other(format!("{}: a BaseCopy part where only Local ones go up", spec.key)));
                break;
            }
        };
        let permit = match gate {
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
        seen(&bytes);
        let up = put(i, bytes);
        inflight.push(async move {
            let r = up.await;
            drop(permit);
            (i, len, r)
        });
        while inflight.len() >= parallelism.max(1) {
            let landed = inflight.next().await.expect("one in flight");
            land(landed, &mut failed);
        }
        if failed.is_some() {
            break;
        }
    }
    while let Some(landed) = inflight.next().await {
        land(landed, &mut failed);
    }
    if let Some(e) = failed {
        return Err(e);
    }
    Ok(done.into_iter().map(|t| t.expect("every part landed")).collect())
}

// ── access tokens (Entra ID, Google OAuth2) ───────────────────────────

/// An access token, kept until a minute before it expires and then
/// fetched again by whichever request asks first.
#[derive(Default)]
pub(crate) struct TokenCache(tokio::sync::Mutex<Option<(String, Instant)>>);

impl TokenCache {
    /// The kept token, or one from `fetch` (a token and its lifetime).
    pub(crate) async fn get<F, Fut>(&self, fetch: F) -> StoreResult<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = StoreResult<(String, Duration)>>,
    {
        let mut kept = self.0.lock().await;
        if let Some((t, until)) = &*kept {
            if Instant::now() < *until {
                return Ok(t.clone());
            }
        }
        let (t, ttl) = fetch().await?;
        // A minute early, so no request carries a token that expires on
        // the way.
        *kept = Some((t.clone(), Instant::now() + ttl.saturating_sub(Duration::from_secs(60))));
        Ok(t)
    }

    /// Forget the token: the service refused it.
    pub(crate) async fn clear(&self) {
        *self.0.lock().await = None;
    }
}

/// `application/x-www-form-urlencoded`.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false))).collect::<Vec<_>>().join("&")
}

/// One request to a token endpoint: its status, and its body as JSON
/// (`Null` when it is not). No retries: the caller's request retries.
pub(crate) async fn token_request(
    client: &HttpsClient,
    req: Request<Full<Bytes>>,
    timeout: Duration,
    ctx: &str,
) -> StoreResult<(u16, serde_json::Value)> {
    let uri = req.uri().to_string();
    let resp = tokio::time::timeout(timeout, client.request(req))
        .await
        .map_err(|_| StoreError::Auth(format!("{ctx}: no response from {uri} within {timeout:?}")))?
        .map_err(|e| StoreError::Auth(format!("{ctx}: {uri}: {e}")))?;
    let status = resp.status().as_u16();
    let body = tokio::time::timeout(timeout, resp.into_body().collect())
        .await
        .map_err(|_| StoreError::Auth(format!("{ctx}: {uri}: the body stalled")))?
        .map_err(|e| StoreError::Auth(format!("{ctx}: {uri}: body: {e}")))?
        .to_bytes();
    Ok((status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)))
}

/// A token's lifetime as the endpoint gave it: `expires_in`, a number or
/// (Azure's instance metadata) a string of one. An hour when it gave none.
pub(crate) fn expires_in(v: &serde_json::Value) -> Duration {
    let secs = match &v["expires_in"] {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    Duration::from_secs(secs.unwrap_or(3600))
}

/// What a token endpoint said when it refused: OAuth2's
/// `error_description` or `error`, or Google's `error.message`.
pub(crate) fn refusal(v: &serde_json::Value) -> String {
    let s = v["error_description"]
        .as_str()
        .or(v["error"]["message"].as_str())
        .or(v["error"].as_str())
        .unwrap_or("");
    s.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base64_std;

    #[test]
    fn base64_decode_inverts_the_crates_encoder() {
        for s in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0u8, 255, 128, 7]] {
            assert_eq!(base64_decode(&base64_std(s)).as_deref(), Some(s), "{s:?}");
        }
        assert_eq!(base64_decode("not base64!"), None);
    }

    #[test]
    fn a_name_is_encoded_with_its_slashes_kept_in_a_path() {
        assert_eq!(encode("p/files/a b+c@u", true), "p/files/a%20b%2Bc%40u");
        assert_eq!(encode("a/b", false), "a%2Fb");
        assert_eq!(encode("é", true), "%C3%A9");
    }

    #[test]
    fn an_outside_writers_etag_names_its_own_object_only() {
        let e = presented("p/a", None, "\"0x8DC1\"");
        assert!(e.starts_with("\"x-") && e.ends_with("-0x8DC1\""), "{e}");
        assert_eq!(foreign_token("p/a", &e).unwrap(), Some("0x8DC1".to_string()));
        assert!(matches!(foreign_token("p/b", &e), Err(StoreError::PreconditionFailed(_))));
        assert_eq!(foreign_token("p/a", "\"0123abcd\"").unwrap(), None, "a content ETag is not a token");
        assert_eq!(presented("p/a", Some("ab12"), "\"0x8DC1\""), "\"ab12\"");
    }

    #[test]
    fn a_form_encodes_its_values_and_lifetimes_read_as_either_type() {
        assert_eq!(form(&[("a", "x y"), ("b", "c:d/e")]), "a=x%20y&b=c%3Ad%2Fe");
        let v = |j: &str| serde_json::from_str::<serde_json::Value>(j).unwrap();
        assert_eq!(expires_in(&v(r#"{"expires_in":3599}"#)), Duration::from_secs(3599));
        assert_eq!(expires_in(&v(r#"{"expires_in":"86399"}"#)), Duration::from_secs(86399));
        assert_eq!(expires_in(&v("{}")), Duration::from_secs(3600));
        assert_eq!(refusal(&v(r#"{"error":"invalid_client","error_description":"AADSTS700016"}"#)), "AADSTS700016");
        assert_eq!(refusal(&v(r#"{"error":{"code":403,"message":"Permission denied"}}"#)), "Permission denied");
    }

    #[tokio::test]
    async fn deletes_run_side_by_side_and_a_gone_key_counts_as_deleted() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (now, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let keys: Vec<String> = (0..100).map(|i| format!("k{i}")).collect();
        let report = delete_each(&keys, 8, |k| {
            let (now, peak) = (&now, &peak);
            async move {
                let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                match k {
                    "k3" => Err(StoreError::NotFound("gone".into())),
                    "k7" => Err(StoreError::Auth("denied".into())),
                    _ => Ok(()),
                }
            }
        })
        .await;
        assert_eq!(report.deleted, 99);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "k7");
        assert_eq!(peak.load(Ordering::SeqCst), 8, "the deletes did not overlap, or overlapped past the bound");
    }

    /// Parts go up side by side, but are read, summed and returned in the
    /// object's order; a failure waits for every upload already started.
    #[tokio::test]
    async fn parts_go_up_side_by_side_and_come_back_in_order() {
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let body: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&body).unwrap();
        let file = Arc::new(f);
        let spec = |parts: Vec<PartSource>| ComposeSpec {
            key: "k",
            local: Some(file.clone()),
            parts,
            base_key: None,
            base_etag: None,
            condition: crate::PutCondition::IfNoneMatchAny,
            stamps: crate::GenerationStamps { generation: 1, epoch: 0, flush_uuid: "f".into(), boundary_source: None, posix: None },
            crc64: None,
            progress: None,
        };
        let parts: Vec<PartSource> = (0..10).map(|i| PartSource::Local { offset: i * 1000, len: 1000 }).collect();
        let (now, peak, finished) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
        let mut streamed = Vec::new();
        let out = upload_parts(&spec(parts.clone()), None, 4, |b| streamed.extend_from_slice(b), |i, b| {
            let (now, peak, finished) = (&now, &peak, &finished);
            async move {
                let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                // Later parts land first.
                tokio::time::sleep(Duration::from_millis(20 - 2 * i as u64)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                finished.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StoreError>((i, b.len()))
            }
        })
        .await
        .unwrap();
        assert_eq!(out, (0..10).map(|i| (i, 1000)).collect::<Vec<_>>());
        assert_eq!(streamed, body, "the sums saw the bytes in the object's order");
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        // Part 2 fails: the call fails, and nothing it started is abandoned.
        let started = AtomicUsize::new(0);
        let r = upload_parts(&spec(parts), None, 4, |_| {}, |i, _| {
            let (started, finished) = (&started, &finished);
            async move {
                started.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                finished.fetch_add(1, Ordering::SeqCst);
                if i == 2 { Err(StoreError::Other("part 2".into())) } else { Ok(()) }
            }
        })
        .await;
        assert!(matches!(r, Err(StoreError::Other(m)) if m == "part 2"));
        assert_eq!(finished.load(Ordering::SeqCst), 10 + started.load(Ordering::SeqCst), "an upload was left running");
    }

    #[tokio::test]
    async fn a_kept_token_is_fetched_once_until_it_is_cleared() {
        let cache = TokenCache::default();
        let n = std::sync::atomic::AtomicUsize::new(0);
        let fetch = || async {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(("t".to_string(), Duration::from_secs(3600)))
        };
        assert_eq!(cache.get(fetch).await.unwrap(), "t");
        assert_eq!(cache.get(fetch).await.unwrap(), "t");
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1);
        cache.clear().await;
        cache.get(fetch).await.unwrap();
        // A token with under a minute left is not kept at all.
        let short = TokenCache::default();
        let m = std::sync::atomic::AtomicUsize::new(0);
        let fetch_short = || async {
            m.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(("s".to_string(), Duration::from_secs(30)))
        };
        short.get(fetch_short).await.unwrap();
        short.get(fetch_short).await.unwrap();
        assert_eq!((n.load(std::sync::atomic::Ordering::SeqCst), m.load(std::sync::atomic::Ordering::SeqCst)), (2, 2));
    }
}

/// A local HTTP server for the token endpoints' tests: it records every
/// request and answers each with what `answer` returns.
#[cfg(test)]
pub(crate) mod mock {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug, Clone)]
    pub(crate) struct Seen {
        pub method: String,
        /// Path and query, as sent.
        pub target: String,
        /// Lowercase names.
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Seen {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        }
    }

    pub(crate) struct Server {
        pub base: String,
        pub seen: Arc<Mutex<Vec<Seen>>>,
    }

    pub(crate) async fn start(answer: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (log, answer) = (seen.clone(), Arc::new(answer));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (log, answer) = (log.clone(), answer.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 16 * 1024];
                    let req = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                        let mut lines = head.lines();
                        let mut first = lines.next().unwrap_or("").split(' ');
                        let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
                        let headers: Vec<(String, String)> = lines
                            .filter_map(|l| l.split_once(':'))
                            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                            .collect();
                        let len: usize =
                            headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                        if body.len() >= len {
                            break Seen { method, target, headers, body: body.to_string() };
                        }
                    };
                    let (status, reply) = answer(&req);
                    log.lock().unwrap().push(req);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        Server { base, seen }
    }
}
