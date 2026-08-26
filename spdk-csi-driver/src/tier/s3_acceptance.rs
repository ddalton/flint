#[cfg(test)]
mod tests {
    use crate::tier::store::s3::*;
    use crate::tier::store::*;
    use crate::tier::arbitrate::{arbitrate, IntentProbe, Verdict};
    use bytes::Bytes;

    /// REAL-S3 acceptance drill — the step-4 gate ("tested against
    /// real S3, including induced 412s in both self-torn and
    /// genuinely-foreign flavors"). Ignored by default; needs a bucket
    /// and ambient credentials:
    ///
    ///   FLINT_TIER_S3_VERIFY_BUCKET=<bucket> AWS_PROFILE=<profile> \
    ///     cargo test --release --lib tier::store::s3 -- --ignored --nocapture
    ///
    /// Everything happens under a fresh `flint-tier-verify/<uuid>/`
    /// prefix and is deleted at the end; the bootstrap's lifecycle
    /// rule (scoped to that prefix) is the only thing that outlives
    /// the run on a shared bucket. Optional:
    /// FLINT_TIER_S3_VERIFY_ENDPOINT for a MinIO rig.
    #[tokio::test]
    #[ignore = "touches a real S3 bucket (set FLINT_TIER_S3_VERIFY_BUCKET)"]
    async fn real_s3_acceptance() {
        let Ok(bucket) = std::env::var("FLINT_TIER_S3_VERIFY_BUCKET") else {
            println!("SKIP: FLINT_TIER_S3_VERIFY_BUCKET not set");
            return;
        };
        let endpoint = std::env::var("FLINT_TIER_S3_VERIFY_ENDPOINT").ok();
        let store = S3Store::connect(bucket.clone(), endpoint).await.unwrap();
        let prefix = format!("flint-tier-verify/{}/", uuid::Uuid::new_v4());
        println!("=== real-S3 drill on s3://{}/{} ===", bucket, prefix);

        // Raw client for the out-of-band roles (foreign writer,
        // crashed uploader).
        let raw = {
            let base = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
            aws_sdk_s3::Client::new(&base)
        };

        // ── A9 bootstrap ─────────────────────────────────────────────
        let report = store.bootstrap(&prefix).await.unwrap();
        println!("bootstrap notes: {:?}", report.notes);
        println!("bootstrap warnings: {:?}", report.warnings);
        assert!(report.ok(), "bootstrap errors: {:?}", report.errors);

        // ── conditional PUT, both flavors, against real S3 ──────────
        let key = format!("{}file.bin", prefix);
        let gen1_body = Bytes::from(vec![0xA5u8; 12 * 1024 * 1024]);
        let gen1_crc = crc64_nvme(&gen1_body);
        let stamps1 = GenerationStamps { generation: 1, epoch: 1, flush_uuid: "uuid-g1".into(), boundary_source: None, posix: None };
        let gen1 = store
            .put_whole(&key, gen1_body.clone(), &PutCondition::IfNoneMatchAny, &stamps1, gen1_crc)
            .await
            .unwrap();
        println!("gen1 published: etag={} crc={:?}", gen1.etag, gen1.crc64_b64);
        assert_eq!(gen1.crc64_b64.as_deref(), Some(crc64_to_b64(gen1_crc).as_str()),
            "S3's full-object CRC64NVME must equal local truth");

        let err = store
            .put_whole(&key, gen1_body.clone(), &PutCondition::IfNoneMatchAny, &stamps1, gen1_crc)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "If-None-Match:* on an existing key must 412, got {:?}", err);
        println!("412 create-race flavor: OK");

        let err = store
            .put_whole(&key, gen1_body.clone(), &PutCondition::IfMatch("\"stale\"".into()), &stamps1, gen1_crc)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
        println!("412 stale-If-Match flavor: OK");

