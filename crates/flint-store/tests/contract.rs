//! The store contract lean relies on, as tests every backend must pass
//! (docs/plans/lean-azure-gcs-backends-plan-2026-11-10.md §2, C1–C8).
//!
//! The memory double runs it on every `cargo test`. A live store runs it
//! when asked (`#[ignore]`, like `presign_live.rs`): S3 or anything that
//! speaks its API, Azure Blob and GCS, against their emulators first and
//! real accounts after.
//!
//! FLINT_CONTRACT_ENDPOINT=http://host:port FLINT_CONTRACT_BUCKET=b \
//! FLINT_CONTRACT_KEY=... FLINT_CONTRACT_SECRET=... [FLINT_CONTRACT_REGION=us-east-1] \
//! [FLINT_CONTRACT_SKIP=c2] \
//!   cargo test -p flint-store --features s3 --test contract -- --ignored --nocapture
//!
//! `FLINT_CONTRACT_SKIP` names checks a store is KNOWN to fail, so the
//! rest still run. Only checks lean does not rest on may be skipped:
//! - `c2`: MinIO and Ozone 2.2.x ignore `If-Match` on DELETE
//!   (lean/SAFETY.md §2), and lean turns its collector off on such a
//!   store rather than refusing it;
//! - `c1-copy`: MinIO ignores `If-None-Match` on a copy's destination
//!   (`c1_copy_destination` says why lean does not need it).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use bytes::Bytes;
use flint_store::memory::MemoryStore;
use flint_store::{
    crc64_nvme, crc64_to_b64, ComposeSpec, GenerationStamps, ObjectStore, PartSource, PutCondition, StoreError,
    MAX_KEY_BYTES,
};

fn stamps(flush: &str) -> GenerationStamps {
    GenerationStamps { generation: 1, epoch: 0, flush_uuid: flush.into(), boundary_source: None, posix: None }
}

fn lost_race(r: &Result<impl std::fmt::Debug, StoreError>) -> bool {
    matches!(r, Err(StoreError::PreconditionFailed(_) | StoreError::Conflict(_)))
}

async fn put(s: &dyn ObjectStore, key: &str, body: &[u8], cond: &PutCondition) -> Result<flint_store::ObjectMeta, StoreError> {
    s.put_whole(key, Bytes::copy_from_slice(body), cond, &stamps("contract"), crc64_nvme(body)).await
}

/// Every ETag a check saw, with the bytes it named: C3 is checked over
/// all of them at the end.
#[derive(Default)]
struct Seen(BTreeMap<String, Vec<u8>>);

impl Seen {
    fn note(&mut self, etag: &str, body: &[u8]) {
        if let Some(was) = self.0.get(etag) {
            assert_eq!(was, body, "C3: one ETag {etag} named two different bodies");
        }
        self.0.insert(etag.to_string(), body.to_vec());
    }
}

/// C1: conditional PUT, create-only and If-Match, on whole puts,
/// composes and copies.
async fn c1_conditional_put(s: &dyn ObjectStore, base: &str, seen: &mut Seen) {
    let k = format!("{base}c1/obj");
    let a = put(s, &k, b"first", &PutCondition::IfNoneMatchAny).await.expect("C1: a create-only PUT of a new key");
    seen.note(&a.etag, b"first");
    assert!(lost_race(&put(s, &k, b"second", &PutCondition::IfNoneMatchAny).await), "C1: a create-only PUT over an object landed");
    assert!(
        lost_race(&put(s, &k, b"second", &PutCondition::IfMatch("\"0123456789abcdef0123456789abcdef\"".into())).await),
        "C1: an If-Match PUT on a made-up ETag landed"
    );
    let b = put(s, &k, b"second", &PutCondition::IfMatch(a.etag.clone())).await.expect("C1: If-Match on the current ETag");
    seen.note(&b.etag, b"second");
    assert_ne!(a.etag, b.etag, "C1: new bytes, same ETag");
    assert!(lost_race(&put(s, &k, b"third", &PutCondition::IfMatch(a.etag.clone())).await), "C1: If-Match on a stale ETag landed");
    assert_eq!(s.get_whole(&k, None).await.unwrap().1, Bytes::from_static(b"second"), "C1: a refused PUT changed the object");

    // A compose is create-only like a whole put, and a refused one
    // leaves no assembly behind (A9).
    let (file, body) = local_file(s);
    let fresh = format!("{base}c1/composed");
    let made = s.compose_generation(&compose(&fresh, &file, body.len() as u64, s)).await.expect("C1: a create-only compose of a new key");
    seen.note(&made.etag, &body);
    assert!(lost_race(&s.compose_generation(&compose(&k, &file, body.len() as u64, s)).await), "C1: a create-only compose over an object landed");
    assert!(s.list_uploads(&format!("{base}c1/")).await.unwrap().is_empty(), "C1: a refused compose left its assembly");

    // A copy: the source guarded by its ETag, the destination create-only.
    let dst = format!("{base}c1/copy");
    assert!(
        lost_race(&s.copy_object(&k, Some(&a.etag), &dst, &PutCondition::IfNoneMatchAny, &stamps("copy")).await),
        "C1: a copy from a stale source ETag landed"
    );
    let c = s.copy_object(&k, Some(&b.etag), &dst, &PutCondition::IfNoneMatchAny, &stamps("copy")).await.expect("C1: a copy");
    seen.note(&c.etag, b"second");
}

