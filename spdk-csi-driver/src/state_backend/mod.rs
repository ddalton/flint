//! Pluggable persistence for NFSv4 / pNFS server state.
//!
//! Phase B of `docs/plans/pnfs-production-readiness.md`. Today every
//! piece of NFSv4 + pNFS state lives in `DashMap`s in process memory:
//! `ClientManager`, `SessionManager`, `StateIdManager`, `LayoutManager`.
//! On MDS restart the maps evaporate and active clients see
//! `STALE_CLIENTID` / `BAD_STATEID` / `STALE_DEVICEID` on their next op
//! — a long-running pNFS PVC effectively has its mount destroyed by a
//! pod roll. Unacceptable for any production deployment.
//!
//! This module introduces a [`StateBackend`] trait so the managers can
//! be backed by either:
//! * [`MemoryBackend`](memory::MemoryBackend) — DashMap-wrapping parity
//!   with today's behaviour (default for tests, dev work, anyone who
//!   doesn't care about restart survival), or
//! * `SqliteBackend` — durable single-file SQLite; ships in production.
//!   Lands in B.2.
//!
//! The records below are deliberately plain (`Vec<u8>`, `u64`, fixed-
//! size byte arrays) so they survive byte-for-byte across process
//! lifetimes. The boundary code in B.3 converts them to/from the
//! richer in-memory types (`Client`, `Session`, `StateEntry`,
//! `LayoutState`).
//!
//! Records intentionally NOT in the trait:
//! * Slot replay-cache contents — RFC 8881 §15.1.10.4 permits losing
//!   them on restart; clients re-issue.
//! * Per-connection state — TCP connections drop and re-establish
//!   regardless.
//! * In-flight RPC futures — they time out client-side and retry.

pub mod extent_alloc;

/// §8's cost-budget bench for the block-layout allocator (ignored by
/// default; run explicitly on Linux — see the module doc).
#[cfg(test)]
mod extent_bench;
pub mod memory;
pub mod sqlite;

pub use memory::MemoryBackend;
pub use sqlite::SqliteBackend;

use std::sync::Arc;

// NOTE (F27, 2026-07-19): `spawn_persist` — the old fire-and-forget
// helper that pushed each mutation to the backend on its own spawned
// tokio task — is GONE. Spawned tasks race each other, so an OPEN's
// put and a fast-following CLOSE's delete could apply in reverse and
// resurrect the deleted row in the durable DB (phantom stateids/locks
// after a failover reload). Mutation sites now call
// [`StateBackend::enqueue_write`], which captures the op in call
// order; ordering is the backend's job (SQLite: one writer thread fed
// by an ordered channel with group commit; memory: applied inline).

/// Convenience: build a default in-memory backend wrapped in `Arc<dyn
/// StateBackend>`. Used by tests and by production when the operator
/// hasn't configured durable persistence (`state.backend: memory`).
pub fn memory_backend() -> Arc<dyn StateBackend> {
    Arc::new(MemoryBackend::new())
}

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Errors a [`StateBackend`] can surface to the caller. Modeled on the
/// distinct failure modes SQLite will produce in B.2 — `MemoryBackend`
/// is infallible but uses the same shape so the boundary code doesn't
/// need a second error path.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StateBackendError {
    /// Underlying storage hiccup (SQLite I/O, disk-full, locked-db, …).
    /// Carries the source error message; the in-memory backend never
    /// produces this variant.
    #[error("backend storage error: {0}")]
    Storage(String),

    /// A row decoded from the backend didn't round-trip cleanly. Most
    /// likely cause is a schema-version mismatch between the running
    /// MDS and the on-disk file — operator should `mv state.db
    /// state.db.bak` and restart, or run a migration. The in-memory
    /// backend never produces this variant.
    #[error("serialization error: {0}")]
    Serialization(String),
}

pub type StateBackendResult<T> = std::result::Result<T, StateBackendError>;

// ── Record types ──────────────────────────────────────────────────────
//
// These mirror the in-memory state types, stripped to plain fields a
// SQLite row can hold. Naming follows the in-memory type with a
// `Record` suffix to make the boundary obvious. Any time you add a
// field to the in-memory type that needs to survive restart, also add
// it here AND bump `SCHEMA_VERSION` in the SQLite backend (B.2).

/// Persisted bits of a CREATE_SESSION response, returned byte-identical
/// on a CREATE_SESSION replay (RFC 8881 §15.1.10.4 / §18.36.4).
/// Mirrors `nfs::v4::state::client::CachedCreateSessionRes` but holds
/// the session id as a 16-byte array so this module doesn't pull in
/// the NFSv4 protocol module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedCreateSessionResRecord {
    pub session_id: [u8; 16],
    pub sequence: u32,
    pub flags: u32,
    pub fore_max_request_size: u32,
    pub fore_max_response_size: u32,
    pub fore_max_response_size_cached: u32,
    pub fore_max_operations: u32,
    pub fore_max_requests: u32,
    /// Back-channel attrs as echoed in `csr_back_chan_attrs` (C9). These
    /// only carry meaning when `flags` has `CREATE_SESSION4_FLAG_CONN_BACK_CHAN`
    /// set; a client ignores the field otherwise. They must be cached
    /// because a replay has to reproduce the original reply exactly — a
    /// replay that echoed the flag but not the attrs would hand the client
    /// a back channel it then rejects with EINVAL.
    ///
    /// `serde(default)` (not a schema bump): this record is stored as JSON
    /// in the `clients.cs_cached_res` TEXT column, so pre-C9 rows simply
    /// deserialize these as 0 — which is consistent, because those rows
    /// also have `flags == 0`.
    #[serde(default)]
    pub back_max_request_size: u32,
    #[serde(default)]
    pub back_max_response_size: u32,
    #[serde(default)]
    pub back_max_response_size_cached: u32,
    #[serde(default)]
    pub back_max_operations: u32,
    #[serde(default)]
    pub back_max_requests: u32,
}

/// One client established via EXCHANGE_ID. Restored on MDS restart so a
/// reconnecting client gets back its existing clientid (no
/// `STALE_CLIENTID`) and any in-flight CREATE_SESSION replay still
/// returns the original byte-identical fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRecord {
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub verifier: u64,
    pub server_owner: String,
    pub server_scope: Vec<u8>,
    pub sequence_id: u32,
    pub flags: u32,
    pub principal: Vec<u8>,
    pub confirmed: bool,
    pub last_cs_sequence: Option<u32>,
    pub cs_cached_res: Option<CachedCreateSessionResRecord>,
    pub initial_cs_sequence: u32,
    /// `true` once the client has issued a successful RECLAIM_COMPLETE.
    /// Persisted because a post-restart MDS must remember whether
    /// pre-restart clients have already exited grace mode — otherwise
    /// a second RECLAIM_COMPLETE would silently succeed instead of
    /// returning `NFS4ERR_COMPLETE_ALREADY`. Default `false` (matches
    /// the SQLite migration's column default), so older DBs migrate
    /// cleanly.
    #[serde(default)]
    pub reclaim_complete: bool,
}

