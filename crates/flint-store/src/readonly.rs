//! A store that reads and cannot write: [`ReadOnly`].
//!
//! The consumer is the lean gateway's `Workspace::read_only` (per-user
//! access design §4.6, phase E): a backend that serves a UI to people with
//! read access builds that user's workspace on this wrapper, and the
//! wrapper, not the verbs, is what keeps a write from reaching the bucket.
//! The verbs refuse first, with an honest typed error; this is the layer
//! that still refuses when a caller goes around them (the workspace hands
//! out its store) or when a verb forgets.
//!
//! It is a backstop, not the enforcement. The enforcement is a credential
//! that cannot write (design D1): a backend should build a read-only
//! workspace on a read-only client, and this wrapper then turns what
//! would have been a 403 from the bucket into the same refusal without
//! the round trip.
//!
//! Two rules the implementation keeps, and the tests pin:
//!
//! - **A write never reaches the inner store.** Every write method answers
//!   [`StoreError::Auth`] — the variant for "well-formed, not allowed" —
//!   naming the verb and the key, and sends nothing. A presigned PUT is a
//!   write: it is a credential to write, handed to someone else.
//! - **Every read is forwarded EXPLICITLY, the trait's defaulted ones
//!   included.** A read left to its default would run the trait's
//!   fallback on this wrapper instead of the inner store's override: the
//!   S3 backend's `get_range_segments` would silently go through
//!   `get_range` and its full-size copy, and `get_version` or
//!   `lifecycle_rules` would answer "this backend has no …" for a
//!   backend that has one. `upload_gate` and `request_counts` are
//!   forwarded too: they describe the inner store and send nothing.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{
    gate, BootstrapReport, ComposeSpec, EpochLease, EpochState, GenerationStamps, LifecycleView,
    ListedObject, ListedVersion, ObjectMeta, ObjectStore, PendingUpload, PutCondition,
    RequestCounts, RetentionOutcome, StoreError, StoreResult,
};

/// An [`ObjectStore`] whose writes are refused and whose reads go to the
/// store it wraps. See the module note.
pub struct ReadOnly<S: ?Sized> {
    inner: Arc<S>,
}

impl<S: ObjectStore + ?Sized> ReadOnly<S> {
    /// Wrap `inner`. There is deliberately no accessor back to it: a
    /// holder of the wrapper that could reach the inner store could write.
    pub fn new(inner: Arc<S>) -> Self {
        ReadOnly { inner }
    }
}

fn refuse<T>(verb: &str, key: &str) -> StoreResult<T> {
    Err(StoreError::Auth(format!("read-only store: {verb} {key}")))
}

#[async_trait]
impl<S: ObjectStore + ?Sized> ObjectStore for ReadOnly<S> {
    // ── writes: refused, nothing sent ────────────────────────────────

    async fn put_whole(
        &self,
        key: &str,
        _body: Bytes,
        _condition: &PutCondition,
        _stamps: &GenerationStamps,
        _crc64: u64,
    ) -> StoreResult<ObjectMeta> {
        refuse("put_whole", key)
    }

    async fn copy_object(
        &self,
        _src_key: &str,
        _src_if_match: Option<&str>,
        dst_key: &str,
        _condition: &PutCondition,
        _stamps: &GenerationStamps,
    ) -> StoreResult<ObjectMeta> {
        refuse("copy_object", dst_key)
    }

    async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta> {
        refuse("compose_generation", spec.key)
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        refuse("delete", key)
    }

    async fn delete_if_match(&self, key: &str, _etag: &str) -> StoreResult<()> {
        refuse("delete_if_match", key)
    }

    async fn delete_version(&self, key: &str, _version_id: &str) -> StoreResult<()> {
        refuse("delete_version", key)
    }

    async fn presign_put(&self, key: &str, _ttl_secs: u64) -> StoreResult<String> {
        refuse("presign_put", key)
    }

    async fn ensure_noncurrent_retention(
        &self,
        prefix: &str,
        _days: u64,
    ) -> StoreResult<RetentionOutcome> {
        refuse("ensure_noncurrent_retention", prefix)
    }

    async fn abort_upload(&self, key: &str, _upload_id: &str) -> StoreResult<()> {
        refuse("abort_upload", key)
    }