/// C1, a copy's destination: a create-only copy over an object is
/// refused. Lean does not rest on it: every copy it makes lands at a key
/// nobody else writes (a conflict copy's fresh uuid, a draft promote's
/// fresh `ui-` flush), and the one named after its source (an ingress
/// adoption) can only receive the source version the source guard pins.
/// So a store that ignores it is run with `FLINT_CONTRACT_SKIP=c1-copy`
/// and recorded. MinIO is one: measured on the box 2026-11-10.
async fn c1_copy_destination(s: &dyn ObjectStore, base: &str) {
    let src = format!("{base}c1-copy/src");
    let dst = format!("{base}c1-copy/dst");
    let a = put(s, &src, b"source", &PutCondition::IfNoneMatchAny).await.unwrap();
    put(s, &dst, b"already here", &PutCondition::IfNoneMatchAny).await.unwrap();
    assert!(
        lost_race(&s.copy_object(&src, Some(&a.etag), &dst, &PutCondition::IfNoneMatchAny, &stamps("copy")).await),
        "C1: a create-only copy over an object landed"
    );
    assert_eq!(s.get_whole(&dst, None).await.unwrap().1, Bytes::from_static(b"already here"), "C1: the refused copy changed the object");
}

/// C2: conditional DELETE.
async fn c2_conditional_delete(s: &dyn ObjectStore, base: &str) {
    let k = format!("{base}c2/obj");
    let a = put(s, &k, b"one", &PutCondition::IfNoneMatchAny).await.unwrap();
    let b = put(s, &k, b"two", &PutCondition::IfMatch(a.etag.clone())).await.unwrap();
    assert!(lost_race(&s.delete_if_match(&k, &a.etag).await), "C2: a DELETE on a stale ETag landed");
    assert!(s.head(&k).await.is_ok(), "C2: the refused DELETE removed the object");
    s.delete_if_match(&k, &b.etag).await.expect("C2: a DELETE on the current ETag");
    assert!(matches!(s.head(&k).await, Err(StoreError::NotFound(_))), "C2: the object outlived its DELETE");
}

/// C3: an ETag names bytes. Equal ETags mean equal bytes (checked over
/// every ETag the run saw), and If-Match is answered by the bytes: a body
/// that changes and changes back is matched by its first ETag, as S3's
/// MD5 ETag is. Lean's inbox does exactly that; an empty cell is the same
/// bytes every time.
async fn c3_etags_name_bytes(s: &dyn ObjectStore, base: &str, seen: &mut Seen) {
    let k = format!("{base}c3/obj");
    let a = put(s, &k, b"same", &PutCondition::IfNoneMatchAny).await.unwrap();
    seen.note(&a.etag, b"same");
    let b = put(s, &k, b"other", &PutCondition::IfMatch(a.etag.clone())).await.unwrap();
    seen.note(&b.etag, b"other");
    let again = put(s, &k, b"same", &PutCondition::IfMatch(b.etag.clone())).await.unwrap();
    seen.note(&again.etag, b"same");
    let c = put(s, &k, b"last", &PutCondition::IfMatch(a.etag.clone()))
        .await
        .expect("C3: If-Match on the first ETag of bytes that came back was refused");
    seen.note(&c.etag, b"last");
    // Two keys holding different bytes never share an ETag.
    for (i, body) in [&b"x"[..], b"y", b"z"].iter().enumerate() {
        let m = put(s, &format!("{base}c3/k{i}"), body, &PutCondition::IfNoneMatchAny).await.unwrap();
        seen.note(&m.etag, body);
    }
}

/// C3 across two clients of one store: a client whose last sight of an
/// object is stale is still answered by the bytes. The other client
/// rewrote it, then put the first bytes back; If-Match on the first ETag
/// must land, and If-Match on bytes that are gone must not. One client
/// alone never reaches this: its own writes keep whatever it caches fresh.
async fn c3_across_clients(a: &dyn ObjectStore, b: &dyn ObjectStore, base: &str) {
    let k = format!("{base}c3x/obj");
    let first = put(a, &k, b"first", &PutCondition::IfNoneMatchAny).await.unwrap();
    let other = put(b, &k, b"other", &PutCondition::IfMatch(first.etag.clone())).await.expect("C3x: b over a's write");
    put(b, &k, b"first", &PutCondition::IfMatch(other.etag.clone())).await.expect("C3x: b puts the first bytes back");
    put(a, &k, b"after", &PutCondition::IfMatch(first.etag.clone()))
        .await
        .expect("C3x: a stale client's If-Match on bytes that came back was refused");
    assert!(
        lost_race(&put(b, &k, b"never", &PutCondition::IfMatch(other.etag.clone())).await),
        "C3x: If-Match on bytes that are gone landed"
    );
}

/// C4: LIST is consistent with what was just written and deleted, and
/// carries the ETag HEAD gives (lean passes one to the other's
/// If-Match) and a last-modified time.
async fn c4_list(s: &dyn ObjectStore, base: &str) {
    let dir = format!("{base}c4/");
    let a = put(s, &format!("{dir}a"), b"aa", &PutCondition::IfNoneMatchAny).await.unwrap();
    put(s, &format!("{dir}b"), b"bbb", &PutCondition::IfNoneMatchAny).await.unwrap();
    let listed = s.list(&dir).await.unwrap();
    let got: Vec<&str> = listed.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(got.len(), 2, "C4: {got:?}");
    let la = listed.iter().find(|o| o.key == format!("{dir}a")).expect("C4: a just-written object is listed");
    assert_eq!(la.etag, a.etag, "C4: LIST and PUT disagree on the ETag");
    assert_eq!(la.etag, s.head(&la.key).await.unwrap().etag, "C4: LIST and HEAD disagree on the ETag");
    assert_eq!(la.size, 2);
    assert!(la.last_modified_unix.is_some(), "C4: LIST carries no last-modified time");
    s.delete(&format!("{dir}b")).await.unwrap();
    let after: Vec<String> = s.list(&dir).await.unwrap().into_iter().map(|o| o.key).collect();
    assert_eq!(after, [format!("{dir}a")], "C4: a deleted object is still listed");
}

