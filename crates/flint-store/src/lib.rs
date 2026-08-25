//! The object store behind a trait — L2 step 4 (design review A13,
//! with A6's conditional-publish contract baked into the signatures).
//!
//! Only generation ASSEMBLY is backend-specific; everything above this
//! trait (interval log, flush pipeline, eviction, hydration, epoch
//! state machine) is backend-neutral. Backends:
//!
//! - `memory` — the in-process test double, implementing the FULL
//!   conditional semantics (412/409, stamps, CRC validation, MPU
//!   state, epoch CAS). Every backend-neutral tier test runs here.
//! - `s3` — MPU + UploadPartCopy + conditional PUT/Complete per A6/A8.
//! - Azure Blob (second, per A13): Put Block List with committed-block
//!   reuse and NATIVE leases for the epoch. GCS later.
//!
//! Contract points the signatures enforce:
//!
//! - **No unconditional publish exists.** Every write carries either
//!   `IfMatch` (update of a known generation) or `IfNoneMatchAny`
//!   (first generation of a new key). A caller that wants an unguarded
//!   overwrite has made a design error upstream (A6).
//! - **The multipart ETag is NOT a content hash** and never appears in
//!   any integrity role; content identity is the full-object
//!   CRC-64/NVME, computed by the flusher from LOCAL truth and
//!   validated server-side at publish (a mismatch fails the publish,
//!   it does not warn).
//! - **Assembly failure aborts the assembly** (A9): no backend may
//!   leave a partial MPU behind on an error path it can reach.
//!
//! Extracted from `spdk-csi-driver/src/tier/store/` (2026-08-25) so
//! flint-lean can depend on the store WITHOUT the hub crate's build
//! (13 binaries, the aws/kube/nfs dep trees). The hub crate re-exports
//! this crate at its old path (`crate::tier::store::*`), so every
//! existing import keeps working. The `s3` backend is feature-gated —
//! test loops that only need the memory double never build the AWS
//! SDK.

pub mod memory;
#[cfg(feature = "s3")]
pub mod s3;

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

// ── CRC-64/NVME ──────────────────────────────────────────────────────
//
// The tier's one content-identity algorithm (A6): linearly combinable
// (so S3 can compute it FULL_OBJECT across multipart, copied parts
// included) and supported end-to-end by both S3 and the local flusher.
// Hand-rolled table CRC — 30 lines beats a dependency; the test pins
// the catalog check value.

/// Reflected form of the CRC-64/NVME polynomial 0xAD93D23594C935A9.
const CRC64_NVME_POLY: u64 = 0x9A6C_9329_AC4B_C9B5;

fn crc64_table() -> &'static [u64; 256] {
    static TABLE: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u64; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut crc = i as u64;
            let mut b = 0;
            while b < 8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ CRC64_NVME_POLY } else { crc >> 1 };
                b += 1;
            }
            t[i] = crc;
            i += 1;
        }
        t
    })
}

/// Streaming CRC-64/NVME (the flusher feeds file ranges through this).
#[derive(Clone)]
pub struct Crc64Nvme(u64);

impl Default for Crc64Nvme {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc64Nvme {
    pub fn new() -> Self {
        Crc64Nvme(u64::MAX)
    }
    pub fn update(&mut self, data: &[u8]) {
        let t = crc64_table();
        for &b in data {
            self.0 = t[((self.0 ^ b as u64) & 0xFF) as usize] ^ (self.0 >> 8);
        }
    }
    pub fn finalize(self) -> u64 {
        self.0 ^ u64::MAX
    }
}

/// One-shot convenience.
pub fn crc64_nvme(data: &[u8]) -> u64 {
    let mut c = Crc64Nvme::new();
    c.update(data);
    c.finalize()
}

/// S3's wire form: base64 of the checksum's 8 big-endian bytes.
pub fn crc64_to_b64(crc: u64) -> String {
    const AL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = crc.to_be_bytes();
    let mut out = String::with_capacity(12);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(AL[(n >> 18) as usize & 63] as char);
        out.push(AL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { AL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { AL[n as usize & 63] as char } else { '=' });
    }
    out
}

// ── stamps and metadata ──────────────────────────────────────────────

/// The A6 publish stamps (`x-amz-meta-flint-*` on S3; each backend maps
/// the bare keys to its metadata namespace). HEAD-based 412 arbitration
/// keys off these — they are how a torn OWN flush is told apart from a
/// genuinely foreign object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationStamps {
    /// (`gen` is reserved in edition 2024 — spelled out.)
    pub generation: u64,
    pub epoch: u64,
    pub flush_uuid: String,
    /// A12: POSIX metadata riding on the object, so a bucket reader —
    /// and the DR import — can restore mode/ownership/mtime without
    /// the manifest. OPTIONAL both ways: absence never makes an object
    /// foreign (steps 4–11 published without it).
    pub posix: Option<PosixStamps>,
}

