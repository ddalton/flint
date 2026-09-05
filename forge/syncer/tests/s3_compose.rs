//! The composed upload against a REAL bucket: the checksum the store
//! accumulates from the parts it reads must be the one S3 validates at
//! CompleteMultipartUpload and reports back as the object's
//! FULL_OBJECT CRC-64/NVME. The memory double proves the arithmetic;
//! only a bucket proves that S3 agrees with it — which is the whole
//! point of a server-side check.
//!
//! Skips unless `FLINT_FORGE_S3_BUCKET` is set; credentials and region
//! come from the ambient AWS chain (the scale rig's scoped key works).
//!
//!   FLINT_FORGE_S3_BUCKET=… AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… \
//!   AWS_REGION=us-west-1 cargo test --features s3 --test s3_compose -- --nocapture
#![cfg(feature = "s3")]

use std::sync::Arc;

use flint_store::s3::S3Store;
use flint_store::{crc64_nvme, crc64_to_b64, ObjectStore};

#[tokio::test]
async fn a_composed_pack_carries_the_checksum_s3_computed_from_the_parts() {
    let Ok(bucket) = std::env::var("FLINT_FORGE_S3_BUCKET") else {
        eprintln!("FLINT_FORGE_S3_BUCKET unset; skipping the real-S3 compose check");
        return;
    };
    let endpoint = std::env::var("FLINT_FORGE_S3_ENDPOINT").ok();
    let store = S3Store::connect(bucket, endpoint).await.expect("connect");

    // Three parts of 64 MiB and a short tail, so the grid has a last
    // part below the minimum and the accumulation crosses part edges.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pack-acceptance.pack");
    let size: u64 = 3 * (64 << 20) + 12_345;
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&path).unwrap();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut buf = vec![0u8; 1 << 20];
        let mut left = size;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            for w in buf[..n].chunks_mut(8) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let b = x.to_le_bytes();
                w.copy_from_slice(&b[..w.len()]);
            }
            f.write_all(&buf[..n]).unwrap();
            left -= n as u64;
        }
    }
    let local_crc = crc64_nvme(&std::fs::read(&path).unwrap());

    let key = format!("acceptance/{}/pack-acceptance.pack", uuid::Uuid::new_v4());
    let progress = Arc::new(std::sync::atomic::AtomicU64::new(0));
    flint_forge::packio::upload_file(&store, &key, &path, 1, Some(progress.clone()))
        .await
        .expect("upload");
    assert_eq!(progress.load(std::sync::atomic::Ordering::Relaxed), size, "every part ticked");

    let head = store.head(&key).await.expect("head");
    assert_eq!(head.size, size);
    assert_eq!(
        head.crc64_b64.as_deref(),
        Some(crc64_to_b64(local_crc).as_str()),
        "S3's FULL_OBJECT CRC-64/NVME must be the file's"
    );
    assert!(
        head.etag.trim_matches('"').ends_with("-4"),
        "four parts composed, etag {}",
        head.etag
    );
    eprintln!("composed {} bytes under {key}: etag {} crc {}", size, head.etag, crc64_to_b64(local_crc));
    store.delete(&key).await.expect("delete");
}