/// C6: a copy keeps the bytes and the CRC, and takes the stamps it was
/// given rather than the source's: a stamp only the source carries is not
/// carried over.
async fn c6_copy(s: &dyn ObjectStore, base: &str) {
    let src = format!("{base}c6/src");
    let dst = format!("{base}c6/dst");
    let body = b"copied bytes";
    let src_stamps = GenerationStamps { boundary_source: Some("source-only".into()), ..stamps("the-source") };
    let a = s
        .put_whole(&src, Bytes::from_static(body), &PutCondition::IfNoneMatchAny, &src_stamps, crc64_nvme(body))
        .await
        .unwrap();
    let c = s.copy_object(&src, Some(&a.etag), &dst, &PutCondition::IfNoneMatchAny, &stamps("the-copy")).await.unwrap();
    assert_eq!(c.crc64_b64.as_deref(), Some(crc64_to_b64(crc64_nvme(body)).as_str()), "C6: the copy's CRC");
    let head = s.head(&dst).await.unwrap();
    assert_eq!(head.crc64_b64, c.crc64_b64, "C6: HEAD disagrees with the copy on the CRC");
    let got = GenerationStamps::from_meta(&head.meta).expect("C6: the copy carries stamps");
    assert_eq!(got.flush_uuid, "the-copy", "C6: the copy's stamps");
    assert_eq!(got.boundary_source, None, "C6: a stamp only the source had was carried over");
    assert_eq!(s.get_whole(&dst, Some(&c.etag)).await.unwrap().1, Bytes::from_static(body));
}

/// C7: every object a flint client writes carries its full CRC-64/NVME,
/// a wrong one fails the write and lands nothing, and reads are guarded:
/// a stale ETag is a precondition failure, a missing object NotFound.
async fn c7_checksums_and_guarded_reads(s: &dyn ObjectStore, base: &str) {
    let k = format!("{base}c7/obj");
    let bad = s
        .put_whole(&k, Bytes::from_static(b"body"), &PutCondition::IfNoneMatchAny, &stamps("c7"), crc64_nvme(b"not the body"))
        .await;
    assert!(matches!(bad, Err(StoreError::ChecksumMismatch(_))), "C7: a wrong CRC: {bad:?}");
    assert!(matches!(s.head(&k).await, Err(StoreError::NotFound(_))), "C7: a write with a wrong CRC landed");

    let want = crc64_to_b64(crc64_nvme(b"body"));
    let a = put(s, &k, b"body", &PutCondition::IfNoneMatchAny).await.unwrap();
    assert_eq!(a.crc64_b64.as_deref(), Some(want.as_str()), "C7: PUT's CRC");
    assert_eq!(s.head(&k).await.unwrap().crc64_b64.as_deref(), Some(want.as_str()), "C7: HEAD's CRC");
    let (meta, body) = s.get_whole(&k, Some(&a.etag)).await.unwrap();
    assert_eq!((meta.crc64_b64.as_deref(), &body[..]), (Some(want.as_str()), &b"body"[..]), "C7: GET");
    assert_eq!(s.get_range(&k, 1, 2, &a.etag).await.unwrap(), Bytes::from_static(b"od"));

    let (file, local) = local_file(s);
    let ck = format!("{base}c7/composed");
    let made = s.compose_generation(&compose(&ck, &file, local.len() as u64, s)).await.unwrap();
    let cwant = crc64_to_b64(crc64_nvme(&local));
    assert_eq!(made.crc64_b64.as_deref(), Some(cwant.as_str()), "C7: a compose's CRC");
    assert_eq!(s.head(&ck).await.unwrap().crc64_b64.as_deref(), Some(cwant.as_str()), "C7: HEAD's CRC of a compose");

    let b = put(s, &k, b"moved", &PutCondition::IfMatch(a.etag.clone())).await.unwrap();
    assert!(lost_race(&s.get_whole(&k, Some(&a.etag)).await), "C7: a GET on a stale ETag");
    assert!(lost_race(&s.get_range(&k, 0, 2, &a.etag).await), "C7: a ranged GET on a stale ETag");
    s.delete(&k).await.unwrap();
    assert!(matches!(s.get_whole(&k, Some(&b.etag)).await, Err(StoreError::NotFound(_))), "C7: a guarded GET of a gone object");
    assert!(matches!(s.head(&k).await, Err(StoreError::NotFound(_))));
}

/// C8: the longest key every provider takes is taken, built as lean's
/// keys are, from path segments a filesystem holds (at most 200 bytes
/// here; MinIO, like a filesystem, refuses a segment over 255).
async fn c8_longest_key(s: &dyn ObjectStore, base: &str) {
    let mut k = format!("{base}c8");
    while k.len() < MAX_KEY_BYTES {
        k.push('/');
        let room = MAX_KEY_BYTES - k.len();
        k.push_str(&"k".repeat(room.min(200)));
    }
    assert_eq!(k.len(), MAX_KEY_BYTES, "fixture");
    assert!(s.provider().key_ok(&k));
    put(s, &k, b"at the limit", &PutCondition::IfNoneMatchAny).await.expect("C8: a key of exactly 1,024 bytes");
    assert_eq!(s.head(&k).await.unwrap().size, 12);
}