/// The A12 per-object POSIX stamps (`flint-mode` is octal; times are
/// unix seconds). Enumerated NON-goals of the round-trip: hard-link
/// structure, sockets/FIFOs/devices, and sparseness — see
/// `tier::manifest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosixStamps {
    /// Full st_mode (type bits included; restore applies the 07777).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_unix: i64,
}

impl PosixStamps {
    pub const META_MODE: &'static str = "flint-mode";
    pub const META_UID: &'static str = "flint-uid";
    pub const META_GID: &'static str = "flint-gid";
    pub const META_MTIME: &'static str = "flint-mtime";

    #[cfg(unix)]
    pub fn from_metadata(m: &std::fs::Metadata) -> PosixStamps {
        use std::os::unix::fs::MetadataExt;
        PosixStamps { mode: m.mode(), uid: m.uid(), gid: m.gid(), mtime_unix: m.mtime() }
    }

    fn to_meta(self) -> Vec<(String, String)> {
        vec![
            (Self::META_MODE.into(), format!("{:o}", self.mode)),
            (Self::META_UID.into(), self.uid.to_string()),
            (Self::META_GID.into(), self.gid.to_string()),
            (Self::META_MTIME.into(), self.mtime_unix.to_string()),
        ]
    }

    /// Lenient: any stamp absent/malformed ⇒ None as a set (never a
    /// foreignness signal).
    pub fn from_meta(meta: &HashMap<String, String>) -> Option<PosixStamps> {
        Some(PosixStamps {
            mode: u32::from_str_radix(meta.get(Self::META_MODE)?, 8).ok()?,
            uid: meta.get(Self::META_UID)?.parse().ok()?,
            gid: meta.get(Self::META_GID)?.parse().ok()?,
            mtime_unix: meta.get(Self::META_MTIME)?.parse().ok()?,
        })
    }
}

impl GenerationStamps {
    pub const META_GEN: &'static str = "flint-gen";
    pub const META_EPOCH: &'static str = "flint-epoch";
    pub const META_FLUSH_UUID: &'static str = "flint-flush-uuid";

    pub fn to_meta(&self) -> Vec<(String, String)> {
        let mut v = vec![
            (Self::META_GEN.into(), self.generation.to_string()),
            (Self::META_EPOCH.into(), self.epoch.to_string()),
            (Self::META_FLUSH_UUID.into(), self.flush_uuid.clone()),
        ];
        if let Some(p) = self.posix {
            v.extend(p.to_meta());
        }
        v
    }

    /// Parse stamps back out of object metadata; None if any IDENTITY
    /// stamp is absent or malformed (an unstamped object is by
    /// definition foreign). The posix set parses leniently.
    pub fn from_meta(meta: &HashMap<String, String>) -> Option<GenerationStamps> {
        Some(GenerationStamps {
            generation: meta.get(Self::META_GEN)?.parse().ok()?,
            epoch: meta.get(Self::META_EPOCH)?.parse().ok()?,
            flush_uuid: meta.get(Self::META_FLUSH_UUID)?.clone(),
            posix: PosixStamps::from_meta(meta),
        })
    }
}

/// What HEAD/GET/publish return about an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub etag: String,
    pub size: u64,
    /// Full-object CRC-64/NVME in wire (base64) form, when the store
    /// has one (objects written by this tier always do).
    pub crc64_b64: Option<String>,
    /// User metadata with backend prefixes stripped (bare keys, e.g.
    /// `flint-gen`).
    pub meta: HashMap<String, String>,
    pub last_modified_unix: Option<u64>,
    /// Backend storage class (S3: None ≡ STANDARD). The A11 IA
    /// copy-source guard reads this: a non-Standard base pays
    /// retrieval on every CLEAN byte copied, silently inverting the
    /// tier's saving — the flusher refuses BaseCopy from it.
    pub storage_class: Option<String>,
}

