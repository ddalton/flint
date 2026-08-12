//! Layout Management
//!
//! Manages layout generation and tracking for pNFS.
//! Implements the FILE layout type as per RFC 8881 Chapter 13.
//!
//! # Protocol References
//! - RFC 8881 Section 12.2 - pNFS Definitions
//! - RFC 8881 Chapter 13 - NFSv4.1 File Layout Type
//! - RFC 8881 Section 18.43 - LAYOUTGET operation

use crate::pnfs::mds::device::{DeviceInfo, DeviceRegistry, DeviceStatus};
use crate::pnfs::config::LayoutPolicy as ConfigLayoutPolicy;
use crate::state_backend::{
    IoModeRecord, LayoutRecord, LayoutSegmentRecord, PlacementRecord, StateBackend, WriteOp,
};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Seconds since the Unix epoch. Wall-clock, and deliberately so: this
/// value has to survive a process, which `Instant` cannot. Everything
/// that measures a DURATION uses the monotonic clock; the wall clock is
/// only ever used to carry an age ACROSS a restart, where the
/// alternative is not a better clock but no memory at all.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A truncate-dirty mark.
///
/// `since` is monotonic but process-scoped; `carried` is the age
/// inherited from previous MDS incarnations. The sum is what the
/// fallback delay ceiling measures.
///
/// Splitting it this way rather than reconstructing an `Instant` in the
/// past is deliberate: `Instant::now() - carried` is not portably
/// representable when the process has been up for less time than the
/// gate has been dirty — which, right after the restart that motivated
/// this, is every time.
#[derive(Debug, Clone, Copy)]
struct GateMark {
    since: std::time::Instant,
    carried: Duration,
    min: u64,
}

impl GateMark {
    fn age(&self) -> Duration {
        self.carried + self.since.elapsed()
    }
}

/// `unix_now` for tests in sibling modules that need to build a
/// placement record with a stamp in the past.
#[cfg(test)]
pub fn unix_now_for_test() -> u64 {
    unix_now()
}

/// Layout state ID (combines with NFSv4 stateid)
pub type LayoutStateId = [u8; 16];

/// The seqid every layout stateid is MINTED with. RFC 8881 §12.5.3 makes
/// the seqid a version counter the server bumps on each CB_LAYOUTRECALL,
/// so it cannot be part of the identity — `other` (the low 12 bytes) is.
/// §20.3.3 also forbids a zero seqid, which is why minting starts at 1
/// rather than 0.
pub const LAYOUT_SEQID_BASE: u32 = 1;

/// `generate_layout`'s refusal when the publish-time gate recheck fires.
/// layoutget matches on it to answer TRYLATER (the same answer the
/// up-front gate check gives) rather than a hard error.
pub const GRANT_RACED_TRUNCATE: &str = "grant raced a truncate";

/// F67 sentinel prefix: a nonzero-size stub has NO placement binding
/// anywhere (backend and stub xattr both empty). Minting a fresh
/// file_id here would strand any striped data behind a new identity —
/// reads would come back as silent zeros through the DS hole path — so
/// the grant is refused instead. Callers match with `starts_with`; the
/// message carries the file key.
pub const ORPHANED_DATA: &str = "F67 orphaned data";

/// Rate-limited (5 s, global) ERROR for orphan-guard trips: a client
/// retrying I/O against an orphaned file must not flood the log, but
/// the operator must reliably see the condition and the remedy.
fn orphan_log(file_key: &str, len: u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = unix_now();
    let last = LAST.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 5 && LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        tracing::error!(
            "🛑 F67: '{}' has {} bytes of data but no placement binding (backend and \
             stub xattr both empty). Refusing I/O rather than serving zeros. Restore \
             the MDS state PVC from backup, or delete the stub to abandon the data.",
            file_key, len
        );
    }
}

/// How long a revoked stateid is remembered so the owner's LAYOUTRETURN
/// can be answered NFS4_OK instead of an error. Comfortably longer than
/// a client's recall-to-return turnaround, short enough that the set
/// cannot grow without bound.
pub const REVOKED_TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// The identity of a layout stateid, as a map key.
///
/// `layouts` and `by_owner` are keyed by this, NOT by the raw 16 bytes:
/// once the seqid advances on a recall, the client's next LAYOUTRETURN
/// carries the bumped value and a raw-bytes lookup would miss it — which
/// is precisely the trap that kept the seqid frozen (audit C1). Spelled as
/// a 16-byte array with the seqid canonicalised rather than as a distinct
/// `[u8; 12]` type, to keep the change to the ten call sites that index
/// these maps.
pub fn state_key(stateid: &LayoutStateId) -> LayoutStateId {
    let mut k = *stateid;
    k[0..4].copy_from_slice(&LAYOUT_SEQID_BASE.to_be_bytes());
    k
}

/// The seqid currently on a stateid.
pub fn seqid_of(stateid: &LayoutStateId) -> u32 {
    u32::from_be_bytes([stateid[0], stateid[1], stateid[2], stateid[3]])
}

/// 16-byte NFSv4.1 session id (mirrors `nfs::v4::protocol::SessionId`).
/// Kept as a plain byte array here so the pNFS layer doesn't pull in
/// the v4 protocol module.
pub type SessionIdBytes = [u8; 16];

/// "Who owns this layout" — RFC 8881 §12.5 ties every issued layout to a
/// specific client. We need this for:
///
/// * **CB_LAYOUTRECALL**: routing the recall to the right backchannel
///   (looked up via `session_id` → CallbackManager).
/// * **LAYOUTRETURN with return_type=ALL**: filter by `clientid`.
/// * **LAYOUTRETURN with return_type=FSID**: filter by `(clientid, fsid)`.
/// * Forensics ("which client is hammering DS-3?").
///
/// Stored alongside `LayoutState` and indexed by `LayoutManager::by_owner`
/// so the FSID/ALL paths don't need O(n) scans of the primary map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayoutOwner {
    /// The 64-bit clientid that the SEQUENCE op resolved to.
    pub client_id: u64,
    /// The 16-byte session id the LAYOUTGET arrived on.
    pub session_id: SessionIdBytes,
    /// Filesystem identifier the layout's filehandle lives in. RFC 8881
    /// §12.5.5: a LAYOUTRETURN with `return_type=FSID` releases all
    /// layouts the client holds in this fsid.
    pub fsid: u64,
}

/// Per-file stripe placement, pinned at first LAYOUTGET and reused
/// verbatim by every later grant for the same file. The stripe map is
/// a pure function of `(device_ids order, stripe_size)` — recomputing
/// it from the live registry re-maps existing data whenever the fleet
/// changes or the registry iterates in a different order (the Phase 0
/// P1 in `docs/plans/pnfs-durable-ds-plan.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlacement {
    /// Stripe unit in bytes, pinned from the config in force at first
    /// grant. A later `layout.stripeSize` change affects new files only.
    pub stripe_size: u64,
    /// Ordered device ids. Order is load-bearing: stripe unit `u` maps
    /// to `device_ids[(u + first_stripe_index) % len]`.
    pub device_ids: Vec<String>,
    /// Immutable per-file identity allocated at pin time (see
    /// `PlacementRecord::file_id`). Nonzero ⇒ layouts carry per-DS v2
    /// file-ID filehandles and DS stripes live at
    /// `{file_id:016x}.stripeN`; 0 ⇒ legacy path-keyed storage.
    pub file_id: u64,
}

impl FilePlacement {
    pub(crate) fn to_record(&self, file_key: &str) -> PlacementRecord {
        PlacementRecord {
            file_key: file_key.to_string(),
            stripe_size: self.stripe_size,
            device_ids: self.device_ids.clone(),
            file_id: self.file_id,
            // Filled in by the caller that knows the gate state; the
            // placement struct itself does not carry it.
            truncate_pending: None,
            truncate_since_unix: None,
        }
    }

    fn from_record(r: &PlacementRecord) -> Self {
        Self {
            stripe_size: r.stripe_size,
            device_ids: r.device_ids.clone(),
            file_id: r.file_id,
        }
    }

    /// The stripe file this placement's slot-`j` DS stores, relative
    /// to the DS data dir. Only meaningful for v2 (file_id != 0) pins.
    pub fn stripe_rel_path(&self, slot: usize) -> String {
        format!("{:016x}.stripe{}", self.file_id, slot)
    }

    /// The per-file stripe rotation the WIRE carries as
    /// `nfl_first_stripe_index` (RFC 8881 §13.4.4): the client maps
    /// stripe unit `u` to device `(u + this) % N`. Derived from the
    /// file_id because every LAYOUTGET ever issued for the file must
    /// agree (rename-stable; see the dispatcher's encode comment).
    /// THE dispatcher's layout encode and the fallback proxy MUST both
    /// call this — a second copy of the formula is how the F66 gate
    /// caught a proxied write landing on the wrong stripe file: file_id
    /// 0x…246d is ODD, so the client's unit 0 lived on device 1 while
    /// an unrotated proxy wrote device 0, and the client's next read
    /// found an absent stripe ⇒ zero bytes. Even file_ids agreed by
    /// coincidence, which made the corruption look like a 50%
    /// intermittency instead of a formula divergence.
    ///
    /// Legacy pins (file_id == 0) rotate by a hash of the FILEHANDLE,
    /// which the proxy path does not carry — so legacy files are
    /// unproxyable and the disposition keeps pre-F66 FailFast for them.
    pub fn wire_first_stripe_index(file_id: u64, width: usize) -> u32 {
        debug_assert!(file_id != 0, "legacy pins rotate by FH hash, not file_id");
        (file_id % width.max(1) as u64) as u32
    }

    /// The device slot that owns FILE offset `offset` — unit index plus
    /// the wire rotation above. v2 pins only (see
    /// [`Self::wire_first_stripe_index`]).
    pub fn slot_for_offset(&self, offset: u64) -> usize {
        debug_assert!(self.stripe_size > 0, "placement with zero stripe_size");
        let width = self.device_ids.len().max(1);
        let unit = (offset / self.stripe_size) as usize;
        (unit + Self::wire_first_stripe_index(self.file_id, width) as usize) % width
    }

    /// Split `[offset, offset+len)` at stripe-unit boundaries: the
    /// chunks a fallback op must fan to, each entirely inside one
    /// slot's stripe file. Client-sized fallback ops (≤ ~1 MiB) against
    /// the 8 MiB default stripe unit yield exactly one chunk almost
    /// always.
    pub fn split_at_stripe_bounds(&self, offset: u64, len: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::with_capacity(2);
        let mut cur = offset;
        let end = offset.saturating_add(len);
        while cur < end {
            let unit_end = (cur / self.stripe_size + 1) * self.stripe_size;
            let chunk_end = unit_end.min(end);
            out.push((cur, chunk_end - cur));
            cur = chunk_end;
        }
        out
    }
}

/// The truncate-dirty gate key for a pinned file. Keyed by the
/// placement's immutable file identity when it has one, so the gate
/// follows the file through RENAME for free; legacy pins fall back to
/// the path key (they can't be renamed anyway — the op is refused).
pub fn truncate_gate_key(placement: &FilePlacement, file_key: &str) -> String {
    if placement.file_id != 0 {
        format!("id:{:016x}", placement.file_id)
    } else {
        format!("path:{}", file_key)
    }
}

/// Allocate a fresh, unique per-file identity for a new pin. Uses the
/// uuid crate (already a workspace dep) — collision-free in practice
/// and free of the determinism trap the old name-hash scheme had
/// (same name ⇒ same id ⇒ a recreated file could read its
/// predecessor's stripes).
/// This MDS's shard ordinal (FLINT_MDS_SHARD_ID; 0 when unset — the
/// single-MDS case and shard 0 are the same namespace). Masked to
/// 8 bits: the file_id namespace prefix.
fn shard_ordinal() -> u64 {
    static ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        std::env::var("FLINT_MDS_SHARD_ID")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|v| v & 0xff)
            .unwrap_or(0)
    })
}

/// Compose a file_id: shard ordinal in the top 8 bits, 56 bits of
/// randomness below. Stripe files are named `{file_id:x}.stripeN` in
/// a flat per-DS namespace shared by ALL shards (sharding Phase 2),
/// so cross-shard ids must be disjoint BY CONSTRUCTION — random-u64
/// collisions would silently cross-wire two volumes' stripes, a class
/// we don't accept probabilistically when determinism costs one shift.
/// (Pre-sharding ids used the full random u64; they all live on
/// shard 0 and keep working — the residual legacy-vs-shard>0 overlap
/// is the same birthday bound as before, on a finite legacy set.)
fn compose_file_id(shard: u64, hi: u64, lo: u64) -> u64 {
    let id = ((shard & 0xff) << 56) | ((hi ^ lo) & 0x00ff_ffff_ffff_ffff);
    // 0 is the legacy sentinel — never allocate it.
    match id {
        0 => 1,
        id => id,
    }
}

fn allocate_file_id() -> u64 {
    let (hi, lo) = uuid::Uuid::new_v4().as_u64_pair();
    compose_file_id(shard_ordinal(), hi, lo)
}

/// The 16-byte pNFS deviceid a striped layout advertises for a given
/// ordered device set. Content-addressed: files with identical
/// placements share one deviceid, so kernel clients cache a single
/// GETDEVICEINFO result per stripe group. The algorithm matches the
/// historical dispatcher encoding (hash of the device ids in order +
/// a `STRIPE:` marker) so a stable fleet's ids don't change across
/// upgrades.
pub fn composite_device_id(device_ids: &[String]) -> [u8; 16] {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    for id in device_ids {
        id.hash(&mut hasher);
    }
    b"STRIPE:".hash(&mut hasher);
    let hash = hasher.finish();

    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&hash.to_be_bytes());
    out[8..16].copy_from_slice(&hash.to_be_bytes());
    out
}

/// Layout manager - manages layout generation and tracking
#[derive(Clone)]
pub struct LayoutManager {
    /// Registry of available devices
    device_registry: Arc<DeviceRegistry>,

    /// Active layouts (keyed by layout stateid).
    layouts: Arc<DashMap<LayoutStateId, LayoutState>>,

    /// Secondary index: client → set of layout stateids the client owns.
    /// Lets `LAYOUTRETURN ALL` and `LAYOUTRETURN FSID` filter without
    /// scanning every issued layout, and lets the backchannel know which
    /// session to send CB_LAYOUTRECALL to. Maintained alongside `layouts`
    /// in `generate_layout` / `return_layout` / `recall_layouts_for_device`.
    by_owner: Arc<DashMap<u64, Vec<LayoutStateId>>>,

    /// Layout policy
    policy: LayoutPolicyImpl,

    /// MDS-wide default stripe size in bytes. Per-volume overrides live
    /// in `volume_geometry`; this is the fallback.
    stripe_size: u64,

    /// Per-volume stripe geometry, keyed by volume directory name, set
    /// at provision time from StorageClass parameters. Acts as a cache
    /// loaded eagerly at startup by `load_volume_geometry`. `None` means
    /// "no geometry declared for this volume", cached as a negative.
    /// `None` = "no geometry declared for this volume", cached as a
    /// negative so absence stays distinguishable from the fleet default.
    volume_geometry: Arc<DashMap<String, Option<VolumeGeometry>>>,


    /// Per-file pinned placements (keyed by export-relative path).
    /// Source of truth for every grant after the first; persisted so
    /// the pin survives MDS restart.
    placements: Arc<DashMap<String, FilePlacement>>,

    /// Composite deviceid → the ordered device ids it stands for.
    /// GETDEVICEINFO resolves striped deviceids here (in placement
    /// order), never from the live registry's iteration order.
    stripe_groups: Arc<DashMap<[u8; 16], Vec<String>>>,

    /// Per-DS pending stripe-file deletions (paths relative to the DS
    /// data dir), drained into HeartbeatResponse instructions.
    /// In-memory + best-effort by design: losing it leaks orphaned
    /// stripe space, never correctness.
    cleanup_queues: Arc<DashMap<String, Vec<String>>>,

    /// Stateids WE revoked, with when. A client that answers a
    /// CB_LAYOUTRECALL by doing the RFC-defined thing — LAYOUTRETURN of
    /// the recalled stateid — must not be told SERVERFAULT for its
    /// trouble: layouts are granted `return_on_close`, so Linux
    /// compounds that LAYOUTRETURN into CLOSE and a failed op aborts the
    /// compound, leaking the open state behind it (audit R3).
    ///
    /// Pruned opportunistically past [`REVOKED_TOMBSTONE_TTL`]; the set
    /// only has to outlive the client's reaction to a recall.
    revoked: Arc<DashMap<LayoutStateId, std::time::Instant>>,

    /// Files whose stripe truncation has NOT yet reached every pinned
    /// DS: gate key (see [`truncate_gate_key`]) → (when it went dirty,
    /// the SMALLEST unconfirmed target size). While a file is here,
    /// LAYOUTGET answers TRYLATER and MDS-fallback I/O parks — stale
    /// bytes beyond the new EOF must never be readable through a fresh
    /// layout. The min-size tracking makes racing truncates safe: the
    /// gate only lifts once the DEEPEST requested cut is confirmed
    /// everywhere (a later, larger set_len can't kill bytes below its
    /// own length). In-memory: an MDS crash inside the
    /// (milliseconds-wide) stub-truncate → DS-ack window can lose a
    /// mark; accepted residual documented in the operator runbook.
    truncate_dirty: Arc<DashMap<String, GateMark>>,

    /// Persistence target. Layouts surviving MDS restart prevents the
    /// kernel from issuing fresh LAYOUTGETs (disruptive but functional)
    /// and lets recall fan-out work correctly post-restart. See
    /// `state_backend::mod.rs` for the lag-bound rationale.
    backend: Arc<dyn StateBackend>,

    /// F67: the durable binding channel. Minting writes the placement
    /// onto the stub (xattr) BEFORE the backend record; recovery reads
    /// it back when the backend has lost the record; the orphan guard
    /// consults stub metadata before ever re-minting over data. Tests
    /// default to [`stub_binding::NoStubs`] (pre-F67 semantics); the
    /// server injects the real xattr binding.
    stub_binding: Arc<dyn super::stub_binding::StubBinding>,

    /// pnfs-block: the NVMe export reconciler, attached once by the
    /// server when `blockExport` is configured (OnceLock: construction
    /// order mirrors the callback manager's late attach). `None` means
    /// this MDS refuses block-class provisions and grant-time host
    /// admission becomes a logged no-op — unit-test shape only, since
    /// CreateVolume gates on it.
    block_export: Arc<std::sync::OnceLock<Arc<super::block_export::BlockExportReconciler>>>,

    callbacks: Arc<std::sync::OnceLock<Arc<super::callback::CallbackManager>>>,

    /// Is an NFS client still leased? Attached late by the server (the
    /// state manager owns the lease table). Only the block-initiator
    /// report reads it — see `client_is_live` for why its unattached
    /// default is "alive".
    lease_oracle: Arc<std::sync::OnceLock<LeaseOracle>>,
}

/// The client-lease verdict, injected rather than imported so the layout
/// manager keeps no dependency on the NFS state machine (and so tests
/// need no lease machinery — the same shape `lease_sweep_pass` uses).
pub type LeaseOracle = Arc<dyn Fn(u64) -> bool + Send + Sync>;

impl LayoutState {
    /// Snapshot the persisted bits of this layout for the
    /// [`StateBackend`].
    pub(crate) fn to_record(&self) -> LayoutRecord {
        LayoutRecord {
            stateid: self.stateid,
            owner_client_id: self.owner.client_id,
            owner_session_id: self.owner.session_id,
            owner_fsid: self.owner.fsid,
            filehandle: self.filehandle.clone(),
            file_ident: self.file_ident.clone(),
            segments: self
                .segments
                .iter()
                .map(|s| LayoutSegmentRecord {
                    offset: s.offset,
                    length: s.length,
                    iomode: io_to_record(s.iomode),
                    device_id: s.device_id.clone(),
                    stripe_index: s.stripe_index,
                    pattern_offset: s.pattern_offset,
                })
                .collect(),
            iomode: io_to_record(self.iomode),
            return_on_close: self.return_on_close,
        }
    }