        // Wrong claimed checksum must FAIL the publish server-side.
        let err = store
            .put_whole(&format!("{}bad-crc", prefix), Bytes::from_static(b"x"),
                &PutCondition::IfNoneMatchAny, &stamps1, 0xDEAD)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::ChecksumMismatch(_) | StoreError::Other(_)),
            "a wrong CRC must not publish, got {:?}", err);
        println!("server-side checksum rejection: OK ({:?})", err);

        // HEAD: stamps roundtrip + checksum surfaced.
        let h = store.head(&key).await.unwrap();
        assert_eq!(GenerationStamps::from_meta(&h.meta), Some(stamps1.clone()));
        assert_eq!(h.size, gen1_body.len() as u64);
        println!("HEAD stamps + size roundtrip: OK");

        // Guarded GET (the hydration posture's input).
        let err = store.get_whole(&key, Some("\"stale\"")).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
        let (_, got) = store.get_whole(&key, Some(&gen1.etag)).await.unwrap();
        assert_eq!(got, gen1_body);
        println!("guarded GET both flavors: OK");

        // ── compose: MPU + UploadPartCopy + conditional Complete ────
        const MB: u64 = 1024 * 1024;
        let dir = tempfile::TempDir::new().unwrap();
        let local = dir.path().join("local.bin");
        let mut gen2_content = vec![0xA5u8; (12 * MB) as usize];
        gen2_content[..(6 * MB) as usize].fill(0x5A); // first 6 MiB dirty
        std::fs::write(&local, &gen2_content).unwrap();
        let gen2_crc = crc64_nvme(&gen2_content);
        let stamps2 = GenerationStamps { generation: 2, epoch: 1, flush_uuid: "uuid-g2".into(), boundary_source: None, posix: None };
        let spec = ComposeSpec {
            key: &key,
            local_path: &local,
            parts: vec![
                PartSource::Local { offset: 0, len: 6 * MB },
                PartSource::BaseCopy { offset: 6 * MB, len: 6 * MB },
            ],
            base_key: None,
            base_etag: Some(gen1.etag.clone()),
            condition: PutCondition::IfMatch(gen1.etag.clone()),
            stamps: stamps2.clone(),
            crc64: gen2_crc,
        };
        let gen2 = store.compose_generation(&spec).await.unwrap();
        println!("gen2 composed: etag={} crc={:?}", gen2.etag, gen2.crc64_b64);
        assert!(gen2.etag.contains('-'), "multipart etag should carry a part count: {}", gen2.etag);
        assert_eq!(gen2.crc64_b64.as_deref(), Some(crc64_to_b64(gen2_crc).as_str()),
            "FULL_OBJECT CRC across Local + BaseCopy parts must equal local truth");
        let (_, got2) = store.get_whole(&key, Some(&gen2.etag)).await.unwrap();
        assert_eq!(got2.as_ref(), gen2_content.as_slice(), "composed bytes must equal local truth");
        println!("compose content + checksum verification: OK");

        // ── the self-torn drill ─────────────────────────────────────
        // The first compose plays "the torn Complete that landed"; this
        // naive retry (same intent, same If-Match on gen1) must hit a
        // REAL 412 (at part-copy or Complete), abort its MPU, and
        // arbitration must ADOPT — never re-upload, never page anyone.
        let err = store.compose_generation(&spec).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "the retry must be fenced with 412, got {:?}", err);
        assert!(store.list_uploads(&prefix).await.unwrap().is_empty(),
            "the fenced compose must have aborted its MPU (A9)");
        let verdict = arbitrate(&store, &IntentProbe {
            key: &key, to_gen: 2, flush_uuid: "uuid-g2", base_etag: Some(&gen1.etag),
        }).await.unwrap();
        match &verdict {
            Verdict::AdoptOwn(m) => assert_eq!(m.etag, gen2.etag),
            other => panic!("self-torn must adopt, got {:?}", other),
        }
        println!("self-torn 412 → abort → AdoptOwn: OK");

        // ── the genuinely-foreign drill ─────────────────────────────
        // An outside writer (raw client, no stamps) replaces the
        // object; the guarded publish 412s; arbitration names it
        // Foreign.
        raw.put_object()
            .bucket(&bucket)
            .key(&key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"foreign bytes"))
            .send()
            .await
            .expect("raw foreign put");
        let err = store.compose_generation(&ComposeSpec {
            condition: PutCondition::IfMatch(gen2.etag.clone()),
            base_etag: Some(gen2.etag.clone()),
            stamps: GenerationStamps { generation: 3, epoch: 1, flush_uuid: "uuid-g3".into(), boundary_source: None, posix: None },
            ..spec.clone()
        }).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "publish over a foreign overwrite must 412, got {:?}", err);
        let verdict = arbitrate(&store, &IntentProbe {
            key: &key, to_gen: 3, flush_uuid: "uuid-g3", base_etag: Some(&gen2.etag),
        }).await.unwrap();
        assert!(matches!(verdict, Verdict::Foreign(Some(_))),
            "foreign overwrite must be named foreign, got {:?}", verdict);
        println!("foreign 412 → Foreign verdict: OK");

        // ── crashed-uploader sweep drill ────────────────────────────
        let orphan_key = format!("{}orphan.bin", prefix);
        let created = raw.create_multipart_upload()
            .bucket(&bucket).key(&orphan_key).send().await.expect("raw MPU create");
        let orphan_id = created.upload_id().unwrap().to_string();
        let pending = store.list_uploads(&prefix).await.unwrap();
        assert!(pending.iter().any(|p| p.upload_id == orphan_id),
            "the sweep must see the crashed assembly");
        store.abort_upload(&orphan_key, &orphan_id).await.unwrap();
        assert!(store.list_uploads(&prefix).await.unwrap().is_empty());
        store.abort_upload(&orphan_key, &orphan_id).await.unwrap(); // idempotent
        println!("MPU sweep (list → abort → idempotent re-abort): OK");

        // ── epoch CAS drill ─────────────────────────────────────────
        let ekey = format!("{}.flint-epoch", prefix);
        let l1 = store.epoch_acquire(&ekey, "hub-a", None).await.unwrap();
        assert_eq!(l1.epoch, 1);
        let err = store.epoch_acquire(&ekey, "hub-b", None).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "blind second claim must 412, got {:?}", err);
        let l1b = store.epoch_renew(&ekey, &l1, None).await.unwrap();
        assert_ne!(l1b.token, l1.token, "renew must rotate the CAS token");
        let err = store.epoch_renew(&ekey, &l1, None).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "stale renew must 412");
        let observed = store.epoch_read(&ekey).await.unwrap().unwrap();
        assert_eq!(observed.holder_id, "hub-a");
        assert!(observed.last_renew_unix.is_some(), "S3's clock must stamp the lease");
        let l2 = store.epoch_acquire(&ekey, "hub-b", Some(&observed)).await.unwrap();
        assert_eq!(l2.epoch, 2);
        let err = store.epoch_renew(&ekey, &l1b, None).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "deposed renew must 412");
        store.epoch_release(&ekey, &l2).await.unwrap();
        assert!(store.epoch_read(&ekey).await.unwrap().is_none());
        println!("epoch CAS lifecycle (claim/renew/depose/release): OK");

        // ── cleanup ─────────────────────────────────────────────────
        for o in store.list(&prefix).await.unwrap() {
            store.delete(&o.key).await.unwrap();
        }
        assert!(store.list(&prefix).await.unwrap().is_empty());
        println!("cleanup: prefix emptied");
        println!("=== real-S3 drill PASSED ===");
    }
}