impl ObjectMeta {
    /// May this object serve as a server-side copy source without
    /// per-byte retrieval charges? (A11's IA copy-source guard.)
    pub fn copy_source_allowed(&self) -> bool {
        matches!(self.storage_class.as_deref(), None | Some("STANDARD"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedObject {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub last_modified_unix: Option<u64>,
}

/// An in-progress multipart assembly (A9's hygiene surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpload {
    pub key: String,
    pub upload_id: String,
    pub initiated_unix: Option<u64>,
}

// ── publish conditions ───────────────────────────────────────────────

/// A6: every publish is conditional. There is deliberately NO
/// unconditional variant — a caller wanting one has an upstream design
/// error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutCondition {
    /// Publish generation g+1 only if the object is still generation
    /// g's bytes (its ETag). The 412 this produces is an internal
    /// fencing event — see `tier::arbitrate`.
    IfMatch(String),
    /// First generation of a new key: fail if ANY object exists
    /// (closes the create race with outside writers).
    IfNoneMatchAny,
}

// ── generation assembly ──────────────────────────────────────────────

/// One part of a composed generation, in object order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartSource {
    /// Dirty range: bytes [offset, offset+len) read from the local
    /// file and uploaded.
    Local { offset: u64, len: u64 },
    /// Clean range: server-side copy of [offset, offset+len) from the
    /// base generation object at the SAME key, guarded by
    /// copy-source-if-match on the base ETag (detects foreign bucket
    /// overwrite; structurally cannot detect local staleness — that is
    /// capture's job, finding C5).
    BaseCopy { offset: u64, len: u64 },
}

/// The flusher's order for one generation publish.
#[derive(Debug, Clone)]
pub struct ComposeSpec<'a> {
    pub key: &'a str,
    /// Local source of `Local` parts. Clean ranges are byte-identical
    /// local and remote by definition, so `crc64` is computable from
    /// this file alone.
    pub local_path: &'a std::path::Path,
    /// Contiguous from offset 0, sizes within the backend's part
    /// granularity (the A11 part-size grid upstream owns that).
    pub parts: Vec<PartSource>,
    /// Where the base generation LIVES, when it is not `key` — the A7
    /// re-key flush publishes a renamed file under its new key while
    /// clean ranges still copy from the old one. None = same as `key`.
    pub base_key: Option<&'a str>,
    /// Base generation's ETag — required iff any part is `BaseCopy`.
    pub base_etag: Option<String>,
    pub condition: PutCondition,
    pub stamps: GenerationStamps,
    /// Full-object CRC-64/NVME of the composed content, from local
    /// truth. Backends hand it to the store for SERVER-SIDE validation
    /// at publish: a mismatch fails the publish, never warns.
    pub crc64: u64,
}

// ── errors ───────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// 412 — the guarded condition failed. Route to arbitration, never
    /// to an operator runbook (A6).
    #[error("precondition failed (412): {0}")]
    PreconditionFailed(String),
    /// 409 — a concurrent conditional writer; retry-after-arbitrate.
    #[error("conditional conflict (409): {0}")]
    Conflict(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// The assembly was fenced (aborted) under us — A8's takeover
    /// sweep produces exactly this at a deposed hub's Complete.
    #[error("no such upload: {0}")]
    NoSuchUpload(String),
    /// Server-side content validation failed — the store computed a
    /// different full-object checksum than local truth claims.
    #[error("checksum mismatch: {0}")]
    ChecksumMismatch(String),
    #[error("store: {0}")]
    Other(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

// ── the epoch surface (A8; the step-7 state machine drives these) ────

/// Observed epoch state (read side — takeover judgment input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochState {
    pub holder_id: String,
    pub epoch: u64,
    /// Backend token guarding the next transition (S3: the epoch
    /// object's ETag; Azure: the lease id).
    pub token: String,
    /// The store's own clock for the last renewal (S3 Last-Modified) —
    /// A8: takeover is judged against the STORE's clock, not ours.
    pub last_renew_unix: Option<u64>,
    /// The holder shut down cleanly and will never publish under this
    /// epoch again — a successor supersedes immediately instead of
    /// waiting out the lease. Written by [`ObjectStore::epoch_release`].
    pub released: bool,
}

/// A held lease (write side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochLease {
    pub holder_id: String,
    pub epoch: u64,
    pub token: String,
}

// ── the trait ────────────────────────────────────────────────────────