    /// Inverse of `to_record`. Used at startup by
    /// [`LayoutManager::load_records`].
    pub(crate) fn from_record(r: LayoutRecord) -> Self {
        Self {
            stateid: r.stateid,
            owner: LayoutOwner {
                client_id: r.owner_client_id,
                session_id: r.owner_session_id,
                fsid: r.owner_fsid,
            },
            filehandle: r.filehandle,
            file_ident: r.file_ident,
            segments: r
                .segments
                .into_iter()
                .map(|s| LayoutSegment {
                    offset: s.offset,
                    length: s.length,
                    iomode: record_to_io(s.iomode),
                    device_id: s.device_id,
                    stripe_index: s.stripe_index,
                    pattern_offset: s.pattern_offset,
                })
                .collect(),
            iomode: record_to_io(r.iomode),
            return_on_close: r.return_on_close,
        }
    }
}

fn io_to_record(m: IoMode) -> IoModeRecord {
    match m {
        IoMode::Read => IoModeRecord::Read,
        IoMode::ReadWrite => IoModeRecord::ReadWrite,
        IoMode::Any => IoModeRecord::Any,
    }
}

fn record_to_io(m: IoModeRecord) -> IoMode {
    match m {
        IoModeRecord::Read => IoMode::Read,
        IoModeRecord::ReadWrite => IoMode::ReadWrite,
        IoModeRecord::Any => IoMode::Any,
    }
}

/// Layout state - tracks an active layout issued to a client
#[derive(Debug, Clone)]
pub struct LayoutState {
    /// Layout stateid
    pub stateid: LayoutStateId,

    /// Owning client + session + filesystem (see `LayoutOwner`).
    pub owner: LayoutOwner,

    /// File handle this layout applies to
    pub filehandle: Vec<u8>,

    /// Which FILE this layout is for, as [`truncate_gate_key`] spells
    /// it. Recorded at grant time from the same placement the stripe
    /// map came from, so the truncate recall (F65) and the
    /// truncate-dirty gate are keyed identically by construction —
    /// two separately-derived keys could drift, and a drifted key
    /// recalls nothing while looking like it worked.
    ///
    /// Empty only for layouts restored from a pre-v7 record; those are
    /// deliberately unmatchable rather than matched by a wildcard.
    pub file_ident: String,

    /// Layout segments
    pub segments: Vec<LayoutSegment>,

    /// I/O mode (read, write, any)
    pub iomode: IoMode,

    /// Whether to return layout on close
    pub return_on_close: bool,
}

/// A single layout segment
#[derive(Debug, Clone)]
pub struct LayoutSegment {
    /// Byte offset where this segment starts
    pub offset: u64,
    
    /// Length of this segment (NFS4_UINT64_MAX for "rest of file")
    pub length: u64,
    
    /// I/O mode for this segment
    pub iomode: IoMode,
    
    /// Device ID to use for this segment
    pub device_id: String,
    
    /// Stripe index (for striped layouts)
    pub stripe_index: u32,
    
    /// Pattern offset (for dense striping)
    pub pattern_offset: u64,
}

/// I/O mode as per RFC 8881 Section 3.3.20
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum IoMode {
    /// Read-only access
    Read = 1,
    
    /// Read-write access
    ReadWrite = 2,
    
    /// Any mode (for layout return)
    Any = 3,
}

/// Layout type as per RFC 8881 Section 3.3.13 (`layouttype4`).
///
/// The OSD2/BLOCK values were SWAPPED against the RFC until 2026-08-09
/// (found while designing the pnfs-block class, see
/// docs/plans/pnfs-block-layout-design.md §1). Harmless while only
/// type 1 is served and the wire decoder maps only 1 and 4 — fatal the
/// day type 3 ships. Do not "fix" these back from memory; the RFC
/// assigns LAYOUT4_OSD2_OBJECTS = 0x2, LAYOUT4_BLOCK_VOLUME = 0x3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LayoutType {
    /// NFSv4.1 Files layout (RFC 8881 Chapter 13)
    NfsV4_1Files = 1,

    /// Object layout (RFC 5664) - future
    Osd2Objects = 2,

    /// Block/volume layout (RFC 5663; RFC 8154/9561 SCSI/NVMe) - future
    BlockVolume = 3,

    /// Flexible File Layout (RFC 8435) - for independent DS storage
    /// Each DS has its own storage, filehandles are DS-specific
    FlexFiles = 4,

    /// SCSI layout (RFC 8154), which RFC 9561 extends to NVMe namespaces
    /// via NGUID/EUI64 designators. THE pnfs-block class's wire type —
    /// the kernel's blocklayout driver gained NVMe device matching for
    /// THIS type (v6.11, commit 3921ae0850a3), not for BlockVolume=3
    /// (design doc §3: "serve type 5").
    Scsi = 5,
}

/// Which layout machinery serves a volume — chosen at provision time
/// from the StorageClass (`layout: pnfs` vs `layout: pnfs-block`) and
/// fixed for the volume's lifetime. This is the per-volume dispatch key
/// the design doc's §5 seam (`layout_type_served`) branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutClass {
    /// NFSv4.1 files layout over the DS fleet (the historical class).
    #[default]
    File,
    /// RFC 8154/9561 SCSI-over-NVMe extents straight to spdk-tgt; the
    /// MDS is an extent allocator (state_backend::extent_alloc).
    Scsi,
}

impl LayoutClass {
    /// The layouttype4 value this class serves on the wire.
    pub fn wire_type(self) -> LayoutType {
        match self {
            LayoutClass::File => LayoutType::NfsV4_1Files,
            LayoutClass::Scsi => LayoutType::Scsi,
        }
    }

    /// The stable string persisted in volume geometry records and
    /// carried in the CreateVolume proto. A closed set: parse errors
    /// are refusals, never defaults — a class misread as File would
    /// silently serve stripe layouts over a volume whose data lives in
    /// extents.
    pub fn as_str(self) -> &'static str {
        match self {
            LayoutClass::File => "file",
            LayoutClass::Scsi => "scsi",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file" | "" => Some(LayoutClass::File),
            "scsi" => Some(LayoutClass::Scsi),
            _ => None,
        }
    }
}

/// Layout policy implementation
#[derive(Debug, Clone, Copy)]
enum LayoutPolicyImpl {
    /// Simple round-robin across all DSs
    RoundRobin,

    /// Interleaved striping for parallel I/O
    Stripe,

    /// Prefer DS on same node as client (future)
    Locality,
}

/// Stripe geometry chosen for a volume at provision time and fixed for
/// its lifetime. A file's placement is pinned at its first layout grant
/// and never re-striped, so changing geometry afterwards could not
/// affect existing data — fixing it at create is honest, not a
/// limitation being papered over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeGeometry {
    /// Stripe unit in bytes.
    pub stripe_size: u64,
    /// Maximum data servers a file in this volume is pinned across.
    /// 0 = every active DS (the historical behaviour).
    pub stripe_width: u32,
    /// Which layout machinery serves this volume (design doc §5: the
    /// per-volume dispatch the `layout_type_served` seam branches on).
    /// Stripe fields are meaningless for [`LayoutClass::Scsi`] volumes.
    pub layout_class: LayoutClass,
}

impl Default for VolumeGeometry {
    fn default() -> Self {
        Self { stripe_size: 0, stripe_width: 0, layout_class: LayoutClass::File }
    }
}

impl LayoutManager {
    /// Create a new layout manager backed by `backend`.
    /// Test-shape constructor: no stub visibility (`NoStubs`), so the
    /// F67 orphan guard never trips and every miss mints — the pre-F67
    /// semantics unit tests were written against. PRODUCTION MUST use
    /// [`Self::new_with_binding`]; the restart drill gates that wiring.
    pub fn new(
        device_registry: Arc<DeviceRegistry>,
        policy: ConfigLayoutPolicy,
        stripe_size: u64,
        backend: Arc<dyn StateBackend>,
    ) -> Self {
        Self::new_with_binding(
            device_registry,
            policy,
            stripe_size,
            backend,
            Arc::new(super::stub_binding::NoStubs),
        )
    }

