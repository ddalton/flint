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
use tracing::{debug, info};

/// Layout state ID (combines with NFSv4 stateid)
pub type LayoutStateId = [u8; 16];

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
    fn to_record(&self, file_key: &str) -> PlacementRecord {
        PlacementRecord {
            file_key: file_key.to_string(),
            stripe_size: self.stripe_size,
            device_ids: self.device_ids.clone(),
            file_id: self.file_id,
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

    /// Stripe size in bytes
    stripe_size: u64,

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
    truncate_dirty: Arc<DashMap<String, (std::time::Instant, u64)>>,

    /// Persistence target. Layouts surviving MDS restart prevents the
    /// kernel from issuing fresh LAYOUTGETs (disruptive but functional)
    /// and lets recall fan-out work correctly post-restart. See
    /// `state_backend::mod.rs` for the lag-bound rationale.
    backend: Arc<dyn StateBackend>,
}

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

/// Layout type as per RFC 8881 Section 12.2.3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LayoutType {
    /// NFSv4.1 Files layout (RFC 8881 Chapter 13)
    NfsV4_1Files = 1,
    
    /// Block/volume layout (RFC 5663) - future
    BlockVolume = 2,
    
    /// Object layout (RFC 5664) - future
    Osd2Objects = 3,
    
    /// Flexible File Layout (RFC 8435) - for independent DS storage
    /// Each DS has its own storage, filehandles are DS-specific
    FlexFiles = 4,
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

impl LayoutManager {
    /// Create a new layout manager backed by `backend`.
    pub fn new(
        device_registry: Arc<DeviceRegistry>,
        policy: ConfigLayoutPolicy,
        stripe_size: u64,
        backend: Arc<dyn StateBackend>,
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
            policy: policy_impl,
            stripe_size,
            placements: Arc::new(DashMap::new()),
            stripe_groups: Arc::new(DashMap::new()),
            cleanup_queues: Arc::new(DashMap::new()),
            truncate_dirty: Arc::new(DashMap::new()),
            backend,
        }
    }

    /// Configured stripe size (bytes) — advertised as the FILE-layout
    /// stripe unit in LAYOUTGET replies.
    pub fn stripe_size(&self) -> u64 {
        self.stripe_size
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
            self.layouts.insert(stateid, layout);
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
    pub fn load_placement_records(&self, records: Vec<PlacementRecord>) {
        let n = records.len();
        for r in &records {
            let placement = FilePlacement::from_record(r);
            self.stripe_groups
                .entry(composite_device_id(&placement.device_ids))
                .or_insert_with(|| placement.device_ids.clone());
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
        self.truncate_dirty
            .entry(gate_key.to_string())
            .and_modify(|(_, min)| *min = (*min).min(size))
            .or_insert_with(|| (std::time::Instant::now(), size));
    }

    /// Lift the gate if a fan-out that confirmed `confirmed_size` on
    /// every pinned DS satisfies the deepest pending cut. Returns
    /// whether the gate was lifted.
    pub fn clear_truncate_dirty_if(&self, gate_key: &str, confirmed_size: u64) -> bool {
        let cleared = self
            .truncate_dirty
            .remove_if(gate_key, |_, (_, min)| confirmed_size <= *min)
            .is_some();
        if cleared {
            info!("Truncate-dirty cleared for '{}' (size {} confirmed)", gate_key, confirmed_size);
        }
        cleared
    }

    /// Unconditionally lift the gate (file deleted — its stripes are
    /// enqueued for deletion outright).
    pub fn clear_truncate_dirty(&self, gate_key: &str) {
        self.truncate_dirty.remove(gate_key);
    }

    /// The gate state: (dirty-since, smallest unconfirmed size).
    pub fn truncate_dirty_state(&self, gate_key: &str) -> Option<(std::time::Instant, u64)> {
        self.truncate_dirty.get(gate_key).map(|e| *e.value())
    }

    /// When the file went truncate-dirty, if it still is.
    pub fn truncate_dirty_since(&self, gate_key: &str) -> Option<std::time::Instant> {
        self.truncate_dirty_state(gate_key).map(|(since, _)| since)
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

        let placement = self
            .placements
            .entry(file_key.to_string())
            .or_insert_with(|| FilePlacement {
                stripe_size: self.stripe_size,
                device_ids,
                file_id: allocate_file_id(),
            })
            .clone();

        self.stripe_groups
            .entry(composite_device_id(&placement.device_ids))
            .or_insert_with(|| placement.device_ids.clone());

        self.backend
            .enqueue_write(WriteOp::PutPlacement(placement.to_record(file_key)));

        info!(
            "📌 Pinned placement for '{}': {} DSes {:?}, stripe_size={}",
            file_key,
            placement.device_ids.len(),
            placement.device_ids,
            placement.stripe_size,
        );
        Ok(placement)
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
        let layout = LayoutState {
            stateid,
            owner,
            filehandle,
            segments,
            iomode,
            return_on_close: true,
        };

        // Track active layouts (primary map + secondary by-client index).
        self.persist(&layout);
        self.layouts.insert(stateid, layout.clone());
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
        if let Some((_, layout)) = self.layouts.remove(stateid) {
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
                entry.retain(|s| s != stateid);
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
        let Some((_, layout)) = self.layouts.remove(stateid) else {
            return false;
        };
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
            entry.retain(|s| s != stateid);
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
    pub fn recall_layouts_for_device(
        &self,
        device_id: &str,
    ) -> Vec<(SessionIdBytes, LayoutStateId)> {
        let mut recalled = Vec::new();

        for entry in self.layouts.iter() {
            let has_device = entry
                .segments
                .iter()
                .any(|seg| seg.device_id == device_id);

            if has_device {
                recalled.push((entry.owner.session_id, entry.stateid));
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

    /// Get layout by stateid
    pub fn get_layout(&self, stateid: &LayoutStateId) -> Option<LayoutState> {
        self.layouts.get(stateid).map(|entry| entry.clone())
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
    fn generate_stateid() -> LayoutStateId {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut stateid = [0u8; 16];
        rng.fill(&mut stateid);
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
        assert_eq!(recalled[0].1, layout.stateid);
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
        assert_eq!(LayoutType::NfsV4_1Files as u32, 1);
        assert_eq!(LayoutType::BlockVolume as u32, 2);
        assert_eq!(LayoutType::Osd2Objects as u32, 3);
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

    fn ds(id: &str) -> DeviceInfo {
        DeviceInfo::new(id.to_string(), format!("{}:2049", id), vec![])
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
        }]);
        assert!(mgr.has_legacy_placements_under("old"));
        assert!(!mgr.has_legacy_placements_under("old2"));
    }
}
