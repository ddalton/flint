//! An in-memory S3 whose ceiling is far above the client's, so a run
//! against it measures the CLIENT.
//!
//! WHAT IT DELIBERATELY REPRODUCES, because lean's checkout breaks
//! without it: every GET carries the manifest's cited etag as If-Match
//! (`checkout.rs:493` refuses when the object "no longer carries" it), so
//! a fake that invents etags fails on the first file and the failure
//! looks like a lean bug. Bodies and etags are therefore SEEDED FROM THE
//! REAL BUCKET — identical bytes, identical etags — and the only thing
//! that changes between the real-S3 arm and this one is who serves them.
//!
//! WHAT IT DELIBERATELY DOES NOT REPRODUCE, and what that costs: no TLS
//! (so the client's decrypt cost is removed, not measured), loopback RTT
//! ~0.05ms instead of ~1ms, and loopback MTU 65536 instead of 9001. This
//! arm therefore CANNOT be compared against the real-S3 numbers. It
//! answers two narrower questions: can lean's client code exceed its
//! observed ceiling at all, and does the parallelism inversion (more
//! streams making throughput WORSE) reproduce with no network present.
//!
//! /__stats is the point of the whole thing: a byte counter the client
//! does not control. If lean reports 400 MiB/s and the server says it
//! pushed 1200, the client's timer is measuring something other than
//! transfer, and that is a finding rather than a rounding error.
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

const LAST_MOD_HTTP: &str = "Mon, 01 Sep 2025 00:00:00 GMT";
const LAST_MOD_ISO: &str = "2025-09-01T00:00:00.000Z";

struct Obj {
    body: Bytes,
    etag: String, // stored UNQUOTED; quoted on the way out, as S3 does
}

struct State {
    objs: RwLock<HashMap<String, Obj>>,
    bytes_out: AtomicU64,
    reqs: AtomicU64,
    range_reqs: AtomicU64,
}

fn pct_decode(s: &str) -> String {
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

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn query(q: Option<&str>) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for kv in q.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        m.insert(pct_decode(k), pct_decode(v));
    }
    m
}

/// S3 returns QUOTED entity-tags. Both sides must be compared with the
/// quotes off — the same rule the gateway learned the hard way.
fn norm_etag(v: &str) -> &str {
    v.trim().trim_matches('"')
}

fn err(status: StatusCode, code: &str) -> Response<Full<Bytes>> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{}</Code></Error>",
        code
    );
    Response::builder()
        .status(status)
        .header("content-type", "application/xml")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