    pub fn new_with_binding(
        device_registry: Arc<DeviceRegistry>,
        policy: ConfigLayoutPolicy,
        stripe_size: u64,
        backend: Arc<dyn StateBackend>,
        stub_binding: Arc<dyn super::stub_binding::StubBinding>,
    ) -> Self {
        let policy_impl = match policy {
            ConfigLayoutPolicy::RoundRobin => LayoutPolicyImpl::RoundRobin,
            ConfigLayoutPolicy::Stripe => LayoutPolicyImpl::Stripe,
            ConfigLayoutPolicy::Locality => LayoutPolicyImpl::Locality,
        };

        info!(
            "Layout manager initialized: policy={:?}, stripe_size={}",
            policy_impl, stripe_size
        );

        Self {
            device_registry,
            layouts: Arc::new(DashMap::new()),
            by_owner: Arc::new(DashMap::new()),
            revoked: Arc::new(DashMap::new()),
            policy: policy_impl,
            stripe_size,
            placements: Arc::new(DashMap::new()),
            stripe_groups: Arc::new(DashMap::new()),
            cleanup_queues: Arc::new(DashMap::new()),
            truncate_dirty: Arc::new(DashMap::new()),
            volume_geometry: Arc::new(DashMap::new()),
            backend,
            stub_binding,
            block_export: Arc::new(std::sync::OnceLock::new()),
            callbacks: Arc::new(std::sync::OnceLock::new()),
            lease_oracle: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Attach the callback channel (server wiring, post-construction —
    /// the back-channel registry must exist first). Second attach is a
    /// no-op; `None` means device notifications are skipped.
    pub fn attach_callback_manager(&self, callbacks: Arc<super::callback::CallbackManager>) {
        let _ = self.callbacks.set(callbacks);
    }

    /// Attach the client-lease verdict (the same one the lease sweep
    /// injects). Same late-attach shape as the callback manager: the
    /// state manager is built alongside this one.
    pub fn attach_lease_oracle(&self, alive: LeaseOracle) {
        let _ = self.lease_oracle.set(alive);
    }

    /// Is this NFS client still leased?
    ///
    /// Defaults to TRUE with no oracle attached, and the direction is
    /// deliberate: the only consumer is the roller's initiator report,
    /// where a false "dead" would let a live client's target be
    /// restarted. Over-reporting merely pauses an upgrade.
    pub fn client_is_live(&self, client_id: u64) -> bool {
        match self.lease_oracle.get() {
            Some(alive) => alive(client_id),
            None => true,
        }
    }

    /// Record that `session` fetched `volume`'s device and which
    /// notifications it accepts (GETDEVICEINFO `gdia_notify_types`
    /// word 0). Idempotent; the newest mask wins.
    /// Record that `client_id` cached `volume`'s device and which
    /// notifications it accepted (GETDEVICEINFO `gdia_notify_types`).
    ///
    /// This is the CB_NOTIFY_DEVICEID address book. It exists separately
    /// from layout state because the client that needs telling is often
    /// NOT a layout holder — in the expand case it had returned every
    /// layout and still served stale device geometry from its cache — so
    /// "who holds a layout" cannot answer "who believes something about
    /// this device".
    ///
    /// **Durable and CLIENT-keyed, both halves measured before they were
    /// built** (`EXPAND=1 MDS_BOUNCE=1`). While this book was in memory,
    /// an expand after an MDS restart notified nobody and the
    /// application got EIO on a volume that had the space: the MDS
    /// granted a layout past the old ceiling, the client could not use
    /// it against its cached device, and the fallback lane refused.
    ///
    /// Keyed on the CLIENT because the session does not survive a
    /// restart — startup deliberately drops persisted sessions so the
    /// kernel re-CREATE_SESSIONs on BADSESSION, and the client returns
    /// with a NEW session id under its EXISTING clientid, issuing no
    /// fresh GETDEVICEINFO. Its cached blocklayout device (whose LENGTH
    /// is snapshotted from the bdev at parse time,
    /// `fs/nfs/blocklayout/dev.c`) outlives the session that fetched it,
    /// so a session-keyed record would have restored dead addresses. The
    /// session is resolved at SEND time from live state, because a
    /// back-channel cannot be persisted at all.
    pub fn note_device_fetch(&self, volume: &str, client_id: u64, notify_mask: u32) {
        if notify_mask == 0 || client_id == 0 {
            // A client that asks for nothing cannot be told anything —
            // recording it would only make the address book lie about
            // reachable clients.
            return;
        }
        // Queued, not awaited: GETDEVICEINFO is a synchronous handler
        // and this write is once per (client, device). The window a
        // queued write opens is an MDS crash between the fetch and the
        // flush; the row would be lost and that client would miss a
        // later expand — the same outcome as before this table existed,
        // for a far smaller window.
        self.backend
            .enqueue_write(WriteOp::PutDeviceNotify(crate::state_backend::DeviceNotifyRecord {
                volume: volume.to_string(),
                client_id,
                notify_mask,
                fetched_unix: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            }));
    }

    /// Forget a volume's notify address book (DeleteVolume).
    pub fn forget_device_notify(&self, volume: &str) {
        self.backend
            .enqueue_write(WriteOp::DeleteDeviceNotify(volume.to_string(), None));
    }

    /// Forget one client's row (its lease expired).
    pub fn forget_device_notify_client(&self, volume: &str, client_id: u64) {
        self.backend.enqueue_write(WriteOp::DeleteDeviceNotify(
            volume.to_string(),
            Some(client_id),
        ));
    }

    /// Tell every client that cached `volume`'s device to drop it — the
    /// online half of expansion (design doc §7). Returns
    /// (accepted, attempted).
    ///
    /// Best-effort on purpose, and the caller must NOT fail an expand
    /// on it: the capacity is real either way, and a client that misses
    /// the notification is in the documented "recycle the mount" state
    /// rather than a broken one. Unreachable sessions are pruned so a
    /// long-lived volume's address book does not accumulate ghosts.
    pub async fn notify_device_changed(&self, volume: &str) -> (usize, usize) {
        let Some(callbacks) = self.callbacks.get().cloned() else {
            return (0, 0);
        };
        let targets = match self.backend.device_notify_list(volume).await {
            Ok(t) => t,
            Err(e) => {
                warn!("device notify list for '{}' failed: {} — no client told", volume, e);
                return (0, 0);
            }
        };
        if targets.is_empty() {
            return (0, 0);
        }
        let deviceid = crate::nvmeof_export::scsi_device_id(volume);
        let mut accepted = 0usize;
        let mut attempted = 0usize;
        for (client_id, mask) in &targets {
            // An expired client is not a target: it holds no session to
            // reach and its next mount fetches the device fresh. Pruned
            // here rather than swept, so the table tracks reality
            // without a second timer to get wrong.
            if !self.client_is_live(*client_id) {
                self.forget_device_notify_client(volume, *client_id);
                continue;
            }
            // Prefer CHANGE — it says what actually happened, and the
            // Linux client treats CHANGE and DELETE identically (both
            // drop the cached deviceid). DELETE is the fallback for a
            // client that only accepts that.
            use crate::nfs::v4::cb_compound::deviceid_notify_type as t;
            let notify_type = if mask & t::CHANGE != 0 {
                t::CHANGE
            } else if mask & t::DELETE != 0 {
                t::DELETE
            } else {
                continue;
            };
            attempted += 1;
            match callbacks
                .send_notify_deviceid_to_client(
                    *client_id,
                    LayoutType::Scsi as u32,
                    deviceid,
                    notify_type,
                )
                .await
            {
                Ok(reply) if reply.status == crate::nfs::v4::protocol::Nfs4Status::Ok => {
                    accepted += 1
                }
                Ok(_) => {}
                // NOT pruned on a failed send, and this is the whole
                // lesson of the in-memory version: right after a restart
                // a live client has no back-channel yet, and dropping it
                // here would re-create the bug the durable book exists
                // to fix. Only a dead lease or a deleted volume removes
                // a row.
                Err(_) => {}
            }
        }
        (accepted, attempted)
    }

    /// Attach the block-export reconciler (server wiring, post-
    /// construction for the same reason as `attach_callback_manager`).
    /// Second attach is a no-op — OnceLock keeps the first.
    pub fn attach_block_export(
        &self,
        reconciler: Arc<super::block_export::BlockExportReconciler>,
    ) {
        let _ = self.block_export.set(reconciler);
    }

    /// The attached reconciler, if this MDS serves block-class volumes.
    pub fn block_export(&self) -> Option<Arc<super::block_export::BlockExportReconciler>> {
        self.block_export.get().cloned()
    }

    /// Is this volume scsi-class per the geometry cache? The attach
    /// path's class gate: admitting a host onto a files-class volume
    /// would build an nvme session to a subsystem that does not exist.
    pub fn volume_is_scsi(&self, volume: &str) -> bool {
        self.volume_geometry
            .get(volume)
            .map(|e| matches!(e.value(), Some(g) if g.layout_class == LayoutClass::Scsi))
            .unwrap_or(false)
    }

    /// Every volume the geometry cache knows to be scsi-class — the
    /// startup reconcile's work list. The cache is loaded eagerly by
    /// `load_volume_geometry`, so this is complete once startup seeding
    /// ran.
    pub fn scsi_volumes(&self) -> Vec<String> {
        self.volume_geometry
            .iter()
            .filter_map(|e| match e.value() {
                Some(g) if g.layout_class == LayoutClass::Scsi => Some(e.key().clone()),
                _ => None,
            })
            .collect()
    }

    /// Record the geometry chosen for `volume` at provision time and
    /// AWAIT its durability before returning.
    ///
    /// Awaited rather than queued on purpose. `enqueue_write` returns
    /// before the op has reached the page cache, so a SIGKILL — the
    /// dominant Kubernetes crash — between CreateVolume's reply and the
    /// writer draining would leave an acknowledged volume whose geometry
    /// silently reverted to the fleet default. Geometry is written once
    /// per volume, on a human-timescale control path, so the await costs
    /// nothing that matters.
    ///
    /// Returns the geometry actually stored, after resolving `0` to the
    /// fleet default.
    pub async fn set_volume_geometry(
        &self,
        volume: &str,
        requested: VolumeGeometry,
    ) -> VolumeGeometry {
        let geom = VolumeGeometry {
            stripe_size: if requested.stripe_size == 0 {
                self.stripe_size
            } else {
                requested.stripe_size
            },
            stripe_width: requested.stripe_width,
            layout_class: requested.layout_class,
        };
        if let Err(e) = self
            .backend
            .put_volume_geometry(&crate::state_backend::VolumeGeometryRecord {
                volume: volume.to_string(),
                stripe_size: geom.stripe_size,
                stripe_width: geom.stripe_width,
                layout_class: geom.layout_class.as_str().to_string(),
            })
            .await
        {
            // Caching it anyway would make THIS MDS stripe correctly while
            // any other shard, and this one after a restart, used the
            // default — one volume striped two ways, invisibly. Refuse the
            // cache so the caller's echo check fails the provision.
            warn!("geometry: persisting volume '{}' failed: {} — not caching", volume, e);
            return VolumeGeometry {
                stripe_size: self.stripe_size,
                stripe_width: 0,
                layout_class: LayoutClass::File,
            };
        }
        self.volume_geometry.insert(volume.to_string(), Some(geom));
        info!(
            "📐 Volume '{}' geometry: stripe_size={} stripe_width={} class={}",
            volume,
            geom.stripe_size,
            if geom.stripe_width == 0 { "all".to_string() } else { geom.stripe_width.to_string() },
            geom.layout_class.as_str(),
        );
        geom
    }

    /// Register a block-class volume's extent arena (its volume_alloc
    /// row: allocation ceiling + bump watermark). Idempotent — an
    /// existing arena's ceiling is left untouched. Awaited for the same
    /// reason geometry is: an acked block-class volume whose arena was
    /// lost cannot grant a single extent.
    pub async fn register_extent_arena(
        &self,
        volume: &str,
        size_ceiling: u64,
    ) -> Result<(), String> {
        match self.backend.extent_register_volume(volume, size_ceiling).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("extent arena for '{volume}': {e}")),
            Err(e) => Err(format!("extent arena for '{volume}': {e}")),
        }
    }

    /// Raise a block volume's allocation ceiling (the CSI expand path).
    /// Returns the ceiling in force — idempotent, so a re-driven expand
    /// answers the same number instead of failing.
    ///
    /// THE BACKING DEVICE MUST ALREADY BE BIG ENOUGH. The ceiling is the
    /// allocator's promise that a bump-allocated offset is addressable on
    /// the lvol; raising it ahead of the device would grant a client
    /// extents past the end of its namespace, and the failure would
    /// surface as an unexplained I/O error at the client with the server
    /// believing everything is fine. Callers grow the export first.
    pub async fn expand_extent_arena(
        &self,
        volume: &str,
        new_ceiling: u64,
    ) -> Result<u64, String> {
        match self.backend.extent_expand_volume(volume, new_ceiling).await {
            Ok(Ok(ceiling)) => Ok(ceiling),
            Ok(Err(e)) => Err(format!("extent arena for '{volume}': {e}")),
            Err(e) => Err(format!("extent arena for '{volume}': {e}")),
        }
    }

    /// Forget a volume's geometry (called on DeleteVolume). Queued, not
    /// awaited: a lost delete leaks one tiny row that the next
    /// CreateVolume of the same name overwrites anyway.
    pub fn forget_volume_geometry(&self, volume: &str) {
        self.volume_geometry.remove(volume);
        self.backend
            .enqueue_write(WriteOp::DeleteVolumeGeometry(volume.to_string()));
    }

    /// Seed the geometry cache from the backend at MDS startup.
    ///
    /// Eager rather than lazy because the cache is the only reader on the
    /// hot path: a LAYOUTGET arriving before the load would pin a file at
    /// the fleet default and never re-stripe it.
    ///
    /// `known_volumes` is the set of volume directories on the export. A
    /// directory with no geometry row is USUALLY benign (every volume
    /// provisioned before geometry existed), but it is also exactly what
    /// a lost row looks like — so it gets one WARN each. That line is the
    /// only signal distinguishing "never declared" from "acked, then
    /// lost", and without it the loss is silent.
    pub async fn load_volume_geometry(&self, known_volumes: &[String]) {
        let records = match self.backend.list_volume_geometry().await {
            Ok(r) => r,
            Err(e) => {
                warn!("geometry: load failed: {} — every volume will use the fleet default", e);
                return;
            }
        };
        let n = records.len();
        for r in records {
            // A class string parse cannot refuse silently into File: a
            // scsi volume misread as file would serve stripe layouts
            // over extents. Unknown class = the volume is skipped (and
            // therefore served at the fleet default, which is loud in
            // the WARN below) rather than misclassified.
            let Some(class) = LayoutClass::parse(&r.layout_class) else {
                warn!(
                    "📐 volume '{}' has unknown layout_class '{}' — record ignored \
                     (written by a newer build?)",
                    r.volume, r.layout_class,
                );
                continue;
            };
            self.volume_geometry.insert(
                r.volume,
                Some(VolumeGeometry {
                    stripe_size: r.stripe_size,
                    stripe_width: r.stripe_width,
                    layout_class: class,
                }),
            );
        }
        for v in known_volumes {
            if !self.volume_geometry.contains_key(v) {
                warn!(
                    "📐 volume '{}' has no geometry record — new files in it will use the \
                     fleet default (stripe_size={}, all data servers). Expected for volumes \
                     created before per-volume geometry; otherwise the record was lost.",
                    v, self.stripe_size,
                );
                // Cache the negative so it costs one lookup, not one per file.
                self.volume_geometry.insert(v.clone(), None);
            }
        }
        info!("📐 MDS loaded {} volume geometry record(s)", n);
    }

    /// The geometry RECORDED for `volume`, or `None` if none ever was.
    ///
    /// A pure cache lookup: everything is loaded at startup, and
    /// `set_volume_geometry` inserts on the create path. The `Option`
    /// keeps "no geometry declared" distinguishable from "geometry that
    /// happens to equal the fleet default" — collapsing them is what let
    /// an idempotent CreateVolume retry echo zeros for a volume that had
    /// been created perfectly well.
    fn recorded_geometry(&self, volume: &str) -> Option<VolumeGeometry> {
        self.volume_geometry.get(volume).and_then(|g| *g)
    }

    /// Geometry in force for `file_key`, which is an export-relative
    /// path — its first component names the volume. Falls back to the
    /// MDS-wide default for legacy volumes and for anything provisioned
    /// before geometry existed.
    fn geometry_for(&self, file_key: &str) -> VolumeGeometry {
        let default = VolumeGeometry {
            stripe_size: self.stripe_size,
            stripe_width: 0,
            layout_class: LayoutClass::File,
        };
        match file_key.split('/').find(|c| !c.is_empty()) {
            Some(volume) => self.recorded_geometry(volume).unwrap_or(default),
            None => default,
        }
    }

    /// Which layout class serves `file_key`'s volume — the per-volume
    /// dispatch the design doc's §5 seam branches on. Legacy volumes
    /// (no geometry record) are File by construction.
    pub fn layout_class_for(&self, file_key: &str) -> LayoutClass {
        self.geometry_for(file_key).layout_class
    }

    /// The state backend, for the dispatcher's scsi extent
    /// transactions (the dispatcher is the async context; the trait
    /// hands it this Arc rather than growing async allocator methods).
    pub fn state_backend(&self) -> Arc<dyn crate::state_backend::StateBackend> {
        Arc::clone(&self.backend)
    }

    /// Resolve a scsi deviceid — the volume's NGUID — back to its
    /// volume by scanning the geometry cache's scsi-class entries.
    /// O(volumes) per call and restart-proof: the identity is derived,
    /// never remembered, so there is no registry to lose. GETDEVICEINFO
    /// is once-per-(client, volume) and cached client-side, so the scan
    /// never sits on a hot path.
    pub fn scsi_volume_for_deviceid(&self, device_id: &[u8; 16]) -> Option<String> {
        self.volume_geometry.iter().find_map(|entry| {
            match entry.value() {
                Some(g) if g.layout_class == LayoutClass::Scsi => {
                    let volume = entry.key();
                    (crate::nvmeof_export::scsi_device_id(volume) == *device_id)
                        .then(|| volume.clone())
                }
                _ => None,
            }
        })
    }

    /// Geometry for `volume`, recording `requested` if none is on file.
    ///
    /// Called from the CreateVolume already-exists path, which is a
    /// ROUTINE path — the CSI provisioner re-issues CreateVolume by name
    /// — so it must report the geometry actually in force. It also
    /// repairs a create that crashed between making the directory and
    /// recording the geometry.
    ///
    /// An existing record WINS over `requested`: geometry is fixed at
    /// creation, so a StorageClass edited between attempts must not
    /// silently re-stripe. The caller's echo check turns that
    /// disagreement into a visible error instead.
    pub async fn ensure_volume_geometry(
        &self,
        volume: &str,
        requested: VolumeGeometry,
    ) -> VolumeGeometry {
        match self.recorded_geometry(volume) {
            Some(g) => g,
            None => self.set_volume_geometry(volume, requested).await,
        }
    }

    /// Repopulate the in-memory primary + by-owner maps from a backend
    /// snapshot. Called once at MDS startup before the listener
    /// accepts. Note: device-counter increments are NOT replayed —
    /// device counts are observable load gauges, not load-bearing for
    /// correctness, and re-incrementing them would require ordering
    /// against DS re-registrations.
    pub fn load_records(&self, records: Vec<LayoutRecord>) {
        for r in records {
            let stateid = r.stateid;
            let layout = LayoutState::from_record(r);
            let cid = layout.owner.client_id;
            self.layouts.insert(state_key(&stateid), layout);
            self.by_owner
                .entry(cid)
                .or_insert_with(Vec::new)
                .push(stateid);
        }
        info!("LayoutManager loaded {} records from backend", self.layouts.len());
    }

    /// Repopulate pinned placements (and their stripe groups) from a
    /// backend snapshot. Called once at MDS startup, before the
    /// listener accepts — a post-restart LAYOUTGET for a pre-restart
    /// file must find its pin, not mint a fresh one from whichever
    /// DSes happen to have re-registered first.
    /// Every file with a cut still pending, as
    /// `(gate_key, file_key, placement, pending_size)` — for re-arming
    /// the retry after a restart (R4).
    pub fn parked_truncates(&self) -> Vec<(String, String, FilePlacement, u64)> {
        self.truncate_dirty
            .iter()
            .filter_map(|e| {
                let gate = e.key().clone();
                let min = e.value().min;
                self.placements.iter().find_map(|p| {
                    if truncate_gate_key(p.value(), p.key()) == gate {
                        Some((gate.clone(), p.key().clone(), p.value().clone(), min))
                    } else {
                        None
                    }
                })
            })
            .collect()
    }

    pub fn load_placement_records(&self, records: Vec<PlacementRecord>) {
        let n = records.len();
        for r in &records {
            let placement = FilePlacement::from_record(r);
            self.stripe_groups
                .entry(composite_device_id(&placement.device_ids))
                .or_insert_with(|| placement.device_ids.clone());
            // R4: re-arm the gate before anything can serve a layout.
            // Restoring the mark without the retry would be a wedge, so
            // MdsServer::load_persisted_state re-arms the retry right
            // after this (see resume_parked_truncates).
            if let Some(pending) = r.truncate_pending {
                let gate = truncate_gate_key(&placement, &r.file_key);
                // Inherit the age. Re-stamping to now() here re-armed the
                // fallback ceiling on EVERY restart, so a client taking the
                // MDS-fallback path during a long park could be DELAYed
                // without bound across repeated bounces — exactly the
                // livelock FALLBACK_DELAY_CEILING_DEFAULT exists to prevent.
                let carried = r
                    .truncate_since_unix
                    .map(|stamp| Duration::from_secs(unix_now().saturating_sub(stamp)))
                    .unwrap_or_default();
                self.truncate_dirty.insert(
                    gate.clone(),
                    GateMark { since: std::time::Instant::now(), carried, min: pending },
                );
                warn!(
                    "⏳ restored truncate gate for '{}' (deepest pending cut {}, \
                     already dirty {}s before this incarnation) — LAYOUTGET \
                     stays TRYLATER until a DS confirms",
                    gate, pending, carried.as_secs(),
                );
            }
            self.placements.insert(r.file_key.clone(), placement);
        }
        info!("LayoutManager loaded {} placements from backend", n);
    }

    /// The pinned placement for `file_key`, if one exists.
    pub fn placement_for(&self, file_key: &str) -> Option<FilePlacement> {
        self.placements.get(file_key).map(|p| p.clone())
    }

    /// Ordered device ids behind a composite (striped) deviceid, if
    /// any placement has registered it.
    pub fn stripe_group_devices(&self, device_id: &[u8; 16]) -> Option<Vec<String>> {
        self.stripe_groups.get(device_id).map(|g| g.clone())
    }

    /// Whether `file_key` has a pinned stripe placement — i.e. its data
    /// lives on the DS fleet and the MDS's local file is a sparse
    /// size-only stub. Read-only: never creates a pin.
    pub fn has_placement(&self, file_key: &str) -> bool {
        self.placements.contains_key(file_key)
    }

    /// Drop the pin for a deleted file so a future file at the same
    /// path gets a fresh placement. Stripe-group entries stay — they
    /// are content-addressed and other files may share them.
    ///
    /// Returns the removed placement (if any) so the caller can
    /// enqueue best-effort DS stripe cleanup for it.
    pub fn forget_placement(&self, file_key: &str) -> Option<FilePlacement> {
        let removed = self.placements.remove(file_key).map(|(_, p)| p);
        if removed.is_some() {
            info!("Placement forgotten for deleted file '{}'", file_key);
        }
        // A deleted file's unconfirmed truncation is moot — its stripes
        // are enqueued for deletion outright.
        if let Some(p) = &removed {
            self.truncate_dirty.remove(&truncate_gate_key(p, file_key));
        }
        self.backend
            .enqueue_write(WriteOp::DeletePlacement(file_key.to_string()));
        removed
    }

    /// Forget every placement whose key lives under `<dir_key>/` and
    /// return them (with their keys) for stripe cleanup. Used by pNFS
    /// volume deletion in the directory-per-volume model, where a CSI
    /// volume owns the whole `<volume_id>/…` subtree. The separator is
    /// part of the match, so deleting volume `foo` never touches
    /// `foobar`'s placements.
    pub fn forget_placements_under(&self, dir_key: &str) -> Vec<(String, FilePlacement)> {
        let prefix = format!("{}/", dir_key.trim_end_matches('/'));
        let keys: Vec<String> = self
            .placements
            .iter()
            .filter(|e| e.key().starts_with(&prefix))
            .map(|e| e.key().clone())
            .collect();
        keys.into_iter()
            .filter_map(|k| self.forget_placement(&k).map(|p| (k, p)))
            .collect()
    }

    /// Whether any placement under `<dir_key>/` is a legacy
    /// (file_id == 0) pin. Those cannot follow a directory rename —
    /// their DS stripes are keyed by the old path — so the RENAME op
    /// refuses the whole directory when one is present.
    pub fn has_legacy_placements_under(&self, dir_key: &str) -> bool {
        let prefix = format!("{}/", dir_key.trim_end_matches('/'));
        self.placements
            .iter()
            .any(|e| e.key().starts_with(&prefix) && e.value().file_id == 0)
    }

    /// Re-key every placement under `<old_dir>/` to `<new_dir>/…`
    /// after a successful directory rename. Without this, a renamed
    /// directory's children keep their old path keys: a fresh reader
    /// at the new path finds no pin, LAYOUTGET mints a fresh one, and
    /// the file reads as holes — silent data loss for any app that
    /// commits by directory rename (Spark's committer does). Returns
    /// the number of placements moved. No-op for file renames (a file
    /// key is never another key's prefix-parent).
    pub fn rename_placements_under(&self, old_dir: &str, new_dir: &str) -> usize {
        let prefix = format!("{}/", old_dir.trim_end_matches('/'));
        let keys: Vec<String> = self
            .placements
            .iter()
            .filter(|e| e.key().starts_with(&prefix))
            .map(|e| e.key().clone())
            .collect();
        let mut moved = 0;
        for old_key in keys {
            let suffix = &old_key[prefix.len()..];
            let new_key = format!("{}/{}", new_dir.trim_end_matches('/'), suffix);
            match self.rename_placement(&old_key, &new_key) {
                Ok(_) => moved += 1,
                Err(e) => tracing::warn!(
                    "💥 dir-rename re-key '{}' → '{}' failed AFTER fs rename: {}",
                    old_key, new_key, e
                ),
            }
        }
        moved
    }

    /// Re-key a pin for NFS RENAME. Only valid for v2 (file_id != 0)
    /// pins — their DS stripes are identity-keyed, so the path key is
    /// pure metadata and the data follows the rename for free. Legacy
    /// path-keyed pins must be REFUSED at the RENAME op instead (their
    /// DS stripes live at the old path; fresh readers of the new name
    /// would resolve to nothing).
    ///
    /// If a pinned file already existed at `new_key` (rename-over), its
    /// pin is dropped and returned so the caller can enqueue its stripe
    /// cleanup.
    pub fn rename_placement(
        &self,
        old_key: &str,
        new_key: &str,
    ) -> Result<Option<FilePlacement>, String> {
        let Some(placement) = self.placement_for(old_key) else {
            // Not pinned — nothing to move.
            return Ok(None);
        };
        if placement.file_id == 0 {
            return Err(format!(
                "legacy path-keyed pin for '{}' cannot be renamed",
                old_key
            ));
        }
        let overwritten = self.forget_placement(new_key);
        self.placements.insert(new_key.to_string(), placement.clone());
        self.placements.remove(old_key);
        // An unconfirmed truncation follows the file automatically:
        // the gate is keyed by the placement's file identity, which
        // the rename preserves.

        // Two ordered enqueues; the writer's group commit typically
        // lands both in one transaction (old row gone ⇔ new row live).
        self.backend
            .enqueue_write(WriteOp::PutPlacement(placement.to_record(new_key)));
        self.backend
            .enqueue_write(WriteOp::DeletePlacement(old_key.to_string()));
        info!(
            "Placement re-keyed for rename: '{}' → '{}' (file_id {:016x})",
            old_key, new_key, placement.file_id
        );
        Ok(overwritten)
    }

    /// Enqueue best-effort deletion of a removed file's stripe files on
    /// its pinned DSes. Drained into HeartbeatResponse instructions by
    /// the control service. In-memory only: a lost queue leaks orphaned
    /// stripe space, never correctness (a recreated file has a fresh
    /// file_id and therefore fresh stripe paths).
    pub fn enqueue_stripe_cleanup(&self, placement: &FilePlacement, file_key: &str) {
        if placement.file_id == 0 {
            // Legacy pin: stripes live at the path-rebased location,
            // which the next same-name file would REUSE — deleting them
            // matters more here, but the relative path depends on the
            // export root which this layer doesn't know. The operations
            // layer passes the rebased path via cleanup_legacy_rel_path.
            return;
        }
        for (slot, device_id) in placement.device_ids.iter().enumerate() {
            let rel = placement.stripe_rel_path(slot);
            self.cleanup_queues
                .entry(device_id.clone())
                .or_default()
                .push(rel);
        }
        debug!(
            "Stripe cleanup enqueued for '{}' (file_id {:016x}, {} DSes)",
            file_key,
            placement.file_id,
            placement.device_ids.len()
        );
    }

    /// Enqueue a legacy (path-keyed) stripe file for deletion on every
    /// DS in the placement. `rel_path` is relative to the DS data dir.
    pub fn enqueue_legacy_cleanup(&self, placement: &FilePlacement, rel_path: &str) {
        for device_id in &placement.device_ids {
            self.cleanup_queues
                .entry(device_id.clone())
                .or_default()
                .push(rel_path.to_string());
        }
    }

    /// Drain the pending stripe-cleanup paths for one DS (called by the
    /// heartbeat handler; the batch rides the HeartbeatResponse).
    pub fn drain_stripe_cleanup(&self, device_id: &str) -> Vec<String> {
        self.cleanup_queues
            .remove(device_id)
            .map(|(_, v)| v)
            .unwrap_or_default()
    }

    /// Mark a file truncate-dirty: `size` has been applied to the MDS
    /// stub but has NOT been confirmed on every pinned DS's stripe
    /// file. Keeps the oldest mark and the smallest size if already
    /// dirty (the ceiling measures the total unconfirmed window; the
    /// gate lifts only when the deepest cut lands).
    pub fn mark_truncate_dirty(&self, gate_key: &str, size: u64) {
        let mut pending = size;
        let mut age = Duration::ZERO;
        self.truncate_dirty
            .entry(gate_key.to_string())
            .and_modify(|m| {
                m.min = m.min.min(size);
                pending = m.min;
                age = m.age();
            })
            .or_insert_with(|| GateMark {
                since: std::time::Instant::now(),
                carried: Duration::ZERO,
                min: size,
            });
        // R4: the gate has to outlive this process. A restart during a
        // PARKED truncate otherwise comes back ungated with the DSes
        // still holding the old bytes.
        //
        // The stamp is wall-clock because it has to survive a process,
        // and it is derived from the monotonic age rather than read
        // fresh so that re-marking an ALREADY-dirty gate cannot reset
        // it — mark keeps the oldest, and so must its persisted form.
        self.persist_gate(gate_key, Some(pending), Some(unix_now().saturating_sub(age.as_secs())));
    }

    /// Mirror the gate onto the file's persisted placement record.
    /// Best-effort and keyed by the gate, so a gate with no matching
    /// placement (there should be none) is simply not written.
    fn persist_gate(&self, gate_key: &str, pending: Option<u64>, since_unix: Option<u64>) {
        for entry in self.placements.iter() {
            if truncate_gate_key(entry.value(), entry.key()) == gate_key {
                let mut rec = entry.value().to_record(entry.key());
                rec.truncate_pending = pending;
                rec.truncate_since_unix = since_unix;
                self.backend.enqueue_write(WriteOp::PutPlacement(rec));
                return;
            }
        }
    }

    /// Lift the gate if a fan-out that confirmed `confirmed_size` on
    /// every pinned DS satisfies the deepest pending cut. Returns
    /// whether the gate was lifted.
    pub fn clear_truncate_dirty_if(&self, gate_key: &str, confirmed_size: u64) -> bool {
        let cleared = self
            .truncate_dirty
            .remove_if(gate_key, |_, m| confirmed_size <= m.min)
            .is_some();
        if cleared {
            self.persist_gate(gate_key, None, None);
            info!("Truncate-dirty cleared for '{}' (size {} confirmed)", gate_key, confirmed_size);
        }
        cleared
    }

    /// Unconditionally lift the gate (file deleted — its stripes are
    /// enqueued for deletion outright).
    pub fn clear_truncate_dirty(&self, gate_key: &str) {
        self.truncate_dirty.remove(gate_key);
        self.persist_gate(gate_key, None, None);
    }

    /// The gate state: (dirty-since IN THIS PROCESS, smallest
    /// unconfirmed size). Callers measuring the unconfirmed window want
    /// `truncate_dirty_age`, not this — `since` restarts at every
    /// incarnation and this accessor cannot see the inherited part.
    pub fn truncate_dirty_state(&self, gate_key: &str) -> Option<(std::time::Instant, u64)> {
        self.truncate_dirty.get(gate_key).map(|e| (e.value().since, e.value().min))
    }

    /// When the file went truncate-dirty in THIS process, if it still
    /// is. Presence checks only; see `truncate_dirty_age` for duration.
    pub fn truncate_dirty_since(&self, gate_key: &str) -> Option<std::time::Instant> {
        self.truncate_dirty_state(gate_key).map(|(since, _)| since)
    }

    /// How long the file has been truncate-dirty IN TOTAL, across every
    /// MDS incarnation since the mark was first set. This is what the
    /// fallback delay ceiling has to measure: a bounce is not progress,
    /// and a gate that has been dirty for an hour is an hour old however
    /// many times the process restarted underneath it.
    pub fn truncate_dirty_age(&self, gate_key: &str) -> Option<Duration> {
        self.truncate_dirty.get(gate_key).map(|e| e.value().age())
    }

    /// Get-or-create the pinned placement for `file_key`.
    ///
    /// First grant for a file pins the *sorted* active device set and
    /// the configured stripe size. `entry()` makes a concurrent
    /// first-grant race pin exactly one placement (both racers compute
    /// identical content anyway, since the list is sorted).
    fn placement_for_grant(&self, file_key: &str) -> Result<FilePlacement, String> {
        if let Some(p) = self.placements.get(file_key) {
            return Ok(p.clone());
        }

        let mut devices = self.device_registry.list_active();
        if devices.is_empty() {
            return Err("No active data servers available".to_string());
        }
        // list_active() sorts, but the pin must not depend on that:
        // sort again here so placement content is deterministic even
        // if the registry's ordering ever regresses.
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));

