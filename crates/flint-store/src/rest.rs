//! What the hand-written REST backends share (Azure Blob, GCS): the
//! HTTPS client, the content ETag's digits, and the encodings both speak.
//! Neither carries a provider SDK (docs/plans/lean-azure-gcs-backends-plan-2026-11-10.md, D1).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use sha2::{Digest, Sha256};

use crate::{StoreError, StoreResult};

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
}
