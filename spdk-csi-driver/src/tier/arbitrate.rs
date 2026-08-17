//! HEAD-based publish arbitration — L2 step 4 (design review A6).
//!
//! A 412 during publish, or an interrupted flush intent found at
//! startup, is ALWAYS arbitrated by HEAD against the stamps — never
//! routed to an operator runbook (the review proved a bare-412 runbook
//! is a data-loss procedure: the design's own crash window produces
//! 412s in normal operation).
//!
//! The posture split (A6): everything this module handles sits on the
//! PUBLISH path, where 412 is an internal fencing event and LOCAL
//! truth wins. Only the hydration-GET's 412 (step 11) is
//! foreign-overwrite, where S3 wins — that posture does not live here.

use super::store::{GenerationStamps, ObjectMeta, ObjectStore, StoreError, StoreResult};

/// The durable flush intent being arbitrated (mirror of the backend's
/// FlushIntentRecord, borrowed).
#[derive(Debug, Clone, Copy)]
pub struct IntentProbe<'a> {
    pub key: &'a str,
    pub to_gen: u64,
    pub flush_uuid: &'a str,
    /// ETag of generation g (None = this intent was creating the first
    /// generation of a new key).
    pub base_etag: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The object at the key is OUR to_gen publish (stamp match): the
    /// Complete landed but its response was lost. Adopt it — record
    /// the meta in the generation row, clear the intent, do NOT
    /// re-upload.
    AdoptOwn(ObjectMeta),
    /// The base generation is still intact: the Complete never landed.
    /// Abort the intent's MPU (if any) and re-flush from local truth.
    RetryFromBase,
    /// Someone else's bytes (foreign stamps, no stamps, or the object
    /// vanished). Publish-path policy is LOCAL WINS (A6) — the flusher
    /// re-flushes guarded on the CURRENT state; a deliberate outside
    /// write is step 12's import-refresh, not this path.
    Foreign(Option<ObjectMeta>),
}