/// One NFSv4.1 session. Slot replay state is deliberately not
/// persisted (see module docs); only the channel attributes and the
/// client/cb-program binding are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: [u8; 16],
    pub client_id: u64,
    pub sequence: u32,
    pub flags: u32,
    pub fore_chan_maxrequestsize: u32,
    pub fore_chan_maxresponsesize: u32,
    pub fore_chan_maxresponsesize_cached: u32,
    pub fore_chan_maxops: u32,
    pub fore_chan_maxrequests: u32,
    pub cb_program: u32,
}

/// Type tag mirroring `nfs::v4::state::stateid::StateType`. Held as
/// its own enum so this module doesn't depend on the NFSv4 layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateTypeRecord {
    Open,
    Lock,
    Delegation,
}

/// One stateid (OPEN / LOCK / DELEGATION). The `seqid` here is the
/// server's current value; a reconnecting client whose request carries
/// `seqid - 1` still validates under `validate_for_read`'s relaxation
/// (see `nfs/v4/state/stateid.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateIdRecord {
    pub other: [u8; 12],
    pub seqid: u32,
    pub state_type: StateTypeRecord,
    pub client_id: u64,
    pub filehandle: Option<Vec<u8>>,
    pub revoked: bool,
}

/// One byte-range lock (LOCK op). The lock's *stateid* was already a
/// [`StateIdRecord`] (`state_type: Lock`) and survived restart — but the
/// lock's substance (range, type, owner) lived only in the in-memory
/// `LockManager`, so after a restart the stateid still validated while
/// conflict enforcement was silently gone: a second client could take a
/// conflicting lock the first client still believed it held. Persisting
/// this record closes that hole; `LockManager::load_records` restores
/// the table at startup.
///
/// `lock_type` uses the NFSv4 wire values (`READ_LT=1`, `WRITE_LT=2`,
/// `READW_LT=3`, `WRITEW_LT=4`) so the record stays protocol-plain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
    pub other: [u8; 12],
    pub seqid: u32,
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub filehandle: Vec<u8>,
    pub lock_type: u32,
    pub offset: u64,
    pub length: u64,
}

/// I/O mode tag mirroring `pnfs::mds::layout::IoMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoModeRecord {
    Read,
    ReadWrite,
    Any,
}

/// One stripe within a layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSegmentRecord {
    pub offset: u64,
    pub length: u64,
    pub iomode: IoModeRecord,
    pub device_id: String,
    pub stripe_index: u32,
    pub pattern_offset: u64,
}

/// One pNFS layout issued to a client. Restored on MDS restart so the
/// client doesn't see `BAD_STATEID` on its next LAYOUTRETURN /
/// LAYOUTCOMMIT. The owning client/session ids let CB_LAYOUTRECALL
/// route correctly after restart too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutRecord {
    pub stateid: [u8; 16],
    pub owner_client_id: u64,
    pub owner_session_id: [u8; 16],
    pub owner_fsid: u64,
    pub filehandle: Vec<u8>,
    pub segments: Vec<LayoutSegmentRecord>,
    pub iomode: IoModeRecord,
    pub return_on_close: bool,
    /// The file identity this layout was issued against — literally
    /// `truncate_gate_key`'s output (`id:<file_id>` for identity pins,
    /// `path:<key>` for legacy ones). Stored rather than recomputed so
    /// the truncate recall (F65) selects layouts by the SAME key the
    /// truncate-dirty gate is filed under: two independently-derived
    /// keys can drift, and a drifted key silently recalls nothing.
    ///
    /// Empty on rows written before schema v7. Such a layout cannot be
    /// matched to a file, so a truncate will not recall it — see
    /// `LayoutManager::recall_layouts_for_file`, which refuses to treat
    /// an empty ident as a wildcard.
    #[serde(default)]
    pub file_ident: String,
}

/// Per-file stripe placement — which DSes (in which order) a file's
/// stripes live on, pinned at first LAYOUTGET. Layout grants MUST
/// reuse this record verbatim: the stripe map is a pure function of
/// `(device_ids order, stripe_size)`, so recomputing it from the live
/// device registry re-maps existing data whenever the fleet changes
/// (the Phase 0 P1 in `docs/plans/pnfs-durable-ds-plan.md`).
///
/// Keyed by the export-relative path. Files pinned with a nonzero
/// `file_id` store their DS stripes under identity-derived names
/// (`{file_id:016x}.stripeN`), so the path key is pure metadata and a
/// RENAME just re-keys the record. Legacy records (file_id 0) share
/// the path identity with the DSes' path-nested storage and cannot be
/// Per-VOLUME stripe geometry, chosen from StorageClass parameters at
/// CreateVolume and fixed for the volume's life.
///
/// Distinct from [`PlacementRecord`], which is per FILE and is written
/// when a file is first laid out. Geometry has to exist BEFORE any file
/// does, so it cannot be derived from placements — and `PlacementRecord`
/// carries no `stripe_width` to derive it from anyway.
///
/// Losing a row is not a data-correctness event: files already laid out
/// keep their pinned placements. It changes only how files not yet
/// created are striped, which is why the loss is worth a loud WARN
/// rather than a refusal to serve.
/// A client that fetched a block volume's pNFS device and asked to be
/// told when it changes — the durable half of CB_NOTIFY_DEVICEID.
///
/// Keyed on the client, not the session: the session a client fetched
/// under does not survive an MDS restart (startup drops persisted
/// sessions on purpose so the kernel re-CREATE_SESSIONs), while its
/// cached device does. See the `device_notify` schema comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceNotifyRecord {
    pub volume: String,
    /// NFSv4 client id — the same u64 the volume's reservation key is.
    pub client_id: u64,
    /// The notification types GRANTED at GETDEVICEINFO (a subset of what
    /// the client requested). Sending a type the client did not ask for
    /// is a protocol violation, so this travels with the address.
    pub notify_mask: u32,
    pub fetched_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeGeometryRecord {
    /// Volume directory name under the MDS export — the first component
    /// of a `PlacementRecord::file_key`.
    pub volume: String,
    /// Stripe unit in bytes. Always resolved: never 0.
    pub stripe_size: u64,
    /// Max data servers a file in this volume is pinned across.
    /// 0 = every active DS.
    pub stripe_width: u32,
    /// Layout class serving this volume: "file" (NFSv4.1 files layout,
    /// the historical class) or "scsi" (RFC 8154/9561 extents — the
    /// pnfs-block StorageClass). Closed set; parsed by
    /// `LayoutClass::parse`, and a value it refuses is a load error,
    /// never a default — a class misread as "file" would silently serve
    /// stripe layouts over a volume whose data lives in extents.
    pub layout_class: String,
}