/// The lease cell: create-only claim, a renewal that moves the token, a
/// stale token refused, and a queue that moves it too.
async fn lease_cell(s: &dyn ObjectStore, base: &str) {
    let k = format!("{base}lease/epoch");
    assert_eq!(s.epoch_read(&k).await.unwrap(), None);
    let l1 = s.epoch_acquire(&k, "w1", None).await.expect("the first claim");
    assert!(lost_race(&s.epoch_acquire(&k, "w2", None).await), "a second create-only claim landed");
    let l2 = s.epoch_renew(&k, &l1, None).await.expect("a renewal");
    assert_ne!(l1.token, l2.token, "a renewal left the token where it was: a live holder would read as dead");
    assert!(lost_race(&s.epoch_renew(&k, &l1, None).await), "a renewal on a stale token landed");
    let seen = s.epoch_read(&k).await.unwrap().expect("the cell");
    assert_eq!((seen.holder_id.as_str(), seen.token.as_str()), ("w1", l2.token.as_str()));
    let queued = s.epoch_enqueue(&k, &seen, "w2").await.expect("an enqueue");
    assert_eq!(queued.waiters, ["w2"]);
    assert_ne!(queued.token, l2.token, "an enqueue left the token where it was");
    assert!(lost_race(&s.epoch_renew(&k, &l2, None).await), "the holder renewed over the queue");
    let adopted = flint_store::EpochLease { token: queued.token.clone(), waiters: queued.waiters.clone(), ..l2 };
    s.epoch_handoff(&k, &adopted, None).await.expect("a handoff");
    let after = s.epoch_read(&k).await.unwrap().unwrap();
    assert!(after.released, "{after:?}");
    assert_eq!(after.handoff.as_deref(), Some("w2"));
}

/// A local file of one full part and a tail, so a compose has two parts
/// on every backend (S3 refuses a non-final part under 5 MiB).
fn local_file(s: &dyn ObjectStore) -> (Arc<std::fs::File>, Vec<u8>) {
    let part = s.min_part_size().max(1) as usize;
    let body: Vec<u8> = (0..part + 1000).map(|i| (i * 31 % 251) as u8).collect();
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(&body).unwrap();
    (Arc::new(f), body)
}

fn compose<'a>(key: &'a str, file: &Arc<std::fs::File>, len: u64, s: &dyn ObjectStore) -> ComposeSpec<'a> {
    let part = s.min_part_size().max(1).min(len);
    let mut parts = vec![PartSource::Local { offset: 0, len: part }];
    if len > part {
        parts.push(PartSource::Local { offset: part, len: len - part });
    }
    ComposeSpec {
        key,
        local: Some(file.clone()),
        parts,
        base_key: None,
        base_etag: None,
        condition: PutCondition::IfNoneMatchAny,
        stamps: stamps("compose"),
        crc64: None,
        progress: None,
    }
}

/// The contract, with a second client of the same store for the checks
/// that need two.
async fn run_contract_with(s: &dyn ObjectStore, second: Option<&dyn ObjectStore>, base: &str, skip: &[&str]) {
    let mut seen = Seen::default();
    let on = |c: &str| !skip.contains(&c);
    if let (Some(b), true) = (second, on("c3x")) {
        c3_across_clients(s, b, base).await;
    }
    if on("c1") {
        c1_conditional_put(s, base, &mut seen).await;
    }
    if on("c1-copy") {
        c1_copy_destination(s, base).await;
    }
    if on("c2") {
        c2_conditional_delete(s, base).await;
    }
    if on("c3") {
        c3_etags_name_bytes(s, base, &mut seen).await;
    }
    if on("c4") {
        c4_list(s, base).await;
    }
    if on("c6") {
        c6_copy(s, base).await;
    }
    if on("c7") {
        c7_checksums_and_guarded_reads(s, base).await;
    }
    if on("c8") {
        c8_longest_key(s, base).await;
    }
    if on("lease") {
        lease_cell(s, base).await;
    }
}

#[tokio::test]
async fn the_memory_double_meets_the_contract() {
    // S3's part floor, so the composes take the shape they take live.
    let mut mem = MemoryStore::new();
    mem.min_part = 5 * 1024 * 1024;
    run_contract_with(&mem, Some(&mem), "contract/", &[]).await;
}

/// The control: a double that answers If-Match by the PUT it last saw
/// rather than by bytes fails C3, so C3 is not vacuous. Built by running
/// C3 against a store whose ETags name writes.
#[tokio::test]
#[should_panic(expected = "C3: If-Match on the first ETag of bytes that came back was refused")]
async fn a_store_whose_etags_name_writes_fails_c3() {
    let s = WriteEtags::default();
    let mut seen = Seen::default();
    c3_etags_name_bytes(&s, "contract/", &mut seen).await;
}

/// The memory double behind ETags that name each write (a counter), as
/// Azure's and GCS's own do: the shape the content-ETag layer exists to
/// hide.
#[derive(Default)]
struct WriteEtags {
    inner: MemoryStore,
    n: std::sync::Mutex<(u64, BTreeMap<String, String>)>,
}

impl WriteEtags {
    fn mint(&self, content: &str) -> String {
        let mut g = self.n.lock().unwrap();
        g.0 += 1;
        let e = format!("\"w{}\"", g.0);
        g.1.insert(e.clone(), content.to_string());
        e
    }
    fn content_of(&self, write: &str) -> Option<String> {
        self.n.lock().unwrap().1.get(write).cloned()
    }
}