#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Whole-object publish (below the multipart threshold).
    /// `crc64` is validated server-side; stamps ride as metadata.
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        condition: &PutCondition,
        stamps: &GenerationStamps,
        crc64: u64,
    ) -> StoreResult<ObjectMeta>;

    /// Assemble and publish one generation from local dirty bytes plus
    /// server-side clean copies. MUST abort its partial assembly on
    /// every failure path (A9) — an error return means nothing is left
    /// pending under the key by this call.
    async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta>;

    async fn head(&self, key: &str) -> StoreResult<ObjectMeta>;

    /// Whole-object read, optionally guarded (hydration always guards:
    /// a 412 here is S3-WINS — the foreign-overwrite posture, opposite
    /// of the publish path).
    async fn get_whole(&self, key: &str, if_match: Option<&str>) -> StoreResult<(ObjectMeta, Bytes)>;

    /// Ranged read (step 11's streaming restore — a multi-GiB
    /// hydration must never buffer the whole object). ALWAYS guarded:
    /// each chunk's If-Match pins the same object version end-to-end;
    /// a 412 mid-restore is the S3-wins foreign-overwrite signal. The
    /// caller never requests past the object's end.
    async fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        if_match: &str,
    ) -> StoreResult<Bytes>;

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>>;

    async fn delete(&self, key: &str) -> StoreResult<()>;

    /// A9 hygiene: every in-progress assembly under the prefix.
    async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>>;

    /// Abort one assembly. Aborting an already-absent upload is Ok —
    /// the sweep and the abort-on-failure paths both race the
    /// lifecycle rule.
    async fn abort_upload(&self, key: &str, upload_id: &str) -> StoreResult<()>;

    /// A9: verify/create the bucket posture (lifecycle abort rule,
    /// versioning, encryption notes, IAM probe). Named prefix so
    /// probes stay under the tier's namespace.
    async fn bootstrap(&self, prefix: &str) -> StoreResult<BootstrapReport>;

    // Epoch (A8). S3 implements CAS re-PUT on the epoch object; Azure
    // will use native blob leases (strictly better — A13).

    async fn epoch_read(&self, key: &str) -> StoreResult<Option<EpochState>>;

    /// Claim: `supersede: None` creates (If-None-Match:*); `Some`
    /// replaces the OBSERVED state via CAS on its token — the caller
    /// (step 7) is responsible for having JUDGED that holder dead
    /// against the store's clock. The new epoch is observed+1.
    async fn epoch_acquire(
        &self,
        key: &str,
        holder_id: &str,
        supersede: Option<&EpochState>,
    ) -> StoreResult<EpochLease>;

    /// Heartbeat CAS. `PreconditionFailed` means deposed: self-fence.
    async fn epoch_renew(&self, key: &str, lease: &EpochLease) -> StoreResult<EpochLease>;

    /// Mark the cell released: a clean handoff. The epoch NUMBER must
    /// survive (deleting the cell would restart numbering at 1 and
    /// break the monotonicity every publish stamp depends on), and the
    /// write must be guarded on `lease`'s own token so a deposed
    /// holder cannot mark a live successor's cell.
    async fn epoch_release(&self, key: &str, lease: &EpochLease) -> StoreResult<()>;

    /// Backend part granularity for the A11 part-size grid.
    fn min_part_size(&self) -> u64;
    fn max_parts(&self) -> usize;
}

/// What bootstrap found/did (A9). `errors` non-empty ⇒ the tier must
/// refuse to start; `warnings` are loud-degrade.
#[derive(Debug, Default, Clone)]
pub struct BootstrapReport {
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl BootstrapReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc64_nvme_catalog_check_value() {
        // CRC-64/NVME check input from the reveng catalog.
        assert_eq!(crc64_nvme(b"123456789"), 0xAE8B_1486_0A79_9888);
        // Streaming across split updates must agree with one-shot.
        let mut c = Crc64Nvme::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.finalize(), 0xAE8B_1486_0A79_9888);
        assert_eq!(crc64_nvme(b""), 0);
    }

    #[test]
    fn crc64_b64_is_8_bytes_big_endian_base64() {
        // 0xAE8B14860A799888 → bytes AE 8B 14 86 0A 79 98 88.
        assert_eq!(crc64_to_b64(0xAE8B_1486_0A79_9888), "rosUhgp5mIg=");
        assert_eq!(crc64_to_b64(0), "AAAAAAAAAAA=");
    }

    #[test]
    fn stamps_roundtrip_and_reject_partial() {
        let s = GenerationStamps { generation: 7, epoch: 3, flush_uuid: "u-1".into(), posix: None };
        let m: HashMap<String, String> = s.to_meta().into_iter().collect();
        assert_eq!(GenerationStamps::from_meta(&m), Some(s));
        let mut partial = m.clone();
        partial.remove(GenerationStamps::META_FLUSH_UUID);
        assert_eq!(
            GenerationStamps::from_meta(&partial),
            None,
            "an unstamped/partially-stamped object is foreign by definition"
        );
    }

    #[test]
    fn posix_stamps_roundtrip_and_stay_lenient() {
        let p = PosixStamps { mode: 0o100644, uid: 501, gid: 20, mtime_unix: 1_723_000_000 };
        let s = GenerationStamps {
            generation: 2,
            epoch: 1,
            flush_uuid: "u-2".into(),
            posix: Some(p),
        };
        let m: HashMap<String, String> = s.to_meta().into_iter().collect();
        assert_eq!(m.get(PosixStamps::META_MODE).map(String::as_str), Some("100644"));
        assert_eq!(GenerationStamps::from_meta(&m), Some(s));
        // A pre-step-12 object (identity stamps only) parses with
        // posix: None — NEVER foreign.
        let mut old = m.clone();
        old.remove(PosixStamps::META_UID);
        let parsed = GenerationStamps::from_meta(&old).expect("identity stamps intact");
        assert_eq!(parsed.posix, None);
    }
}