/// renamed safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRecord {
    pub file_key: String,
    pub stripe_size: u64,
    /// Ordered device ids. Order is load-bearing: stripe unit `u` maps
    /// to `device_ids[(u + first_stripe_index) % len]`.
    pub device_ids: Vec<String>,
    /// Immutable per-file identity, allocated at pin time. 0 = legacy
    /// path-keyed pin (pre-upgrade record, serde default).
    #[serde(default)]
    pub file_id: u64,
    /// The deepest size change not yet confirmed on every pinned DS —
    /// the truncate-dirty gate, persisted.
    ///
    /// The gate lived only in memory, so an MDS restart during a PARKED
    /// truncate (a pinned DS unregistered or with no control listener,
    /// where the retry is designed to run for hours) came back with the
    /// stub reporting the new size, the DSes still holding the old
    /// bytes, and nothing gating LAYOUTGET — permanent, and silent
    /// (audit R4). Restoring it re-arms both the gate and the retry.
    ///
    /// `None` = nothing pending; stored as -1 so the column stays NOT
    /// NULL like its siblings.
    #[serde(default)]
    pub truncate_pending: Option<u64>,
    /// Wall-clock seconds since the epoch at which the gate above was
    /// FIRST armed — not when it was last written.
    ///
    /// The gate's age drives `fallback_delay_ceiling`, past which a
    /// client taking MDS-fallback I/O is failed rather than delayed
    /// forever. That age lived only in an `Instant`, so every MDS
    /// restart re-stamped it to now() and re-armed the ceiling: during
    /// a long park with periodic bounces a fallback client could be
    /// DELAYed without bound, which is precisely the livelock the
    /// ceiling exists to prevent.
    ///
    /// Wall-clock, because it must outlive the process. A backwards
    /// clock jump makes the gate look YOUNGER (saturating subtraction),
    /// which delays the fail-fast rather than triggering it early —
    /// the safe direction for a value whose only job is to bound a wait.
    #[serde(default)]
    pub truncate_since_unix: Option<u64>,
}

/// Persistent id↔path mapping behind version-2 (id-based) NFSv4
/// metadata filehandles. v2 FHs are minted when a path is too long to
/// embed in the 128-byte handle (RFC 8881 NFS4_FHSIZE); the handle
/// carries only `(instance_id, file_id)` and this record is what a
/// restarted server resolves it from. A lost record → NFS4ERR_STALE →
/// the client re-walks the path — same recovery as any stale handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FhMappingRecord {
    /// Random non-zero u64 allocated at mint time. Never reused for a
    /// different path (REMOVE deletes the record; a recreated file
    /// gets a fresh id — new file, new filehandle, per NFS semantics).
    pub file_id: u64,
    /// Absolute (normalized) path the handle resolves to. RENAME
    /// re-writes this in place — the id follows the file.
    pub path: String,
}

// ── Write ops ─────────────────────────────────────────────────────────

/// One point-write against the backend, capturable at a sync mutation
/// site via [`StateBackend::enqueue_write`]. Every variant targets a
/// single row identified by [`WriteOp::key`]; two ops with the same
/// key are ordered by call order and the later one wins, which is what
/// lets the SQLite writer coalesce a put-then-delete burst to just the
/// delete without changing the observable final state.
#[derive(Debug, Clone)]
pub enum WriteOp {
    PutClient(ClientRecord),
    DeleteClient(u64),
    PutSession(SessionRecord),
    DeleteSession([u8; 16]),
    PutStateid(StateIdRecord),
    DeleteStateid([u8; 12]),
    PutLock(LockRecord),
    DeleteLock([u8; 12]),
    PutLayout(LayoutRecord),
    DeleteLayout([u8; 16]),
    PutPlacement(PlacementRecord),
    DeletePlacement(String),
    PutVolumeGeometry(VolumeGeometryRecord),
    DeleteVolumeGeometry(String),
    PutDeviceNotify(DeviceNotifyRecord),
    /// Forget a volume's notify book (DeleteVolume), or one client's row.
    DeleteDeviceNotify(String, Option<u64>),
    PutFhMapping(FhMappingRecord),
    DeleteFhMapping(u64),
}

/// Coalescing identity of a [`WriteOp`]: (table, primary key). The
/// SQLite writer keeps at most one queued op per key — the latest —
/// so its queue is bounded by live-key count, not op rate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WriteOpKey {
    Client(u64),
    Session([u8; 16]),
    Stateid([u8; 12]),
    Lock([u8; 12]),
    Layout([u8; 16]),
    Placement(String),
    VolumeGeometry(String),
    FhMapping(u64),
    /// `None` = the whole volume's book (DeleteVolume).
    DeviceNotify(String, Option<u64>),
}

// ── S3 tier records (L2 step 2 — design review A3 + A6) ─────────────

/// One durable dirty BIT (A3): "this file has content mutations that
/// generation `g` in the bucket does not have." Keyed by file identity
/// (dev, ino) like the change counter; `path` is the last known path,
/// best-effort until the identity-keyed rows of step 6 (A7).
/// `dirtied_unix` is the FIRST dirtying mutation of the cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierDirtyEntry {
    pub dev: u64,
    pub ino: u64,
    pub path: Option<String>,
    pub dirtied_unix: u64,
    /// Sequence of the newest mark folded into this row (capture's
    /// process-monotonic counter). The flusher's clean-clear deletes
    /// ONLY at the sequence it observed (`tier_clear_dirty_if_seq`):
    /// a drain that re-marks after the observation bumps this and the
    /// delete no-ops — an acked mutation's bit can never be lost to a
    /// racing clear (A3).
    pub mark_seq: u64,
}

/// One durable flush intent (A6): committed BEFORE
/// CreateMultipartUpload/PUT so a crashed flush is arbitrable by HEAD
/// (own stamp at g+1 ⇒ adopt; ETag == base ⇒ abort + re-flush).
/// `from_gen`/`base_etag` are None for a new object's first flush;
/// `mpu_id` is filled in once CreateMultipartUpload returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushIntentRecord {
    pub flush_uuid: String,
    pub path: String,
    pub from_gen: Option<u64>,
    pub to_gen: u64,
    pub mpu_id: Option<String>,
    pub base_etag: Option<String>,
    pub created_unix: u64,
}

impl WriteOp {
    pub fn key(&self) -> WriteOpKey {
        match self {
            WriteOp::PutClient(c) => WriteOpKey::Client(c.client_id),
            WriteOp::DeleteClient(id) => WriteOpKey::Client(*id),
            WriteOp::PutSession(s) => WriteOpKey::Session(s.session_id),
            WriteOp::DeleteSession(id) => WriteOpKey::Session(*id),
            WriteOp::PutStateid(s) => WriteOpKey::Stateid(s.other),
            WriteOp::DeleteStateid(o) => WriteOpKey::Stateid(*o),
            WriteOp::PutLock(l) => WriteOpKey::Lock(l.other),
            WriteOp::DeleteLock(o) => WriteOpKey::Lock(*o),
            WriteOp::PutLayout(l) => WriteOpKey::Layout(l.stateid),
            WriteOp::DeleteLayout(s) => WriteOpKey::Layout(*s),
            WriteOp::PutPlacement(p) => WriteOpKey::Placement(p.file_key.clone()),
            WriteOp::DeletePlacement(k) => WriteOpKey::Placement(k.clone()),
            WriteOp::PutVolumeGeometry(g) => WriteOpKey::VolumeGeometry(g.volume.clone()),
            WriteOp::DeleteVolumeGeometry(v) => WriteOpKey::VolumeGeometry(v.clone()),
            WriteOp::PutDeviceNotify(r) => WriteOpKey::DeviceNotify(r.volume.clone(), Some(r.client_id)),
            WriteOp::DeleteDeviceNotify(v, c) => WriteOpKey::DeviceNotify(v.clone(), *c),
            WriteOp::PutFhMapping(m) => WriteOpKey::FhMapping(m.file_id),
            WriteOp::DeleteFhMapping(id) => WriteOpKey::FhMapping(*id),
        }
    }