async fn handle(req: Request<Incoming>, st: Arc<State>) -> Result<Response<Full<Bytes>>, Infallible> {
    st.reqs.fetch_add(1, Ordering::Relaxed);
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let q = query(req.uri().query());
    let headers = req.headers().clone();

    if path == "/__stats" {
        let s = format!(
            "bytes_out {}\nreqs {}\nrange_reqs {}\nobjects {}\n",
            st.bytes_out.load(Ordering::Relaxed),
            st.reqs.load(Ordering::Relaxed),
            st.range_reqs.load(Ordering::Relaxed),
            st.objs.read().unwrap().len()
        );
        return Ok(Response::new(Full::new(Bytes::from(s))));
    }

    // Path-style addressing: /{bucket}/{key...}. flint-store forces
    // path-style whenever an endpoint override is set (s3.rs:95), so
    // virtual-host style never arrives here.
    let trimmed = path.trim_start_matches('/');
    let (_bucket, raw_key) = match trimmed.split_once('/') {
        Some((b, k)) => (b, k),
        None => (trimmed, ""),
    };
    let key = pct_decode(raw_key);

    // ListObjectsV2 — lean lists by plain prefix with no delimiter
    // (s3.rs:650), so delimiter/CommonPrefixes are not modelled.
    if method == Method::GET && key.is_empty() && q.get("list-type").map(|v| v == "2").unwrap_or(false) {
        let prefix = q.get("prefix").cloned().unwrap_or_default();
        let max_keys: usize = q.get("max-keys").and_then(|v| v.parse().ok()).unwrap_or(1000);
        let after = q.get("continuation-token").cloned().unwrap_or_default();
        let objs = st.objs.read().unwrap();
        let mut keys: Vec<&String> = objs
            .keys()
            .filter(|k| k.starts_with(&prefix) && (after.is_empty() || **k > after))
            .collect();
        keys.sort();
        let truncated = keys.len() > max_keys;
        let page: Vec<&&String> = keys.iter().take(max_keys).collect();
        let next = page.last().map(|k| (**k).clone()).unwrap_or_default();
        let mut xml = String::with_capacity(page.len() * 200 + 512);
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
        xml.push_str(&format!(
            "<Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{}</MaxKeys><IsTruncated>{}</IsTruncated>",
            xml_esc(_bucket), xml_esc(&prefix), page.len(), max_keys, truncated
        ));
        if truncated {
            xml.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", xml_esc(&next)));
        }
        for k in page {
            let o = &objs[*k];
            xml.push_str(&format!(
                "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                xml_esc(k), LAST_MOD_ISO, xml_esc(&o.etag), o.body.len()
            ));
        }
        xml.push_str("</ListBucketResult>");
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(Full::new(Bytes::from(xml)))
            .unwrap());
    }

    match method {
        Method::GET | Method::HEAD => {
            let (body, etag, total) = {
                let objs = st.objs.read().unwrap();
                match objs.get(&key) {
                    Some(o) => (o.body.clone(), o.etag.clone(), o.body.len()),
                    None => return Ok(err(StatusCode::NOT_FOUND, "NoSuchKey")),
                }
            };
            // The If-Match that checkout relies on. A fake that ignored
            // this would let a stale-etag bug through silently.
            if let Some(m) = headers.get("if-match").and_then(|v| v.to_str().ok()) {
                if norm_etag(m) != norm_etag(&etag) {
                    return Ok(err(StatusCode::PRECONDITION_FAILED, "PreconditionFailed"));
                }
            }
            let mut status = StatusCode::OK;
            let mut slice = body;
            let mut content_range = None;
            if let Some(r) = headers.get("range").and_then(|v| v.to_str().ok()) {
                st.range_reqs.fetch_add(1, Ordering::Relaxed);
                if let Some(spec) = r.strip_prefix("bytes=") {
                    let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                    let start: usize = a.parse().unwrap_or(0);
                    let end: usize = b.parse().unwrap_or(total.saturating_sub(1)).min(total.saturating_sub(1));
                    if start > end || start >= total {
                        return Ok(err(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange"));
                    }
                    slice = slice.slice(start..=end); // zero-copy
                    content_range = Some(format!("bytes {}-{}/{}", start, end, total));
                    status = StatusCode::PARTIAL_CONTENT;
                }
            }
            let n = slice.len() as u64;
            let mut b = Response::builder()
                .status(status)
                .header("etag", format!("\"{}\"", etag))
                .header("last-modified", LAST_MOD_HTTP)
                .header("accept-ranges", "bytes")
                .header("content-length", n.to_string())
                .header("x-amz-request-id", "FAKE000000000001");
            if let Some(cr) = content_range {
                b = b.header("content-range", cr);
            }
            if method == Method::HEAD {
                return Ok(b.body(Full::new(Bytes::new())).unwrap());
            }
            st.bytes_out.fetch_add(n, Ordering::Relaxed);
            Ok(b.body(Full::new(slice)).unwrap())
        }
        Method::PUT => {
            let body = req.into_body().collect().await.map(|c| c.to_bytes()).unwrap_or_default();
            // Content-addressed so a re-PUT of identical bytes is stable.
            let mut h: u64 = 0xcbf29ce484222325;
            for c in body.iter() {
                h ^= *c as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            let etag = format!("{:016x}{:016x}", h, body.len() as u64);
            st.objs.write().unwrap().insert(key, Obj { body, etag: etag.clone() });
            Ok(Response::builder()
                .status(200)
                .header("etag", format!("\"{}\"", etag))
                .body(Full::new(Bytes::new()))
                .unwrap())
        }
        Method::DELETE => {
            st.objs.write().unwrap().remove(&key);
            Ok(Response::builder().status(204).body(Full::new(Bytes::new())).unwrap())
        }
        _ => Ok(err(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed")),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    let mut seed_dir = String::new();
    let mut etag_file = String::new();
    let mut key_prefix = String::new();
    let mut addr = "127.0.0.1:9000".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seed-dir" => seed_dir = args.next().unwrap_or_default(),
            "--etags" => etag_file = args.next().unwrap_or_default(),
            "--key-prefix" => key_prefix = args.next().unwrap_or_default(),
            "--listen" => addr = args.next().unwrap_or_default(),
            _ => {}
        }
    }

    // key -> etag, taken from the REAL bucket. Without this the If-Match
    // on every GET fails and the arm is vacuous.
    let mut etags: HashMap<String, String> = HashMap::new();
    if !etag_file.is_empty() {
        for line in std::fs::read_to_string(&etag_file)?.lines() {
            if let Some((k, e)) = line.split_once('\t') {
                etags.insert(k.to_string(), norm_etag(e).to_string());
            }
        }
    }

    let mut objs: HashMap<String, Obj> = HashMap::new();
    let mut loaded = 0u64;
    let mut missing_etag = 0u64;
    if !seed_dir.is_empty() {
        let mut stack = vec![std::path::PathBuf::from(&seed_dir)];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)? {
                let e = e?;
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let rel = p.strip_prefix(&seed_dir).unwrap().to_string_lossy().to_string();
                let key = if key_prefix.is_empty() { rel.clone() } else { format!("{}{}", key_prefix, rel) };
                let body = Bytes::from(std::fs::read(&p)?);
                loaded += body.len() as u64;
                let etag = match etags.get(&key) {
                    Some(t) => t.clone(),
                    None => {
                        missing_etag += 1;
                        continue; // NEVER invent one: a wrong etag 412s and looks like a lean bug
                    }
                };
                objs.insert(key, Obj { body, etag });
            }
        }
    }

    // A seeding failure must be LOUD. A fake serving 0 objects would
    // time as an instant win.
    eprintln!(
        "fakes3: {} objects, {:.2} GiB resident, {} skipped for want of a real etag",
        objs.len(),
        loaded as f64 / 1073741824.0,
        missing_etag
    );
    if objs.is_empty() {
        eprintln!("fakes3: REFUSING to serve an empty store — every read would be a 404 timed as success");
        std::process::exit(2);
    }
    if missing_etag > 0 {
        eprintln!("fakes3: WARNING {} objects have no etag and are NOT served; checkout will 404 on them", missing_etag);
    }

    let st = Arc::new(State {
        objs: RwLock::new(objs),
        bytes_out: AtomicU64::new(0),
        reqs: AtomicU64::new(0),
        range_reqs: AtomicU64::new(0),
    });

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("fakes3: listening on http://{}", addr);
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true).ok();
        let st = st.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, service_fn(move |r| handle(r, st.clone())))
                .await;
        });
    }
}

use hyper::server::conn::http1;