        // Per-volume geometry. Width narrows the pin: striping across
        // fewer DSes lowers a file's peak bandwidth but also shrinks its
        // failure domain, because a pinned DS going missing makes the
        // file unavailable (see `placement_refuses_when_pinned_device_missing`).
        // Pinning all DSes — the default — means any single DS loss
        // affects every file in the fleet.
        //
        // The subset is taken from the SORTED head, so it is a pure
        // function of the fleet and reproducible; it deliberately does
        // NOT hash-spread volumes across different subsets, because that
        // would make the fleet's failure domains overlap in a way no
        // operator could reason about. Spreading is a later decision
        // that needs a policy, not an accident of hashing.
        let geometry = self.geometry_for(file_key);
        if geometry.stripe_width > 0 {
            let want = geometry.stripe_width as usize;
            if want < devices.len() {
                devices.truncate(want);
            }
        }
        // Capacity honesty: pins are forever, so warn loudly when a
        // new file is being pinned onto a nearly-full DS. (Placement
        // still proceeds — capacity-aware selection is future work;
        // the client sees clean NOSPC from the DS if it truly fills.)
        for d in &devices {
            if d.capacity > 0 && d.used as f64 / d.capacity as f64 > 0.90 {
                tracing::warn!(
                    "📛 pinning '{}' onto nearly-full DS {} ({:.0}% used of {} GiB)",
                    file_key,
                    d.device_id,
                    100.0 * d.used as f64 / d.capacity as f64,
                    d.capacity / (1024 * 1024 * 1024)
                );
            }
        }
        let device_ids: Vec<String> = devices.into_iter().map(|d| d.device_id).collect();

        // Miss-handling holds the map entry (shard lock) end to end so
        // two racing grants cannot mint twice — the loser must observe
        // the winner's placement, and the stub xattr must match the map
        // (a lost race that overwrote the xattr with a DIFFERENT mint
        // would recreate F67 at the next restart).
        use dashmap::mapref::entry::Entry;
        let placement = match self.placements.entry(file_key.to_string()) {
            Entry::Occupied(e) => e.get().clone(),
            Entry::Vacant(v) => {
                // F67 recovery: the backend lost the record but the stub
                // still carries the binding. Re-adopt it verbatim —
                // file_id AND device order are identity, not preference.
                if let Some(recovered) = self.stub_binding.read(file_key) {
                    info!(
                        "🩹 F67: recovered placement for '{}' from its stub binding \
                         (file_id {:016x}, {} DSes) — backend record was missing",
                        file_key,
                        recovered.file_id,
                        recovered.device_ids.len(),
                    );
                    self.backend
                        .enqueue_write(WriteOp::PutPlacement(recovered.to_record(file_key)));
                    v.insert(recovered.clone());
                    recovered
                } else {
                    // F67 orphan guard: a stub with bytes and NO binding
                    // anywhere means minting would strand data behind a
                    // fresh identity. Refuse. (Native MDS files also land
                    // here on their first LAYOUTGET: the refusal maps to
                    // LAYOUTUNAVAILABLE, the client falls back to MDS
                    // I/O, and the fallback disposition serves dense
                    // stubs — so native files keep working while the
                    // ambiguous sparse case fails LOUD, never as zeros.)
                    if let Some(meta) = self.stub_binding.stub_meta(file_key) {
                        if meta.len > 0 {
                            orphan_log(file_key, meta.len);
                            return Err(format!(
                                "{ORPHANED_DATA}: '{}' has {} bytes but no placement \
                                 binding (backend and stub xattr both empty) — refusing \
                                 to mint a fresh file_id over existing data",
                                file_key, meta.len
                            ));
                        }
                    }
                    let placement = FilePlacement {
                        // The VOLUME's stripe size, not the fleet
                        // default — the placement is what every later
                        // grant reads, so this is the single point where
                        // per-volume geometry becomes durable for the
                        // file.
                        stripe_size: geometry.stripe_size,
                        device_ids,
                        file_id: allocate_file_id(),
                    };
                    // Binding BEFORE backend record: a crash between the
                    // two recovers from the xattr; the reverse order
                    // would leave a backend id no stub can corroborate.
                    // A failed binding write refuses the grant — an
                    // unbound id must never reach the wire.
                    if let Err(e) = self.stub_binding.write(file_key, &placement) {
                        return Err(format!(
                            "F67: cannot write placement binding for '{}': {} — \
                             refusing the grant rather than issuing an unbound file_id",
                            file_key, e
                        ));
                    }
                    self.backend
                        .enqueue_write(WriteOp::PutPlacement(placement.to_record(file_key)));
                    info!(
                        "📌 Pinned placement for '{}': {} DSes {:?}, stripe_size={}",
                        file_key,
                        placement.device_ids.len(),
                        placement.device_ids,
                        placement.stripe_size,
                    );
                    v.insert(placement.clone());
                    placement
                }
            }
        };

        self.stripe_groups
            .entry(composite_device_id(&placement.device_ids))
            .or_insert_with(|| placement.device_ids.clone());