#[async_trait::async_trait]
impl ObjectStore for WriteEtags {
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        condition: &PutCondition,
        stamps: &GenerationStamps,
        crc64: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        // Translate a write ETag back to the bytes' ETag the double keys on.
        let cond = match condition {
            PutCondition::IfMatch(w) => match self.content_of(w) {
                Some(c) => PutCondition::IfMatch(c),
                None => return Err(StoreError::PreconditionFailed(w.clone())),
            },
            other => other.clone(),
        };
        let mut m = self.inner.put_whole(key, body, &cond, stamps, crc64).await?;
        // Only the latest write's ETag answers If-Match.
        let current = m.etag.clone();
        self.n.lock().unwrap().1.retain(|_, c| c != &current);
        m.etag = self.mint(&current);
        Ok(m)
    }
    async fn copy_object(&self, _: &str, _: Option<&str>, _: &str, _: &PutCondition, _: &GenerationStamps) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        unimplemented!("the control does not copy")
    }
    async fn compose_generation(&self, _: &ComposeSpec<'_>) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        unimplemented!("the control does not compose")
    }
    async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.head(key).await
    }
    async fn get_whole(&self, key: &str, if_match: Option<&str>) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.inner.get_whole(key, if_match).await
    }
    async fn get_range(&self, key: &str, offset: u64, len: u64, if_match: &str) -> flint_store::StoreResult<Bytes> {
        self.inner.get_range(key, offset, len, if_match).await
    }
    async fn list(&self, prefix: &str) -> flint_store::StoreResult<Vec<flint_store::ListedObject>> {
        self.inner.list(prefix).await
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.inner.delete(key).await
    }
    async fn list_uploads(&self, prefix: &str) -> flint_store::StoreResult<Vec<flint_store::PendingUpload>> {
        self.inner.list_uploads(prefix).await
    }
    async fn abort_upload(&self, key: &str, upload_id: &str) -> flint_store::StoreResult<()> {
        self.inner.abort_upload(key, upload_id).await
    }
    async fn bootstrap(&self, prefix: &str) -> flint_store::StoreResult<flint_store::BootstrapReport> {
        self.inner.bootstrap(prefix).await
    }
    async fn epoch_read(&self, key: &str) -> flint_store::StoreResult<Option<flint_store::EpochState>> {
        self.inner.epoch_read(key).await
    }
    async fn epoch_acquire(&self, key: &str, holder_id: &str, supersede: Option<&flint_store::EpochState>) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.inner.epoch_acquire(key, holder_id, supersede).await
    }
    async fn epoch_renew(&self, key: &str, lease: &flint_store::EpochLease, echo: Option<&str>) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.inner.epoch_renew(key, lease, echo).await
    }
    async fn epoch_release(&self, key: &str, lease: &flint_store::EpochLease) -> flint_store::StoreResult<()> {
        self.inner.epoch_release(key, lease).await
    }
    fn min_part_size(&self) -> u64 {
        self.inner.min_part_size()
    }
    fn max_parts(&self) -> usize {
        self.inner.max_parts()
    }
}

/// A live Azure Blob container: Azurite first, a real account after.
///
/// FLINT_CONTRACT_AZURE_ENDPOINT=http://127.0.0.1:10000/devstoreaccount1 \
/// FLINT_CONTRACT_AZURE_ACCOUNT=devstoreaccount1 FLINT_CONTRACT_AZURE_KEY=<Azurite's published key> \
/// [FLINT_CONTRACT_AZURE_CONTAINER=contract] [FLINT_CONTRACT_SKIP=...] \
///   cargo test -p flint-store --features azure --test contract a_live_azure -- --ignored --nocapture
///
/// No endpoint is the account's own service. The container is created
/// when missing; everything written is under one fresh prefix, deleted at
/// the end.
#[cfg(feature = "azure")]
#[tokio::test]
#[ignore]
async fn a_live_azure_store_meets_the_contract() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let store = flint_store::azure::AzureStore::with_shared_key(
        env("FLINT_CONTRACT_AZURE_ACCOUNT"),
        &env("FLINT_CONTRACT_AZURE_KEY"),
        std::env::var("FLINT_CONTRACT_AZURE_CONTAINER").unwrap_or_else(|_| "contract".into()),
        std::env::var("FLINT_CONTRACT_AZURE_ENDPOINT").ok(),
    )
    .unwrap();
    store.ensure_container().await.expect("the container");
    assert_eq!(store.provider(), flint_store::Provider::Azure);
    // The control: the service checks the signature. A wrong key is
    // refused, or everything below passed without authorization at all.
    let wrong = flint_store::azure::AzureStore::with_shared_key(
        env("FLINT_CONTRACT_AZURE_ACCOUNT"),
        "d3Jvbmcga2V5IHdyb25nIGtleSB3cm9uZyBrZXkgd3Jvbmcga2V5IHdyb25nIGtleQ==",
        store.container(),
        std::env::var("FLINT_CONTRACT_AZURE_ENDPOINT").ok(),
    )
    .unwrap();
    let refused = wrong.list("").await;
    assert!(matches!(refused, Err(flint_store::StoreError::Auth(_))), "a wrong key was not refused: {refused:?}");
    let skip_env = std::env::var("FLINT_CONTRACT_SKIP").unwrap_or_default();
    let skip: Vec<&str> = skip_env.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let second = flint_store::azure::AzureStore::with_shared_key(
        env("FLINT_CONTRACT_AZURE_ACCOUNT"),
        &env("FLINT_CONTRACT_AZURE_KEY"),
        store.container(),
        std::env::var("FLINT_CONTRACT_AZURE_ENDPOINT").ok(),
    )
    .unwrap();
    let base = format!("flint-contract/{}/", uuid::Uuid::new_v4());
    run_contract_with(&store, Some(&second), &base, &skip).await;
    for o in store.list(&base).await.unwrap() {
        let _ = store.delete(&o.key).await;
    }
    eprintln!("contract: passed against azure container {} (skipped {skip:?})", store.container());
}

