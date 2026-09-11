//! The in-process ObjectStore test double.
//!
//! Implements the FULL conditional contract — 412 on both put flavors,
//! copy-source-if-match at part-copy time, server-side CRC-64/NVME
//! validation at publish, MPU state with list/abort, epoch CAS — so
//! every backend-neutral tier test (arbitration, flush pipeline,
//! hydration) runs against semantics, not stubs. Failure injection
//! covers the two adversarial publish shapes A6's arbitration must
//! tell apart:
//!
//! - [`inject_torn_complete`]: the Complete LANDS server-side but the
//!   response is lost (network tear) — the object exists at g+1 with
//!   our stamps while the client saw an error.
//! - [`inject_crash_before_complete`]: the assembly dies before
//!   Complete — parts pending, nothing published, base object intact.
//!
//! [`inject_torn_complete`]: MemoryStore::inject_torn_complete
//! [`inject_crash_before_complete`]: MemoryStore::inject_crash_before_complete

use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone)]
struct StoredObject {
    bytes: Bytes,
    etag: String,
    crc64: u64,
    meta: HashMap<String, String>,
    last_modified_unix: u64,
    /// The version's identity (boundary-verbs plan D7). Minted from a
    /// per-store COUNTER, never a uuid or a clock: a reproducible
    /// battery is the whole reason this double exists, and a
    /// nondeterministic id would make every version-citing assertion
    /// order-dependent in a way that only shows up under load.
    version_id: String,
    /// A delete marker. Under versioning a DELETE never destroys data:
    /// it makes the key read as absent while every prior version stays
    /// fetchable by id.
    deleted: bool,
}

impl StoredObject {
    fn to_meta(&self) -> ObjectMeta {
        ObjectMeta {
            etag: self.etag.clone(),
            size: self.bytes.len() as u64,
            crc64_b64: Some(crc64_to_b64(self.crc64)),
            meta: self.meta.clone(),
            last_modified_unix: Some(self.last_modified_unix),
            // The fake models STANDARD only; the IA guard's unit tests
            // exercise copy_allowed=false at the planner level.
            storage_class: None,
            version_id: Some(self.version_id.clone()),
        }
    }
}

struct Mpu {
    key: String,
    parts: BTreeMap<usize, Bytes>,
    initiated_unix: u64,
}

/// A key's version chain: oldest first, LAST is current. Nothing in
/// this double ever overwrites in place — that is the entire point of
/// modelling versions, since an in-place overwrite is the destructive
/// design versioned staging exists to avoid (§2.4.2).
#[derive(Default, Clone)]
struct VersionChain {
    versions: Vec<StoredObject>,
}