/// Arbitrate one interrupted/412'd publish intent by HEAD.
pub async fn arbitrate(
    store: &dyn ObjectStore,
    probe: &IntentProbe<'_>,
) -> StoreResult<Verdict> {
    let meta = match store.head(probe.key).await {
        Ok(m) => m,
        // The key is gone entirely. With a base generation that
        // existed, that is a foreign deletion; for a first-generation
        // intent it just means our create never landed.
        Err(StoreError::NotFound(_)) => {
            return Ok(if probe.base_etag.is_none() {
                Verdict::RetryFromBase
            } else {
                Verdict::Foreign(None)
            });
        }
        Err(e) => return Err(e),
    };

    // Own stamp at to_gen ⇒ torn own flush.
    if let Some(stamps) = GenerationStamps::from_meta(&meta.meta) {
        if stamps.flush_uuid == probe.flush_uuid && stamps.generation == probe.to_gen {
            return Ok(Verdict::AdoptOwn(meta));
        }
    }

    // Base generation still in place ⇒ our Complete never landed.
    if probe.base_etag == Some(meta.etag.as_str()) {
        return Ok(Verdict::RetryFromBase);
    }

    Ok(Verdict::Foreign(Some(meta)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::store::memory::MemoryStore;
    use crate::tier::store::{
        crc64_nvme, ComposeSpec, PartSource, PutCondition,
    };
    use bytes::Bytes;

    fn stamps(generation: u64, uuid: &str) -> GenerationStamps {
        GenerationStamps { generation, epoch: 1, flush_uuid: uuid.into() }
    }

    async fn seed_base(store: &MemoryStore, key: &str) -> ObjectMeta {
        let body = Bytes::from_static(b"generation-1 bytes");
        store
            .put_whole(key, body.clone(), &PutCondition::IfNoneMatchAny, &stamps(1, "u-1"), crc64_nvme(&body))
            .await
            .unwrap()
    }

    fn write_local(dir: &std::path::Path, content: &[u8]) -> std::path::PathBuf {
        let p = dir.join("local.bin");
        std::fs::write(&p, content).unwrap();
        p
    }

    /// The self-torn flavor, induced end to end: compose publishes but
    /// the response is lost; the retry's If-Match 412s; arbitration
    /// must ADOPT, not re-upload and not page an operator.
    #[tokio::test]
    async fn torn_own_complete_is_adopted() {
        let store = MemoryStore::new();
        let base = seed_base(&store, "v/f").await;
        let dir = tempfile::TempDir::new().unwrap();
        let content = b"generation-2 bytes!";
        let local = write_local(dir.path(), content);
        let spec = ComposeSpec {
            key: "v/f",
            local_path: &local,
            parts: vec![PartSource::Local { offset: 0, len: content.len() as u64 }],
            base_etag: Some(base.etag.clone()),
            condition: PutCondition::IfMatch(base.etag.clone()),
            stamps: stamps(2, "u-2"),
            crc64: crc64_nvme(content),
        };

        store.inject_torn_complete();
        let err = store.compose_generation(&spec).await.unwrap_err();
        assert!(matches!(err, StoreError::Other(_)), "the tear presents as a network error");

        // The naive retry hits the fencing 412 (the object is already
        // at generation 2 — its etag is no longer the base's).
        let retry = store.compose_generation(&spec).await.unwrap_err();
        assert!(matches!(retry, StoreError::PreconditionFailed(_)));

        let verdict = arbitrate(
            &store,
            &IntentProbe { key: "v/f", to_gen: 2, flush_uuid: "u-2", base_etag: Some(&base.etag) },
        )
        .await
        .unwrap();
        match verdict {
            Verdict::AdoptOwn(meta) => {
                assert_eq!(meta.size, content.len() as u64);
                assert_eq!(
                    meta.crc64_b64.as_deref(),
                    Some(crate::tier::store::crc64_to_b64(crc64_nvme(content)).as_str()),
                    "the adopted object must carry the full-object checksum"
                );
            }
            other => panic!("expected AdoptOwn, got {:?}", other),
        }
    }

    /// The crash-before-Complete flavor: nothing published, base
    /// intact, an orphan MPU pending — verdict is RetryFromBase and
    /// the A9 sweep can see + abort the orphan.
    #[tokio::test]
    async fn crash_before_complete_retries_from_base_and_sweep_reaps() {
        let store = MemoryStore::new();
        let base = seed_base(&store, "v/g").await;
        let dir = tempfile::TempDir::new().unwrap();
        let content = b"generation-2 bytes!";
        let local = write_local(dir.path(), content);
        let spec = ComposeSpec {
            key: "v/g",
            local_path: &local,
            parts: vec![PartSource::Local { offset: 0, len: content.len() as u64 }],
            base_etag: Some(base.etag.clone()),
            condition: PutCondition::IfMatch(base.etag.clone()),
            stamps: stamps(2, "u-9"),
            crc64: crc64_nvme(content),
        };
        store.inject_crash_before_complete();
        store.compose_generation(&spec).await.unwrap_err();

        let verdict = arbitrate(
            &store,
            &IntentProbe { key: "v/g", to_gen: 2, flush_uuid: "u-9", base_etag: Some(&base.etag) },
        )
        .await
        .unwrap();
        assert_eq!(verdict, Verdict::RetryFromBase);

        // The orphan is visible to the hygiene surface and abortable;
        // after the sweep the re-flush succeeds.
        let pending = store.list_uploads("v/").await.unwrap();
        assert_eq!(pending.len(), 1, "the crashed assembly must be visible to the sweep");
        store.abort_upload(&pending[0].key, &pending[0].upload_id).await.unwrap();
        assert!(store.list_uploads("v/").await.unwrap().is_empty());
        store.compose_generation(&spec).await.unwrap();
    }

    /// The genuinely-foreign flavor: an outside writer replaced the
    /// object. The verdict must be Foreign — never adopt, never treat
    /// as own.
    #[tokio::test]
    async fn foreign_overwrite_is_named_foreign() {
        let store = MemoryStore::new();
        let base = seed_base(&store, "v/h").await;
        // An outside writer (no flint stamps).
        store.raw_put("v/h", Bytes::from_static(b"foreign bytes"), vec![]);

        let verdict = arbitrate(
            &store,
            &IntentProbe { key: "v/h", to_gen: 2, flush_uuid: "u-3", base_etag: Some(&base.etag) },
        )
        .await
        .unwrap();
        match verdict {
            Verdict::Foreign(Some(meta)) => assert_eq!(meta.size, 13),
            other => panic!("expected Foreign(Some), got {:?}", other),
        }

        // A foreign DELETION of an existing generation is also foreign.
        store.delete("v/h").await.unwrap();
        let verdict = arbitrate(
            &store,
            &IntentProbe { key: "v/h", to_gen: 2, flush_uuid: "u-3", base_etag: Some(&base.etag) },
        )
        .await
        .unwrap();
        assert_eq!(verdict, Verdict::Foreign(None));

        // But an absent object for a FIRST-generation intent is just
        // "our create never landed": retry.
        let verdict = arbitrate(
            &store,
            &IntentProbe { key: "v/new", to_gen: 1, flush_uuid: "u-4", base_etag: None },
        )
        .await
        .unwrap();
        assert_eq!(verdict, Verdict::RetryFromBase);
    }

    /// A torn flush from an OLDER intent (same file, different uuid)
    /// must not be adopted by a newer intent — the uuid is the
    /// identity, not the generation number alone.
    #[tokio::test]
    async fn stamp_match_requires_the_uuid_not_just_the_generation() {
        let store = MemoryStore::new();
        let base = seed_base(&store, "v/i").await;
        // Someone's (or an older incarnation's) gen-2 with a different
        // flush uuid.
        let body = Bytes::from_static(b"gen-2 by other flush");
        store.raw_put(
            "v/i",
            body,
            stamps(2, "u-OLD").to_meta(),
        );
        let verdict = arbitrate(
            &store,
            &IntentProbe { key: "v/i", to_gen: 2, flush_uuid: "u-NEW", base_etag: Some(&base.etag) },
        )
        .await
        .unwrap();
        assert!(
            matches!(verdict, Verdict::Foreign(Some(_))),
            "a stamp with the wrong uuid is NOT ours: {:?}",
            verdict
        );
    }
}