/// A live GCS bucket: storage-testbench first, a real bucket after.
///
/// FLINT_CONTRACT_GCS_ENDPOINT=http://127.0.0.1:9023 FLINT_CONTRACT_GCS_EMULATOR=1 \
/// [FLINT_CONTRACT_GCS_BUCKET=contract] [FLINT_CONTRACT_SKIP=...] \
///   cargo test -p flint-store --features gcs --test contract a_live_gcs -- --ignored --nocapture
///
/// Without `FLINT_CONTRACT_GCS_EMULATOR` the credentials come from the
/// environment as a deployment's do (`GcsStore::from_env`), and a wrong
/// token must be refused. An emulator authorizes nothing, so it has no
/// such control. The bucket is created on an emulator; everything written
/// is under one fresh prefix, deleted at the end.
#[cfg(feature = "gcs")]
#[tokio::test]
#[ignore]
async fn a_live_gcs_store_meets_the_contract() {
    use flint_store::gcs::GcsStore;
    let bucket = std::env::var("FLINT_CONTRACT_GCS_BUCKET").unwrap_or_else(|_| "contract".into());
    let endpoint = std::env::var("FLINT_CONTRACT_GCS_ENDPOINT").ok();
    let emulator = std::env::var("FLINT_CONTRACT_GCS_EMULATOR").is_ok_and(|v| v == "1");
    let base = format!("flint-contract/{}/", uuid::Uuid::new_v4());
    let open = || -> GcsStore {
        let s = if emulator {
            GcsStore::emulator(endpoint.clone().expect("FLINT_CONTRACT_GCS_ENDPOINT is not set"), bucket.clone())
        } else {
            GcsStore::from_env(bucket.clone(), endpoint.clone())
        };
        s.unwrap().with_scratch_root(base.clone())
    };
    let store = open();
    if emulator {
        store.ensure_bucket("flint-contract").await.expect("the bucket");
    } else {
        let wrong = GcsStore::with_token("flint-contract-wrong-token", bucket.clone(), endpoint.clone()).unwrap();
        let refused = wrong.list("").await;
        assert!(matches!(refused, Err(StoreError::Auth(_))), "a wrong token was not refused: {refused:?}");
    }
    assert_eq!(store.provider(), flint_store::Provider::Gcs);
    let skip_env = std::env::var("FLINT_CONTRACT_SKIP").unwrap_or_default();
    let skip: Vec<&str> = skip_env.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let second = open();
    run_contract_with(&store, Some(&second), &base, &skip).await;
    gcs_composes_nest_and_leave_no_parts(&store, &base).await;
    for o in store.list(&base).await.unwrap() {
        let _ = store.delete(&o.key).await;
    }
    eprintln!("contract: passed against gcs bucket {bucket} (skipped {skip:?}{})", if emulator { ", emulator" } else { "" });
}

/// GCS assembles a large object from part objects: past 32 parts the
/// compose nests, and whatever happens the parts are gone after. The
/// upload sweep sees a crashed compose's parts and removes them.
#[cfg(feature = "gcs")]
async fn gcs_composes_nest_and_leave_no_parts(s: &flint_store::gcs::GcsStore, base: &str) {
    let part = 1000u64;
    let n = 70u64;
    let body: Vec<u8> = (0..part * n).map(|i| (i * 7 % 253) as u8).collect();
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(&body).unwrap();
    let file = Arc::new(f);
    let key = format!("{base}gcs/nested");
    let spec = ComposeSpec {
        parts: (0..n).map(|i| PartSource::Local { offset: i * part, len: part }).collect(),
        ..compose(&key, &file, body.len() as u64, s)
    };
    let before = s.request_counts().unwrap();
    let made = s.compose_generation(&spec).await.expect("a 70-part compose");
    let after = s.request_counts().unwrap();
    // 70 part uploads; three composes of at most 32, then the one that lands.
    assert_eq!((after.put - before.put, after.multipart - before.multipart), (70, 4), "the compose did not nest");
    assert_eq!(made.crc64_b64.as_deref(), Some(crc64_to_b64(crc64_nvme(&body)).as_str()));
    assert_eq!(s.get_whole(&key, Some(&made.etag)).await.unwrap().1, Bytes::from(body.clone()), "the nested compose's bytes");
    // The same parts again, create-only over it: refused, and its parts gone too.
    assert!(lost_race(&s.compose_generation(&spec).await), "a create-only compose over an object landed");
    assert!(s.list_uploads(base).await.unwrap().is_empty(), "a compose left parts behind");
    // A crashed compose's part, as the sweep finds it.
    let stray = format!("{base}{}0123456789abcdef/00000", flint_store::gcs::PARTS_DIR);
    put(s, &stray, b"half a file", &PutCondition::IfNoneMatchAny).await.unwrap();
    let pending = s.list_uploads(base).await.unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].upload_id, "0123456789abcdef");
    assert!(pending[0].initiated_unix.is_some(), "a part with no date is never swept");
    assert!(s.abort_upload(&format!("{base}gcs/"), "0123456789abcdef").await.is_err(), "an abort outside a parts directory");
    s.abort_upload(&pending[0].key, &pending[0].upload_id).await.unwrap();
    assert!(s.list_uploads(base).await.unwrap().is_empty(), "the abort left the part");
}