impl VersionChain {
    /// The current version, or None if the key reads as absent (empty
    /// chain, or a delete marker on top).
    fn current(&self) -> Option<&StoredObject> {
        match self.versions.last() {
            Some(o) if !o.deleted => Some(o),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Inner {
    chains: BTreeMap<String, VersionChain>,
    uploads: HashMap<String, Mpu>,
    /// Deterministic version-id source (see `StoredObject::version_id`).
    version_seq: u64,
    /// The bucket's lifecycle rules. Modelled because D8's hazard is a
    /// rule the CUSTOMER already owns — the battery has to be able to
    /// plant one (`plant_lifecycle_rule`) and watch gated mode refuse.
    lifecycle: Vec<LifecycleView>,
}

impl Inner {
    fn next_version_id(&mut self) -> String {
        self.version_seq += 1;
        format!("v{:012}", self.version_seq)
    }

    /// The current version of `key`, or None if absent.
    fn current(&self, key: &str) -> Option<&StoredObject> {
        self.chains.get(key).and_then(|c| c.current())
    }

    /// Append a version and make it current.
    fn push(&mut self, key: &str, mut obj: StoredObject) -> String {
        let vid = self.next_version_id();
        obj.version_id = vid.clone();
        self.chains.entry(key.to_string()).or_default().versions.push(obj);
        vid
    }

    /// A keyed DELETE: a delete marker, never destruction.
    fn push_delete_marker(&mut self, key: &str) {
        if self.chains.get(key).map(|c| c.current().is_none()).unwrap_or(true) {
            return; // already absent: S3 still adds a marker; we keep the chain quiet
        }
        let vid = self.next_version_id();
        self.chains.entry(key.to_string()).or_default().versions.push(StoredObject {
            bytes: Bytes::new(),
            etag: String::new(),
            crc64: 0,
            meta: HashMap::new(),
            last_modified_unix: now_unix(),
            version_id: vid,
            deleted: true,
        });
    }

    fn version(&self, key: &str, version_id: &str) -> Option<&StoredObject> {
        self.chains
            .get(key)?
            .versions
            .iter()
            .find(|o| o.version_id == version_id && !o.deleted)
    }

    /// Version-scoped DELETE: removes exactly that version. Removing
    /// the current version promotes the one beneath it, exactly as S3
    /// does.
    fn remove_version(&mut self, key: &str, version_id: &str) {
        if let Some(chain) = self.chains.get_mut(key) {
            chain.versions.retain(|o| o.version_id != version_id);
            if chain.versions.is_empty() {
                self.chains.remove(key);
            }
        }
    }
}

const INJECT_NONE: u8 = 0;
const INJECT_TORN_COMPLETE: u8 = 1;
const INJECT_CRASH_BEFORE_COMPLETE: u8 = 2;

pub struct MemoryStore {
    inner: Mutex<Inner>,
    upload_seq: AtomicU64,
    /// One-shot failure injection for the NEXT compose publish.
    inject: AtomicU8,
    /// Set by the crash injection: the failing compose must LEAVE its
    /// orphan MPU (the A9 sweep's test state) instead of aborting it.
    leave_orphan: AtomicBool,
    /// Step-11 drill injections: counted get_range failures / stall.
    fail_get_range_count: AtomicU64,
    /// Counted HEAD failures — the sweep's transient-throttle drill.
    fail_head_count: AtomicU64,
    /// Counted `epoch_renew` failures that are NOT a 412: the store
    /// unreachable from the holder (forge X13's drill).
    fail_epoch_renew_count: AtomicU64,
    /// The next n whole PUTs LAND and then report 409. See
    /// `inject_put_lands_then_fails`.
    fail_put_after_land_count: AtomicU64,
    fail_put_after_land_key: Mutex<String>,
    stall_next_get_range_ms: AtomicU64,
    /// A latency every get_range pays, in ms: the double's stand-in for
    /// a network round trip, so a test can observe whether fetches
    /// OVERLAP rather than time them.
    get_range_delay_ms: AtomicU64,
    /// While set, every whole PUT parks at the door until
    /// `release_held_puts`: a stalled upload a test can run something
    /// BESIDE (a batch beside a fold's upload), with the ordering held
    /// by the gate rather than by a sleep the scheduler may outlast.
    hold_puts: AtomicBool,
    held_puts: Mutex<Vec<tokio::sync::oneshot::Sender<()>>>,
    /// get_range calls in flight, and the most there have ever been at
    /// once: a fan-out's bound, observed from both sides.
    inflight_get_range: AtomicU64,
    peak_get_range: AtomicU64,
    /// Model a project-scoped proxy that STRIPS `x-amz-version-id`:
    /// every write still succeeds and reports no version. This is D8's
    /// silent-degradation hazard, and the only way to test that the
    /// conformance probe refuses instead of degrading.
    strip_version_ids: AtomicBool,
    /// Model a bucket whose lifecycle rules are readable but not
    /// writable (a scoped operator principal): the backstop cannot be
    /// provisioned, which is a DEGRADATION, not a torn view.
    fail_lifecycle_writes: AtomicBool,
    /// Per-operation call counts. Cost measurement is otherwise
    /// guesswork: the request SHAPE of a tick is the thing the plan
    /// prices, and it is not derivable from the code by reading.
    ops: Mutex<std::collections::BTreeMap<&'static str, u64>>,
    pub min_part: u64,
    pub max_parts: usize,
}

/// Leaves the in-flight count on drop, so an early `return` still
/// leaves.
struct InFlight<'a>(&'a AtomicU64);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// Age an object's last-modified stamp. Liveness here is judged
    /// against the STORE's clock (A8), so a test about a dead holder
    /// has to be able to move that clock rather than sleep.
    pub fn backdate_epoch(&self, key: &str, secs: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(chain) = inner.chains.get_mut(key) {
            for v in chain.versions.iter_mut() {
                v.last_modified_unix = v.last_modified_unix.saturating_sub(secs);
            }
        }
    }

    /// Plant a lifecycle rule the way a customer's own fleet policy
    /// would. D8's destroyer is a rule flint never wrote.
    pub fn plant_lifecycle_rule(&self, rule: LifecycleView) {
        self.inner.lock().unwrap().lifecycle.push(rule);
    }

    pub fn new() -> Self {
        MemoryStore {
            ops: Mutex::new(Default::default()),
            inner: Mutex::new(Inner::default()),
            upload_seq: AtomicU64::new(1),
            inject: AtomicU8::new(INJECT_NONE),
            leave_orphan: AtomicBool::new(false),
            fail_get_range_count: AtomicU64::new(0),
            fail_head_count: AtomicU64::new(0),
            fail_epoch_renew_count: AtomicU64::new(0),
            fail_put_after_land_count: AtomicU64::new(0),
            fail_put_after_land_key: Mutex::new(String::new()),
            stall_next_get_range_ms: AtomicU64::new(0),
            get_range_delay_ms: AtomicU64::new(0),
            hold_puts: AtomicBool::new(false),
            held_puts: Mutex::new(Vec::new()),
            inflight_get_range: AtomicU64::new(0),
            peak_get_range: AtomicU64::new(0),
            strip_version_ids: AtomicBool::new(false),
            fail_lifecycle_writes: AtomicBool::new(false),
            // Tiny granularity by default so tests compose small
            // files; S3's real limits live in the S3 backend.
            min_part: 1,
            max_parts: 10_000,
        }
    }

    /// Every write from here on reports no version id — a proxy that
    /// strips the header (D8's refusal arm).
    pub fn strip_version_ids(&self, on: bool) {
        self.strip_version_ids.store(on, Ordering::SeqCst);
    }

    /// Lifecycle writes fail from here on; reads keep working.
    pub fn fail_lifecycle_writes(&self, on: bool) {
        self.fail_lifecycle_writes.store(on, Ordering::SeqCst);
    }

    /// Next compose: Complete LANDS but the response is lost.
    pub fn inject_torn_complete(&self) {
        self.inject.store(INJECT_TORN_COMPLETE, Ordering::SeqCst);
    }

    /// Next compose: dies before Complete — MPU left pending.
    pub fn inject_crash_before_complete(&self) {
        self.inject.store(INJECT_CRASH_BEFORE_COMPLETE, Ordering::SeqCst);
    }

    /// Step-11 drills: the NEXT get_range fails with a transport
    /// error — a single failure is absorbed by the chunk-retry loop.
    pub fn inject_get_range_failure(&self) {
        self.inject_get_range_failures(1);
    }

    /// The next `n` get_range calls fail with a transport error —
    /// exceed the chunk-retry budget to force the truncate-back path.
    pub fn inject_get_range_failures(&self, n: u64) {
        self.fail_get_range_count.store(n, Ordering::SeqCst);
    }

    /// Every get_range from now on sleeps `ms` first. Unlike the
    /// one-shot stall this is persistent, and it is what makes a
    /// fan-out OBSERVABLE: calls that overlap are in flight together
    /// for the whole delay, so `peak_get_range_in_flight` reads the
    /// caller's bound rather than the scheduler's luck.
    pub fn inject_get_range_delay_ms(&self, ms: u64) {
        self.get_range_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// Every whole PUT from now on parks until `release_held_puts`
    /// (`false` lets new PUTs through again; a PUT already parked stays
    /// parked). Checked at the start of each PUT, so a test can stall
    /// one task's upload, let the next caller's through at once, and
    /// release the stalled one when it chooses.
    pub fn inject_put_hold(&self, on: bool) {
        self.hold_puts.store(on, Ordering::SeqCst);
    }

    /// How many whole PUTs are parked right now.
    pub fn held_puts(&self) -> usize {
        self.held_puts.lock().unwrap().len()
    }

    /// Let every parked PUT land.
    pub fn release_held_puts(&self) {
        for tx in self.held_puts.lock().unwrap().drain(..) {
            let _ = tx.send(());
        }
    }

    /// The most get_range calls ever in flight at once since the last
    /// reset. A fan-out bounded at N reads exactly N here when there is
    /// work for N — and exactly 1 when it is not a fan-out at all.
    pub fn peak_get_range_in_flight(&self) -> u64 {
        self.peak_get_range.load(Ordering::SeqCst)
    }
    pub fn reset_peak_get_range_in_flight(&self) {
        self.peak_get_range.store(0, Ordering::SeqCst);
    }

    /// The next `n` HEAD calls fail with a transport error.
    ///
    /// The condition this models is not exotic: the foreign-key sweep
    /// issues one HEAD per unknown object, which on a large bucket is
    /// exactly the shape that earns a 503 SlowDown. The SDK's default
    /// retry absorbs a blip; a sustained throttle outlives it.
    pub fn inject_head_failures(&self, n: u64) {
        self.fail_head_count.store(n, Ordering::SeqCst);
    }

    /// The next `n` renewals fail as a transport error — not a 412, so
    /// the holder cannot tell "deposed" from "cut off". Zero clears it.
    pub fn inject_epoch_renew_failures(&self, n: u64) {
        self.fail_epoch_renew_count.store(n, Ordering::SeqCst);
    }

    /// The next `n` whole PUTs STORE THE OBJECT and then report 409.
    ///
    /// This is the one failure a conditional writer cannot interpret.
    /// S3 answers a contended conditional write with 409
    /// `ConditionalRequestConflict`, and a lost response to a PUT that
    /// landed looks the same from the client: the write is durable and
    /// the caller was told it failed. Every other injector here models
    /// a failure that did NOT happen server-side; this one models the
    /// failure that DID.
    /// `key_contains` narrows it to one object — a batch uploads its
    /// packs BEFORE the snapshot CAS, so an untargeted injection would
    /// break a pack upload and never reach the write under test.
    pub fn inject_put_lands_then_fails(&self, key_contains: &str, n: u64) {
        *self.fail_put_after_land_key.lock().unwrap() = key_contains.to_string();
        self.fail_put_after_land_count.store(n, Ordering::SeqCst);
    }

    /// Step-11 drills: the NEXT get_range stalls `ms` before serving —
    /// a slow hydration to race concurrent I/O and priority against.
    pub fn inject_get_range_stall(&self, ms: u64) {
        self.stall_next_get_range_ms.store(ms, Ordering::SeqCst);
    }

    /// Test surface: plant an object directly (foreign writers, torn
    /// own states, import fixtures). Stamps go in as given.
    pub fn raw_put(&self, key: &str, bytes: Bytes, meta: Vec<(String, String)>) -> ObjectMeta {
        let obj = StoredObject {
            etag: put_etag(&bytes),
            crc64: crc64_nvme(&bytes),
            meta: meta.into_iter().collect(),
            last_modified_unix: now_unix(),
            bytes,
            version_id: String::new(),
            deleted: false,
        };
        let mut inner = self.inner.lock().unwrap();
        let vid = inner.push(key, obj);
        let mut m = inner.current(key).map(|o| o.to_meta()).expect("just pushed");
        m.version_id = Some(vid);
        m
    }

    /// Test surface: the NONCURRENT-VERSION LIFECYCLE BACKSTOP (D8),
    /// modelled as an explicit method because it is a timer in reality
    /// and an untestable one if left implicit.
    ///
    /// The hazard it exists to demonstrate is INVERTED from the
    /// eager-prefix design's: gated staging makes the **cited** version
    /// noncurrent the moment a newer generation is staged, so a
    /// `NoncurrentVersionExpiration` rule on `files/` runs a clock
    /// against live cited data — and never against the newest uncited
    /// bytes, which are current and which no lifecycle rule can reach.
    /// Reaping a cited noncurrent version dangles the manifest; that is
    /// `Inv_CitedVersionLives`'s required-firing mutation and leg B23's
    /// abandoned-mid-stage endgame.
    ///
    /// Returns the (key, version_id) pairs reaped.
    pub fn expire_noncurrent(&self, older_than_secs: u64) -> Vec<(String, String)> {
        let now = now_unix();
        let mut inner = self.inner.lock().unwrap();
        let mut reaped = vec![];
        let keys: Vec<String> = inner.chains.keys().cloned().collect();
        for k in keys {
            let Some(chain) = inner.chains.get_mut(&k) else { continue };
            let last = chain.versions.len().saturating_sub(1);
            let doomed: Vec<String> = chain
                .versions
                .iter()
                .enumerate()
                .filter(|(i, o)| {
                    *i != last && now.saturating_sub(o.last_modified_unix) >= older_than_secs
                })
                .map(|(_, o)| o.version_id.clone())
                .collect();
            for vid in doomed {
                chain.versions.retain(|o| o.version_id != vid);
                reaped.push((k.clone(), vid));
            }
        }
        reaped
    }

    /// Test surface: how many versions a key currently holds (leg B21's
    /// oracle — steady state must return to ONE live version per key,
    /// reclaimed by flint's exact per-citation GC, NOT by waiting for
    /// the backstop).
    pub fn version_count(&self, key: &str) -> usize {
        self.inner.lock().unwrap().chains.get(key).map(|c| c.versions.len()).unwrap_or(0)
    }

    /// Test surface: a DEPOSED writer's late CompleteMultipartUpload.
    /// Assembles whatever parts exist (unconditionally — the deposed
    /// straggler of the A8 drill carries no working guard); answers
    /// `NoSuchUpload` if the assembly was fenced away, exactly S3's
    /// answer after the takeover abort-sweep.
    pub fn raw_complete_upload(&self, upload_id: &str) -> StoreResult<ObjectMeta> {
        let mut inner = self.inner.lock().unwrap();
        let Some(mpu) = inner.uploads.remove(upload_id) else {
            return Err(StoreError::NoSuchUpload(format!("upload {}", upload_id)));
        };
        let mut bytes = Vec::new();
        let n_parts = mpu.parts.len().max(1);
        for part in mpu.parts.values() {
            bytes.extend_from_slice(part);
        }
        let bytes = Bytes::from(bytes);
        let obj = StoredObject {
            etag: mpu_etag(&bytes, n_parts),
            crc64: crc64_nvme(&bytes),
            meta: HashMap::new(),
            last_modified_unix: now_unix(),
            bytes,
                   version_id: String::new(),
            deleted: false,
        };
        let mkey = mpu.key.clone();
        let vid = inner.push(&mkey, obj);
        let mut m = inner.current(&mkey).expect("just pushed").to_meta();
        m.version_id = Some(vid);
        Ok(m)
    }

    /// Test surface: an orphan MPU (for the A9 sweep tests).
    pub fn raw_begin_upload(&self, key: &str) -> String {
        let id = format!("mpu-{}", self.upload_seq.fetch_add(1, Ordering::SeqCst));
        self.inner.lock().unwrap().uploads.insert(
            id.clone(),
            Mpu { key: key.to_string(), parts: BTreeMap::new(), initiated_unix: now_unix() },
        );
        id
    }

    fn check_condition(
        existing: Option<&StoredObject>,
        condition: &PutCondition,
    ) -> StoreResult<()> {
        match (condition, existing) {
            (PutCondition::IfMatch(want), Some(o)) if o.etag == *want => Ok(()),
            (PutCondition::IfMatch(_), Some(_)) => {
                Err(StoreError::PreconditionFailed("If-Match: etag differs".into()))
            }
            (PutCondition::IfMatch(_), None) => {
                Err(StoreError::PreconditionFailed("If-Match: no object".into()))
            }
            (PutCondition::IfNoneMatchAny, None) => Ok(()),
            (PutCondition::IfNoneMatchAny, Some(_)) => {
                Err(StoreError::PreconditionFailed("If-None-Match: object exists".into()))
            }
            (PutCondition::Unconditional, _) => Ok(()),
        }
    }
}

fn put_etag(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("\"{:x}\"", h.finalize())
}

/// Multipart etag: content hash + part count — deliberately a DIFFERENT
/// string than a whole-put of the same bytes, mirroring S3's "the
/// multipart ETag is not a content hash".
fn mpu_etag(bytes: &[u8], parts: usize) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("\"{:x}-{}\"", h.finalize(), parts)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EpochBody {
    holder_id: String,
    epoch: u64,
    /// Clean-shutdown mark — see `ObjectStore::epoch_release`.
    #[serde(default)]
    released: bool,
    /// The holder's observed-state echo (`LeaseEcho`), opaque here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    echo: Option<String>,
}

impl MemoryStore {
    fn bump(&self, op: &'static str) {
        *self.ops.lock().unwrap().entry(op).or_insert(0) += 1;
    }
    /// Call counts since construction or the last reset.
    pub fn op_counts(&self) -> std::collections::BTreeMap<&'static str, u64> {
        self.ops.lock().unwrap().clone()
    }
    pub fn reset_op_counts(&self) {
        self.ops.lock().unwrap().clear();
    }
    pub fn total_ops(&self) -> u64 {
        self.ops.lock().unwrap().values().sum()
    }
}

#[async_trait]
impl ObjectStore for MemoryStore {
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        condition: &PutCondition,
        stamps: &GenerationStamps,
        crc64: u64,
    ) -> StoreResult<ObjectMeta> {
        self.bump("put_whole");
        if self.hold_puts.load(Ordering::SeqCst) {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.held_puts.lock().unwrap().push(tx);
            // A dropped sender (a release that drained the list) lets
            // the PUT through as well.
            let _ = rx.await;
        }
        let actual = crc64_nvme(&body);
        if actual != crc64 {
            return Err(StoreError::ChecksumMismatch(format!(
                "put {}: claimed {:#x}, content is {:#x}",
                key, crc64, actual
            )));
        }
        let mut inner = self.inner.lock().unwrap();
        Self::check_condition(inner.current(key), condition)?;
        let obj = StoredObject {
            etag: put_etag(&body),
            crc64,
            meta: stamps.to_meta().into_iter().collect(),
            last_modified_unix: now_unix(),
            bytes: body,
            version_id: String::new(),
            deleted: false,
        };
        // The version id is minted by `push`; reporting a meta built
        // BEFORE it would hand the caller `Some("")` — and a manifest
        // that cites version "" makes the citation GC match nothing and
        // reap every real version of the key.
        let vid = inner.push(key, obj);
        let mut m = inner.current(key).expect("just pushed").to_meta();
        m.version_id = Some(vid);
        if self.strip_version_ids.load(Ordering::SeqCst) {
            // The version still EXISTS — the proxy only hid its name.
            // That asymmetry is the hazard: everything keeps working
            // until a citation needs to name a version.
            m.version_id = None;
        }
        // The object is already stored. Reporting the failure HERE is
        // the whole point: the caller cannot tell this from a write
        // that never happened.
        if self.fail_put_after_land_count.load(Ordering::SeqCst) > 0 {
            let pat = self.fail_put_after_land_key.lock().unwrap().clone();
            if (pat.is_empty() || key.contains(pat.as_str()))
                && self
                    .fail_put_after_land_count
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                    .is_ok()
            {
                return Err(StoreError::Conflict(format!(
                    "injected: the PUT of {key} landed and the response did not"
                )));
            }
        }
        Ok(m)
    }

    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> StoreResult<ObjectMeta> {
        self.bump("copy_object");
        let mut inner = self.inner.lock().unwrap();
        let src = inner
            .current(src_key)
            .ok_or_else(|| StoreError::NotFound(src_key.to_string()))?;
        // The SOURCE guard. A 412 here is not the destination's: the
        // object the caller read has been overwritten, and copying the
        // replacement would move bytes nobody asked for.
        if let Some(want) = src_if_match {
            if src.etag != want {
                return Err(StoreError::PreconditionFailed(format!(
                    "copy source {src_key} is at {} , not {want}",
                    src.etag
                )));
            }
        }
        let bytes = src.bytes.clone();
        // The CRC travels with the BYTES — they are the same bytes. The
        // stamps do NOT: a copy is a new publish under a new key, and
        // wearing the source's generation would file one object's
        // history under another's name. A double that copied the source
        // meta wholesale would pass a test the real store fails.
        let crc64 = src.crc64;
        Self::check_condition(inner.current(dst_key), condition)?;
        let obj = StoredObject {
            etag: put_etag(&bytes),
            crc64,
            meta: stamps.to_meta().into_iter().collect(),
            last_modified_unix: now_unix(),
            bytes,
            version_id: String::new(),
            deleted: false,
        };
        let vid = inner.push(dst_key, obj);
        let mut m = inner.current(dst_key).expect("just pushed").to_meta();
        m.version_id = Some(vid);
        if self.strip_version_ids.load(Ordering::SeqCst) {
            m.version_id = None;
        }
        Ok(m)
    }

