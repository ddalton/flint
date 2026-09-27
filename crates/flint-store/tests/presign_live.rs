//! A presigned PUT against a LIVE S3-compatible server: does the server
//! enforce what `S3Store::presign_put` signs? Not a unit test (`#[ignore]`,
//! and it needs a server): the memory double enforces the signed headers
//! because it was written to, and whether MinIO, RustFS, Ozone or S3 do is
//! a property of each backend that only a request to it can show.
//!
//! The URL is redeemed with curl, the way git-lfs does it: a plain PUT
//! carrying the upload action's headers.
//!
//! FLINT_PRESIGN_ENDPOINT=http://host:port FLINT_PRESIGN_BUCKET=b \
//! FLINT_PRESIGN_KEY=... FLINT_PRESIGN_SECRET=... \
//!   cargo test -p flint-store --features s3 --test presign_live -- --ignored --nocapture
#![cfg(feature = "s3")]

use flint_store::s3::S3Store;
use flint_store::ObjectStore;
use sha2::{Digest, Sha256};
use std::process::Command;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"))
}

/// PUT `body` to `url` with `headers`, leaving out any header named in
/// `drop`; the HTTP status and the response body.
fn put(url: &str, headers: &std::collections::BTreeMap<String, String>, drop: &[&str], body: &[u8]) -> (u16, String) {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("body");
    std::fs::write(&f, body).unwrap();
    let mut c = Command::new("curl");
    c.args(["-sS", "-X", "PUT", "-o", "-", "-w", "\n%{http_code}", "--data-binary"])
        .arg(format!("@{}", f.display()));
    for (k, v) in headers {
        if !drop.iter().any(|d| d.eq_ignore_ascii_case(k)) {
            c.arg("-H").arg(format!("{k}: {v}"));
        }
    }
    let out = c.arg(url).output().expect("curl");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (resp, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    (code.trim().parse().unwrap_or(0), resp.chars().take(300).collect())
}

#[tokio::test]
#[ignore]
async fn a_presigned_put_is_enforced_by_the_server() {
    let store = S3Store::with_credentials(
        env("FLINT_PRESIGN_BUCKET"),
        env("FLINT_PRESIGN_ENDPOINT"),
        "us-east-1",
        env("FLINT_PRESIGN_KEY"),
        env("FLINT_PRESIGN_SECRET"),
    )
    .unwrap();
    let run = uuid::Uuid::new_v4();
    let good = format!("the bytes the oid names, run {run}").into_bytes();
    let oid: [u8; 32] = Sha256::digest(&good).into();
    let key = |leg: &str| format!("lfs-presign-{run}/{leg}");
    let signed = |p: &flint_store::PresignedPut| {
        p.headers.keys().cloned().collect::<Vec<_>>().join(",")
    };
    let mut verdicts = Vec::new();
    // A refusal counts only if the server JUDGED the request: a 404 (no
    // such bucket) or a 5xx refuses every leg alike, and the first run of
    // this test counted exactly that as enforcement.
    let mut check = |leg: &str, want_ok: bool, got: (u16, String)| {
        let ok = (200..300).contains(&got.0);
        let judged = got.0 != 404 && got.0 < 500 && got.0 != 0;
        let pass = if want_ok { ok } else { !ok && judged };
        println!("LEG {leg}: HTTP {} ({}) {}", got.0, if pass { "as required" } else { "NOT ENFORCED" }, got.1.replace('\n', " "));
        verdicts.push((leg.to_string(), pass));
    };

    // 1. The honest upload: the named bytes, every signed header.
    let p = store.presign_put(&key("honest"), 600, &oid).await.unwrap();
    println!("signed headers: {}", signed(&p));
    let honest = put(&p.url, &p.headers, &[], &good);
    assert!((200..300).contains(&honest.0), "PREMISE: the honest upload must land, got {honest:?}");
    check("honest upload lands", true, honest);
    // 2. The same URL again: create-only.
    check("second redemption refused", false, put(&p.url, &p.headers, &[], &good));
    // 3. Other bytes under the oid's checksum.
    let p = store.presign_put(&key("wrong-bytes"), 600, &oid).await.unwrap();
    check("wrong bytes refused", false, put(&p.url, &p.headers, &[], b"not the bytes the oid names"));
    // 4 and 5. A client that drops a signed header.
    let p = store.presign_put(&key("no-checksum"), 600, &oid).await.unwrap();
    check("checksum header dropped refused", false, put(&p.url, &p.headers, &["x-amz-checksum-sha256"], b"other bytes, no checksum"));
    let p = store.presign_put(&key("no-if-none-match"), 600, &oid).await.unwrap();
    check("if-none-match dropped refused", false, put(&p.url, &p.headers, &["if-none-match"], &good));

    // What is stored: the honest bytes, and nothing under the refused legs.
    let (_, got) = store.get_whole(&key("honest"), None).await.unwrap();
    assert_eq!(&got[..], &good[..], "the honest leg stored other bytes");
    for leg in ["wrong-bytes", "no-checksum"] {
        let stored = store.get_whole(&key(leg), None).await;
        println!("STORED {leg}: {}", if stored.is_ok() { "YES" } else { "no" });
        verdicts.push((format!("{leg} stored nothing"), stored.is_err()));
    }
    let failed: Vec<_> = verdicts.iter().filter(|(_, p)| !p).map(|(l, _)| l.as_str()).collect();
    assert!(failed.is_empty(), "the server did not enforce: {failed:?}");
}