    /// Refused: the S3 bootstrap writes lifecycle rules and a probe
    /// object, and a read-only holder has no bucket posture to set up.
    async fn bootstrap(&self, prefix: &str) -> StoreResult<BootstrapReport> {
        refuse("bootstrap", prefix)
    }

    async fn epoch_acquire(
        &self,
        key: &str,
        _holder_id: &str,
        _supersede: Option<&EpochState>,
    ) -> StoreResult<EpochLease> {
        refuse("epoch_acquire", key)
    }

    async fn epoch_renew(
        &self,
        key: &str,
        _lease: &EpochLease,
        _echo: Option<&str>,
    ) -> StoreResult<EpochLease> {
        refuse("epoch_renew", key)
    }

    async fn epoch_release(&self, key: &str, _lease: &EpochLease) -> StoreResult<()> {
        refuse("epoch_release", key)
    }

    async fn epoch_enqueue(
        &self,
        key: &str,
        _observed: &EpochState,
        _holder_id: &str,
    ) -> StoreResult<EpochState> {
        refuse("epoch_enqueue", key)
    }

    async fn epoch_handoff(
        &self,
        key: &str,
        _lease: &EpochLease,
        _echo: Option<&str>,
    ) -> StoreResult<()> {
        refuse("epoch_handoff", key)
    }

    // ── reads: forwarded, defaulted ones included ────────────────────