        Ok(placement)
    }

    /// F67: read-only recovery for non-grant paths (the MDS fallback
    /// disposition). Like `placement_for`, but a map miss also consults
    /// the stub binding so a records-lost MDS still recognizes striped
    /// files it has not re-granted yet.
    pub fn placement_or_recovered(&self, file_key: &str) -> Option<FilePlacement> {
        if let Some(p) = self.placements.get(file_key) {
            return Some(p.clone());
        }
        let recovered = self.stub_binding.read(file_key)?;
        info!(
            "🩹 F67: recovered placement for '{}' from its stub binding (fallback path)",
            file_key
        );
        self.backend
            .enqueue_write(WriteOp::PutPlacement(recovered.to_record(file_key)));
        self.placements
            .entry(file_key.to_string())
            .or_insert_with(|| recovered.clone());
        Some(recovered)
    }

    /// F67: stub visibility for the fallback disposition's orphan
    /// check (operations has no filesystem access of its own).
    pub fn stub_meta(&self, file_key: &str) -> Option<super::stub_binding::StubMeta> {
        self.stub_binding.stub_meta(file_key)
    }

    /// F67 boot backfill: write the stub binding for every placement
    /// the backend restored that does not carry one yet. Converges
    /// pre-F67 fleets to full xattr coverage on the first boot after
    /// upgrade. Returns (written, failed).
    pub fn backfill_stub_bindings(&self) -> (usize, usize) {
        let mut written = 0usize;
        let mut failed = 0usize;
        for entry in self.placements.iter() {
            let (file_key, placement) = (entry.key(), entry.value());
            if self.stub_binding.read(file_key).is_some() {
                continue;
            }
            // Only bind stubs that exist — a placement whose stub is
            // gone is a delete in flight, not backfill material.
            if self.stub_binding.stub_meta(file_key).is_none() {
                continue;
            }
            match self.stub_binding.write(file_key, placement) {
                Ok(()) => written += 1,
                Err(e) => {
                    failed += 1;
                    warn!("F67 backfill: binding write for '{}' failed: {}", file_key, e);
                }
            }
        }
        (written, failed)
    }

    fn persist(&self, l: &LayoutState) {
        self.backend.enqueue_write(WriteOp::PutLayout(l.to_record()));
    }

    fn persist_delete(&self, stateid: LayoutStateId) {
        self.backend.enqueue_write(WriteOp::DeleteLayout(stateid));
    }

    /// Generate a new layout for a file.
    ///
    /// `owner` identifies the client / session / fsid that this layout is
    /// issued to. RFC 8881 §12.5 ties every layout to a specific client
    /// for recall and return-by-clientid semantics; CB_LAYOUTRECALL routes
    /// through the owner's session.
    /// Register a scsi-class grant in the layout state machine, minting
    /// the stateid CB_LAYOUTRECALL and LAYOUTRETURN address. The extent
    /// list itself is deliberately NOT state here — the allocator's
    /// grant rows (extent_grants) are the authority on what the client
    /// holds; this entry is the RECALL HANDLE: stateid ↔ (owner,
    /// file_ident), which is everything the recall/return lifecycle
    /// needs to find and revoke the grant. `file_ident` is the file_key
    /// itself: scsi files have no placement, hence no truncate_gate_key
    /// — their truncate path is extent reclaim, not the DS fanout gate,
    /// so there is no C6 publish-recheck here either (nothing to race).
    pub fn register_scsi_layout(
        &self,
        owner: LayoutOwner,
        filehandle: Vec<u8>,
        file_key: &str,
        iomode: IoMode,
    ) -> [u8; 16] {
        let stateid = Self::generate_stateid();
        let layout = LayoutState {
            stateid,
            owner,
            filehandle,
            file_ident: file_key.to_string(),
            segments: Vec::new(),
            iomode,
            return_on_close: true,
        };
        self.persist(&layout);
        self.layouts.insert(state_key(&stateid), layout);
        self.by_owner
            .entry(owner.client_id)
            .or_insert_with(Vec::new)
            .push(stateid);
        stateid
    }

    /// RENAME follow-through for scsi files: recall handles are keyed by
    /// file_ident (the export-relative path at grant time), and a handle
    /// left under the old key is a handle `recall_layouts_for_file(new)`
    /// can never find — the reclaim then skips the recall and goes
    /// straight to fencing a perfectly responsive client. Re-key them.
    /// Same-volume renames only make sense here (the extents live in the
    /// old volume's lvol); the caller polices that.
    pub fn rekey_scsi_layouts(&self, old_key: &str, new_key: &str) -> usize {
        let stateids: Vec<[u8; 16]> = self
            .layouts
            .iter()
            .filter(|e| e.value().file_ident == old_key)
            .map(|e| e.value().stateid)
            .collect();
        let mut n = 0;
        for sid in stateids {
            if let Some(mut l) = self.layouts.get_mut(&state_key(&sid)) {
                l.file_ident = new_key.to_string();
                self.persist(&l);
                n += 1;
            }
        }
        n
    }

    /// Remove a scsi layout by stateid, returning it so the caller can
    /// drop the allocator's grant rows for its file. `None` = unknown
    /// stateid (already returned, revoked, or never granted) — the
    /// BadStateId shape, benign on the return path.
    pub fn take_scsi_layout(&self, stateid: &[u8; 16]) -> Option<LayoutState> {
        let (_, layout) = self.layouts.remove(&state_key(stateid))?;
        self.persist_delete(*stateid);
        if let Some(mut v) = self.by_owner.get_mut(&layout.owner.client_id) {
            v.retain(|s| s != stateid);
        }
        Some(layout)
    }

    pub fn generate_layout(
        &self,
        owner: LayoutOwner,
        filehandle: Vec<u8>,
        file_key: &str,
        offset: u64,
        length: u64,
        iomode: IoMode,
    ) -> Result<LayoutState, String> {
        // Every grant goes through the file's pinned placement — never
        // the live registry's current membership/order. A file whose
        // pinned DS is gone gets a refusal (client retries/backs off),
        // not a silently re-mapped stripe pattern.
        let placement = self.placement_for_grant(file_key)?;
        let mut devices = Vec::with_capacity(placement.device_ids.len());
        for id in &placement.device_ids {
            match self.device_registry.get(id) {
                Some(d) if d.status == DeviceStatus::Active => devices.push(d),
                _ => {
                    return Err(format!(
                        "placement device '{}' for '{}' is not active — refusing layout \
                         rather than re-mapping stripes",
                        id, file_key,
                    ));
                }
            }
        }

        debug!(
            "💥 Generating layout: file='{}', offset={}, length={}, iomode={:?}, devices={}",
            file_key,
            offset,
            length,
            iomode,
            devices.len()
        );

        let segments = match self.policy {
            LayoutPolicyImpl::RoundRobin => {
                self.generate_roundrobin_layout(offset, length, &devices)?
            }
            LayoutPolicyImpl::Stripe => {
                self.generate_stripe_layout(offset, length, &devices, placement.stripe_size)?
            }
            LayoutPolicyImpl::Locality => {
                // TODO: Implement locality-aware layout
                self.generate_roundrobin_layout(offset, length, &devices)?
            }
        };

        let stateid = Self::generate_stateid();
        let gate_ident = truncate_gate_key(&placement, file_key);
        let layout = LayoutState {
            stateid,
            owner,
            filehandle,
            // Same placement the stripe map above came from, so this is
            // byte-identical to the key note_truncate files the gate
            // under. Do NOT recompute it from file_key alone.
            file_ident: gate_ident.clone(),
            segments,
            iomode,
            return_on_close: true,
        };

        // PUBLISH, THEN RE-READ THE GATE (audit C6).
        //
        // layoutget checked truncate_dirty before calling in here, and there
        // is no lock spanning the two — LayoutManager has none, and `layouts`
        // and `truncate_dirty` are independent DashMaps. So a truncate can
        // arm the mark between that check and this insert, and its recall
        // scan iterates `layouts` before this entry is in it: the grant
        // escapes the gate AND the recall. The retry task only re-fans-out,
        // never re-recalls, so on the park path a microsecond race becomes an
        // hours-long exposure with the gate showing dirty the whole time.
        //
        // Publishing first and re-reading after is sufficient by
        // construction: the mark is set before the recall's scan, and this
        // entry is visible before the recheck — so any interleaving is caught
        // by one side or the other.
        self.persist(&layout);
        self.layouts.insert(state_key(&stateid), layout.clone());
        if self.truncate_dirty.contains_key(&gate_ident) {
            self.layouts.remove(&state_key(&stateid));
            self.persist_delete(stateid);
            warn!(
                "⏳ LAYOUTGET for '{}' raced a truncate — mark armed between the \
                 gate check and the publish; revoking the grant and refusing",
                file_key,
            );
            return Err(GRANT_RACED_TRUNCATE.to_string());
        }
        self.by_owner
            .entry(owner.client_id)
            .or_insert_with(Vec::new)
            .push(stateid);

        debug!(
            "🎯 Generated pNFS layout with {} segments, stateid={:?}, client={}",
            layout.segments.len(),
            &stateid[0..4],
            owner.client_id,
        );
        debug!("   📊 Layout details:");
        for (i, seg) in layout.segments.iter().enumerate() {
            debug!("      Segment {}: device={}, offset={}, length={}", 
                  i, seg.device_id, seg.offset, seg.length);
        }
        debug!("   ✅ Client will now perform parallel I/O across {} data servers!", layout.segments.len());

        Ok(layout)
    }

    /// Generate round-robin layout (simplest policy)
    fn generate_roundrobin_layout(
        &self,
        offset: u64,
        length: u64,
        devices: &[DeviceInfo],
    ) -> Result<Vec<LayoutSegment>, String> {
        if devices.is_empty() {
            return Err("No devices available".to_string());
        }

        let mut segments = Vec::new();
        let current_offset = offset;
        let _end_offset = offset.saturating_add(length);

        // Simple round-robin: assign entire range to first device
        // In a more sophisticated implementation, we would split across multiple devices
        let device = &devices[0];

        segments.push(LayoutSegment {
            offset: current_offset,
            length: if length == u64::MAX {
                u64::MAX  // NFS4_UINT64_MAX means "rest of file"
            } else {
                length
            },
            iomode: IoMode::ReadWrite,
            device_id: device.device_id.clone(),
            stripe_index: 0,
            pattern_offset: 0,
        });

        Ok(segments)
    }

    /// Generate striped layout for parallel I/O. `stripe_size` comes
    /// from the file's pinned placement, not the live config.
    fn generate_stripe_layout(
        &self,
        offset: u64,
        length: u64,
        devices: &[DeviceInfo],
        stripe_size: u64,
    ) -> Result<Vec<LayoutSegment>, String> {
        if devices.is_empty() {
            return Err("No devices available".to_string());
        }

        let mut segments = Vec::new();
        let num_devices = devices.len();

        // Align offset to stripe boundary
        let stripe_start = (offset / stripe_size) * stripe_size;
        let mut current_offset = offset;
        let end_offset = if length == u64::MAX {
            u64::MAX
        } else {
            offset.saturating_add(length)
        };

        // If length is u64::MAX (rest of file), create a single segment
        // spanning the entire remaining file across all devices
        if length == u64::MAX {
            for (i, device) in devices.iter().enumerate() {
                segments.push(LayoutSegment {
                    offset: current_offset,
                    length: u64::MAX,
                    iomode: IoMode::ReadWrite,
                    device_id: device.device_id.clone(),
                    stripe_index: i as u32,
                    pattern_offset: stripe_start,
                });
            }
            return Ok(segments);
        }

        // Generate striped segments
        let mut stripe_index = ((offset / stripe_size) % (num_devices as u64)) as usize;

        while current_offset < end_offset {
            let device = &devices[stripe_index % num_devices];
            
            // Calculate segment length (either stripe_size or remaining bytes)
            let remaining = end_offset - current_offset;
            let segment_length = stripe_size.min(remaining);

            segments.push(LayoutSegment {
                offset: current_offset,
                length: segment_length,
                iomode: IoMode::ReadWrite,
                device_id: device.device_id.clone(),
                stripe_index: stripe_index as u32,
                pattern_offset: stripe_start,
            });

            current_offset += segment_length;
            stripe_index += 1;
        }

        debug!(
            "Generated striped layout: {} segments across {} devices",
            segments.len(),
            num_devices
        );

        Ok(segments)
    }

    /// Return a layout (client releases it). Cleans the secondary
    /// by-client index alongside the primary map so the indexes stay
    /// consistent.
    pub fn return_layout(&self, stateid: &LayoutStateId) -> Result<(), String> {
        // A layout WE revoked, being handed back by a client doing exactly
        // what the recall asked. That is success, not an error.
        if self.revoked.remove(&state_key(stateid)).is_some() {
            debug!(
                "LAYOUTRETURN of revoked stateid {:?} — answering OK (we took it)",
                &stateid[0..4],
            );
            return Ok(());
        }
        if let Some((_, layout)) = self.layouts.remove(&state_key(stateid)) {
            debug!(
                "Layout returned: stateid={:?}, segments={}, client={}",
                &stateid[0..4],
                layout.segments.len(),
                layout.owner.client_id,
            );

            // Drop from the by-client index. Empty entries are removed so the
            // map doesn't accumulate stale clientid keys after long-running
            // clients hand back all their layouts.
            if let Some(mut entry) = self.by_owner.get_mut(&layout.owner.client_id) {
                entry.retain(|s| state_key(s) != state_key(stateid));
                let now_empty = entry.is_empty();
                drop(entry);
                if now_empty {
                    self.by_owner.remove(&layout.owner.client_id);
                }
            }

            // Decrement active layout counts for affected devices
            for segment in &layout.segments {
                let _ = self.device_registry.decrement_layout_count(&segment.device_id);
            }

            self.persist_delete(*stateid);

            Ok(())
        } else {
            Err(format!("Layout not found: {:?}", &stateid[0..4]))
        }
    }

    /// Server-side forcible removal of a layout — RFC 5661 §12.5.5.2
    /// permits the server to revoke a layout after CB_LAYOUTRECALL
    /// when the client doesn't return it within the deadline. Same
    /// effect as `return_layout` (drop from primary + secondary
    /// indexes, decrement device counters) but **idempotent**: a
    /// second call (or a race with the client's own LAYOUTRETURN)
    /// is a no-op rather than an error.
    ///
    /// Returns `true` if this call removed an active layout, `false`
    /// if it was already gone. The caller can use that to log the
    /// distinction; functionally either outcome is fine.
    ///
    /// Subsequent client uses of this stateid (LAYOUTGET extension,
    /// LAYOUTRETURN, LAYOUTCOMMIT) will see "not found" and the
    /// dispatcher maps that to `NFS4ERR_BAD_STATEID`. We don't keep
    /// a tombstone set — a removed entry is indistinguishable from
    /// "never existed," and the spec doesn't distinguish them on
    /// the wire either.
    pub fn revoke_layout(&self, stateid: &LayoutStateId) -> bool {
        let Some((_, layout)) = self.layouts.remove(&state_key(stateid)) else {
            return false;
        };
        let now = std::time::Instant::now();
        self.revoked.retain(|_, t| now.duration_since(*t) < REVOKED_TOMBSTONE_TTL);
        self.revoked.insert(state_key(stateid), now);
        info!(
            "🚫 Layout revoked: stateid={:?}, segments={}, client={}",
            &stateid[0..4],
            layout.segments.len(),
            layout.owner.client_id,
        );
        // Same index cleanup as `return_layout` — keep the by_owner
        // and device counters in sync. Logic is duplicated rather
        // than refactored shared because the *log line* differs (and
        // the caller cares about which one ran).
        if let Some(mut entry) = self.by_owner.get_mut(&layout.owner.client_id) {
            entry.retain(|s| state_key(s) != state_key(stateid));
            let now_empty = entry.is_empty();
            drop(entry);
            if now_empty {
                self.by_owner.remove(&layout.owner.client_id);
            }
        }
        for segment in &layout.segments {
            let _ = self.device_registry.decrement_layout_count(&segment.device_id);
        }
        self.persist_delete(*stateid);
        true
    }

    /// Every client id currently holding at least one layout (files or
    /// scsi — both live in `by_owner`). The lease sweep's candidate
    /// enumeration: a holder whose lease is gone is a dead client whose
    /// layouts nothing else will ever return.
    pub fn owner_clients(&self) -> Vec<u64> {
        self.by_owner
            .iter()
            .filter(|e| !e.value().is_empty())
            .map(|e| *e.key())
            .collect()
    }

    /// Return all layouts held by `client_id` (RFC 8881 §18.44.3
    /// `LAYOUTRETURN4_ALL`). Returns the list of stateids that were
    /// released so the caller can cancel any in-flight CB_LAYOUTRECALL
    /// for them.
    pub fn return_all_for_client(&self, client_id: u64) -> Vec<LayoutStateId> {
        let stateids: Vec<LayoutStateId> = self.by_owner
            .get(&client_id)
            .map(|entry| entry.clone())
            .unwrap_or_default();
        for sid in &stateids {
            let _ = self.return_layout(sid);
        }
        stateids
    }

    /// Return all layouts held by `client_id` in `fsid` (RFC 8881 §18.44.3
    /// `LAYOUTRETURN4_FSID`).
    pub fn return_fsid_for_client(&self, client_id: u64, fsid: u64) -> Vec<LayoutStateId> {
        let stateids: Vec<LayoutStateId> = self.by_owner
            .get(&client_id)
            .map(|entry| {
                entry.iter()
                    .filter(|sid| {
                        self.layouts
                            .get(*sid)
                            .map(|l| l.owner.fsid == fsid)
                            .unwrap_or(false)
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        for sid in &stateids {
            let _ = self.return_layout(sid);
        }
        stateids
    }

    /// Enumerate active layouts owned by `client_id`. Used by the
    /// CB_LAYOUTRECALL backchannel (Task #4) when a device fails — we
    /// need to find every layout of every client that referenced the
    /// dead device so we can recall them.
    pub fn layouts_for_client(&self, client_id: u64) -> Vec<LayoutStateId> {
        self.by_owner
            .get(&client_id)
            .map(|entry| entry.clone())
            .unwrap_or_default()
    }

    /// Find every layout whose segments touch `device_id`, paired
    /// with the session id of the client that owns it. Used by the
    /// CB_LAYOUTRECALL fan-out on DS-death (Phase A.4): each pair is
    /// one CB CALL routed to a specific back-channel.
    ///
    /// Returns `(session_id, layout_stateid)` tuples — both are 16-
    /// byte fixed opaques. The session id comes from `LayoutOwner`
    /// (set on LAYOUTGET); a single layout has exactly one session.
    /// One client with multiple layouts on the dead device produces
    /// multiple pairs with the same session id.
    /// Advance one layout's stateid for a CB_LAYOUTRECALL and return
    /// what to put on the wire: `(owner session, BUMPED stateid,
    /// filehandle)`.
    ///
    /// RFC 8881 §12.5.3: the server increments the seqid on each
    /// CB_LAYOUTRECALL, and §12.5.5.2.1 makes the client CHECK it. Linux
    /// implements that check exactly (`pnfs_check_callback_stateid`):
    /// `newseq <= oldseq` → **NFS4ERR_OLD_STATEID**, `newseq > oldseq+1`
    /// → NFS4ERR_DELAY. So the recall must carry precisely one more than
    /// the client holds, or the client refuses it before draining
    /// anything. Bump first, then hand out the bumped value; the map key
    /// is `other`, so this does not move the entry.
    ///
    /// EVERY recall path goes through here. The seqid bump was fixed
    /// once (audit C1) on the truncate path only, and the DS-death path
    /// kept sending the stale seqid for months — invisible because the
    /// client's refusal is answered by forced server-side revocation,
    /// which looks like a working recall from the outside.
    fn bump_recall_stateid(
        &self,
        key: &LayoutStateId,
    ) -> Option<(SessionIdBytes, LayoutStateId, Vec<u8>)> {
        let mut entry = self.layouts.get_mut(key)?;
        let next = seqid_of(&entry.stateid).wrapping_add(1);
        let next = if next == 0 { LAYOUT_SEQID_BASE } else { next };
        entry.stateid[0..4].copy_from_slice(&next.to_be_bytes());
        let to_send = entry.stateid;
        let owner_session = entry.owner.session_id;
        let fh = entry.filehandle.clone();
        let snapshot = entry.clone();
        drop(entry);
        self.persist(&snapshot);
        Some((owner_session, to_send, fh))
    }

    pub fn recall_layouts_for_device(
        &self,
        device_id: &str,
    ) -> Vec<(SessionIdBytes, LayoutStateId)> {
        // Two phases, and not for style: a DashMap write taken while one
        // of its own iterators is live deadlocks on the shard lock, and
        // the bump below is a write.
        let mut hits: Vec<LayoutStateId> = Vec::new();
        for entry in self.layouts.iter() {
            if entry.segments.iter().any(|seg| seg.device_id == device_id) {
                hits.push(state_key(&entry.stateid));
            }
        }

        let mut recalled = Vec::new();
        for key in hits {
            if let Some((session, stateid, _fh)) = self.bump_recall_stateid(&key) {
                recalled.push((session, stateid));
            }
        }

        if !recalled.is_empty() {
            info!(
                "Recalling {} layout(s) using device {}",
                recalled.len(),
                device_id
            );
        }

        recalled
    }

    /// Every outstanding layout for ONE file, for the truncate recall
    /// (F65). Returns `(session_id, stateid, filehandle)` per layout —
    /// the filehandle is what makes the CB_LAYOUTRECALL per-file rather
    /// than session-wide, which matters here in a way it does not for a
    /// dead DS: a session-wide recall on every SETATTR(size) would drop
    /// the client's layouts for every OTHER file too.
    ///
    /// `file_ident` is [`truncate_gate_key`]'s output. An empty ident
    /// never matches: a layout restored from a pre-v7 record does not
    /// know its file, and matching it would mean recalling every such
    /// layout on every truncate. That is a real (narrow) hole — layouts
    /// that survived an upgrade-restart are not recalled — and it
    /// closes itself as those layouts are returned; it is not papered
    /// over with a wildcard.
    pub fn recall_layouts_for_file(
        &self,
        file_ident: &str,
    ) -> Vec<(SessionIdBytes, LayoutStateId, Vec<u8>)> {
        if file_ident.is_empty() {
            return Vec::new();
        }
        let mut recalled = Vec::new();
        let mut hits: Vec<LayoutStateId> = Vec::new();
        let mut unidentified = 0usize;
        for entry in self.layouts.iter() {
            if entry.file_ident.is_empty() {
                unidentified += 1;
                continue;
            }
            if entry.file_ident == file_ident {
                hits.push(state_key(&entry.stateid));
            }
        }
        // The scan above is finished before a single write below: a DashMap
        // write taken while one of its own iterators is live deadlocks on the
        // shard lock.
        for key in hits {
            if let Some(triple) = self.bump_recall_stateid(&key) {
                recalled.push(triple);
            }
        }
        if unidentified > 0 {
            warn!(
                "Truncate recall for '{}': {} layout(s) restored from a pre-v7 \
                 record carry no file identity and were NOT considered — they \
                 predate the column and cannot be matched to a file",
                file_ident, unidentified,
            );
        }
        if !recalled.is_empty() {
            info!(
                "Recalling {} layout(s) for truncated file {}",
                recalled.len(),
                file_ident,
            );
        }
        recalled
    }

    /// Get layout by stateid
    pub fn get_layout(&self, stateid: &LayoutStateId) -> Option<LayoutState> {
        self.layouts.get(&state_key(stateid)).map(|entry| entry.clone())
    }

    /// Get all active layouts
    pub fn active_layouts(&self) -> Vec<LayoutState> {
        self.layouts.iter().map(|entry| entry.clone()).collect()
    }

    /// Get layout count
    pub fn layout_count(&self) -> usize {
        self.layouts.len()
    }

    /// Generate a unique layout stateid
    /// Mint a stateid: random `other`, seqid pinned to
    /// [`LAYOUT_SEQID_BASE`].
    ///
    /// The seqid bytes are NOT randomised. They were, and that was two
    /// bugs in one: the client's first recall could never be seqid+1 of
    /// anything meaningful, and one mint in 2^32 handed out the
    /// §20.3.3-forbidden zero.
    fn generate_stateid() -> LayoutStateId {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut stateid = [0u8; 16];
        rng.fill(&mut stateid[4..16]);
        stateid[0..4].copy_from_slice(&LAYOUT_SEQID_BASE.to_be_bytes());
        stateid
    }
}

// `LayoutManager` no longer has a `Default` impl: the type now
// requires a backend. Construction sites (production = MDS startup,
// tests = each #[test] fn) pass it explicitly.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnfs::mds::device::DeviceInfo;

    /// Test-only LayoutOwner so the test fixtures don't have to fabricate
    /// a real session id every time. Production code routes ownership
    /// through `CompoundContext`.
    fn test_owner(client_id: u64) -> LayoutOwner {
        LayoutOwner {
            client_id,
            session_id: [0u8; 16],
            fsid: 1,
        }
    }

    #[test]
    fn file_ids_are_shard_disjoint_by_construction() {
        // Top byte = shard ordinal, regardless of the random bits.
        assert_eq!(compose_file_id(0, 0xdead_beef, 0x1234) >> 56, 0);
        assert_eq!(compose_file_id(5, 0xdead_beef, 0x1234) >> 56, 5);
        assert_eq!(compose_file_id(255, u64::MAX, 0) >> 56, 255);
        // Ordinals beyond 8 bits wrap into it (chart never renders
        // >255 shards; the mask just keeps the layout invariant).
        assert_eq!(compose_file_id(256 + 3, 1, 2) >> 56, 3);

        // Identical randomness on different shards can never collide.
        assert_ne!(compose_file_id(1, 42, 7), compose_file_id(2, 42, 7));

        // The zero sentinel is never allocated, even for shard 0 with
        // zero randomness.
        assert_eq!(compose_file_id(0, 0, 0), 1);
        // ...and shard-0 ids keep the low-56 randomness intact.
        assert_eq!(compose_file_id(0, 0xab, 0), 0xab);
    }

    // ── F67: durable placement binding + orphan guard ───────────────────

    use crate::pnfs::mds::stub_binding::{test_support::MemoryStubBinding, StubMeta};

    fn f67_manager(
        devices: &[&str],
    ) -> (Arc<MemoryStubBinding>, LayoutManager) {
        let registry = Arc::new(DeviceRegistry::new());
        for id in devices {
            registry
                .register(DeviceInfo::new(
                    id.to_string(),
                    format!("{id}:2049"),
                    vec!["nvme0n1".to_string()],
                ))
                .unwrap();
        }
        let binding = Arc::new(MemoryStubBinding::default());
        let mgr = LayoutManager::new_with_binding(
            registry,
            ConfigLayoutPolicy::Stripe,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
            Arc::clone(&binding) as Arc<dyn crate::pnfs::mds::stub_binding::StubBinding>,
        );
        (binding, mgr)
    }

    fn grant(mgr: &LayoutManager, file: &str) -> Result<LayoutState, String> {
        mgr.generate_layout(test_owner(1), vec![1], file, 0, 16 * 1024 * 1024, IoMode::ReadWrite)
    }

    #[test]
    fn orphan_guard_refuses_sparse_stub_without_binding() {
        let (binding, mgr) = f67_manager(&["ds-1", "ds-2"]);
        binding
            .metas
            .lock()
            .unwrap()
            .insert("/vol/orphan".into(), StubMeta { len: 4096, blocks: 0 });

        let err = grant(&mgr, "/vol/orphan").unwrap_err();
        assert!(
            err.starts_with(ORPHANED_DATA),
            "refusal must carry the F67 sentinel, got: {err}"
        );
        assert!(mgr.placement_for("/vol/orphan").is_none(), "no mint may leak");
        assert!(
            binding.bindings.lock().unwrap().is_empty(),
            "no binding may be written for a refused grant"
        );
    }

    #[test]
    fn orphan_guard_refuses_dense_stub_too() {
        // A dense stub is an MDS-native file. Minting a placement for
        // it would point layouts at absent stripes — zeros — while the
        // real data sits in the stub. The refusal maps to
        // LAYOUTUNAVAILABLE and the client falls back to MDS I/O,
        // where the disposition SERVES dense stubs.
        let (binding, mgr) = f67_manager(&["ds-1"]);
        binding
            .metas
            .lock()
            .unwrap()
            .insert("/vol/native".into(), StubMeta { len: 4096, blocks: 8 });

        let err = grant(&mgr, "/vol/native").unwrap_err();
        assert!(err.starts_with(ORPHANED_DATA), "got: {err}");
    }

    #[test]
    fn mint_proceeds_for_absent_and_empty_stubs_and_writes_the_binding() {
        let (binding, mgr) = f67_manager(&["ds-1", "ds-2"]);
        // Absent stub (freshly created file, stub not visible yet).
        grant(&mgr, "/vol/new").unwrap();
        // Empty stub (OPEN created it, nothing written).
        binding
            .metas
            .lock()
            .unwrap()
            .insert("/vol/empty".into(), StubMeta { len: 0, blocks: 0 });
        grant(&mgr, "/vol/empty").unwrap();

        let bindings = binding.bindings.lock().unwrap();
        for key in ["/vol/new", "/vol/empty"] {
            let placement = mgr.placement_for(key).expect("minted");
            assert_eq!(
                bindings.get(key),
                Some(&placement),
                "stub binding must mirror the map exactly for '{key}'"
            );
            assert_ne!(placement.file_id, 0);
        }
    }

    #[test]
    fn recovery_from_stub_binding_restores_the_exact_identity() {
        let (binding, mgr) = f67_manager(&["ds-1", "ds-2"]);
        // The stub remembers a binding the backend lost. Device order
        // deliberately DIFFERS from the registry's sorted order — the
        // recovered identity must win verbatim.
        let remembered = FilePlacement {
            stripe_size: 4 * 1024 * 1024,
            device_ids: vec!["ds-2".into(), "ds-1".into()],
            file_id: 0x00b97e4be38c246d,
        };
        binding
            .metas
            .lock()
            .unwrap()
            .insert("/vol/f".into(), StubMeta { len: 1 << 30, blocks: 0 });
        binding
            .bindings
            .lock()
            .unwrap()
            .insert("/vol/f".into(), remembered.clone());

        grant(&mgr, "/vol/f").unwrap();
        assert_eq!(
            mgr.placement_for("/vol/f"),
            Some(remembered),
            "recovery must restore file_id, stripe_size AND device order verbatim"
        );
    }

    #[test]
    fn failed_binding_write_refuses_the_grant() {
        let (binding, mgr) = f67_manager(&["ds-1"]);
        binding
            .fail_writes
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let err = grant(&mgr, "/vol/f").unwrap_err();
        assert!(err.contains("unbound"), "got: {err}");
        assert!(
            mgr.placement_for("/vol/f").is_none(),
            "an unbindable placement must never enter the map"
        );
    }

    #[test]
    fn backfill_binds_restored_placements_once() {
        let (binding, mgr) = f67_manager(&["ds-1", "ds-2"]);
        mgr.load_placement_records(vec![crate::state_backend::PlacementRecord {
            file_key: "/vol/old".into(),
            stripe_size: 8 * 1024 * 1024,
            device_ids: vec!["ds-1".into(), "ds-2".into()],
            file_id: 0x77,
            truncate_pending: None,
            truncate_since_unix: None,
        }]);
        // Stub exists → backfill binds it. A second pass is a no-op.
        binding
            .metas
            .lock()
            .unwrap()
            .insert("/vol/old".into(), StubMeta { len: 1024, blocks: 0 });
        assert_eq!(mgr.backfill_stub_bindings(), (1, 0));
        assert_eq!(mgr.backfill_stub_bindings(), (0, 0), "idempotent");
        assert_eq!(
            binding.bindings.lock().unwrap().get("/vol/old").map(|p| p.file_id),
            Some(0x77)
        );
    }

    #[test]
    fn test_layout_generation_single_device() {
        let registry = Arc::new(DeviceRegistry::new());
        let device = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );
        registry.register(device).unwrap();

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        );

        let layout = manager
            .generate_layout(
                test_owner(1),
                vec![0, 1, 2, 3],
                "file-a",
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        assert!(!layout.segments.is_empty());
        assert_eq!(layout.iomode, IoMode::ReadWrite);
    }

    #[test]
    fn test_layout_generation_striped() {
        let registry = Arc::new(DeviceRegistry::new());
        
        // Register 3 devices
        for i in 1..=3 {
            let device = DeviceInfo::new(
                format!("ds-test-{}", i),
                format!("10.0.0.{}:2049", i),
                vec![format!("nvme{}n1", i)],
            );
            registry.register(device).unwrap();
        }

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::Stripe,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        );

        let layout = manager
            .generate_layout(
                test_owner(1),
                vec![0, 1, 2, 3],
                "file-a",
                0,
                24 * 1024 * 1024, // 24 MB across 3 devices
                IoMode::ReadWrite,
            )
            .unwrap();

        // Should have 3 segments (one per device)
        assert_eq!(layout.segments.len(), 3);
    }

    #[test]
    fn test_layout_return() {
        let registry = Arc::new(DeviceRegistry::new());
        let device = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );
        registry.register(device).unwrap();

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        );

        let layout = manager
            .generate_layout(
                test_owner(1),
                vec![0, 1, 2, 3],
                "file-a",
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        let stateid = layout.stateid;
        
        // Return the layout
        assert!(manager.return_layout(&stateid).is_ok());
        
        // Should no longer exist
        assert!(manager.get_layout(&stateid).is_none());
    }

    #[test]
    fn test_layout_recall() {
        let registry = Arc::new(DeviceRegistry::new());

        // Register 2 devices
        for i in 1..=2 {
            let device = DeviceInfo::new(
                format!("ds-test-{}", i),
                format!("10.0.0.{}:2049", i),
                vec![format!("nvme{}n1", i)],
            );
            registry.register(device).unwrap();
        }

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        );

        // Generate layout (will use available devices)
        let layout = manager
            .generate_layout(
                test_owner(1),
                vec![0, 1, 2, 3],
                "file-a",
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        // Find which device was actually used
        let device_used = &layout.segments[0].device_id;

        // Recall layouts for that device. Returns (session_id,
        // stateid) pairs for the CB fan-out path.
        let recalled = manager.recall_layouts_for_device(device_used);

        assert_eq!(recalled.len(), 1, "expected exactly one (sid, stateid) pair");
        // The recall carries the layout's `other` with a BUMPED seqid.
        // This assertion used to demand the stateid unchanged, which is
        // how the missing bump survived: the test pinned the bug (the
        // client answers NFS4ERR_OLD_STATEID to an un-advanced seqid —
        // see `the_ds_death_recall_bumps_the_seqid_too_and_shares_the_counter`).
        assert_eq!(&recalled[0].1[4..], &layout.stateid[4..], "same layout");
        assert_eq!(seqid_of(&recalled[0].1), seqid_of(&layout.stateid) + 1);
        assert_eq!(recalled[0].0, layout.owner.session_id);
    }

    #[test]
    fn test_layout_state_tracking() {
        let registry = Arc::new(DeviceRegistry::new());
        let device = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );
        registry.register(device).unwrap();

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        );

        // Initially no layouts
        assert_eq!(manager.layout_count(), 0);

        // Generate first layout
        let layout1 = manager
            .generate_layout(
                test_owner(1),
                vec![1, 2, 3, 4],
                "file-1",
                0,
                5 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        assert_eq!(manager.layout_count(), 1);

        // Generate second layout
        let layout2 = manager
            .generate_layout(
                test_owner(1),
                vec![5, 6, 7, 8],
                "file-2",
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        assert_eq!(manager.layout_count(), 2);

        // Return first layout
        manager.return_layout(&layout1.stateid).unwrap();
        assert_eq!(manager.layout_count(), 1);

        // Return second layout
        manager.return_layout(&layout2.stateid).unwrap();
        assert_eq!(manager.layout_count(), 0);
    }

    #[test]
    fn test_layout_segments_for_striping() {
        let registry = Arc::new(DeviceRegistry::new());

        // Register 3 devices
        for i in 1..=3 {
            let device = DeviceInfo::new(
                format!("ds-test-{}", i),
                format!("10.0.0.{}:2049", i),
                vec![format!("nvme{}n1", i)],
            );
            registry.register(device).unwrap();
        }

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::Stripe,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        );

        // Request 24 MB (should create 3 segments of 8 MB each)
        let layout = manager
            .generate_layout(
                test_owner(1),
                vec![0, 1, 2, 3],
                "file-a",
                0,
                24 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        // Should have 3 segments (one per device)
        assert_eq!(layout.segments.len(), 3);

        // Each segment should be 8 MB
        for seg in &layout.segments {
            assert_eq!(seg.length, 8 * 1024 * 1024);
        }

        // All segments should use different devices
        let device_ids: Vec<&String> = layout.segments.iter()
            .map(|s| &s.device_id)
            .collect();
        assert_eq!(device_ids.len(), 3);
    }

    #[test]
    fn test_iomode_variants() {
        assert_eq!(IoMode::Read as u32, 1);
        assert_eq!(IoMode::ReadWrite as u32, 2);
        assert_eq!(IoMode::Any as u32, 3);
    }

    #[test]
    fn test_by_owner_index_and_return_all() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(DeviceInfo::new(
            "ds-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        )).unwrap();
        let mgr = LayoutManager::new(registry, ConfigLayoutPolicy::RoundRobin, 8 * 1024 * 1024, crate::state_backend::memory_backend());

        // Two clients each get two layouts.
        let l_a1 = mgr.generate_layout(test_owner(1), vec![1], "f1", 0, 1024, IoMode::ReadWrite).unwrap();
        let l_a2 = mgr.generate_layout(test_owner(1), vec![2], "f2", 0, 1024, IoMode::ReadWrite).unwrap();
        let l_b1 = mgr.generate_layout(test_owner(2), vec![3], "f3", 0, 1024, IoMode::ReadWrite).unwrap();
        let l_b2 = mgr.generate_layout(test_owner(2), vec![4], "f4", 0, 1024, IoMode::ReadWrite).unwrap();

        // layouts_for_client returns the right pair, in the order they were issued.
        assert_eq!(mgr.layouts_for_client(1), vec![l_a1.stateid, l_a2.stateid]);
        assert_eq!(mgr.layouts_for_client(2), vec![l_b1.stateid, l_b2.stateid]);

        // return_all_for_client(1) drops both of client 1's layouts and the
        // by_owner key, but leaves client 2 untouched.
        let dropped = mgr.return_all_for_client(1);
        assert_eq!(dropped.len(), 2);
        assert!(mgr.get_layout(&l_a1.stateid).is_none());
        assert!(mgr.get_layout(&l_a2.stateid).is_none());
        assert!(mgr.layouts_for_client(1).is_empty());
        assert_eq!(mgr.layouts_for_client(2).len(), 2);

        // Idempotent: a second LAYOUTRETURN ALL on the same client is a no-op.
        assert_eq!(mgr.return_all_for_client(1), Vec::<LayoutStateId>::new());
    }

    /// Two-device fixture for the F65 recall tests.
    fn recall_fixture() -> LayoutManager {
        let registry = Arc::new(DeviceRegistry::new());
        for i in 1..=2 {
            registry
                .register(DeviceInfo::new(
                    format!("ds-{}", i),
                    format!("10.0.0.{}:2049", i),
                    vec![format!("nvme{}n1", i)],
                ))
                .unwrap();
        }
        LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        )
    }

    /// F65: the recall selector must key off the SAME string the
    /// truncate gate is filed under. If these two ever drift, the
    /// recall silently matches nothing and looks like it worked.
    #[test]
    fn layout_ident_equals_the_truncate_gate_key() {
        let mgr = recall_fixture();
        let layout = mgr
            .generate_layout(
                test_owner(1),
                vec![0xAB],
                "file-a",
                0,
                8 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        let placement = mgr.placement_for("file-a").expect("pinned on first grant");
        assert_eq!(layout.file_ident, truncate_gate_key(&placement, "file-a"));
        // Identity pins key by file_id, so the ident survives a RENAME.
        assert!(layout.file_ident.starts_with("id:"));
    }

    /// A truncate of one file must not disturb layouts on another.
    /// The dead-DS fan-out sends a session-wide recall (empty FH) on
    /// purpose; doing that here would drop the client's layouts for
    /// every other file on every SETATTR(size).
    #[test]
    fn recall_for_file_selects_only_that_file() {
        let mgr = recall_fixture();
        let a = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let b = mgr
            .generate_layout(test_owner(1), vec![0xB], "file-b", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        assert_ne!(a.file_ident, b.file_ident);

        let hits = mgr.recall_layouts_for_file(&a.file_ident);
        assert_eq!(hits.len(), 1, "exactly one layout is for file-a");
        // The recall carries a BUMPED seqid (RFC 8881 §12.5.3), so compare
        // identity, not raw bytes.
        assert_eq!(state_key(&hits[0].1), state_key(&a.stateid));
        assert_eq!(seqid_of(&hits[0].1), seqid_of(&a.stateid) + 1);
        // The filehandle rides along — it is what makes the
        // CB_LAYOUTRECALL per-file rather than session-wide.
        assert_eq!(hits[0].2, vec![0xA]);

        // file-b is untouched and still recallable in its own right.
        assert_eq!(mgr.recall_layouts_for_file(&b.file_ident).len(), 1);
    }

    /// Every layout for the file, across clients and sessions — a
    /// truncate has to reach all of them, not just the requester's.
    #[test]
    fn recall_for_file_spans_clients() {
        let mgr = recall_fixture();
        let one = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let two = mgr
            .generate_layout(test_owner(2), vec![0xA], "file-a", 0, 1 << 20, IoMode::Read)
            .unwrap();

        let hits = mgr.recall_layouts_for_file(&one.file_ident);
        assert_eq!(hits.len(), 2);
        let ids: Vec<_> = hits.iter().map(|(_, s, _)| state_key(s)).collect();
        assert!(ids.contains(&state_key(&one.stateid)) && ids.contains(&state_key(&two.stateid)));
    }

    /// An empty ident must never behave as a wildcard. Layouts restored
    /// from a pre-v7 record carry no file identity; matching them would
    /// recall every such layout on every truncate of any file.
    #[test]
    fn empty_ident_never_matches() {
        let mgr = recall_fixture();
        let live = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();

        // A layout as it would come back from a pre-v7 row.
        let legacy = LayoutState {
            stateid: [0x5A; 16],
            owner: test_owner(9),
            filehandle: vec![0xEE],
            file_ident: String::new(),
            segments: live.segments.clone(),
            iomode: IoMode::ReadWrite,
            return_on_close: true,
        };
        mgr.layouts.insert(legacy.stateid, legacy);

        // Asking with an empty ident matches nothing at all...
        assert!(mgr.recall_layouts_for_file("").is_empty());
        // ...and the identity-less layout is not swept up by a real one.
        let hits = mgr.recall_layouts_for_file(&live.file_ident);
        assert_eq!(hits.len(), 1);
        assert_eq!(state_key(&hits[0].1), state_key(&live.stateid));
    }

    /// AUDIT R3. A client that answers a recall by doing the RFC-defined
    /// thing — LAYOUTRETURN of the recalled stateid — must not be told
    /// the layout is bad. Layouts are `return_on_close`, so Linux
    /// compounds that return into CLOSE and a failed op aborts the
    /// compound, leaking the open behind it.
    #[test]
    fn layoutreturn_of_a_stateid_we_revoked_is_ok() {
        let mgr = recall_fixture();
        let l = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let recalls = mgr.recall_layouts_for_file(&l.file_ident);
        let recalled_stateid = recalls[0].1;
        assert!(mgr.revoke_layout(&recalled_stateid));

        // The client now hands it back, carrying the bumped seqid.
        assert!(
            mgr.return_layout(&recalled_stateid).is_ok(),
            "a cooperative post-recall LAYOUTRETURN is success, not BAD_STATEID",
        );
        // Idempotent only once — the tombstone is consumed, and a second
        // return of a stateid we never issued is still an error.
        assert!(mgr.return_layout(&recalled_stateid).is_err());
    }

    /// AUDIT R4. The gate is derived state that has to outlive the
    /// process: a restart during a PARKED truncate otherwise comes back
    /// with the stub at the new size, the DSes holding the old bytes,
    /// and nothing gating LAYOUTGET.
    #[test]
    fn truncate_gate_survives_a_restart() {
        let mgr = recall_fixture();
        let l = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let gate = l.file_ident.clone();
        mgr.mark_truncate_dirty(&gate, 4096);

        // What the backend would hand back on the next boot.
        let mut rec = mgr.placement_for("file-a").unwrap().to_record("file-a");
        rec.truncate_pending = Some(4096);

        let restarted = recall_fixture();
        assert!(restarted.truncate_dirty_since(&gate).is_none());
        restarted.load_placement_records(vec![rec]);
        assert!(
            restarted.truncate_dirty_since(&gate).is_some(),
            "the gate must come back armed",
        );
        assert_eq!(restarted.truncate_dirty_state(&gate).map(|(_, m)| m), Some(4096));

        // And it is re-armable work, not a wedge: the parked list names
        // the file so the retry can be spawned again.
        let parked = restarted.parked_truncates();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].0, gate);
        assert_eq!(parked[0].1, "file-a");
        assert_eq!(parked[0].3, 4096);

        // A placement with no pending cut restores clean.
        let clean = recall_fixture();
        clean.load_placement_records(vec![mgr.placement_for("file-a").unwrap().to_record("file-a")]);
        assert!(clean.truncate_dirty_since(&gate).is_none());
        assert!(clean.parked_truncates().is_empty());
    }

    /// The gate's AGE has to survive the restart too, not just the gate.
    ///
    /// `fallback_delay_ceiling` fails a client's MDS-fallback I/O once
    /// the gate has been dirty past the ceiling — that is the only thing
    /// bounding the wait. The age lived in a process-local `Instant`, so
    /// every restart re-stamped it to now() and re-armed the ceiling: an
    /// MDS that bounces more often than the ceiling could DELAY a
    /// fallback client forever, which is exactly the livelock the
    /// ceiling exists to prevent.
    #[test]
    fn truncate_gate_age_survives_a_restart() {
        let mgr = recall_fixture();
        mgr.generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let gate = truncate_gate_key(&mgr.placement_for("file-a").unwrap(), "file-a");
        mgr.mark_truncate_dirty(&gate, 4096);

        // The record as the backend would hand it back one hour later.
        let mut rec = mgr.placement_for("file-a").unwrap().to_record("file-a");
        rec.truncate_pending = Some(4096);
        rec.truncate_since_unix = Some(unix_now() - 3600);

        let restarted = recall_fixture();
        restarted.load_placement_records(vec![rec.clone()]);

        let age = restarted.truncate_dirty_age(&gate).expect("gate re-armed");
        assert!(
            age >= Duration::from_secs(3600),
            "the gate was armed an hour before this process started; \
             age came back as {}s — the ceiling has been re-armed by the restart",
            age.as_secs(),
        );

        // The pre-upgrade row: no stamp recorded. It must come back
        // armed and simply look freshly armed — the behaviour before
        // this column existed — rather than fail to restore.
        let mut legacy = rec.clone();
        legacy.truncate_since_unix = None;
        let fresh = recall_fixture();
        fresh.load_placement_records(vec![legacy]);
        let age = fresh.truncate_dirty_age(&gate).expect("gate re-armed without a stamp");
        assert!(age < Duration::from_secs(60), "an unknown stamp means newly armed");

        // A backwards wall-clock jump — a stamp in the FUTURE — must not
        // underflow into a huge age and fail-fast every client. Saturating
        // subtraction makes it look newly armed, which delays the
        // fail-fast rather than triggering it early: the safe direction.
        let mut skewed = rec.clone();
        skewed.truncate_since_unix = Some(unix_now() + 86_400);
        let skew = recall_fixture();
        skew.load_placement_records(vec![skewed]);
        let age = skew.truncate_dirty_age(&gate).expect("gate re-armed under clock skew");
        assert!(age < Duration::from_secs(60), "a future stamp must not underflow");
    }

    /// Re-marking an already-dirty gate keeps the OLDEST stamp, in the
    /// persisted form as well as in memory. If the persisted stamp were
    /// refreshed on every mark, a file being repeatedly truncated would
    /// hold the ceiling open indefinitely — the same unbounded wait as
    /// the restart bug, reached without a restart.
    #[tokio::test]
    async fn remarking_a_dirty_gate_does_not_refresh_the_persisted_stamp() {
        // Reads the PERSISTED record, not the in-memory mark. An earlier
        // version of this test asserted on `truncate_dirty_age` and was
        // therefore unable to fail: `and_modify` never touches `since`,
        // so the in-memory age is correct no matter what gets written
        // down. The mutation run caught it — a persisted-stamp refresh
        // sailed through green. The bug it must catch is a file
        // truncated repeatedly holding the ceiling open forever, which
        // is the restart bug reached without a restart.
        let backend = crate::state_backend::memory_backend();
        let registry = Arc::new(DeviceRegistry::new());
        for i in 1..=2 {
            registry
                .register(DeviceInfo::new(
                    format!("ds-{}", i),
                    format!("10.0.0.{}:2049", i),
                    vec![format!("nvme{}n1", i)],
                ))
                .unwrap();
        }
        let mgr = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
            backend.clone(),
        );
        mgr.generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let placement = mgr.placement_for("file-a").unwrap();
        let gate = truncate_gate_key(&placement, "file-a");

        // Arm it as if an hour ago.
        let armed_at = unix_now() - 3600;
        let mut rec = placement.to_record("file-a");
        rec.truncate_pending = Some(4096);
        rec.truncate_since_unix = Some(armed_at);
        mgr.load_placement_records(vec![rec]);

        // A second, deeper truncate arrives on the same still-dirty gate.
        mgr.mark_truncate_dirty(&gate, 2048);

        let persisted = backend
            .get_placement("file-a")
            .await
            .unwrap()
            .expect("placement persisted");
        let stamp = persisted.truncate_since_unix.expect("stamp persisted");
        assert!(
            stamp <= armed_at + 5,
            "re-marking refreshed the persisted stamp: armed at {}, written back as {} \
             ({}s younger) — the ceiling would never be reached by a file that keeps \
             being truncated",
            armed_at,
            stamp,
            stamp.saturating_sub(armed_at),
        );
        assert_eq!(
            persisted.truncate_pending,
            Some(2048),
            "the deeper cut is still what gets written down",
        );
        assert!(
            mgr.truncate_dirty_age(&gate).unwrap() >= Duration::from_secs(3600),
            "the in-memory age must carry the inherited hour too",
        );
    }

    /// AUDIT C1. The minted seqid must be a known non-zero constant, not
    /// random: RFC 8881 §12.5.5.2.1 has the client check that a recall's
    /// seqid is exactly one higher than the one it holds, and §20.3.3
    /// forbids zero. Randomising all sixteen bytes made both impossible.
    #[test]
    fn minted_stateid_has_a_known_nonzero_seqid() {
        let mgr = recall_fixture();
        let l = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(seqid_of(&l.stateid), LAYOUT_SEQID_BASE);
        assert_ne!(LAYOUT_SEQID_BASE, 0, "RFC 8881 §20.3.3 forbids a zero seqid");
        // and the identity half is still random
        let m = mgr
            .generate_layout(test_owner(1), vec![0xB], "file-b", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        assert_ne!(l.stateid[4..16], m.stateid[4..16]);
    }

    /// AUDIT C1. Each recall must advance the seqid, and the layout must
    /// still be findable afterwards — the reason the seqid was frozen is
    /// that `layouts` was keyed by the whole 16 bytes, so bumping it used
    /// to lose the entry.
    #[test]
    fn recall_advances_the_seqid_and_the_layout_stays_addressable() {
        let mgr = recall_fixture();
        let l = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let minted = seqid_of(&l.stateid);

        let first = mgr.recall_layouts_for_file(&l.file_ident);
        assert_eq!(first.len(), 1);
        assert_eq!(seqid_of(&first[0].1), minted + 1, "recall must send seqid+1");

        let second = mgr.recall_layouts_for_file(&l.file_ident);
        assert_eq!(seqid_of(&second[0].1), minted + 2, "each recall advances again");

        // The client will come back with the BUMPED stateid. Both that and
        // the original must still resolve to this layout.
        assert!(mgr.get_layout(&second[0].1).is_some(), "bumped stateid resolves");
        assert!(mgr.get_layout(&l.stateid).is_some(), "original stateid resolves");
        assert!(mgr.return_layout(&second[0].1).is_ok(), "LAYOUTRETURN with the bumped seqid");
        assert!(mgr.get_layout(&l.stateid).is_none());
        assert!(mgr.layouts_for_client(1).is_empty(), "by_owner cleaned up too");
    }

    /// AUDIT C1, THE OTHER HALF — rig-found on 2026-08-11, months after
    /// the truncate path was fixed.
    ///
    /// `recall_layouts_for_device` (the DS-death path) handed out the
    /// STORED stateid, unbumped, so the client's
    /// `pnfs_check_callback_stateid` saw `newseq <= oldseq` and answered
    /// NFS4ERR_OLD_STATEID without draining anything. It stayed
    /// invisible because a refused recall is answered by forced
    /// server-side revocation, which from the outside looks exactly like
    /// a recall that worked. Both paths now share one bump, and this
    /// test covers the device path specifically — plus the property that
    /// makes twin paths safe: they advance the SAME counter, so
    /// alternating them still yields strictly increasing seqids.
    #[test]
    fn the_ds_death_recall_bumps_the_seqid_too_and_shares_the_counter() {
        let mgr = recall_fixture();
        let l = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let minted = seqid_of(&l.stateid);
        let device = l.segments[0].device_id.clone();

        let first = mgr.recall_layouts_for_device(&device);
        assert_eq!(first.len(), 1, "the layout uses that device");
        assert_eq!(
            seqid_of(&first[0].1),
            minted + 1,
            "a DS-death recall must send seqid+1, or the client answers OLD_STATEID"
        );
        assert_eq!(
            &first[0].1[4..],
            &l.stateid[4..],
            "only the seqid moves — `other` identifies the layout"
        );

        // Alternate the paths: one shared counter, strictly increasing.
        let second = mgr.recall_layouts_for_file(&l.file_ident);
        assert_eq!(seqid_of(&second[0].1), minted + 2);
        let third = mgr.recall_layouts_for_device(&device);
        assert_eq!(seqid_of(&third[0].1), minted + 3);

        // And the layout is still addressable by both spellings.
        assert!(mgr.get_layout(&third[0].1).is_some(), "bumped stateid resolves");
        assert!(mgr.get_layout(&l.stateid).is_some(), "original stateid resolves");
    }

    /// AUDIT C6. A grant that passes layoutget's gate check and then has
    /// the mark arm under it must not survive publication — otherwise it
    /// escapes the gate (already checked) AND the recall (its snapshot
    /// ran before the insert).
    #[test]
    fn grant_that_races_an_arming_truncate_is_refused() {
        let mgr = recall_fixture();
        // Pin the placement first so the gate key is computable, then arm
        // the mark: this is the state layoutget finds itself in after its
        // own check passed.
        let first = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();
        let gate = first.file_ident.clone();
        mgr.return_layout(&first.stateid).unwrap();
        mgr.mark_truncate_dirty(&gate, 0);

        let err = mgr
            .generate_layout(test_owner(2), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .expect_err("a grant published under an armed mark must be refused");
        assert_eq!(err, GRANT_RACED_TRUNCATE);
        // And nothing was left behind for a later recall to miss.
        assert!(mgr.recall_layouts_for_file(&gate).is_empty());
        assert_eq!(mgr.layout_count(), 0);
        assert!(mgr.layouts_for_client(2).is_empty());

        // Once the cut confirms, grants resume.
        assert!(mgr.clear_truncate_dirty_if(&gate, 0));
        assert!(mgr
            .generate_layout(test_owner(2), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .is_ok());
    }

    /// The ident survives the persistence round-trip — otherwise every
    /// layout restored after an MDS restart becomes unrecallable and
    /// F65 quietly reopens.
    #[test]
    fn file_ident_survives_the_record_round_trip() {
        let mgr = recall_fixture();
        let layout = mgr
            .generate_layout(test_owner(1), vec![0xA], "file-a", 0, 1 << 20, IoMode::ReadWrite)
            .unwrap();

        let restored = LayoutState::from_record(layout.to_record());
        assert_eq!(restored.file_ident, layout.file_ident);
        assert!(!restored.file_ident.is_empty());

        // And a restored layout is still selectable by that ident.
        let fresh = recall_fixture();
        fresh.load_records(vec![restored.to_record()]);
        assert_eq!(fresh.recall_layouts_for_file(&layout.file_ident).len(), 1);
    }

    #[test]
    fn test_return_fsid_filters_by_fsid() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(DeviceInfo::new(
            "ds-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        )).unwrap();
        let mgr = LayoutManager::new(registry, ConfigLayoutPolicy::RoundRobin, 8 * 1024 * 1024, crate::state_backend::memory_backend());

        // Same client holds layouts in two filesystems; LAYOUTRETURN FSID
        // should release only the one matching the filter.
        let owner_fs1 = LayoutOwner { client_id: 7, session_id: [0; 16], fsid: 100 };
        let owner_fs2 = LayoutOwner { client_id: 7, session_id: [0; 16], fsid: 200 };
        let l_in_fs1 = mgr.generate_layout(owner_fs1, vec![1], "f1", 0, 1024, IoMode::Read).unwrap();
        let l_in_fs2 = mgr.generate_layout(owner_fs2, vec![2], "f2", 0, 1024, IoMode::Read).unwrap();

        let dropped = mgr.return_fsid_for_client(7, 100);
        assert_eq!(dropped, vec![l_in_fs1.stateid]);
        assert!(mgr.get_layout(&l_in_fs1.stateid).is_none());
        assert!(mgr.get_layout(&l_in_fs2.stateid).is_some());
        assert_eq!(mgr.layouts_for_client(7), vec![l_in_fs2.stateid]);
    }

    #[test]
    fn test_layout_type_values() {
        // RFC 8881 §3.3.13 layouttype4. This test previously pinned the
        // OSD2/BLOCK values SWAPPED — it asserted the bug. Checked
        // against the RFC, not against the enum, on 2026-08-09.
        assert_eq!(LayoutType::NfsV4_1Files as u32, 1);
        assert_eq!(LayoutType::Osd2Objects as u32, 2);
        assert_eq!(LayoutType::BlockVolume as u32, 3);
        assert_eq!(LayoutType::FlexFiles as u32, 4);
    }

    /// Phase A.5: server-side forcible revocation. Same end-state
    /// as `return_layout` (gone from primary + by_owner index)
    /// but idempotent on a second call. The dispatcher's
    /// LAYOUTRETURN/LAYOUTGET arms see "not found" on a revoked
    /// stateid and surface NFS4ERR_BAD_STATEID — no separate
    /// tombstone is needed.
    #[test]
    fn test_revoke_layout_idempotent_and_clears_indexes() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(DeviceInfo::new(
            "ds-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        )).unwrap();
        let mgr = LayoutManager::new(registry, ConfigLayoutPolicy::RoundRobin, 8 * 1024 * 1024, crate::state_backend::memory_backend());

        let layout = mgr
            .generate_layout(test_owner(42), vec![1], "f1", 0, 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.layout_count(), 1);
        assert_eq!(mgr.layouts_for_client(42), vec![layout.stateid]);

        // First revoke removes the layout and reports true.
        assert!(mgr.revoke_layout(&layout.stateid));
        assert_eq!(mgr.layout_count(), 0);
        assert!(mgr.get_layout(&layout.stateid).is_none());
        // by_owner index is cleared (no empty entries left behind).
        assert!(mgr.layouts_for_client(42).is_empty());

        // Second revoke is a no-op — important because the recall-
        // deadline timer races with client LAYOUTRETURN: both must
        // be safe to invoke.
        assert!(!mgr.revoke_layout(&layout.stateid));
    }

    /// Multi-client safety: revoking client A's layout doesn't
    /// touch client B's layouts on the same device.
    #[test]
    fn test_revoke_layout_isolates_per_client() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(DeviceInfo::new(
            "ds-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        )).unwrap();
        let mgr = LayoutManager::new(registry, ConfigLayoutPolicy::RoundRobin, 8 * 1024 * 1024, crate::state_backend::memory_backend());

        let l_a = mgr.generate_layout(test_owner(1), vec![1], "f1", 0, 1024, IoMode::ReadWrite).unwrap();
        let l_b = mgr.generate_layout(test_owner(2), vec![2], "f2", 0, 1024, IoMode::ReadWrite).unwrap();
        assert_eq!(mgr.layout_count(), 2);

        assert!(mgr.revoke_layout(&l_a.stateid));
        assert!(mgr.get_layout(&l_a.stateid).is_none());
        assert!(mgr.get_layout(&l_b.stateid).is_some());
        assert!(mgr.layouts_for_client(1).is_empty());
        assert_eq!(mgr.layouts_for_client(2), vec![l_b.stateid]);
    }

    /// The CB_NOTIFY_DEVICEID address book: who gets told when a block
    /// volume's device changes. Three properties that decide whether a
    /// notification lands at all —
    ///  - a client that asked for NOTHING is not recorded (telling it
    ///    is impossible, and a phantom entry would make the send path
    ///    report reachable clients that are not);
    ///  - the newest mask wins (a client may re-fetch with different
    ///    notify types);
    ///  - DeleteVolume forgets the book, because the deviceid is
    ///    derived from the volume NAME and a re-created volume would
    ///    otherwise inherit the dead one's subscribers.
    #[tokio::test]
    async fn device_notify_address_book_records_filters_and_forgets() {
        // Against SQLITE, not the memory backend: the book is durable
        // now, and the memory backend deliberately no-ops it (a
        // block-class volume cannot exist there). A memory-backed
        // assertion would pass on an implementation that stored nothing.
        let registry = Arc::new(DeviceRegistry::new());
        let sq = Arc::new(crate::state_backend::SqliteBackend::open_in_memory().unwrap());
        let backend: Arc<dyn crate::state_backend::StateBackend> = Arc::clone(&sq) as _;
        let mgr = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            1 << 20,
            Arc::clone(&backend),
        );

        mgr.note_device_fetch("vol", 11, 0x6);
        mgr.note_device_fetch("vol", 12, 0); // asked for nothing
        mgr.note_device_fetch("vol", 0, 0x6); // no client id resolved
        sq.flush().await.unwrap();
        assert_eq!(
            backend.device_notify_list("vol").await.unwrap(),
            vec![(11, 0x6)],
            "only a client that asked for notifications is recorded"
        );

        mgr.note_device_fetch("vol", 11, 0x2);
        sq.flush().await.unwrap();
        assert_eq!(
            backend.device_notify_list("vol").await.unwrap(),
            vec![(11, 0x2)],
            "the newest mask wins"
        );

        // Another volume is a separate book (the deviceid derives from
        // the volume name).
        mgr.note_device_fetch("vol2", 11, 0x6);
        mgr.forget_device_notify("vol");
        sq.flush().await.unwrap();
        assert!(backend.device_notify_list("vol").await.unwrap().is_empty());
        assert_eq!(backend.device_notify_list("vol2").await.unwrap(), vec![(11, 0x6)]);

        // ...and one client can be dropped alone (its lease expired).
        mgr.note_device_fetch("vol2", 12, 0x6);
        mgr.forget_device_notify_client("vol2", 11);
        sq.flush().await.unwrap();
        assert_eq!(backend.device_notify_list("vol2").await.unwrap(), vec![(12, 0x6)]);
    }

    /// THE REGRESSION FOR THE MEASURED BUG (`EXPAND=1 MDS_BOUNCE=1`):
    /// the book must survive the MDS process. A second LayoutManager
    /// over the same backend is what a restart looks like from here —
    /// and before this was durable it came up empty, so the expand that
    /// followed notified nobody and the application got EIO on a volume
    /// that had the space.
    #[tokio::test]
    async fn the_notify_book_survives_a_new_layout_manager_over_the_same_state() {
        let sq = Arc::new(crate::state_backend::SqliteBackend::open_in_memory().unwrap());
        let backend: Arc<dyn crate::state_backend::StateBackend> = Arc::clone(&sq) as _;
        {
            let mgr = LayoutManager::new(
                Arc::new(DeviceRegistry::new()),
                ConfigLayoutPolicy::RoundRobin,
                1 << 20,
                Arc::clone(&backend),
            );
            mgr.note_device_fetch("vol", 42, 0x6);
            sq.flush().await.unwrap();
        }
        let restarted = LayoutManager::new(
            Arc::new(DeviceRegistry::new()),
            ConfigLayoutPolicy::RoundRobin,
            1 << 20,
            Arc::clone(&backend),
        );
        assert_eq!(
            backend.device_notify_list("vol").await.unwrap(),
            vec![(42, 0x6)],
            "the restarted MDS must still know who cached this device"
        );
        // No callback channel attached, so nothing is sent — but the
        // target was FOUND, which is the half that used to be lost.
        assert_eq!(restarted.notify_device_changed("vol").await, (0, 0));
    }

    /// With no callback channel attached — every unit-test shape, and a
    /// live MDS between construction and wiring — notification is a
    /// silent no-op, never a panic and never a failed expand.
    #[tokio::test]
    async fn device_notify_without_a_callback_channel_is_a_noop() {
        let registry = Arc::new(DeviceRegistry::new());
        let mgr = stripe_mgr(&registry, 1 << 20);
        mgr.note_device_fetch("vol", 3, 0x6);
        assert_eq!(mgr.notify_device_changed("vol").await, (0, 0));
    }

    // ── Phase 0: per-file placement pinning ──────────────────────────
    // (docs/plans/pnfs-durable-ds-plan.md — the stripe map must be a
    // pure function of the pinned placement, never of the live
    // registry's membership or iteration order.)

    fn stripe_mgr(registry: &Arc<DeviceRegistry>, stripe: u64) -> LayoutManager {
        LayoutManager::new(
            Arc::clone(registry),
            ConfigLayoutPolicy::Stripe,
            stripe,
            crate::state_backend::memory_backend(),
        )
    }

    fn stripe_mgr_on(
        registry: &Arc<DeviceRegistry>,
        stripe: u64,
        backend: Arc<dyn StateBackend>,
    ) -> LayoutManager {
        LayoutManager::new(Arc::clone(registry), ConfigLayoutPolicy::Stripe, stripe, backend)
    }

    fn ds(id: &str) -> DeviceInfo {
        DeviceInfo::new(id.to_string(), format!("{}:2049", id), vec![])
    }

    /// F66's load-bearing test: the fallback proxy must address the
    /// same bytes to the same DS the CLIENT's layout does. The client
    /// contract is the WIRE (RFC 8881 §13.4.4): unit `u` maps to
    /// device `(u + nfl_first_stripe_index) % N` over the ds_addr
    /// pattern, and flint encodes `nfl_first_stripe_index =
    /// wire_first_stripe_index(file_id, N)` (the dispatcher calls the
    /// same function `slot_for_offset` does — this test pins the
    /// composition). The first version of this test compared against
    /// `generate_stripe_layout` — flint-INTERNAL bookkeeping that does
    /// not rotate — and passed while the fsx gate caught a proxied
    /// write on the wrong stripe file: file_id 0x…246d is ODD, the
    /// client's unit 0 lived on device 1, the unrotated proxy wrote
    /// device 0, and the client's next read found an absent stripe ⇒
    /// zero bytes. Even file_ids agreed by coincidence — a formula
    /// divergence disguised as 50% flakiness. The reference here is
    /// the wire model, and the odd/even file_id axis is the
    /// regression.
    #[test]
    fn proxy_slot_mapping_matches_the_wire_contract() {
        for width in 1..=5usize {
            let stripe = 1024 * 1024u64;
            // Both parities of file_id — the axis the fsx gate caught.
            for file_id in [2u64, 7, 0x00b9_7e4b_e38c_246d, 0x1000] {
                let placement = FilePlacement {
                    stripe_size: stripe,
                    device_ids: (0..width).map(|i| format!("ds-{}", i)).collect(),
                    file_id,
                };
                let fsi = FilePlacement::wire_first_stripe_index(file_id, width) as usize;
                assert_eq!(fsi, (file_id as usize) % width, "the wire formula itself");
                for (off, len) in [
                    (0u64, 16 * stripe),
                    (stripe / 2, 5 * stripe),
                    (3 * stripe + 17, 2 * stripe),
                    (11 * stripe - 1, 3),
                ] {
                    // The client's mapping, straight from the RFC.
                    for probe in [off, off + len / 2, off + len - 1] {
                        let unit = (probe / stripe) as usize;
                        let client_slot = (unit + fsi) % width;
                        assert_eq!(
                            placement.slot_for_offset(probe),
                            client_slot,
                            "width={} file_id={:#x} offset={} — proxy diverges from the wire",
                            width, file_id, probe
                        );
                    }
                    // Chunking must tile the range, each chunk in one slot.
                    let chunks = placement.split_at_stripe_bounds(off, len);
                    assert_eq!(chunks.iter().map(|(_, l)| l).sum::<u64>(), len);
                    let mut cursor = off;
                    for (co, cl) in &chunks {
                        assert_eq!(*co, cursor, "chunks must be contiguous");
                        assert!(*cl > 0);
                        assert_eq!(
                            placement.slot_for_offset(*co),
                            placement.slot_for_offset(co + cl - 1),
                            "a chunk must not straddle two slots"
                        );
                        cursor += cl;
                    }
                }
            }
        }
    }

    /// The exact fsx-gate failure, as a unit test: width 2, the odd
    /// file_id from the failing run. Unit 0 belongs to device 1 — an
    /// unrotated mapping says device 0 and corrupts.
    #[test]
    fn odd_file_id_rotates_unit_zero_off_device_zero() {
        let placement = FilePlacement {
            stripe_size: 8 * 1024 * 1024,
            device_ids: vec!["ds-host-1".into(), "ds-host-2".into()],
            file_id: 0x00b9_7e4b_e38c_246d, // odd — the run-2/3 failure
        };
        assert_eq!(placement.slot_for_offset(0x1f000), 1, "unit 0 → ds-host-2 (.stripe1)");
        assert_eq!(placement.stripe_rel_path(1), "00b97e4be38c246d.stripe1");
    }

    /// Per-volume stripe SIZE overrides the fleet default, and applies
    /// to files under that volume's directory only.
    #[tokio::test]
    async fn a_volume_geometry_overrides_the_default_stripe_size() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.set_volume_geometry(
            "narrow",
            VolumeGeometry { stripe_size: 1024 * 1024, stripe_width: 0, layout_class: LayoutClass::File },
        )
        .await;

        mgr.generate_layout(test_owner(1), vec![1], "narrow/f", 0, 4 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.placement_for("narrow/f").unwrap().stripe_size, 1024 * 1024);

        // A file in an un-configured volume still gets the fleet default.
        mgr.generate_layout(test_owner(1), vec![1], "other/f", 0, 4 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.placement_for("other/f").unwrap().stripe_size, 8 * 1024 * 1024);
    }

    /// Stripe WIDTH narrows the pin. This is the blast-radius control:
    /// a file pinned to 2 of 4 DSes survives the loss of the other two,
    /// where the default all-DS pin makes every file depend on every DS.
    #[tokio::test]
    async fn a_volume_stripe_width_narrows_the_pin() {
        let registry = Arc::new(DeviceRegistry::new());
        for id in ["ds-1", "ds-2", "ds-3", "ds-4"] {
            registry.register(ds(id)).unwrap();
        }
        let mgr = stripe_mgr(&registry, 1024 * 1024);
        mgr.set_volume_geometry("narrow", VolumeGeometry { stripe_size: 0, stripe_width: 2, layout_class: LayoutClass::File }).await;

        mgr.generate_layout(test_owner(1), vec![1], "narrow/f", 0, 4 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        let pinned = mgr.placement_for("narrow/f").unwrap().device_ids;
        assert_eq!(pinned, vec!["ds-1".to_string(), "ds-2".to_string()], "narrowed to the sorted head");

        // Default width still takes the whole fleet.
        mgr.generate_layout(test_owner(1), vec![1], "wide/f", 0, 4 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.placement_for("wide/f").unwrap().device_ids.len(), 4);
    }

    /// A width larger than the fleet is not an error — it means "all of
    /// them", and must not truncate to something surprising or panic.
    #[tokio::test]
    async fn a_stripe_width_wider_than_the_fleet_uses_the_whole_fleet() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();
        let mgr = stripe_mgr(&registry, 1024 * 1024);
        mgr.set_volume_geometry("v", VolumeGeometry { stripe_size: 0, stripe_width: 16, layout_class: LayoutClass::File }).await;
        mgr.generate_layout(test_owner(1), vec![1], "v/f", 0, 2 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.placement_for("v/f").unwrap().device_ids.len(), 2);
    }

    /// Geometry survives an MDS restart. It lives in the state backend
    /// alongside placements, so this is the test that the record
    /// round-trips — without it a restarted MDS would silently fall back
    /// to the fleet default for every NEW file in the volume, leaving one
    /// volume striped two different ways.
    #[tokio::test]
    async fn volume_geometry_survives_a_restart() {
        let backend: Arc<dyn StateBackend> = crate::state_backend::memory_backend();
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();

        let first = stripe_mgr_on(&registry, 8 * 1024 * 1024, Arc::clone(&backend));
        first
            .set_volume_geometry(
                "vol",
                VolumeGeometry { stripe_size: 2 * 1024 * 1024, stripe_width: 1, layout_class: LayoutClass::File },
            )
            .await;

        // A fresh manager over the same backend = the restarted MDS.
        let restarted = stripe_mgr_on(&registry, 8 * 1024 * 1024, Arc::clone(&backend));
        restarted.load_volume_geometry(&["vol".to_string()]).await;
        restarted
            .generate_layout(test_owner(1), vec![1], "vol/f", 0, 4 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        let p = restarted.placement_for("vol/f").unwrap();
        assert_eq!(p.stripe_size, 2 * 1024 * 1024, "stripe size lost across restart");
        assert_eq!(p.device_ids.len(), 1, "stripe width lost across restart");
    }

    /// A volume directory with no geometry record must load as an
    /// explicit negative, so the fleet default is used and the operator
    /// gets one WARN — the only signal separating "never declared" from
    /// "acked, then lost".
    #[tokio::test]
    async fn a_volume_without_a_geometry_record_falls_back_to_the_default() {
        let backend: Arc<dyn StateBackend> = crate::state_backend::memory_backend();
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();

        let mgr = stripe_mgr_on(&registry, 8 * 1024 * 1024, backend);
        mgr.load_volume_geometry(&["legacy".to_string()]).await;
        mgr.generate_layout(test_owner(1), vec![1], "legacy/f", 0, 4 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        let p = mgr.placement_for("legacy/f").unwrap();
        assert_eq!(p.stripe_size, 8 * 1024 * 1024);
        assert_eq!(p.device_ids.len(), 2, "no record ⇒ all data servers");
    }

    /// Deleting a volume must drop its geometry, or a volume re-created
    /// at the same name would inherit the previous StorageClass's
    /// geometry instead of its own.
    #[tokio::test]
    async fn scsi_layout_register_take_round_trip() {
        let backend: Arc<dyn StateBackend> =
            Arc::new(crate::state_backend::SqliteBackend::open_in_memory().unwrap());
        let registry = Arc::new(DeviceRegistry::new());
        let mgr = stripe_mgr_on(&registry, 8 * 1024 * 1024, Arc::clone(&backend));
        let owner = LayoutOwner { client_id: 7, session_id: [1; 16], fsid: 1 };
        let sid = mgr.register_scsi_layout(
            owner,
            vec![0xab],
            "volA/model.bin",
            IoMode::ReadWrite,
        );
        let taken = mgr.take_scsi_layout(&sid).expect("registered handle is takeable");
        assert_eq!(taken.owner.client_id, 7);
        assert_eq!(taken.file_ident, "volA/model.bin", "recall handle keys on the file_key");
        assert!(taken.segments.is_empty(), "extents live in the allocator, not here");
        assert!(
            mgr.take_scsi_layout(&sid).is_none(),
            "second take is the benign BadStateId shape"
        );
    }

    #[tokio::test]
    async fn deleting_a_volume_forgets_its_geometry() {
        let backend: Arc<dyn StateBackend> = crate::state_backend::memory_backend();
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();
        let mgr = stripe_mgr_on(&registry, 8 * 1024 * 1024, Arc::clone(&backend));

        mgr.set_volume_geometry("v", VolumeGeometry { stripe_size: 1024 * 1024, stripe_width: 1, layout_class: LayoutClass::File })
            .await;
        mgr.forget_volume_geometry("v");

        let after = stripe_mgr_on(&registry, 8 * 1024 * 1024, backend);
        after.load_volume_geometry(&["v".to_string()]).await;
        after
            .generate_layout(test_owner(1), vec![1], "v/f", 0, 2 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        let p = after.placement_for("v/f").unwrap();
        assert_eq!(p.stripe_size, 8 * 1024 * 1024, "stale geometry survived delete");
        assert_eq!(p.device_ids.len(), 2);
    }

    /// A zero stripe_size means "use the fleet default" — it must be
    /// resolved at record time, not stored as 0 and later divided by.
    #[tokio::test]
    async fn a_zero_stripe_size_resolves_to_the_fleet_default() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);
        let got = mgr.set_volume_geometry("v", VolumeGeometry { stripe_size: 0, stripe_width: 0, layout_class: LayoutClass::File }).await;
        assert_eq!(got.stripe_size, 8 * 1024 * 1024);
    }

    fn segment_devices(l: &LayoutState) -> Vec<String> {
        l.segments.iter().map(|s| s.device_id.clone()).collect()
    }

    /// The core Phase 0 property: an MDS restart with the registry
    /// re-populated in the OPPOSITE order grants the identical stripe
    /// map, because the placement (not the registry) is the source of
    /// truth. Exercises the full persist → list → load loop.
    #[tokio::test]
    async fn placement_pins_stripe_map_across_restart_and_reorder() {
        let backend: Arc<dyn StateBackend> =
            Arc::new(crate::state_backend::MemoryBackend::new());

        let registry1 = Arc::new(DeviceRegistry::new());
        registry1.register(ds("ds-b")).unwrap();
        registry1.register(ds("ds-a")).unwrap();
        let mgr1 = LayoutManager::new(
            Arc::clone(&registry1),
            ConfigLayoutPolicy::Stripe,
            8 * 1024 * 1024,
            Arc::clone(&backend),
        );

        let l1 = mgr1
            .generate_layout(test_owner(1), vec![1], "f", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        // Pinned placement is sorted regardless of registration order.
        assert_eq!(
            mgr1.placement_for("f").unwrap().device_ids,
            vec!["ds-a".to_string(), "ds-b".to_string()]
        );

        // spawn_persist is fire-and-forget; wait (bounded) for the
        // record to land before simulating the restart.
        let mut records = Vec::new();
        for _ in 0..200 {
            records = backend.list_placements().await.unwrap();
            if !records.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(records.len(), 1, "placement was never persisted");

        // "Restart": fresh registry populated in REVERSE order, fresh
        // manager, placements loaded from the backend.
        let registry2 = Arc::new(DeviceRegistry::new());
        registry2.register(ds("ds-a")).unwrap();
        registry2.register(ds("ds-b")).unwrap();
        let mgr2 = LayoutManager::new(
            Arc::clone(&registry2),
            ConfigLayoutPolicy::Stripe,
            8 * 1024 * 1024,
            Arc::clone(&backend),
        );
        mgr2.load_placement_records(records);

        let l2 = mgr2
            .generate_layout(test_owner(1), vec![1], "f", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(
            segment_devices(&l1),
            segment_devices(&l2),
            "stripe map re-mapped across restart/reorder — Phase 0 P1 regression"
        );
    }

    /// Registering a new DS must not re-map files striped before it
    /// joined; only new files see the wider fleet.
    #[test]
    fn placement_survives_fleet_growth() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        let before = mgr
            .generate_layout(test_owner(1), vec![1], "old-file", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();

        registry.register(ds("ds-3")).unwrap();

        let after = mgr
            .generate_layout(test_owner(1), vec![1], "old-file", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(segment_devices(&before), segment_devices(&after));
        assert_eq!(
            mgr.placement_for("old-file").unwrap().device_ids.len(),
            2,
            "pre-growth file's placement must stay 2-wide"
        );

        let fresh = mgr
            .generate_layout(test_owner(1), vec![2], "new-file", 0, 24 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.placement_for("new-file").unwrap().device_ids.len(), 3);
        assert!(segment_devices(&fresh).contains(&"ds-3".to_string()));
    }

    /// A file whose pinned DS is gone gets a REFUSAL, not a silently
    /// re-mapped layout over the survivors.
    #[test]
    fn placement_refuses_when_pinned_device_missing() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "f", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();

        registry.unregister("ds-2").unwrap();

        let err = mgr
            .generate_layout(test_owner(1), vec![1], "f", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap_err();
        assert!(
            err.contains("not active"),
            "expected refusal mentioning the missing device, got: {}",
            err
        );

        // A NEW file pins the surviving fleet fine.
        mgr.generate_layout(test_owner(1), vec![2], "g", 0, 8 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(
            mgr.placement_for("g").unwrap().device_ids,
            vec!["ds-1".to_string()]
        );
    }

    /// Stripe size is pinned per file: a config change affects new
    /// files only.
    #[test]
    fn stripe_size_pinned_per_file() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();

        // "Restarted" manager configured with 1 MiB stripes, but the
        // old file's placement (8 MiB) is already pinned.
        let mgr = stripe_mgr(&registry, 1024 * 1024);
        mgr.load_placement_records(vec![PlacementRecord {
            file_key: "old-file".into(),
            stripe_size: 8 * 1024 * 1024,
            device_ids: vec!["ds-1".into(), "ds-2".into()],
            file_id: 0,
            truncate_pending: None,
            truncate_since_unix: None,
        }]);

        let old = mgr
            .generate_layout(test_owner(1), vec![1], "old-file", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert!(
            old.segments.iter().all(|s| s.length == 8 * 1024 * 1024),
            "pinned 8 MiB stripe must survive a 1 MiB config"
        );

        let new = mgr
            .generate_layout(test_owner(1), vec![2], "new-file", 0, 2 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert!(new.segments.iter().all(|s| s.length == 1024 * 1024));
    }

    /// Grants register the composite deviceid → ordered-device-list
    /// mapping that GETDEVICEINFO resolves; order is the placement's.
    #[test]
    fn stripe_group_registered_for_getdeviceinfo() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-2")).unwrap();
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "f", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();

        let placement = mgr.placement_for("f").unwrap();
        let group = mgr
            .stripe_group_devices(&composite_device_id(&placement.device_ids))
            .expect("stripe group must be registered at grant time");
        assert_eq!(group, vec!["ds-1".to_string(), "ds-2".to_string()]);
    }

    /// Deleting a file drops its pin; a re-created file at the same
    /// path pins the CURRENT fleet.
    #[test]
    fn forget_placement_allows_fresh_pin() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "f", 0, 8 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.placement_for("f").unwrap().device_ids.len(), 1);

        mgr.forget_placement("f");
        assert!(mgr.placement_for("f").is_none());

        registry.register(ds("ds-2")).unwrap();
        mgr.generate_layout(test_owner(1), vec![1], "f", 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        assert_eq!(mgr.placement_for("f").unwrap().device_ids.len(), 2);
    }

    /// P0-2 identity core: every pin allocates a unique nonzero
    /// file_id, and a forget→re-pin cycle (NFS REMOVE + recreate)
    /// yields a DIFFERENT id — the recreated file can never resolve
    /// its predecessor's DS stripe files.
    #[test]
    fn remove_recreate_gets_fresh_file_id() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "f", 0, 8 << 20, IoMode::ReadWrite).unwrap();
        let first = mgr.placement_for("f").unwrap();
        assert_ne!(first.file_id, 0, "new pins must be identity-keyed");

        let forgotten = mgr.forget_placement("f").expect("pin existed");
        assert_eq!(forgotten.file_id, first.file_id);

        mgr.generate_layout(test_owner(1), vec![2], "f", 0, 8 << 20, IoMode::ReadWrite).unwrap();
        let second = mgr.placement_for("f").unwrap();
        assert_ne!(second.file_id, 0);
        assert_ne!(second.file_id, first.file_id, "recreated file must get a fresh identity");
    }

    /// RENAME re-keys the pin without touching the identity, so the
    /// data (keyed by file_id on the DSes) follows the new name.
    #[test]
    fn rename_moves_pin_keeps_identity() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "old", 0, 16 << 20, IoMode::ReadWrite).unwrap();
        let before = mgr.placement_for("old").unwrap();

        let overwritten = mgr.rename_placement("old", "new").unwrap();
        assert!(overwritten.is_none());
        assert!(mgr.placement_for("old").is_none());
        let after = mgr.placement_for("new").unwrap();
        assert_eq!(after.file_id, before.file_id, "identity must survive rename");
        assert_eq!(after.device_ids, before.device_ids);
    }

    /// Rename-over: the clobbered target's pin comes back so the
    /// caller can reclaim its stripes.
    #[test]
    fn rename_over_returns_overwritten_pin() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "src", 0, 8 << 20, IoMode::ReadWrite).unwrap();
        mgr.generate_layout(test_owner(1), vec![2], "dst", 0, 8 << 20, IoMode::ReadWrite).unwrap();
        let dst_id = mgr.placement_for("dst").unwrap().file_id;

        let overwritten = mgr.rename_placement("src", "dst").unwrap().expect("dst pin returned");
        assert_eq!(overwritten.file_id, dst_id);
    }

    /// Legacy (file_id 0) pins refuse rename — their DS stripes are
    /// path-keyed; the op layer surfaces NFS4ERR_NOTSUPP.
    #[test]
    fn rename_refuses_legacy_pin() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);
        mgr.load_placement_records(vec![PlacementRecord {
            file_key: "legacy".into(),
            stripe_size: 8 << 20,
            device_ids: vec!["ds-1".into()],
            file_id: 0,
            truncate_pending: None,
            truncate_since_unix: None,
        }]);
        assert!(mgr.rename_placement("legacy", "elsewhere").is_err());
        assert!(mgr.placement_for("legacy").is_some(), "refused rename must not lose the pin");
    }

    /// Cleanup queue: REMOVE enqueues one stripe path per DS slot,
    /// drained exactly once per device.
    #[test]
    fn cleanup_queue_per_slot_paths() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        registry.register(ds("ds-2")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "gone", 0, 16 << 20, IoMode::ReadWrite).unwrap();
        let p = mgr.forget_placement("gone").unwrap();
        mgr.enqueue_stripe_cleanup(&p, "gone");

        let ds1 = mgr.drain_stripe_cleanup("ds-1");
        let ds2 = mgr.drain_stripe_cleanup("ds-2");
        assert_eq!(ds1, vec![format!("{:016x}.stripe0", p.file_id)]);
        assert_eq!(ds2, vec![format!("{:016x}.stripe1", p.file_id)]);
        assert!(mgr.drain_stripe_cleanup("ds-1").is_empty(), "drain is once-only");
    }

    /// The truncate-dirty gate lifts only when the DEEPEST pending cut
    /// is confirmed — a racing larger set_len can't kill bytes below
    /// its own length, so it must not clear a smaller pending one.
    #[test]
    fn truncate_gate_min_size_semantics() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.mark_truncate_dirty("id:00000000000000aa", 100);
        mgr.mark_truncate_dirty("id:00000000000000aa", 50); // deeper cut
        mgr.mark_truncate_dirty("id:00000000000000aa", 200); // shallower — no-op on min

        assert!(
            !mgr.clear_truncate_dirty_if("id:00000000000000aa", 100),
            "confirming 100 must NOT lift the gate while 50 is pending"
        );
        assert!(mgr.truncate_dirty_since("id:00000000000000aa").is_some());
        assert!(
            mgr.clear_truncate_dirty_if("id:00000000000000aa", 50),
            "confirming the deepest cut lifts the gate"
        );
        assert!(mgr.truncate_dirty_since("id:00000000000000aa").is_none());
    }

    /// The gate is keyed by file identity, so it survives RENAME with
    /// no explicit hand-off, and REMOVE drops it with the pin.
    #[test]
    fn truncate_gate_follows_rename_and_dies_with_remove() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        mgr.generate_layout(test_owner(1), vec![1], "a", 0, 8 << 20, IoMode::ReadWrite).unwrap();
        let p = mgr.placement_for("a").unwrap();
        let gate = truncate_gate_key(&p, "a");
        mgr.mark_truncate_dirty(&gate, 0);

        mgr.rename_placement("a", "b").unwrap();
        let p_b = mgr.placement_for("b").unwrap();
        assert_eq!(
            truncate_gate_key(&p_b, "b"),
            gate,
            "identity key makes the gate rename-proof"
        );
        assert!(mgr.truncate_dirty_since(&gate).is_some());

        mgr.forget_placement("b");
        assert!(
            mgr.truncate_dirty_since(&gate).is_none(),
            "REMOVE moots the unconfirmed truncation"
        );
    }

    /// Directory rename re-keys every child placement (Spark commits
    /// by renaming its _temporary attempt dir); the `<dir>/` prefix
    /// match never crosses into a sibling whose name merely shares the
    /// characters (stage vs stage2).
    #[test]
    fn dir_rename_sweeps_children_prefix_safe() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);

        for f in ["stage/a.parquet", "stage/sub/b.parquet", "stage2/c.parquet"] {
            mgr.generate_layout(test_owner(1), vec![1], f, 0, 8 << 20, IoMode::ReadWrite)
                .unwrap();
        }
        let id_a = mgr.placement_for("stage/a.parquet").unwrap().file_id;

        let moved = mgr.rename_placements_under("stage", "final");
        assert_eq!(moved, 2);
        assert!(mgr.placement_for("stage/a.parquet").is_none());
        assert_eq!(
            mgr.placement_for("final/a.parquet").unwrap().file_id,
            id_a,
            "identity travels with the re-keyed pin"
        );
        assert!(mgr.placement_for("final/sub/b.parquet").is_some());
        assert!(
            mgr.placement_for("stage2/c.parquet").is_some(),
            "sibling with shared name prefix must be untouched"
        );
        assert!(!mgr.has_legacy_placements_under("final"));
    }

    /// A legacy (file_id == 0) pin under a directory blocks that
    /// directory's rename at the guard.
    #[test]
    fn legacy_child_detected_under_dir() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(ds("ds-1")).unwrap();
        let mgr = stripe_mgr(&registry, 8 * 1024 * 1024);
        mgr.load_placement_records(vec![crate::state_backend::PlacementRecord {
            file_key: "old/legacy.bin".into(),
            stripe_size: 8 << 20,
            device_ids: vec!["ds-1".into()],
            file_id: 0,
            truncate_pending: None,
            truncate_since_unix: None,
        }]);
        assert!(mgr.has_legacy_placements_under("old"));
        assert!(!mgr.has_legacy_placements_under("old2"));
    }
}