    async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta> {
        self.bump("compose_generation");
        if spec.parts.is_empty() {
            return Err(StoreError::Other("compose: no parts".into()));
        }
        if spec.parts.len() > self.max_parts {
            return Err(StoreError::Other(format!(
                "compose: {} parts exceeds the backend maximum {}",
                spec.parts.len(),
                self.max_parts
            )));
        }
        let upload_id = self.raw_begin_upload(spec.key);
        // Everything below aborts the MPU on error (A9): no failure
        // path may leave the assembly pending — except the injected
        // crash, whose whole point is the orphan.
        let result = self.compose_inner(spec, &upload_id).await;
        if result.is_err() && !self.leave_orphan.swap(false, Ordering::SeqCst) {
            let mut inner = self.inner.lock().unwrap();
            inner.uploads.remove(&upload_id);
        }
        result
    }

    async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
        self.bump("head");
        if self
            .fail_head_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| c.checked_sub(1))
            .is_ok()
        {
            return Err(StoreError::Other("injected head failure".into()));
        }
        self.inner
            .lock()
            .unwrap()
            .current(key)
            .map(|o| o.to_meta())
            .ok_or_else(|| StoreError::NotFound(key.into()))
    }

    /// A stand-in URL: deterministic, unmistakably not real, and
    /// refused for a key that does not exist. A test can assert that
    /// the right key was signed and that a missing object is not
    /// advertised; whether a git client can actually fetch it is a
    /// question only a real store answers, and forge's storm leg is
    /// where that is asked.
    async fn presign_get(&self, key: &str, ttl_secs: u64) -> StoreResult<String> {
        self.bump("presign_get");
        let inner = self.inner.lock().unwrap();
        if inner.current(key).is_none() {
            return Err(StoreError::NotFound(key.into()));
        }
        Ok(format!("memory://presigned/{key}?ttl={ttl_secs}"))
    }

    /// A stand-in URL for a write. Unlike the read side it does NOT
    /// require the object to exist — that is the whole point of an
    /// upload — so what a test can assert is the key, and that an
    /// object already present was not offered one.
    async fn presign_put(&self, key: &str, ttl_secs: u64) -> StoreResult<String> {
        self.bump("presign_put");
        Ok(format!("memory://presigned-put/{key}?ttl={ttl_secs}"))
    }

    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> StoreResult<(ObjectMeta, Bytes)> {
        self.bump("get_whole");
        let inner = self.inner.lock().unwrap();
        let o = inner
            .current(key)
            .ok_or_else(|| StoreError::NotFound(key.into()))?;
        if let Some(want) = if_match {
            if o.etag != want {
                return Err(StoreError::PreconditionFailed(format!(
                    "get {}: etag {} != {}",
                    key, o.etag, want
                )));
            }
        }
        Ok((o.to_meta(), o.bytes.clone()))
    }

    async fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        if_match: &str,
    ) -> StoreResult<Bytes> {
        self.bump("get_range");
        let _in_flight = {
            let now = self.inflight_get_range.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_get_range.fetch_max(now, Ordering::SeqCst);
            InFlight(&self.inflight_get_range)
        };
        // Counted injections for the step-11 drills.
        if self
            .fail_get_range_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| c.checked_sub(1))
            .is_ok()
        {
            return Err(StoreError::Other("injected get_range failure".into()));
        }
        let stall = self.stall_next_get_range_ms.swap(0, Ordering::SeqCst);
        if stall > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(stall)).await;
        }
        let delay = self.get_range_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let inner = self.inner.lock().unwrap();
        let o = inner
            .current(key)
            .ok_or_else(|| StoreError::NotFound(key.into()))?;
        if o.etag != if_match {
            return Err(StoreError::PreconditionFailed(format!(
                "get_range {}: etag {} != {}",
                key, o.etag, if_match
            )));
        }
        let start = offset.min(o.bytes.len() as u64) as usize;
        let end = (offset + len).min(o.bytes.len() as u64) as usize;
        Ok(o.bytes.slice(start..end))
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>> {
        self.bump("list");
        Ok(self
            .inner
            .lock()
            .unwrap()
            .chains
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            // Current versions only, delete markers skipped — a LIST
            // must never report a key a GET would 404 on.
            .filter_map(|(k, c)| {
                c.current().map(|o| ListedObject {
                    key: k.clone(),
                    size: o.bytes.len() as u64,
                    etag: o.etag.clone(),
                    last_modified_unix: Some(o.last_modified_unix),
                })
            })
            .collect())
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        self.bump("delete");
        self.inner.lock().unwrap().push_delete_marker(key);
        Ok(())
    }

    async fn head_version(&self, key: &str, version_id: &str) -> StoreResult<ObjectMeta> {
        self.bump("head_version");
        self.inner
            .lock()
            .unwrap()
            .version(key, version_id)
            .map(|o| o.to_meta())
            .ok_or_else(|| StoreError::NotFound(format!("{key}?versionId={version_id}")))
    }

    async fn get_version(&self, key: &str, version_id: &str) -> StoreResult<(ObjectMeta, Bytes)> {
        self.bump("get_version");
        let inner = self.inner.lock().unwrap();
        let o = inner
            .version(key, version_id)
            .ok_or_else(|| StoreError::NotFound(format!("{key}?versionId={version_id}")))?;
        Ok((o.to_meta(), o.bytes.clone()))
    }

    async fn delete_version(&self, key: &str, version_id: &str) -> StoreResult<()> {
        self.bump("delete_version");
        // Idempotent, exactly as S3: deleting an absent version is Ok —
        // the GC pass and the operator sweep race each other by design.
        self.inner.lock().unwrap().remove_version(key, version_id);
        Ok(())
    }

    async fn list_versions(&self, prefix: &str) -> StoreResult<Vec<ListedVersion>> {
        self.bump("list_versions");
        let inner = self.inner.lock().unwrap();
        let mut out = vec![];
        for (k, chain) in inner.chains.range(prefix.to_string()..) {
            if !k.starts_with(prefix) {
                break;
            }
            let last = chain.versions.len().saturating_sub(1);
            for (i, o) in chain.versions.iter().enumerate() {
                out.push(ListedVersion {
                    key: k.clone(),
                    version_id: o.version_id.clone(),
                    etag: o.etag.clone(),
                    size: o.bytes.len() as u64,
                    is_current: i == last,
                    is_delete_marker: o.deleted,
                    last_modified_unix: Some(o.last_modified_unix),
                });
            }
        }
        Ok(out)
    }

    async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>> {
        self.bump("list_uploads");
        let inner = self.inner.lock().unwrap();
        let mut v: Vec<PendingUpload> = inner
            .uploads
            .iter()
            .filter(|(_, m)| m.key.starts_with(prefix))
            .map(|(id, m)| PendingUpload {
                key: m.key.clone(),
                upload_id: id.clone(),
                initiated_unix: Some(m.initiated_unix),
            })
            .collect();
        v.sort_by(|a, b| a.upload_id.cmp(&b.upload_id));
        Ok(v)
    }

    async fn abort_upload(&self, _key: &str, upload_id: &str) -> StoreResult<()> {
        self.bump("abort_upload");
        // Absent is Ok: abort races the lifecycle rule by design.
        self.inner.lock().unwrap().uploads.remove(upload_id);
        Ok(())
    }

    async fn bootstrap(&self, _prefix: &str) -> StoreResult<BootstrapReport> {
        self.bump("bootstrap");
        Ok(BootstrapReport {
            notes: vec!["memory store: no bucket posture to verify".into()],
            ..Default::default()
        })
    }

    async fn lifecycle_rules(&self) -> StoreResult<Vec<LifecycleView>> {
        self.bump("lifecycle_rules");
        Ok(self.inner.lock().unwrap().lifecycle.clone())
    }

    async fn ensure_noncurrent_retention(
        &self,
        prefix: &str,
        days: u64,
    ) -> StoreResult<RetentionOutcome> {
        self.bump("ensure_noncurrent_retention");
        if self.fail_lifecycle_writes.load(Ordering::SeqCst) {
            return Err(StoreError::Other("lifecycle writes are denied".into()));
        }
        let rule_id = format!("flint-lean-noncurrent-{}", prefix.trim_end_matches('/').replace('/', "-"));
        let mut inner = self.inner.lock().unwrap();
        // Read-merge-append, exactly as S3 must: an existing conforming
        // rule is left alone and NOTHING is rewritten.
        if let Some(r) = inner
            .lifecycle
            .iter()
            .find(|r| r.enabled && r.prefix == prefix && r.noncurrent_days == Some(days))
        {
            return Ok(RetentionOutcome {
                rule_id: r.id.clone(),
                noncurrent_days: days,
                created: false,
            });
        }
        inner.lifecycle.retain(|r| r.id != rule_id);
        inner.lifecycle.push(LifecycleView {
            id: rule_id.clone(),
            enabled: true,
            prefix: prefix.to_string(),
            noncurrent_days: Some(days),
            expired_delete_marker: true,
        });
        Ok(RetentionOutcome { rule_id, noncurrent_days: days, created: true })
    }

    async fn epoch_read(&self, key: &str) -> StoreResult<Option<EpochState>> {
        self.bump("epoch_read");
        let inner = self.inner.lock().unwrap();
        let Some(o) = inner.current(key) else {
            return Ok(None);
        };
        let body: EpochBody = serde_json::from_slice(&o.bytes)
            .map_err(|e| StoreError::Other(format!("epoch body: {}", e)))?;
        Ok(Some(EpochState {
            holder_id: body.holder_id,
            epoch: body.epoch,
            token: o.etag.clone(),
            last_renew_unix: Some(o.last_modified_unix),
            released: body.released,
            echo: body.echo,
        }))
    }

    async fn epoch_acquire(
        &self,
        key: &str,
        holder_id: &str,
        supersede: Option<&EpochState>,
    ) -> StoreResult<EpochLease> {
        self.bump("epoch_acquire");
        let epoch = supersede.map_or(1, |s| s.epoch + 1);
        let body = Bytes::from(
            serde_json::to_vec(&EpochBody {
                holder_id: holder_id.into(),
                epoch,
                released: false,
                echo: None,
            })
            .unwrap(),
        );
        let condition = match supersede {
            None => PutCondition::IfNoneMatchAny,
            Some(s) => PutCondition::IfMatch(s.token.clone()),
        };
        let mut inner = self.inner.lock().unwrap();
        Self::check_condition(inner.current(key), &condition)?;
        let obj = StoredObject {
            etag: put_etag(&body),
            crc64: crc64_nvme(&body),
            meta: HashMap::new(),
            last_modified_unix: now_unix(),
            bytes: body,
                   version_id: String::new(),
            deleted: false,
        };
        let token = obj.etag.clone();
        inner.push(&key, obj);
        Ok(EpochLease { holder_id: holder_id.into(), epoch, token })
    }

    async fn epoch_renew(
        &self,
        key: &str,
        lease: &EpochLease,
        echo: Option<&str>,
    ) -> StoreResult<EpochLease> {
        self.bump("epoch_renew");
        if self
            .fail_epoch_renew_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| c.checked_sub(1))
            .is_ok()
        {
            return Err(StoreError::Other("injected renew failure: store unreachable".into()));
        }
        let body = Bytes::from(
            serde_json::to_vec(&EpochBody {
                holder_id: lease.holder_id.clone(),
                epoch: lease.epoch,
                released: false,
                echo: echo.map(|e| e.to_string()),
            })
            .unwrap(),
        );
        let mut inner = self.inner.lock().unwrap();
        Self::check_condition(
            inner.current(key),
            &PutCondition::IfMatch(lease.token.clone()),
        )?;
        // Same holder/epoch, fresh Last-Modified; the etag must CHANGE
        // so a stale holder's renew CAS fails — salt with the clock.
        let mut salted = body.to_vec();
        salted.extend_from_slice(&now_unix().to_be_bytes());
        salted.extend_from_slice(&self.upload_seq.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        let obj = StoredObject {
            etag: put_etag(&salted),
            crc64: crc64_nvme(&body),
            meta: HashMap::new(),
            last_modified_unix: now_unix(),
            bytes: body,
                   version_id: String::new(),
            deleted: false,
        };
        let token = obj.etag.clone();
        inner.push(&key, obj);
        Ok(EpochLease { holder_id: lease.holder_id.clone(), epoch: lease.epoch, token })
    }

    async fn epoch_release(&self, key: &str, lease: &EpochLease) -> StoreResult<()> {
        self.bump("epoch_release");
        // Mark, never delete — deleting restarts epoch numbering at 1.
        let body = Bytes::from(
            serde_json::to_vec(&EpochBody {
                holder_id: lease.holder_id.clone(),
                epoch: lease.epoch,
                released: true,
                // A released cell reports no live sidecar: clearing the
                // echo is the point, not an omission.
                echo: None,
            })
            .unwrap(),
        );
        let mut inner = self.inner.lock().unwrap();
        Self::check_condition(
            inner.current(key),
            &PutCondition::IfMatch(lease.token.clone()),
        )?;
        let obj = StoredObject {
            etag: put_etag(&body),
            crc64: crc64_nvme(&body),
            meta: HashMap::new(),
            last_modified_unix: now_unix(),
            bytes: body,
                   version_id: String::new(),
            deleted: false,
        };
        inner.push(&key, obj);
        Ok(())
    }

    fn min_part_size(&self) -> u64 {
        self.min_part
    }

    fn max_parts(&self) -> usize {
        self.max_parts
    }
}