    /// Short tag for persist-failure log lines.
    pub fn label(&self) -> &'static str {
        match self {
            WriteOp::PutClient(_) => "client.put",
            WriteOp::DeleteClient(_) => "client.delete",
            WriteOp::PutSession(_) => "session.put",
            WriteOp::DeleteSession(_) => "session.delete",
            WriteOp::PutStateid(_) => "stateid.put",
            WriteOp::DeleteStateid(_) => "stateid.delete",
            WriteOp::PutLock(_) => "lock.put",
            WriteOp::DeleteLock(_) => "lock.delete",
            WriteOp::PutLayout(_) => "layout.put",
            WriteOp::DeleteLayout(_) => "layout.delete",
            WriteOp::PutPlacement(_) => "placement.put",
            WriteOp::DeletePlacement(_) => "placement.delete",
            WriteOp::PutVolumeGeometry(_) => "volume_geometry.put",
            WriteOp::DeleteVolumeGeometry(_) => "volume_geometry.delete",
            WriteOp::PutDeviceNotify(_) => "device_notify.put",
            WriteOp::DeleteDeviceNotify(..) => "device_notify.delete",
            WriteOp::PutFhMapping(_) => "fh_mapping.put",
            WriteOp::DeleteFhMapping(_) => "fh_mapping.delete",
        }
    }
}

// ── The trait ─────────────────────────────────────────────────────────

/// Pluggable persistence for NFSv4 / pNFS server state.
///
/// All methods are async because the production impl ([`SqliteBackend`]
/// in B.2) does blocking disk I/O; the in-memory impl is trivially
/// async-compatible. Returning `Result` everywhere keeps the boundary
/// code (B.3) on a single error type even though `MemoryBackend` is
/// infallible in practice.
///
/// Idempotency contract: `put_*` is upsert (last-writer-wins on
/// matching primary key); `delete_*` on a non-existent key is `Ok(())`,
/// not an error — both backends rely on the upper layers' DashMap
/// semantics where double-removes are no-ops.
///
/// `load_all_*` exists for the boundary code to populate the
/// in-memory caches at startup. Hot-path reads go through the
/// in-memory cache; the trait is only consulted on writes and on
/// startup.
#[async_trait]
pub trait StateBackend: Send + Sync {
    /// Ordered, fire-and-forget write from a sync mutation site.
    ///
    /// The op is captured in CALL order — the F27 ordering guarantee.
    /// The old `spawn_persist` pattern spawned one tokio task per
    /// mutation, and tasks race: an OPEN's put and a fast-following
    /// CLOSE's delete could reach the DB in reverse order, and the
    /// late put (INSERT OR REPLACE) resurrected the deleted row —
    /// after a failover reload that is a phantom stateid, or worse a
    /// phantom byte-range lock blocking another client. `enqueue_write`
    /// is sync, so the capture happens at the mutation site itself.
    ///
    /// Durability is asynchronous (SQLite: group-committed by the
    /// writer thread within a few ms; errors logged under the
    /// `state_persist` target). Callers that must observe completion
    /// use the async `put_*`/`delete_*` methods instead, which resolve
    /// once the write is committed.
    fn enqueue_write(&self, op: WriteOp);

    // Clients
    async fn put_client(&self, c: &ClientRecord) -> StateBackendResult<()>;
    async fn get_client(&self, client_id: u64) -> StateBackendResult<Option<ClientRecord>>;
    async fn list_clients(&self) -> StateBackendResult<Vec<ClientRecord>>;
    async fn delete_client(&self, client_id: u64) -> StateBackendResult<()>;

    // Sessions
    async fn put_session(&self, s: &SessionRecord) -> StateBackendResult<()>;
    async fn get_session(&self, session_id: &[u8; 16]) -> StateBackendResult<Option<SessionRecord>>;
    async fn list_sessions(&self) -> StateBackendResult<Vec<SessionRecord>>;
    async fn delete_session(&self, session_id: &[u8; 16]) -> StateBackendResult<()>;

    // StateIds
    async fn put_stateid(&self, s: &StateIdRecord) -> StateBackendResult<()>;
    async fn get_stateid(&self, other: &[u8; 12]) -> StateBackendResult<Option<StateIdRecord>>;
    async fn list_stateids(&self) -> StateBackendResult<Vec<StateIdRecord>>;
    async fn delete_stateid(&self, other: &[u8; 12]) -> StateBackendResult<()>;

    // Byte-range locks (keyed by the lock stateid's `other`, same as
    // the LockManager's in-memory table)
    async fn put_lock(&self, l: &LockRecord) -> StateBackendResult<()>;
    async fn get_lock(&self, other: &[u8; 12]) -> StateBackendResult<Option<LockRecord>>;
    async fn list_locks(&self) -> StateBackendResult<Vec<LockRecord>>;
    async fn delete_lock(&self, other: &[u8; 12]) -> StateBackendResult<()>;

    // Layouts
    async fn put_layout(&self, l: &LayoutRecord) -> StateBackendResult<()>;
    async fn get_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<Option<LayoutRecord>>;
    async fn list_layouts(&self) -> StateBackendResult<Vec<LayoutRecord>>;
    async fn delete_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<()>;

    // Per-file stripe placements (pNFS Phase 0)
    async fn put_placement(&self, p: &PlacementRecord) -> StateBackendResult<()>;
    async fn get_placement(&self, file_key: &str) -> StateBackendResult<Option<PlacementRecord>>;
    async fn list_placements(&self) -> StateBackendResult<Vec<PlacementRecord>>;
    async fn delete_placement(&self, file_key: &str) -> StateBackendResult<()>;

    // Per-volume stripe geometry. `put` is awaited by CreateVolume
    // rather than queued: a queued op has not reached the page cache
    // when it returns, and SIGKILL (the dominant Kubernetes crash) would
    // lose an acknowledged provision's geometry.
    async fn put_volume_geometry(&self, g: &VolumeGeometryRecord) -> StateBackendResult<()>;
    async fn get_volume_geometry(&self, volume: &str) -> StateBackendResult<Option<VolumeGeometryRecord>>;
    async fn list_volume_geometry(&self) -> StateBackendResult<Vec<VolumeGeometryRecord>>;

