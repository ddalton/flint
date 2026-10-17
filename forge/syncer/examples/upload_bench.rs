//! One pack upload through forge's own path (`packio::upload_file`) to a
//! real bucket, timed — the measurement behind
//! FLINT_FORGE_UPLOAD_PART_PARALLELISM. The store is built exactly as
//! the syncer builds it, from the same two variables, so an arm differs
//! from another only in the knob.
//!
//!   FLINT_FORGE_BUCKET, FLINT_FORGE_ENDPOINT (optional)
//!   FLINT_FORGE_UPLOAD_PART_PARALLELISM (8), FLINT_FORGE_UPLOAD_INFLIGHT_MB (512; 0 = no gate)
//!
//!   upload_bench <file> <key>
//!
//! Prints one line: `par=<n> inflight_mb=<n> bytes=<n> ms=<n> mib_s=<x>`.
//! Peak RSS is the caller's to take (`/usr/bin/time -f %M`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use flint_forge::packio::upload_file;
use flint_store::s3::S3Store;
use flint_store::ObjectStore;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: upload_bench <file> <key>");
        std::process::exit(2);
    }
    let path = PathBuf::from(&args[1]);
    let key = &args[2];
    let bucket = std::env::var("FLINT_FORGE_BUCKET").expect("FLINT_FORGE_BUCKET");
    let endpoint = std::env::var("FLINT_FORGE_ENDPOINT").ok();
    let par = env_u64("FLINT_FORGE_UPLOAD_PART_PARALLELISM", 8).max(1) as usize;
    let inflight_mb = env_u64("FLINT_FORGE_UPLOAD_INFLIGHT_MB", 512);

    let store = S3Store::connect(bucket, endpoint)
        .await
        .expect("store connect")
        .with_part_parallelism(par)
        .with_upload_inflight_max_bytes(inflight_mb * 1024 * 1024);
    let store = Arc::new(store) as Arc<dyn ObjectStore>;

    let bytes = std::fs::metadata(&path).expect("stat").len();
    let t = Instant::now();
    upload_file(store.as_ref(), key, &path, 1, None).await.expect("upload");
    let ms = t.elapsed().as_millis();
    let mib_s = bytes as f64 / (1024.0 * 1024.0) / (ms as f64 / 1000.0);
    println!("par={par} inflight_mb={inflight_mb} bytes={bytes} ms={ms} mib_s={mib_s:.1}");
}