impl MemoryStore {
    async fn compose_inner(
        &self,
        spec: &ComposeSpec<'_>,
        upload_id: &str,
    ) -> StoreResult<ObjectMeta> {
        self.bump("compose_inner");
        // The real store cannot hash a copied part it never reads; the
        // double refuses the same spec so a caller learns it here.
        if spec.crc64.is_none()
            && spec.parts.iter().any(|p| matches!(p, PartSource::BaseCopy { .. }))
        {
            return Err(StoreError::Other(
                "compose: the checksum cannot be accumulated over a copied part; supply crc64".into(),
            ));
        }
        // Parts must be contiguous from 0 and respect granularity —
        // catching a flusher part-grid bug here beats catching it on
        // real S3.
        let mut expect = 0u64;
        for (i, p) in spec.parts.iter().enumerate() {
            let (off, len) = match p {
                PartSource::Local { offset, len } | PartSource::BaseCopy { offset, len } => {
                    (*offset, *len)
                }
            };
            if off != expect {
                return Err(StoreError::Other(format!(
                    "compose: part {} at offset {} but object is contiguous to {}",
                    i, off, expect
                )));
            }
            if len < self.min_part && i + 1 != spec.parts.len() {
                return Err(StoreError::Other(format!(
                    "compose: part {} len {} under the backend minimum {}",
                    i, len, self.min_part
                )));
            }
            expect = off + len;
        }

        // Assemble: local reads + guarded base copies (the copy guard
        // is evaluated per part against the CURRENT object, exactly
        // like x-amz-copy-source-if-match).
        let mut assembled = Vec::with_capacity(expect as usize);
        for (i, p) in spec.parts.iter().enumerate() {
            let bytes = match p {
                PartSource::Local { offset, len } => {
                    read_local(spec.local_path, *offset, *len)?
                }
                PartSource::BaseCopy { offset, len } => {
                    let want = spec.base_etag.as_deref().ok_or_else(|| {
                        StoreError::Other("compose: BaseCopy without base_etag".into())
                    })?;
                    let inner = self.inner.lock().unwrap();
                    let base_key = spec.base_key.unwrap_or(spec.key);
                    let base = inner.current(base_key).ok_or_else(|| {
                        StoreError::PreconditionFailed("copy-source: no base object".into())
                    })?;
                    if base.etag != want {
                        return Err(StoreError::PreconditionFailed(
                            "copy-source-if-match: base etag differs".into(),
                        ));
                    }
                    let s = *offset as usize;
                    let e = (*offset + *len) as usize;
                    if e > base.bytes.len() {
                        return Err(StoreError::Other(format!(
                            "copy-source range [{}, {}) beyond base size {}",
                            s,
                            e,
                            base.bytes.len()
                        )));
                    }
                    base.bytes.slice(s..e)
                }
            };
            if bytes.len() as u64
                != match p {
                    PartSource::Local { len, .. } | PartSource::BaseCopy { len, .. } => *len,
                }
            {
                return Err(StoreError::Other(format!("compose: part {} short read", i)));
            }
            spec.note_progress(bytes.len() as u64);
            self.inner
                .lock()
                .unwrap()
                .uploads
                .get_mut(upload_id)
                .ok_or_else(|| StoreError::NoSuchUpload(upload_id.into()))?
                .parts
                .insert(i, bytes.clone());
            assembled.extend_from_slice(&bytes);
        }

        // S3 validates the full-object CRC64-NVME at
        // CompleteMultipartUpload, so a composed upload whose checksum
        // does not match its bytes fails there and not on the next
        // read. Without this the double was WEAKER than the backend on
        // the one path that carries the largest objects: every
        // multipart upload in the tree — forge's packs above the
        // whole-put ceiling, lean's chunks — had its checksum accepted
        // untested, and a wrong CRC would first be seen by a real
        // bucket.
        let actual = crc64_nvme(&assembled);
        if let Some(claimed) = spec.crc64 {
            if actual != claimed {
                return Err(StoreError::ChecksumMismatch(format!(
                    "compose {}: claimed {:#x}, content is {:#x}",
                    spec.key, claimed, actual
                )));
            }
        }

        match self.inject.swap(INJECT_NONE, Ordering::SeqCst) {
            INJECT_CRASH_BEFORE_COMPLETE => {
                // Died before Complete: the MPU stays pending — the
                // one error path that deliberately leaves the orphan
                // (the A9 sweep exists for it).
                self.leave_orphan.store(true, Ordering::SeqCst);
                return Err(StoreError::Other("injected: crash before Complete".into()));
            }
            INJECT_TORN_COMPLETE => {
                // Complete lands server-side; the response is lost.
                self.finish_complete(spec, upload_id, assembled)?;
                return Err(StoreError::Other("injected: Complete response lost".into()));
            }
            _ => {}
        }

        let meta = self.finish_complete(spec, upload_id, assembled)?;
        Ok(meta)
    }