    // ── Block-layout extent allocator (design doc §8; extent_alloc.rs
    // owns the transaction semantics, FlintExtents.tla the theorems). ──
    //
    // The nested Result separates TRANSPORT failures (outer — the
    // backend is unreachable/broken) from ALLOCATOR VERDICTS (inner —
    // refusals the wire layer maps to NFS errors: Conflict/NotQuiescent
    // → LAYOUTTRYLATER, NoSpace → NOSPC, CommitRejected → BADLAYOUT).
    // Collapsing them would turn "another client holds this range" into
    // a storage error, and storage errors get retried forever.
    //
    // Only the sqlite backend implements these: §8 is explicit that
    // extent durability is sqlite-only and must be STRONGER than the
    // rest of the state (an extent map lost while data survives = F67's
    // silent zeros, with no stub to hang an xattr on). MemoryBackend
    // refuses, and a block-class volume on a memory-backed MDS fails
    // loudly at provision time, never at I/O time.

    async fn extent_register_volume(
        &self,
        volume: &str,
        size_ceiling: u64,
    ) -> StateBackendResult<Result<(), extent_alloc::ExtentAllocError>>;

    /// Raise the arena ceiling (the CSI expand path). Raise-only and
    /// idempotent; returns the ceiling in force. The CALLER must have
    /// grown the backing device first — see `extent_alloc::expand_volume`.
    async fn extent_expand_volume(
        &self,
        volume: &str,
        new_ceiling: u64,
    ) -> StateBackendResult<Result<u64, extent_alloc::ExtentAllocError>>;