/// A local HTTP server standing in for a token endpoint (Entra ID, the
/// instance metadata service, Google's metadata server, STS): it answers
/// every request with `answer(method and target, body)` and counts them.
#[cfg(any(feature = "azure", feature = "gcs"))]
mod idp {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub struct Idp {
        pub base: String,
        pub asked: Arc<AtomicUsize>,
    }

    impl Idp {
        pub fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    pub async fn start(answer: impl Fn(&str, &str) -> (u16, String) + Send + Sync + 'static) -> Idp {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(AtomicUsize::new(0));
        let (count, answer) = (asked.clone(), Arc::new(answer));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (count, answer) = (count.clone(), answer.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 16 * 1024];
                    let (line, body) = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                            .unwrap_or(0);
                        if body.len() >= len {
                            break (head.lines().next().unwrap_or("").to_string(), body.to_string());
                        }
                    };
                    count.fetch_add(1, Ordering::SeqCst);
                    let (status, reply) = answer(&line, &body);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        Idp { base, asked }
    }

    /// An Entra-shaped access token Azurite's `--oauth basic` takes: it
    /// checks the issuer, the audience and the lifetime, not the signature.
    #[cfg(feature = "azure")]
    pub fn entra_jwt(audience: &str) -> String {
        let b64 = |b: &[u8]| flint_store::base64_std(b).trim_end_matches('=').replace('+', "-").replace('/', "_");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let claims = serde_json::json!({
            "aud": audience, "iss": "https://sts.windows.net/00000000-0000-0000-0000-000000000001/",
            "iat": now - 60, "nbf": now - 60, "exp": now + 3600, "oid": "flint-contract",
        });
        format!("{}.{}.{}", b64(br#"{"alg":"RS256","typ":"JWT"}"#), b64(claims.to_string().as_bytes()), b64(b"unsigned"))
    }
}

/// A live Azure container reached with Entra ID tokens: Azurite over
/// HTTPS with `--oauth basic`, the token endpoint a local stand-in. The
/// federated arm runs the whole contract; the managed identity's runs one
/// round trip. Each has its control: a token for another audience is
/// refused, or Azurite was not checking tokens at all.
///
/// FLINT_CONTRACT_AZURE_ENDPOINT=https://127.0.0.1:10443/acct FLINT_CONTRACT_AZURE_ACCOUNT=acct \
/// SSL_CERT_FILE=<the CA Azurite's certificate chains to> \
///   cargo test -p flint-store --features azure --test contract a_live_azure_store_takes_entra -- --ignored --nocapture
#[cfg(feature = "azure")]
#[tokio::test]
#[ignore]
async fn a_live_azure_store_takes_entra_tokens() {
    use flint_store::azure::{AzureStore, EntraSource};
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let (account, endpoint) = (env("FLINT_CONTRACT_AZURE_ACCOUNT"), env("FLINT_CONTRACT_AZURE_ENDPOINT"));
    assert!(endpoint.starts_with("https://"), "Azurite takes tokens over HTTPS only");
    let container = std::env::var("FLINT_CONTRACT_AZURE_CONTAINER").unwrap_or_else(|_| "contract-entra".into());
    let entra = idp::start(|line, body| {
        assert!(line.starts_with("POST /tenant-a/oauth2/v2.0/token "), "{line}");
        assert!(body.contains("client_assertion=pod-token") && body.contains("client_id=client-b"), "{body}");
        (200, serde_json::json!({ "access_token": idp::entra_jwt("https://storage.azure.com"), "expires_in": 3599 }).to_string())
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "pod-token").unwrap();
    let federated = || EntraSource::Federated {
        tenant: "tenant-a".into(),
        client_id: "client-b".into(),
        token_file: dir.path().join("token"),
        authority: entra.base.clone(),
    };
    let store = AzureStore::with_entra(&account, federated(), &container, Some(endpoint.clone())).unwrap();
    store.ensure_container().await.expect("the container, with a token");
    let second = AzureStore::with_entra(&account, federated(), &container, Some(endpoint.clone())).unwrap();
    let base = format!("flint-contract/{}/", uuid::Uuid::new_v4());
    run_contract_with(&store, Some(&second), &base, &[]).await;
    assert_eq!(entra.asked(), 2, "one grant per client, kept for the run");

    // The managed identity, from a stand-in instance metadata service.
    let imds = idp::start(|line, _| {
        assert!(line.starts_with("GET /metadata/identity/oauth2/token?api-version=2018-02-01&resource="), "{line}");
        (200, serde_json::json!({ "access_token": idp::entra_jwt("https://storage.azure.com"), "expires_in": "86399" }).to_string())
    })
    .await;
    let mi = AzureStore::with_entra(
        &account,
        EntraSource::ManagedIdentity { client_id: None, endpoint: imds.base.clone() },
        &container,
        Some(endpoint.clone()),
    )
    .unwrap();
    let k = format!("{base}mi/obj");
    let made = put(&mi, &k, b"by managed identity", &PutCondition::IfNoneMatchAny).await.expect("a put with the managed identity's token");
    assert_eq!(mi.get_whole(&k, Some(&made.etag)).await.unwrap().1, Bytes::from_static(b"by managed identity"));
    assert_eq!(imds.asked(), 1);

    // The control: a token for another resource is refused.
    let wrong = idp::start(|_, _| {
        (200, serde_json::json!({ "access_token": idp::entra_jwt("https://vault.azure.net"), "expires_in": 3599 }).to_string())
    })
    .await;
    let refused = AzureStore::with_entra(
        &account,
        EntraSource::ManagedIdentity { client_id: None, endpoint: wrong.base.clone() },
        &container,
        Some(endpoint),
    )
    .unwrap()
    .list(&base)
    .await;
    assert!(matches!(refused, Err(StoreError::Auth(_))), "a token for another audience was not refused: {refused:?}");
    for o in store.list(&base).await.unwrap() {
        let _ = store.delete(&o.key).await;
    }
    eprintln!("contract: passed against azure container {container} with Entra tokens (federated, managed identity)");
}

/// A live GCS bucket reached through workload identity: the metadata
/// server, and Workload Identity Federation (STS, then impersonation),
/// each a local stand-in. storage-testbench checks no bearer, so this is
/// the plumbing: a token fetched by the right flow, kept, and sent.
///
/// FLINT_CONTRACT_GCS_ENDPOINT=http://127.0.0.1:9023 \
///   cargo test -p flint-store --features gcs --test contract a_live_gcs_store_takes_workload -- --ignored --nocapture
#[cfg(feature = "gcs")]
#[tokio::test]
#[ignore]
async fn a_live_gcs_store_takes_workload_identity() {
    use flint_store::gcs::GcsStore;
    let endpoint = std::env::var("FLINT_CONTRACT_GCS_ENDPOINT").expect("FLINT_CONTRACT_GCS_ENDPOINT is not set");
    let bucket = std::env::var("FLINT_CONTRACT_GCS_BUCKET").unwrap_or_else(|_| "contract".into());
    GcsStore::emulator(endpoint.clone(), bucket.clone()).unwrap().ensure_bucket("flint-contract").await.unwrap();
    let base = format!("flint-contract/{}/", uuid::Uuid::new_v4());

    let md = idp::start(|line, _| {
        assert!(line.starts_with("GET /computeMetadata/v1/instance/service-accounts/default/token "), "{line}");
        (200, r#"{"access_token":"md-token","expires_in":3599,"token_type":"Bearer"}"#.into())
    })
    .await;
    let host = md.base.trim_start_matches("http://").to_string();
    let open_md = || GcsStore::with_metadata_server(Some(host.clone()), bucket.clone(), Some(endpoint.clone())).unwrap().with_scratch_root(base.clone());
    let (a, b) = (open_md(), open_md());
    run_contract_with(&a, Some(&b), &format!("{base}md/"), &[]).await;
    assert_eq!(md.asked(), 2, "one token per client, kept for the run");

    let sts = idp::start(|line, body| {
        if line.starts_with("POST /v1/token ") {
            assert!(body.contains("subject_token=ksa-token"), "{body}");
            return (200, r#"{"access_token":"federated","token_type":"Bearer","expires_in":3600}"#.into());
        }
        assert!(line.contains(":generateAccessToken "), "{line}");
        (200, r#"{"accessToken":"impersonated","expireTime":"2099-01-01T00:00:00Z"}"#.into())
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "ksa-token").unwrap();
    let config = serde_json::json!({
        "type": "external_account",
        "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/k",
        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
        "token_url": format!("{}/v1/token", sts.base),
        "credential_source": { "file": dir.path().join("token") },
        "service_account_impersonation_url": format!("{}/v1/projects/-/serviceAccounts/lean@p.iam.gserviceaccount.com:generateAccessToken", sts.base),
    })
    .to_string();
    let wif = GcsStore::with_credentials_json(&config, bucket.clone(), Some(endpoint.clone())).unwrap().with_scratch_root(base.clone());
    let k = format!("{base}wif/obj");
    let made = put(&wif, &k, b"by federation", &PutCondition::IfNoneMatchAny).await.expect("a put with a federated token");
    assert_eq!(wif.get_whole(&k, Some(&made.etag)).await.unwrap().1, Bytes::from_static(b"by federation"));
    assert_eq!(sts.asked(), 2, "one exchange and one impersonation");
    for o in a.list(&base).await.unwrap() {
        let _ = a.delete(&o.key).await;
    }
    eprintln!("contract: passed against gcs bucket {bucket} through the metadata server and federation (stand-ins)");
}

/// A live S3-API store, MinIO and Ozone included. Everything it writes is
/// under one fresh prefix, deleted at the end.
#[cfg(feature = "s3")]
#[tokio::test]
#[ignore]
async fn a_live_s3_store_meets_the_contract() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let store = flint_store::s3::S3Store::with_credentials(
        env("FLINT_CONTRACT_BUCKET"),
        env("FLINT_CONTRACT_ENDPOINT"),
        std::env::var("FLINT_CONTRACT_REGION").unwrap_or_else(|_| "us-east-1".into()),
        env("FLINT_CONTRACT_KEY"),
        env("FLINT_CONTRACT_SECRET"),
    )
    .unwrap();
    let skip_env = std::env::var("FLINT_CONTRACT_SKIP").unwrap_or_default();
    let skip: Vec<&str> = skip_env.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let second = flint_store::s3::S3Store::with_credentials(
        env("FLINT_CONTRACT_BUCKET"),
        env("FLINT_CONTRACT_ENDPOINT"),
        std::env::var("FLINT_CONTRACT_REGION").unwrap_or_else(|_| "us-east-1".into()),
        env("FLINT_CONTRACT_KEY"),
        env("FLINT_CONTRACT_SECRET"),
    )
    .unwrap();
    let base = format!("flint-contract/{}/", uuid::Uuid::new_v4());
    run_contract_with(&store, Some(&second), &base, &skip).await;
    for o in store.list(&base).await.unwrap() {
        let _ = store.delete(&o.key).await;
    }
    eprintln!("contract: passed against {} (skipped {skip:?})", env("FLINT_CONTRACT_ENDPOINT"));
}