    async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
        self.inner.head(key).await
    }

    async fn get_whole(&self, key: &str, if_match: Option<&str>) -> StoreResult<(ObjectMeta, Bytes)> {
        self.inner.get_whole(key, if_match).await
    }

    async fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        if_match: &str,
    ) -> StoreResult<Bytes> {
        self.inner.get_range(key, offset, len, if_match).await
    }

    async fn get_range_segments(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        if_match: &str,
    ) -> StoreResult<Vec<Bytes>> {
        self.inner.get_range_segments(key, offset, len, if_match).await
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>> {
        self.inner.list(prefix).await
    }

    async fn head_version(&self, key: &str, version_id: &str) -> StoreResult<ObjectMeta> {
        self.inner.head_version(key, version_id).await
    }

    async fn get_version(&self, key: &str, version_id: &str) -> StoreResult<(ObjectMeta, Bytes)> {
        self.inner.get_version(key, version_id).await
    }

    async fn list_versions(&self, prefix: &str) -> StoreResult<Vec<ListedVersion>> {
        self.inner.list_versions(prefix).await
    }

    /// Forwarded: a presigned GET grants a read, which this holder has.
    async fn presign_get(&self, key: &str, ttl_secs: u64) -> StoreResult<String> {
        self.inner.presign_get(key, ttl_secs).await
    }

    async fn lifecycle_rules(&self) -> StoreResult<Vec<LifecycleView>> {
        self.inner.lifecycle_rules().await
    }

    async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>> {
        self.inner.list_uploads(prefix).await
    }

    async fn epoch_read(&self, key: &str) -> StoreResult<Option<EpochState>> {
        self.inner.epoch_read(key).await
    }

    fn min_part_size(&self) -> u64 {
        self.inner.min_part_size()
    }

    fn max_parts(&self) -> usize {
        self.inner.max_parts()
    }

    fn upload_gate(&self) -> Option<Arc<gate::ByteGate>> {
        self.inner.upload_gate()
    }

    fn request_counts(&self) -> Option<RequestCounts> {
        self.inner.request_counts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use crate::PartSource;
    use std::sync::Mutex;

    /// The trait's writes. Each must be refused without a request.
    const WRITES: &[&str] = &[
        "put_whole", "copy_object", "compose_generation", "delete", "delete_if_match",
        "delete_version", "presign_put", "ensure_noncurrent_retention", "abort_upload",
        "bootstrap", "epoch_acquire", "epoch_renew", "epoch_release", "epoch_enqueue",
        "epoch_handoff",
    ];

    /// The trait's reads, and the four methods that describe the store
    /// without sending anything. Each must reach the inner store.
    const READS: &[&str] = &[
        "head", "get_whole", "get_range", "get_range_segments", "list", "head_version",
        "get_version", "list_versions", "presign_get", "lifecycle_rules", "list_uploads",
        "epoch_read", "min_part_size", "max_parts", "upload_gate", "request_counts",
    ];

    /// The `fn` names declared in the block whose opening LINE is exactly
    /// `opener` (so a string literal quoting it does not count), up to the
    /// `}` at the opener's own indentation.
    fn declared(src: &str, opener: &str) -> Vec<String> {
        let lines: Vec<&str> = src.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.trim() == opener)
            .unwrap_or_else(|| panic!("no line `{opener}` in the source"));
        let indent = &lines[start][..lines[start].len() - lines[start].trim_start().len()];
        let close = format!("{indent}}}");
        let end = start + lines[start..].iter().position(|l| *l == close).expect("the block closes");
        let mut names: Vec<String> = lines[start + 1..end]
            .iter()
            .filter_map(|l| {
                let l = l.trim_start();
                let rest = l.strip_prefix("async fn ").or_else(|| l.strip_prefix("fn "))?;
                Some(rest[..rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?].to_string())
            })
            .collect();
        names.sort();
        names
    }

    /// The census, and its control. Every trait method is filed as a read
    /// or a write, and BOTH this wrapper and the recording double below
    /// name every one of them. A method the trait gains with a default
    /// fails here: left to its default on the wrapper, a read runs the
    /// trait's fallback instead of the inner store's override.
    #[test]
    fn every_trait_method_is_filed_and_every_one_is_implemented_by_name() {
        let mut filed: Vec<String> = WRITES.iter().chain(READS).map(|s| s.to_string()).collect();
        filed.sort();
        let mut dedup = filed.clone();
        dedup.dedup();
        assert_eq!(filed, dedup, "a method is filed twice");

        let trait_fns = declared(include_str!("lib.rs"), "pub trait ObjectStore: Send + Sync {");
        // The control: the parser finds the trait's first and last
        // methods, so an empty or truncated parse cannot agree by accident.
        assert!(trait_fns.contains(&"put_whole".to_string()) && trait_fns.contains(&"request_counts".to_string()));
        assert_eq!(trait_fns, filed, "the trait's methods and the read/write filing disagree");

        let src = include_str!("readonly.rs");
        assert_eq!(
            declared(src, "impl<S: ObjectStore + ?Sized> ObjectStore for ReadOnly<S> {"),
            filed,
            "ReadOnly must name every trait method, defaulted ones included"
        );
        assert_eq!(
            declared(src, "impl ObjectStore for Recorder {"),
            filed,
            "the recording double must name every trait method, or it cannot see the wrapper call one"
        );
    }

    /// A `MemoryStore` that records every trait method called on it, by
    /// the method's OWN name. Op counts alone cannot see a defaulted read
    /// left unforwarded: `get_range_segments`'s default calls `get_range`,
    /// which the double counts the same either way.
    struct Recorder {
        inner: MemoryStore,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Recorder {
        fn saw(&self, m: &'static str) {
            self.calls.lock().unwrap().push(m);
        }
        fn take(&self) -> Vec<&'static str> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    #[async_trait]
    impl ObjectStore for Recorder {
        async fn put_whole(
            &self,
            key: &str,
            body: Bytes,
            condition: &PutCondition,
            stamps: &GenerationStamps,
            crc64: u64,
        ) -> StoreResult<ObjectMeta> {
            self.saw("put_whole");
            self.inner.put_whole(key, body, condition, stamps, crc64).await
        }
        async fn copy_object(
            &self,
            src_key: &str,
            src_if_match: Option<&str>,
            dst_key: &str,
            condition: &PutCondition,
            stamps: &GenerationStamps,
        ) -> StoreResult<ObjectMeta> {
            self.saw("copy_object");
            self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
        }
        async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta> {
            self.saw("compose_generation");
            self.inner.compose_generation(spec).await
        }
        async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
            self.saw("head");
            self.inner.head(key).await
        }
        async fn get_whole(&self, key: &str, if_match: Option<&str>) -> StoreResult<(ObjectMeta, Bytes)> {
            self.saw("get_whole");
            self.inner.get_whole(key, if_match).await
        }
        async fn get_range(&self, key: &str, offset: u64, len: u64, if_match: &str) -> StoreResult<Bytes> {
            self.saw("get_range");
            self.inner.get_range(key, offset, len, if_match).await
        }
        async fn get_range_segments(
            &self,
            key: &str,
            offset: u64,
            len: u64,
            if_match: &str,
        ) -> StoreResult<Vec<Bytes>> {
            self.saw("get_range_segments");
            self.inner.get_range_segments(key, offset, len, if_match).await
        }
        async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>> {
            self.saw("list");
            self.inner.list(prefix).await
        }
        async fn delete(&self, key: &str) -> StoreResult<()> {
            self.saw("delete");
            self.inner.delete(key).await
        }
        async fn delete_if_match(&self, key: &str, etag: &str) -> StoreResult<()> {
            self.saw("delete_if_match");
            self.inner.delete_if_match(key, etag).await
        }
        async fn head_version(&self, key: &str, version_id: &str) -> StoreResult<ObjectMeta> {
            self.saw("head_version");
            self.inner.head_version(key, version_id).await
        }
        async fn get_version(&self, key: &str, version_id: &str) -> StoreResult<(ObjectMeta, Bytes)> {
            self.saw("get_version");
            self.inner.get_version(key, version_id).await
        }
        async fn delete_version(&self, key: &str, version_id: &str) -> StoreResult<()> {
            self.saw("delete_version");
            self.inner.delete_version(key, version_id).await
        }
        async fn list_versions(&self, prefix: &str) -> StoreResult<Vec<ListedVersion>> {
            self.saw("list_versions");
            self.inner.list_versions(prefix).await
        }
        async fn presign_get(&self, key: &str, ttl_secs: u64) -> StoreResult<String> {
            self.saw("presign_get");
            self.inner.presign_get(key, ttl_secs).await
        }
        async fn presign_put(&self, key: &str, ttl_secs: u64) -> StoreResult<String> {
            self.saw("presign_put");
            self.inner.presign_put(key, ttl_secs).await
        }
        async fn lifecycle_rules(&self) -> StoreResult<Vec<LifecycleView>> {
            self.saw("lifecycle_rules");
            self.inner.lifecycle_rules().await
        }
        async fn ensure_noncurrent_retention(&self, prefix: &str, days: u64) -> StoreResult<RetentionOutcome> {
            self.saw("ensure_noncurrent_retention");
            self.inner.ensure_noncurrent_retention(prefix, days).await
        }
        async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>> {
            self.saw("list_uploads");
            self.inner.list_uploads(prefix).await
        }
        async fn abort_upload(&self, key: &str, upload_id: &str) -> StoreResult<()> {
            self.saw("abort_upload");
            self.inner.abort_upload(key, upload_id).await
        }
        async fn bootstrap(&self, prefix: &str) -> StoreResult<BootstrapReport> {
            self.saw("bootstrap");
            self.inner.bootstrap(prefix).await
        }
        async fn epoch_read(&self, key: &str) -> StoreResult<Option<EpochState>> {
            self.saw("epoch_read");
            self.inner.epoch_read(key).await
        }
        async fn epoch_acquire(
            &self,
            key: &str,
            holder_id: &str,
            supersede: Option<&EpochState>,
        ) -> StoreResult<EpochLease> {
            self.saw("epoch_acquire");
            self.inner.epoch_acquire(key, holder_id, supersede).await
        }
        async fn epoch_renew(&self, key: &str, lease: &EpochLease, echo: Option<&str>) -> StoreResult<EpochLease> {
            self.saw("epoch_renew");
            self.inner.epoch_renew(key, lease, echo).await
        }
        async fn epoch_release(&self, key: &str, lease: &EpochLease) -> StoreResult<()> {
            self.saw("epoch_release");
            self.inner.epoch_release(key, lease).await
        }
        async fn epoch_enqueue(&self, key: &str, observed: &EpochState, holder_id: &str) -> StoreResult<EpochState> {
            self.saw("epoch_enqueue");
            self.inner.epoch_enqueue(key, observed, holder_id).await
        }
        async fn epoch_handoff(&self, key: &str, lease: &EpochLease, echo: Option<&str>) -> StoreResult<()> {
            self.saw("epoch_handoff");
            self.inner.epoch_handoff(key, lease, echo).await
        }
        fn min_part_size(&self) -> u64 {
            self.saw("min_part_size");
            self.inner.min_part_size()
        }
        fn max_parts(&self) -> usize {
            self.saw("max_parts");
            self.inner.max_parts()
        }
        fn upload_gate(&self) -> Option<Arc<gate::ByteGate>> {
            self.saw("upload_gate");
            self.inner.upload_gate()
        }
        fn request_counts(&self) -> Option<RequestCounts> {
            self.saw("request_counts");
            self.inner.request_counts()
        }
    }

    fn stamps() -> GenerationStamps {
        GenerationStamps { generation: 1, epoch: 0, flush_uuid: "ro-test".into(), boundary_source: None, posix: None }
    }

    /// F10's store half. Every write on `ReadOnly` answers `Auth` and the
    /// inner store sees NOTHING — not the call, not a request; every read
    /// reaches the inner store under its own name and answers what the
    /// inner store answers. A write wrongly forwarded, a read wrongly
    /// refused, or a defaulted read left to the trait's fallback fails it.
    #[tokio::test]
    async fn a_read_only_store_refuses_every_write_unsent_and_forwards_every_read() {
        // Non-default shapes wherever a wrapper's own answer could pass
        // for the inner store's: part limits, a byte gate, lifecycle rules.
        let mut mem = MemoryStore::new().with_upload_inflight_max_bytes(1 << 20);
        mem.min_part = 7;
        mem.max_parts = 9;
        mem.plant_lifecycle_rule(LifecycleView {
            id: "customer".into(),
            enabled: true,
            prefix: "p/".into(),
            noncurrent_days: Some(3),
            expired_delete_marker: false,
        });
        let rec = Arc::new(Recorder { inner: mem, calls: Mutex::new(vec![]) });
        let body = Bytes::from_static(b"hello, read-only world");
        let crc = crate::crc64_nvme(&body);
        let v1 = rec.inner.put_whole("p/k", body.clone(), &PutCondition::IfNoneMatchAny, &stamps(), crc).await.unwrap();
        let v2 = rec
            .inner
            .put_whole("p/k", body.clone(), &PutCondition::IfMatch(v1.etag.clone()), &stamps(), crc)
            .await
            .unwrap();
        let head_before = rec.inner.head("p/k").await.unwrap();
        let lease = rec.inner.epoch_acquire("p/epoch", "holder", None).await.unwrap();
        let cell = rec.inner.epoch_read("p/epoch").await.unwrap().unwrap();
        let upload_id = rec.inner.raw_begin_upload("p/mpu");
        rec.inner.reset_op_counts();
        rec.take();

        let ro = ReadOnly::new(rec.clone());

        // ── writes ──
        let mut refused: Vec<&str> = vec![];
        let mut expect_refused = |verb: &'static str, r: StoreResult<()>| {
            match r {
                Err(StoreError::Auth(m)) if m.starts_with(&format!("read-only store: {verb} ")) => {}
                other => panic!("{verb} on a read-only store answered {other:?}, not Auth"),
            }
            assert_eq!(rec.take(), Vec::<&str>::new(), "{verb} reached the inner store");
            assert_eq!(rec.inner.total_ops(), 0, "{verb} sent a request: {:?}", rec.inner.op_counts());
            refused.push(verb);
        };
        let spec = ComposeSpec {
            key: "p/composed",
            local_path: std::path::Path::new("/nonexistent"),
            parts: vec![PartSource::Local { offset: 0, len: 1 }],
            base_key: None,
            base_etag: None,
            condition: PutCondition::Unconditional,
            stamps: stamps(),
            crc64: Some(0),
            progress: None,
        };
        let cond = PutCondition::Unconditional;
        expect_refused("put_whole", ro.put_whole("p/new", body.clone(), &cond, &stamps(), crc).await.map(drop));
        expect_refused("copy_object", ro.copy_object("p/k", None, "p/copy", &cond, &stamps()).await.map(drop));
        expect_refused("compose_generation", ro.compose_generation(&spec).await.map(drop));
        expect_refused("delete", ro.delete("p/k").await);
        expect_refused("delete_if_match", ro.delete_if_match("p/k", &v2.etag).await);
        expect_refused("delete_version", ro.delete_version("p/k", v1.version_id.as_deref().unwrap()).await);
        expect_refused("presign_put", ro.presign_put("p/k", 60).await.map(drop));
        expect_refused("ensure_noncurrent_retention", ro.ensure_noncurrent_retention("p/", 30).await.map(drop));
        expect_refused("abort_upload", ro.abort_upload("p/mpu", &upload_id).await);
        expect_refused("bootstrap", ro.bootstrap("p/").await.map(drop));
        expect_refused("epoch_acquire", ro.epoch_acquire("p/epoch2", "h2", None).await.map(drop));
        expect_refused("epoch_renew", ro.epoch_renew("p/epoch", &lease, None).await.map(drop));
        expect_refused("epoch_release", ro.epoch_release("p/epoch", &lease).await);
        expect_refused("epoch_enqueue", ro.epoch_enqueue("p/epoch", &cell, "h3").await.map(drop));
        expect_refused("epoch_handoff", ro.epoch_handoff("p/epoch", &lease, None).await);
        assert_eq!(refused, WRITES, "every write, and only the writes, were exercised");
        // And the bucket is as it was: the object, its versions, the
        // cell, the pending upload.
        assert_eq!(rec.inner.head("p/k").await.unwrap(), head_before);
        assert_eq!(rec.inner.version_count("p/k"), 2);
        assert_eq!(rec.inner.epoch_read("p/epoch").await.unwrap().unwrap(), cell);
        assert_eq!(rec.inner.list_uploads("p/").await.unwrap().len(), 1);
        rec.take();

        // ── reads ──
        let mut forwarded: Vec<&str> = vec![];
        let mut reached = |verb: &'static str| {
            assert_eq!(rec.take(), vec![verb], "{verb} did not reach the inner store as itself");
            forwarded.push(verb);
        };
        let vid1 = v1.version_id.clone().unwrap();
        assert_eq!(ro.head("p/k").await.unwrap(), head_before);
        reached("head");
        let whole = ro.get_whole("p/k", Some(&v2.etag)).await.unwrap();
        assert_eq!(whole, rec.inner.get_whole("p/k", Some(&v2.etag)).await.unwrap());
        assert_eq!(whole.1, body);
        reached("get_whole");
        assert_eq!(ro.get_range("p/k", 7, 9, &v2.etag).await.unwrap(), body.slice(7..16));
        reached("get_range");
        assert_eq!(ro.get_range_segments("p/k", 7, 9, &v2.etag).await.unwrap().concat(), body.slice(7..16));
        reached("get_range_segments");
        assert_eq!(ro.list("p/").await.unwrap(), rec.inner.list("p/").await.unwrap());
        reached("list");
        assert_eq!(ro.head_version("p/k", &vid1).await.unwrap(), rec.inner.head_version("p/k", &vid1).await.unwrap());
        reached("head_version");
        assert_eq!(ro.get_version("p/k", &vid1).await.unwrap(), rec.inner.get_version("p/k", &vid1).await.unwrap());
        reached("get_version");
        let versions = ro.list_versions("p/").await.unwrap();
        assert_eq!(versions, rec.inner.list_versions("p/").await.unwrap());
        reached("list_versions");
        assert!(versions.iter().filter(|v| v.key == "p/k").count() == 2, "{versions:?}");
        assert_eq!(ro.presign_get("p/k", 60).await.unwrap(), rec.inner.presign_get("p/k", 60).await.unwrap());
        reached("presign_get");
        let rules = ro.lifecycle_rules().await.unwrap();
        assert_eq!(rules, rec.inner.lifecycle_rules().await.unwrap());
        assert_eq!(rules.len(), 1);
        reached("lifecycle_rules");
        assert_eq!(ro.list_uploads("p/").await.unwrap(), rec.inner.list_uploads("p/").await.unwrap());
        reached("list_uploads");
        assert_eq!(ro.epoch_read("p/epoch").await.unwrap(), Some(cell.clone()));
        reached("epoch_read");
        assert_eq!(ro.min_part_size(), 7);
        reached("min_part_size");
        assert_eq!(ro.max_parts(), 9);
        reached("max_parts");
        let gate = ro.upload_gate().expect("the inner store has a byte gate");
        assert!(Arc::ptr_eq(&gate, &rec.inner.upload_gate().unwrap()));
        reached("upload_gate");
        let counts = ro.request_counts().expect("the inner store counts");
        assert_eq!(counts, rec.inner.request_counts().unwrap());
        assert!(counts.get > 0 && counts.put == 0, "{counts:?}");
        reached("request_counts");
        assert_eq!(forwarded, READS, "every read, and only the reads, were exercised");

        // Reads never write either.
        let writes: Vec<_> = rec
            .inner
            .op_counts()
            .into_iter()
            .filter(|(op, _)| !READS.contains(op))
            .collect();
        assert_eq!(writes, vec![], "a read sent a write");
    }
}