    /// Bytes an allocating LAYOUTGET could still be granted. Zero = the
    /// next write grant returns `NoSpace`; the MDS-lane belt reads this
    /// to tell "arena full" (ENOSPC) apart from "no block fallback lane"
    /// (EIO).
    async fn extent_volume_headroom(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<u64, extent_alloc::ExtentAllocError>>;

    // ── S3 tier (L2 step 2 — A3 dirty bit + A6 flush intents) ────────
    //
    // These bypass the ordered `enqueue_write` queue on purpose: the
    // tier tables are disjoint from every other table (no cross-table
    // ordering to preserve), and A3's contract is that the caller
    // AWAITS durability before acking the client — the fire-and-forget
    // lane is exactly what these must not use.

    /// Upsert dirty bits, one transaction. An existing row keeps its
    /// original `dirtied_unix` (first-dirty time) and gains a path if
    /// it had none (`COALESCE` semantics).
    async fn tier_mark_dirty(&self, entries: &[TierDirtyEntry]) -> StateBackendResult<()>;

    /// Every file whose bit is set — the crash fallback's work list.
    async fn tier_list_dirty(&self) -> StateBackendResult<Vec<TierDirtyEntry>>;

    /// Clear one file's bit — ONLY in the same logical step that
    /// commits generation g+1 (the flusher, step 5). Eviction requires
    /// the bit clear (A4/A5).
    async fn tier_clear_dirty(&self, dev: u64, ino: u64) -> StateBackendResult<()>;

    /// The A3-safe clear: delete the bit row only if its `mark_seq`
    /// still equals `mark_seq` (what the flusher observed before
    /// publishing). Returns whether a row was deleted; false means a
    /// newer mark landed and the bit must survive. The flusher's
    /// clean-clear protocol (step 5) is the only caller.
    async fn tier_clear_dirty_if_seq(
        &self,
        dev: u64,
        ino: u64,
        mark_seq: u64,
    ) -> StateBackendResult<bool>;

    /// Create or update (mpu_id backfill) a flush intent.
    async fn put_flush_intent(&self, i: &FlushIntentRecord) -> StateBackendResult<()>;

    /// All open intents — the startup arbitration sweep's work list
    /// (steps 4/7).
    async fn list_flush_intents(&self) -> StateBackendResult<Vec<FlushIntentRecord>>;

    /// Close an intent (flush published, aborted, or adopted).
    async fn delete_flush_intent(&self, flush_uuid: &str) -> StateBackendResult<()>;

    /// Highest logical end covered by a file's COMMITTED extents (0 if
    /// none). The stub's length only advances at LAYOUTCOMMIT, so this
    /// is what tells an in-flight write apart from a real EOF.
    async fn extent_committed_end(
        &self,
        volume: &str,
        file_id: u64,
    ) -> StateBackendResult<Result<u64, extent_alloc::ExtentAllocError>>;

    /// LAYOUTGET's allocation transaction. `fresh_only` skips free-list
    /// reuse — REQUIRED until the MDS grows the NVMe initiator that can
    /// write_zeroes a reused range before the layout leaves the server
    /// (GrantedExtent::needs_scrub; FlintExtentsBlindProvision.cfg is
    /// the world where a reused range ships unscrubbed).
    async fn extent_grant(
        &self,
        volume: &str,
        file_id: u64,
        client_id: u64,
        logical_offset: u64,
        length: u64,
        fresh_only: bool,
    ) -> StateBackendResult<Result<Vec<extent_alloc::GrantedExtent>, extent_alloc::ExtentAllocError>>;

    /// READ-layout query: committed extents overlapping the range, with
    /// grant rows for holder visibility — never allocates, never returns
    /// uncommitted extents (the wire layer presents gaps as NONE_DATA).
    async fn extent_grant_read(
        &self,
        volume: &str,
        file_id: u64,
        client_id: u64,
        logical_offset: u64,
        length: u64,
    ) -> StateBackendResult<Result<Vec<extent_alloc::GrantedExtent>, extent_alloc::ExtentAllocError>>;

    /// LAYOUTCOMMIT's allocator half: promote INVALID→RW under a live
    /// (client, gen)-matching grant. Returns extents promoted.
    async fn extent_commit(
        &self,
        volume: &str,
        file_id: u64,
        client_id: u64,
        logical_offset: u64,
        length: u64,
    ) -> StateBackendResult<Result<u64, extent_alloc::ExtentAllocError>>;

    /// LAYOUTRETURN: drop the client's grant rows over the range.
    async fn extent_layout_return(
        &self,
        volume: &str,
        file_id: u64,
        client_id: u64,
        logical_offset: u64,
        length: u64,
    ) -> StateBackendResult<Result<usize, extent_alloc::ExtentAllocError>>;

    /// The recall snapshot: live unfenced holders over the range —
    /// advisory only; the free re-validates regardless.
    async fn extent_reclaim_snapshot(
        &self,
        volume: &str,
        file_id: u64,
        logical_offset: u64,
        length: u64,
    ) -> StateBackendResult<Result<Vec<u64>, extent_alloc::ExtentAllocError>>;

    /// Server-side revocation bookkeeping for an unresponsive holder.
    async fn extent_fence_client(
        &self,
        volume: &str,
        client_id: u64,
    ) -> StateBackendResult<Result<usize, extent_alloc::ExtentAllocError>>;

    /// The free transaction (FreeRevalidates inside; fenced-holder
    /// ranges quarantine).
    async fn extent_reclaim_complete(
        &self,
        volume: &str,
        file_id: u64,
        logical_offset: u64,
        length: u64,
        now_unix: i64,
    ) -> StateBackendResult<Result<extent_alloc::FreeOutcome, extent_alloc::ExtentAllocError>>;

    /// DeleteVolume's sweep of every allocator row for the volume.
    async fn extent_drop_volume(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<u64, extent_alloc::ExtentAllocError>>;

    /// Record a client's NVMe host identity on the volume's desired
    /// allow-list and return the full DISTINCT list after the upsert —
    /// what the block-export reconciler converges spdk-tgt onto.
    async fn block_host_admit(
        &self,
        volume: &str,
        client_id: u64,
        host_nqn: &str,
        now_unix: i64,
    ) -> StateBackendResult<Result<Vec<String>, extent_alloc::ExtentAllocError>>;

    /// Drop a client's admission (the durable half of the functional
    /// fence). Returns `(evicted_nqns, remaining_desired_list)`.
    async fn block_host_evict(
        &self,
        volume: &str,
        client_id: u64,
    ) -> StateBackendResult<Result<(Vec<String>, Vec<String>), extent_alloc::ExtentAllocError>>;

    /// The volume's desired allow-list (distinct, sorted).
    async fn block_hosts(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<Vec<String>, extent_alloc::ExtentAllocError>>;

    /// Record a NODE's attachment (CSI ControllerPublish) on the
    /// volume's desired allow-list — the pre-NFS admission that lets the
    /// csi-node's `nvme connect` succeed before the first LAYOUTGET.
    /// Refused (`FencedClient`) while any fence record names the NQN.
    async fn block_node_attach(
        &self,
        volume: &str,
        host_nqn: &str,
        node_name: &str,
        now_unix: i64,
    ) -> StateBackendResult<Result<Vec<String>, extent_alloc::ExtentAllocError>>;

    /// Drop a node's attach row (ControllerUnpublish). Returns
    /// `(row_removed, remaining_desired_list)`; idempotent.
    async fn block_node_detach(
        &self,
        volume: &str,
        host_nqn: &str,
    ) -> StateBackendResult<Result<(bool, Vec<String>), extent_alloc::ExtentAllocError>>;

    /// Every live block-layout initiator on this MDS, across all
    /// volumes — the maintenance roller's view of who would lose their
    /// device if this node's spdk-tgt restarted (design §11).
    ///
    /// Cross-volume by design: the roller asks about a NODE, and a node
    /// hosts one target serving every block volume on the shard.
    /// Record that `client_id` cached `volume`'s pNFS device and which
    /// notifications it accepted. Idempotent; the newest mask wins.
    async fn device_notify_put(&self, rec: &DeviceNotifyRecord) -> StateBackendResult<()>;

    /// Everyone to tell when `volume`'s device changes: `(client_id,
    /// notify_mask)`. The session is NOT here — it is resolved at send
    /// time, because a back-channel cannot be persisted.
    async fn device_notify_list(&self, volume: &str) -> StateBackendResult<Vec<(u64, u32)>>;

    /// Forget one client's row, or the whole volume's book (`None`).
    async fn device_notify_forget(
        &self,
        volume: &str,
        client_id: Option<u64>,
    ) -> StateBackendResult<()>;

    async fn block_initiators(
        &self,
    ) -> StateBackendResult<Result<Vec<extent_alloc::BlockInitiatorRow>, extent_alloc::ExtentAllocError>>;

    /// Write the durable fence record (the positive `fenced_clients`
    /// row). Captures the client's host_nqn from `block_hosts` — so it
    /// must run BEFORE the eviction — and returns it for the log.
    async fn block_fence_record(
        &self,
        volume: &str,
        client_id: u64,
        now_unix: i64,
    ) -> StateBackendResult<Result<String, extent_alloc::ExtentAllocError>>;

    /// Is this client fenced on this volume? The admission guard.
    async fn block_is_fenced(
        &self,
        volume: &str,
        client_id: u64,
    ) -> StateBackendResult<Result<bool, extent_alloc::ExtentAllocError>>;

    /// Every `(volume, client_id)` fence record — startup replay reads
    /// this to re-acquire EA-RO on each still-fenced volume.
    async fn block_fenced_all(
        &self,
    ) -> StateBackendResult<Result<Vec<(String, u64)>, extent_alloc::ExtentAllocError>>;

    /// Mark a client's fence DELIVERED — the reservation preempt was
    /// confirmed at the target. Licenses the reclaim to FREE the
    /// client's fenced extents instead of quarantining them
    /// (FreeRequiresDelivered). `true` if an undelivered record was
    /// marked.
    async fn block_fence_delivered(
        &self,
        volume: &str,
        client_id: u64,
        now_unix: i64,
    ) -> StateBackendResult<Result<bool, extent_alloc::ExtentAllocError>>;

    /// THE QUARANTINE SWEEP: free every parked range of the volume whose
    /// own remembered holders are ALL confirmed-excluded. Runs after the
    /// reconcile pass's preempt retry, which is what turns an
    /// unconfirmed fence into a confirmed one; without it a range parked
    /// by a fence that landed late leaks until an operator intervenes.
    /// Returns `(ranges, bytes)` released.
    async fn block_sweep_quarantine(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<(u64, u64), extent_alloc::ExtentAllocError>>;

    /// Every distinct (volume, client_id) pair holding grant rows —
    /// the lease sweep's durable candidate source.
    async fn block_grant_clients(
        &self,
    ) -> StateBackendResult<Result<Vec<(String, u64)>, extent_alloc::ExtentAllocError>>;

    /// The lease sweep's bulk return: delete every grant row the client
    /// holds on the volume, gated in-transaction on a CONFIRMED fence
    /// (refused as `UnconfirmedFence` otherwise). Returns rows removed.
    async fn block_revoke_client(
        &self,
        volume: &str,
        client_id: u64,
    ) -> StateBackendResult<Result<u64, extent_alloc::ExtentAllocError>>;

    /// A target announces where it can be dialed (design §12's target
    /// registry). Idempotent and level-triggered: called every reconcile
    /// pass, so a listener change converges without an operator.
    async fn block_target_register(
        &self,
        target_id: &str,
        traddr: &str,
        trsvcid: u16,
        now_unix: i64,
    ) -> StateBackendResult<Result<(), extent_alloc::ExtentAllocError>>;

    /// Every registered target — observability and the startup audit.
    async fn block_target_list(
        &self,
    ) -> StateBackendResult<Result<Vec<extent_alloc::BlockTargetRow>, extent_alloc::ExtentAllocError>>;

    /// Seat a volume at `composer` if it has no seat; returns the seat
    /// that stands either way (which the caller MUST compare — a seat
    /// naming someone else is never silently overwritten). Seating a
    /// volume for the first time is also its first assembly, so it
    /// grants the epoch-1 lease in the same transaction.
    async fn block_seat_volume(
        &self,
        volume: &str,
        composer: &str,
        now_unix: i64,
        lease_expires_unix: i64,
    ) -> StateBackendResult<Result<extent_alloc::BlockSeat, extent_alloc::ExtentAllocError>>;

    /// Grant the serving lease for a composition — ASSEMBLY's act, and
    /// the only way a lease comes into being.
    async fn block_lease_grant(
        &self,
        volume: &str,
        epoch: i64,
        holder: &str,
        expires_unix: i64,
    ) -> StateBackendResult<Result<extent_alloc::BlockLease, extent_alloc::ExtentAllocError>>;

    /// Extend a standing lease, RECORD-CONDITIONED: refused for a holder
    /// the seat no longer names (however healthy it is) and for one the
    /// seat names but assembly has not yet granted.
    async fn block_lease_renew(
        &self,
        volume: &str,
        holder: &str,
        expires_unix: i64,
    ) -> StateBackendResult<Result<extent_alloc::BlockLease, extent_alloc::ExtentAllocError>>;

    /// The standing lease on a volume — where the eviction horizon is
    /// read from.
    async fn block_lease(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<Option<extent_alloc::BlockLease>, extent_alloc::ExtentAllocError>>;

    /// Every lease a target holds: the dead-man's work list.
    async fn block_leases_held(
        &self,
        holder: &str,
    ) -> StateBackendResult<Result<Vec<extent_alloc::BlockLease>, extent_alloc::ExtentAllocError>>;

    /// Surrender a lease — what the dead-man does once it has suspended
    /// the export.
    async fn block_lease_drop(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<bool, extent_alloc::ExtentAllocError>>;

    /// The volume's seat alone — who composes it, without asking where
    /// that composer answers. What the export reconciler's converge
    /// guard needs: it can only configure the tgt on its own socket, so
    /// it wants WHO, never WHERE.
    async fn block_volume_seat(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<Option<extent_alloc::BlockSeat>, extent_alloc::ExtentAllocError>>;

    /// THE RESOLUTION: volume → seat → dialable coordinates, in one
    /// read. `UnseatedVolume` / `UnknownComposer` are refusals, never
    /// invitations to fall back on a configured address.
    async fn block_resolve_target(
        &self,
        volume: &str,
    ) -> StateBackendResult<
        Result<(extent_alloc::BlockSeat, extent_alloc::BlockTargetRow), extent_alloc::ExtentAllocError>,
    >;

    /// Every seat, for the startup audit.
    async fn block_seat_list(
        &self,
    ) -> StateBackendResult<Result<Vec<extent_alloc::BlockSeat>, extent_alloc::ExtentAllocError>>;

    /// THE PROMOTION CAS: move the seat to `candidate` if it still reads
    /// what the caller saw, the candidate is registered, and its leg is
    /// in sync. The epoch advances by exactly one.
    async fn block_promote(
        &self,
        volume: &str,
        expected_epoch: i64,
        expected_composer: &str,
        candidate: &str,
        now_unix: i64,
    ) -> StateBackendResult<Result<extent_alloc::BlockSeat, extent_alloc::ExtentAllocError>>;

    /// The volume's legs and their sync marks — the election gate's
    /// input, and what a promotion refusal is explained against.
    async fn block_legs(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<Vec<extent_alloc::BlockLeg>, extent_alloc::ExtentAllocError>>;

    /// Move a leg's sync mark. The degrade barrier and the rebuild will
    /// both use this; seating does NOT (an insert-if-absent there, so a
    /// converge can never clear a stale mark).
    async fn block_leg_mark(
        &self,
        volume: &str,
        target_id: &str,
        sync_state: &str,
        now_unix: i64,
    ) -> StateBackendResult<Result<(), extent_alloc::ExtentAllocError>>;

    /// Clear a client's fence (release / lease recovery). `true` if a
    /// record was removed.
    async fn block_unfence(
        &self,
        volume: &str,
        client_id: u64,
    ) -> StateBackendResult<Result<bool, extent_alloc::ExtentAllocError>>;

    async fn delete_volume_geometry(&self, volume: &str) -> StateBackendResult<()>;

    // id↔path mappings behind v2 (id-based) metadata filehandles.
    // put upserts on file_id (RENAME re-writes the path in place).
    async fn put_fh_mapping(&self, m: &FhMappingRecord) -> StateBackendResult<()>;
    async fn list_fh_mappings(&self) -> StateBackendResult<Vec<FhMappingRecord>>;
    async fn delete_fh_mapping(&self, file_id: u64) -> StateBackendResult<()>;

    /// Atomically bump the persisted instance counter and return the
    /// new value. Called once at MDS start; the value is mixed into
    /// device-id prefixes so post-restart device ids never collide
    /// with pre-restart ones. Old client caches see `STALE_DEVICEID`
    /// and re-fetch — much better than silent identity collision.
    async fn increment_instance_counter(&self) -> StateBackendResult<u64>;

    /// Read the current persisted instance counter without mutating
    /// it. Mostly for diagnostics + tests.
    async fn get_instance_counter(&self) -> StateBackendResult<u64>;

    /// Returns the persistent per-deployment server identifier.
    /// **Distinct from `instance_counter`** — the counter increments
    /// on every restart (so old device-id caches go stale and clients
    /// re-fetch); this id is generated once at DB-creation time and
    /// reused for the lifetime of the state.db. It's what
    /// `FileHandleManager::instance_id` stamps into every NFSv4 file
    /// handle so cached FHs SURVIVE an MDS restart instead of erroring
    /// with `NFS4ERR_BADHANDLE`.
    ///
    /// First-call semantics: generate a non-zero random `u64` and
    /// persist it atomically (INSERT-OR-IGNORE pattern in SQLite,
    /// `OnceLock` in MemoryBackend); subsequent calls return the same
    /// value byte-for-byte across process lifetimes.
    async fn get_or_init_server_id(&self) -> StateBackendResult<u64>;
}

#[cfg(test)]
mod tests {
    //! Trait-level tests — anything that should hold for *every*
    //! backend impl. Each backend module has its own tests for impl-
    //! specific behaviour (e.g. SqliteBackend's restart survival).
    use super::*;

    /// Round-trip every record type through a backend, then compare.
    /// Generic over backend so SqliteBackend in B.2 can reuse this.
    pub(crate) async fn round_trip_all<B: StateBackend>(b: &B) {
        let cs = CachedCreateSessionResRecord {
            session_id: [9u8; 16],
            sequence: 7,
            flags: 0x101,
            fore_max_request_size: 4096,
            fore_max_response_size: 4096,
            fore_max_response_size_cached: 1024,
            fore_max_operations: 16,
            fore_max_requests: 8,
            back_max_request_size: 4096,
            back_max_response_size: 4096,
            back_max_response_size_cached: 0,
            back_max_operations: 2,
            back_max_requests: 1,
        };
        let client = ClientRecord {
            client_id: 42,
            owner: b"owner-bytes".to_vec(),
            verifier: 0xdead_beef,
            server_owner: "flint-pnfs".into(),
            server_scope: b"flint-pnfs-mds".to_vec(),
            sequence_id: 3,
            flags: 0x4000_0000,
            principal: b"alice@FLINT".to_vec(),
            confirmed: true,
            last_cs_sequence: Some(7),
            cs_cached_res: Some(cs.clone()),
            initial_cs_sequence: 1,
            reclaim_complete: false,
        };
        b.put_client(&client).await.unwrap();
        assert_eq!(b.get_client(42).await.unwrap(), Some(client.clone()));

        let session = SessionRecord {
            session_id: [9u8; 16],
            client_id: 42,
            sequence: 7,
            flags: 1,
            fore_chan_maxrequestsize: 4096,
            fore_chan_maxresponsesize: 4096,
            fore_chan_maxresponsesize_cached: 1024,
            fore_chan_maxops: 16,
            fore_chan_maxrequests: 8,
            cb_program: 0x4000_0001,
        };
        b.put_session(&session).await.unwrap();
        assert_eq!(b.get_session(&[9u8; 16]).await.unwrap(), Some(session.clone()));

        let stateid = StateIdRecord {
            other: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            seqid: 5,
            state_type: StateTypeRecord::Open,
            client_id: 42,
            filehandle: Some(b"/foo/bar".to_vec()),
            revoked: false,
        };
        b.put_stateid(&stateid).await.unwrap();
        assert_eq!(
            b.get_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap(),
            Some(stateid.clone())
        );

        let lock = LockRecord {
            other: [2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            seqid: 1,
            client_id: 42,
            owner: b"lock-owner-1".to_vec(),
            filehandle: b"/foo/bar".to_vec(),
            lock_type: 2, // WRITE_LT
            offset: 4096,
            length: 0, // to EOF
        };
        b.put_lock(&lock).await.unwrap();
        assert_eq!(
            b.get_lock(&[2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap(),
            Some(lock.clone())
        );

        let layout = LayoutRecord {
            stateid: [7u8; 16],
            owner_client_id: 42,
            owner_session_id: [9u8; 16],
            owner_fsid: 100,
            filehandle: vec![0xCA, 0xFE, 0xBA, 0xBE],
            segments: vec![
                LayoutSegmentRecord {
                    offset: 0,
                    length: 8 * 1024 * 1024,
                    iomode: IoModeRecord::ReadWrite,
                    device_id: "ds-1".into(),
                    stripe_index: 0,
                    pattern_offset: 0,
                },
                LayoutSegmentRecord {
                    offset: 8 * 1024 * 1024,
                    length: 8 * 1024 * 1024,
                    iomode: IoModeRecord::ReadWrite,
                    device_id: "ds-2".into(),
                    stripe_index: 1,
                    pattern_offset: 0,
                },
            ],
            iomode: IoModeRecord::ReadWrite,
            return_on_close: true,
            file_ident: "id:00000000cafed00d".into(),
        };
        b.put_layout(&layout).await.unwrap();
        // Round-trip includes file_ident: a backend that dropped it
        // would leave every restored layout unrecallable on truncate.
        assert_eq!(b.get_layout(&[7u8; 16]).await.unwrap(), Some(layout.clone()));

        let placement = PlacementRecord {
            file_key: "vol-1/data/train.bin".into(),
            stripe_size: 8 * 1024 * 1024,
            device_ids: vec!["ds-1".into(), "ds-2".into(), "ds-3".into()],
            file_id: 0,
            truncate_pending: None,
            truncate_since_unix: None,
        };
        b.put_placement(&placement).await.unwrap();
        assert_eq!(
            b.get_placement("vol-1/data/train.bin").await.unwrap(),
            Some(placement.clone())
        );

        // list_* surfaces what we put in. Use len-then-contains rather
        // than equality so the test is robust to backend ordering.
        assert_eq!(b.list_clients().await.unwrap().len(), 1);
        assert_eq!(b.list_sessions().await.unwrap().len(), 1);
        assert_eq!(b.list_stateids().await.unwrap().len(), 1);
        assert_eq!(b.list_locks().await.unwrap().len(), 1);
        assert_eq!(b.list_layouts().await.unwrap().len(), 1);
        assert_eq!(b.list_placements().await.unwrap().len(), 1);

        // Deletes are idempotent — second delete is Ok, not Err.
        b.delete_client(42).await.unwrap();
        b.delete_client(42).await.unwrap();
        assert!(b.get_client(42).await.unwrap().is_none());

        b.delete_session(&[9u8; 16]).await.unwrap();
        b.delete_session(&[9u8; 16]).await.unwrap();
        assert!(b.get_session(&[9u8; 16]).await.unwrap().is_none());

        b.delete_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap();
        b.delete_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap();
        assert!(b.get_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap().is_none());

        b.delete_lock(&[2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap();
        b.delete_lock(&[2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap();
        assert!(b.get_lock(&[2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap().is_none());

        b.delete_layout(&[7u8; 16]).await.unwrap();
        b.delete_layout(&[7u8; 16]).await.unwrap();
        assert!(b.get_layout(&[7u8; 16]).await.unwrap().is_none());

        b.delete_placement("vol-1/data/train.bin").await.unwrap();
        b.delete_placement("vol-1/data/train.bin").await.unwrap();
        assert!(b.get_placement("vol-1/data/train.bin").await.unwrap().is_none());
    }

    /// Instance counter starts at 0, increments monotonically, and
    /// `get_instance_counter` reflects the latest value. Generic so
    /// SqliteBackend in B.2 can reuse — that impl additionally verifies
    /// the counter survives a backend re-open over the same file.
    pub(crate) async fn instance_counter_monotonic<B: StateBackend>(b: &B) {
        assert_eq!(b.get_instance_counter().await.unwrap(), 0);
        assert_eq!(b.increment_instance_counter().await.unwrap(), 1);
        assert_eq!(b.increment_instance_counter().await.unwrap(), 2);
        assert_eq!(b.get_instance_counter().await.unwrap(), 2);
    }

    /// `get_or_init_server_id` returns a non-zero value, the same
    /// value on every subsequent call, and (importantly) NEVER
    /// returns 0 — `FileHandleManager` treats 0-stamped handles as
    /// uninitialised. Generic over backend so SqliteBackend's test
    /// reuses this; SqliteBackend's separate restart-survival test
    /// proves the value also survives `open()` round-trips.
    pub(crate) async fn server_id_stable_and_nonzero<B: StateBackend>(b: &B) {
        let id1 = b.get_or_init_server_id().await.unwrap();
        let id2 = b.get_or_init_server_id().await.unwrap();
        let id3 = b.get_or_init_server_id().await.unwrap();
        assert_ne!(id1, 0, "server_id must be non-zero");
        assert_eq!(id1, id2, "server_id must be stable");
        assert_eq!(id2, id3, "server_id must be stable");
    }

    /// Upserts overwrite. Important — the higher layer calls
    /// `put_client` after `mark_confirmed` to persist the bit flip,
    /// expecting the new record to replace the old.
    pub(crate) async fn put_overwrites<B: StateBackend>(b: &B) {
        let mut c = ClientRecord {
            client_id: 1,
            owner: b"o".to_vec(),
            verifier: 1,
            server_owner: "s".into(),
            server_scope: b"sc".to_vec(),
            sequence_id: 1,
            flags: 0,
            principal: b"p".to_vec(),
            confirmed: false,
            last_cs_sequence: None,
            cs_cached_res: None,
            initial_cs_sequence: 1,
            reclaim_complete: false,
        };
        b.put_client(&c).await.unwrap();
        c.confirmed = true;
        c.last_cs_sequence = Some(2);
        b.put_client(&c).await.unwrap();
        let got = b.get_client(1).await.unwrap().unwrap();
        assert!(got.confirmed);
        assert_eq!(got.last_cs_sequence, Some(2));
        assert_eq!(b.list_clients().await.unwrap().len(), 1, "upsert, not append");
    }
}