    fn finish_complete(
        &self,
        spec: &ComposeSpec<'_>,
        upload_id: &str,
        assembled: Vec<u8>,
    ) -> StoreResult<ObjectMeta> {
        let actual = crc64_nvme(&assembled);
        if let Some(claimed) = spec.crc64 {
            if actual != claimed {
                return Err(StoreError::ChecksumMismatch(format!(
                    "compose {}: claimed {:#x}, assembled is {:#x}",
                    spec.key, claimed, actual
                )));
            }
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.uploads.remove(upload_id).is_none() {
            // Fenced by a takeover sweep or lifecycle abort.
            return Err(StoreError::NoSuchUpload(upload_id.into()));
        }
        Self::check_condition(inner.current(spec.key), &spec.condition)?;
        let parts = spec.parts.len();
        let bytes = Bytes::from(assembled);
        let obj = StoredObject {
            etag: mpu_etag(&bytes, parts),
            crc64: actual,
            meta: spec.stamps.to_meta().into_iter().collect(),
            last_modified_unix: now_unix(),
            bytes,
                   version_id: String::new(),
            deleted: false,
        };
        let vid = inner.push(spec.key, obj);
        let mut m = inner.current(spec.key).expect("just pushed").to_meta();
        m.version_id = Some(vid);
        Ok(m)
    }
}

fn read_local(path: &std::path::Path, offset: u64, len: u64) -> StoreResult<Bytes> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)
        .map_err(|e| StoreError::Other(format!("local open {:?}: {}", path, e)))?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| StoreError::Other(format!("local seek: {}", e)))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)
        .map_err(|e| StoreError::Other(format!("local read: {}", e)))?;
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamps(generation: u64) -> GenerationStamps {
        GenerationStamps { generation, epoch: 1, flush_uuid: format!("u-{}", generation), boundary_source: None, posix: None }
    }

    #[tokio::test]
    async fn conditional_puts_and_gets_enforce_the_contract() {
        let s = MemoryStore::new();
        let body = Bytes::from_static(b"gen-1");
        let crc = crc64_nvme(&body);

        // A wrong claimed checksum must fail the publish, not warn.
        let err = s
            .put_whole("k", body.clone(), &PutCondition::IfNoneMatchAny, &stamps(1), crc ^ 1)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::ChecksumMismatch(_)));

        let m1 = s
            .put_whole("k", body.clone(), &PutCondition::IfNoneMatchAny, &stamps(1), crc)
            .await
            .unwrap();
        // Create-new against an existing object: 412.
        let err = s
            .put_whole("k", body.clone(), &PutCondition::IfNoneMatchAny, &stamps(1), crc)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));

        // Guarded update with the right etag works; the stale etag 412s.
        let b2 = Bytes::from_static(b"gen-2");
        let m2 = s
            .put_whole("k", b2.clone(), &PutCondition::IfMatch(m1.etag.clone()), &stamps(2), crc64_nvme(&b2))
            .await
            .unwrap();
        let err = s
            .put_whole("k", b2.clone(), &PutCondition::IfMatch(m1.etag.clone()), &stamps(3), crc64_nvme(&b2))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));

        // Guarded GET: the hydration posture's input — stale etag 412s.
        let (gm, gb) = s.get_whole("k", Some(&m2.etag)).await.unwrap();
        assert_eq!(gb, b2);
        assert_eq!(GenerationStamps::from_meta(&gm.meta).unwrap().generation, 2);
        let err = s.get_whole("k", Some(&m1.etag)).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
    }

    #[tokio::test]
    async fn epoch_cas_lease_lifecycle() {
        let s = MemoryStore::new();
        const K: &str = "vol/.flint-epoch";

        // First claim creates; a second blind claim 412s.
        let l1 = s.epoch_acquire(K, "hub-a", None).await.unwrap();
        assert_eq!(l1.epoch, 1);
        let err = s.epoch_acquire(K, "hub-b", None).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));

        // Renew with the live token; the token rotates so the OLD one
        // is dead afterwards (a stale holder's heartbeat must fail).
        let l1b = s.epoch_renew(K, &l1, None).await.unwrap();
        assert_ne!(l1b.token, l1.token, "renew must rotate the CAS token");
        let err = s.epoch_renew(K, &l1, None).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "stale token must be dead");

        // Takeover: supersede the OBSERVED state (step 7 judges the
        // holder dead first); epoch increments; the deposed lease is
        // fully fenced.
        let observed = s.epoch_read(K).await.unwrap().unwrap();
        assert_eq!(observed.holder_id, "hub-a");
        let l2 = s.epoch_acquire(K, "hub-b", Some(&observed)).await.unwrap();
        assert_eq!(l2.epoch, 2);
        let err = s.epoch_renew(K, &l1b, None).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "deposed renew must fail");
        let err = s.epoch_release(K, &l1b).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "deposed release must fail");

        // The holder releases: the cell is MARKED, not deleted. Losing
        // it would restart epoch numbering at 1 for the next claimant,
        // and every publish stamp on the volume is already past that.
        s.epoch_release(K, &l2).await.unwrap();
        let after = s.epoch_read(K).await.unwrap().expect("the cell must survive a release");
        assert!(after.released, "a clean shutdown marks the cell released");
        assert_eq!(after.epoch, 2, "and the epoch number is preserved");
        assert_eq!(after.holder_id, "hub-b");

        // The successor supersedes the released cell at epoch+1 — the
        // fast path `claim` takes with no quiet wait.
        let l3 = s.epoch_acquire(K, "hub-c", Some(&after)).await.unwrap();
        assert_eq!(l3.epoch, 3, "numbering continues, it does not reset");
        assert!(
            !s.epoch_read(K).await.unwrap().unwrap().released,
            "a fresh claim clears the released mark"
        );
    }

    #[tokio::test]
    async fn compose_mixes_local_and_guarded_base_copy() {
        let s = MemoryStore::new();
        // Base generation: 8 bytes.
        let base_body = Bytes::from_static(b"AAAABBBB");
        let base = s
            .put_whole("f", base_body.clone(), &PutCondition::IfNoneMatchAny, &stamps(1), crc64_nvme(&base_body))
            .await
            .unwrap();
        // Local truth: first 4 bytes rewritten, tail clean.
        let dir = tempfile::TempDir::new().unwrap();
        let local = dir.path().join("f.local");
        std::fs::write(&local, b"XXXXBBBB").unwrap();

        let spec = ComposeSpec {
            progress: None,
            key: "f",
            local_path: &local,
            parts: vec![
                PartSource::Local { offset: 0, len: 4 },
                PartSource::BaseCopy { offset: 4, len: 4 },
            ],
            base_key: None,
            base_etag: Some(base.etag.clone()),
            condition: PutCondition::IfMatch(base.etag.clone()),
            stamps: stamps(2),
            crc64: Some(crc64_nvme(b"XXXXBBBB")),
        };
        let m2 = s.compose_generation(&spec).await.unwrap();
        let (_, bytes) = s.get_whole("f", Some(&m2.etag)).await.unwrap();
        assert_eq!(bytes, Bytes::from_static(b"XXXXBBBB"));
        assert!(m2.etag.ends_with("-2\""), "multipart etag must not look like a content hash");

        // A foreign overwrite between generations fences the NEXT
        // compose at the copy-source guard.
        s.raw_put("f", Bytes::from_static(b"foreignXX"), vec![]);
        let err = s.compose_generation(&spec).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
        assert!(
            s.list_uploads("f").await.unwrap().is_empty(),
            "a failed compose must abort its MPU (A9)"
        );
    }
    /// `crc64: None` — the checksum comes from the parts the store
    /// reads, and the published metadata carries the object's true
    /// CRC. A copied part cannot be hashed that way and is refused
    /// before anything moves, with no upload left pending.
    #[tokio::test]
    async fn compose_without_a_claimed_checksum_publishes_the_content_crc() {
        let s = MemoryStore::new();
        let dir = tempfile::TempDir::new().unwrap();
        let local = dir.path().join("g.local");
        let content: Vec<u8> = (0..24u8).collect();
        std::fs::write(&local, &content).unwrap();
        let spec = ComposeSpec {
            progress: None,
            key: "g",
            local_path: &local,
            parts: vec![
                PartSource::Local { offset: 0, len: 8 },
                PartSource::Local { offset: 8, len: 8 },
                PartSource::Local { offset: 16, len: 8 },
            ],
            base_key: None,
            base_etag: None,
            condition: PutCondition::IfNoneMatchAny,
            stamps: stamps(1),
            crc64: None,
        };
        let m = s.compose_generation(&spec).await.unwrap();
        assert_eq!(m.crc64_b64.as_deref(), Some(crc64_to_b64(crc64_nvme(&content)).as_str()));
        let (h, bytes) = s.get_whole("g", None).await.unwrap();
        assert_eq!(&bytes[..], &content[..]);
        assert_eq!(h.crc64_b64, m.crc64_b64);

        let base_body = Bytes::from_static(b"AAAABBBB");
        let base = s
            .put_whole("h", base_body.clone(), &PutCondition::IfNoneMatchAny, &stamps(1), crc64_nvme(&base_body))
            .await
            .unwrap();
        std::fs::write(&local, b"XXXXBBBB").unwrap();
        let err = s
            .compose_generation(&ComposeSpec {
                progress: None,
                key: "h",
                local_path: &local,
                parts: vec![
                    PartSource::Local { offset: 0, len: 4 },
                    PartSource::BaseCopy { offset: 4, len: 4 },
                ],
                base_key: None,
                base_etag: Some(base.etag.clone()),
                condition: PutCondition::IfMatch(base.etag.clone()),
                stamps: stamps(2),
                crc64: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Other(ref m) if m.contains("copied part")), "{err:?}");
        assert!(s.list_uploads("h").await.unwrap().is_empty(), "nothing pending after a refusal");
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use crate::{crc64_from_b64, crc64_to_b64};

    fn stamps(g: u64) -> GenerationStamps {
        GenerationStamps {
            generation: g,
            epoch: 7,
            flush_uuid: format!("flush-{g}"),
            boundary_source: None,
            posix: None,
        }
    }

    #[tokio::test]
    async fn a_copy_carries_the_bytes_and_the_checksum_but_not_the_stamps() {
        let s = MemoryStore::new();
        let body = Bytes::from_static(b"the bytes that move");
        let src = s
            .put_whole("a.txt", body.clone(), &PutCondition::IfNoneMatchAny, &stamps(3), crc64_nvme(&body))
            .await
            .unwrap();

        let dst = s
            .copy_object("a.txt", Some(&src.etag), "b.txt", &PutCondition::IfNoneMatchAny, &stamps(1))
            .await
            .unwrap();

        let (meta, got) = s.get_whole("b.txt", None).await.unwrap();
        assert_eq!(got, body, "a copy is byte-identical or it is not a copy");
        assert_eq!(
            dst.crc64_b64, src.crc64_b64,
            "identical bytes have an identical checksum — a copy cannot change one"
        );
        // THE trap this test exists for: the destination must not wear
        // the source's publish history.
        assert_eq!(
            meta.meta.get("flint-gen").map(String::as_str),
            Some("1"),
            "the destination's generation is its OWN, not the source's 3: {:?}",
            meta.meta
        );
        assert_eq!(
            meta.meta.get(GenerationStamps::META_FLUSH_UUID).map(String::as_str),
            Some("flush-1"),
            "and its flush_uuid names the publish that WROTE IT, not flush-3: {:?}",
            meta.meta
        );
        // The source is untouched — copy, not move.
        assert!(s.head("a.txt").await.is_ok());
    }

    #[tokio::test]
    async fn a_copy_refuses_when_the_source_moved_under_it() {
        let s = MemoryStore::new();
        let v1 = Bytes::from_static(b"version one");
        let src = s
            .put_whole("a.txt", v1, &PutCondition::IfNoneMatchAny, &stamps(1), crc64_nvme(b"version one"))
            .await
            .unwrap();
        let v2 = Bytes::from_static(b"version two");
        s.put_whole("a.txt", v2, &PutCondition::IfMatch(src.etag.clone()), &stamps(2), crc64_nvme(b"version two"))
            .await
            .unwrap();

        let err = s
            .copy_object("a.txt", Some(&src.etag), "b.txt", &PutCondition::IfNoneMatchAny, &stamps(1))
            .await
            .expect_err("copying an object that moved copies something the caller never read");
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "{err:?}");
        assert!(matches!(s.head("b.txt").await, Err(StoreError::NotFound(_))), "and nothing landed");
    }

    #[tokio::test]
    async fn a_copy_honours_the_destination_precondition() {
        let s = MemoryStore::new();
        let a = Bytes::from_static(b"aaaa");
        s.put_whole("a.txt", a, &PutCondition::IfNoneMatchAny, &stamps(1), crc64_nvme(b"aaaa"))
            .await
            .unwrap();
        let b = Bytes::from_static(b"bbbb");
        let existing = s
            .put_whole("b.txt", b.clone(), &PutCondition::IfNoneMatchAny, &stamps(1), crc64_nvme(b"bbbb"))
            .await
            .unwrap();

        let err = s
            .copy_object("a.txt", None, "b.txt", &PutCondition::IfNoneMatchAny, &stamps(2))
            .await
            .expect_err("a rename must not silently clobber its destination");
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "{err:?}");
        let (_, still) = s.get_whole("b.txt", Some(&existing.etag)).await.unwrap();
        assert_eq!(still, b, "the destination is untouched by a refused copy");
    }

    /// `ComposeSpec::base_key` has existed for the A7 re-key flush and
    /// `base_key: Some(..)` appears NOWHERE in this repository — not in
    /// production, not in a test. `copy_object`'s MPU arm is its first
    /// caller, so this is the branch's first coverage.
    #[tokio::test]
    async fn a_compose_can_copy_ranges_from_a_different_key() {
        let s = MemoryStore::new();
        let body = Bytes::from_static(b"AAAABBBB");
        let src = s
            .put_whole("old/name", body.clone(), &PutCondition::IfNoneMatchAny, &stamps(1), crc64_nvme(&body))
            .await
            .unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        let spec = ComposeSpec {
            progress: None,
            key: "new/name",
            // Never read: every part is a BaseCopy.
            local_path: &dir.path().join("nothing-here"),
            parts: vec![PartSource::BaseCopy { offset: 0, len: 8 }],
            base_key: Some("old/name"),
            base_etag: Some(src.etag.clone()),
            condition: PutCondition::IfNoneMatchAny,
            stamps: stamps(1),
            crc64: Some(crc64_nvme(&body)),
        };
        let m = s.compose_generation(&spec).await.unwrap();
        let (_, got) = s.get_whole("new/name", Some(&m.etag)).await.unwrap();
        assert_eq!(got, body, "the ranges came from the OTHER key");
        assert!(s.head("old/name").await.is_ok(), "and the source survives");
    }

    #[test]
    fn crc64_b64_round_trips_and_refuses_what_is_not_one() {
        for v in [0u64, 1, u64::MAX, 0x0123_4567_89ab_cdef] {
            assert_eq!(crc64_from_b64(&crc64_to_b64(v)), Some(v), "round trip {v:#x}");
        }
        // A caller handed `None` has something other than a CRC-64, and
        // guessing a number would validate a publish against a checksum
        // nobody computed.
        // "AAAAAAAAAAA" is the discriminating case: 11 significant
        // characters decode to exactly 8 bytes, so the byte-count check
        // alone ACCEPTS it. Only the canonical-form check rejects it —
        // without that case the padding guard is a line no control can
        // move, which is how dead defensive code survives.
        for bad in ["", "AAAA", "not-base64!!", "AAAAAAAAAAAA", "AAAAAAAAAAA", "AAAAAAAAAAAAAAA="] {
            assert_eq!(crc64_from_b64(bad), None, "{bad:?} is not a canonical CRC-64");
        }
        assert_eq!(crc64_from_b64("AAAAAAAAAAA="), Some(0), "the canonical form IS accepted");
    }
}
