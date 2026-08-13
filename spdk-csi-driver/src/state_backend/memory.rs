//! In-process [`StateBackend`] implementation backed by `DashMap`s and
//! an `AtomicU64`. Behavioural parity with today's
//! `Client`/`Session`/`StateId`/`Layout` managers — choose this when
//! you don't care about restart survival (tests, Lima dev work, smoke
//! runs). Production deployments should pick the SQLite backend
//! shipping in B.2.
//!
//! All operations are constant-time on the underlying DashMap shard,
//! so this stays on the hot path with no measurable overhead vs. the
//! current direct-DashMap accesses. The boundary code (B.3) caches
//! reads in the existing per-manager DashMaps anyway, so the trait
//! cost is paid only on writes.

use super::{
    ClientRecord, FhMappingRecord, LayoutRecord, LockRecord, PlacementRecord, SessionRecord,
    VolumeGeometryRecord,
    StateBackend, StateBackendError, StateBackendResult, StateIdRecord, WriteOp,
};
use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// In-memory [`StateBackend`]. All maps shard internally so concurrent
/// readers and writers don't contend on a global lock.
#[derive(Default)]
pub struct MemoryBackend {
    clients: DashMap<u64, ClientRecord>,
    sessions: DashMap<[u8; 16], SessionRecord>,
    stateids: DashMap<[u8; 12], StateIdRecord>,
    locks: DashMap<[u8; 12], LockRecord>,
    layouts: DashMap<[u8; 16], LayoutRecord>,
    placements: DashMap<String, PlacementRecord>,
    volume_geometry: DashMap<String, VolumeGeometryRecord>,
    fh_mappings: DashMap<u64, FhMappingRecord>,
    /// The target registry and the per-volume serving seats (design
    /// §12). See the impl block for why these two are real here while
    /// the admission tables are not.
    block_targets: DashMap<String, super::extent_alloc::BlockTargetRow>,
    block_seats: DashMap<String, super::extent_alloc::BlockSeat>,
    instance_counter: AtomicU64,
    /// Lazily-initialised per-deployment server id. `OnceLock` makes
    /// the first call atomic (no two threads observe different values)
    /// without paying for a mutex on every read.
    server_id: OnceLock<u64>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StateBackend for MemoryBackend {
    /// Applied inline — DashMap ops are sync and infallible, so the
    /// "queue" is the call itself. Call order = apply order trivially.
    fn enqueue_write(&self, op: WriteOp) {
        match op {
            WriteOp::PutClient(c) => {
                self.clients.insert(c.client_id, c);
            }
            WriteOp::DeleteClient(id) => {
                self.clients.remove(&id);
            }
            WriteOp::PutSession(s) => {
                self.sessions.insert(s.session_id, s);
            }
            WriteOp::DeleteSession(id) => {
                self.sessions.remove(&id);
            }
            WriteOp::PutStateid(s) => {
                self.stateids.insert(s.other, s);
            }
            WriteOp::DeleteStateid(o) => {
                self.stateids.remove(&o);
            }
            WriteOp::PutLock(l) => {
                self.locks.insert(l.other, l);
            }
            WriteOp::DeleteLock(o) => {
                self.locks.remove(&o);
            }
            WriteOp::PutLayout(l) => {
                self.layouts.insert(l.stateid, l);
            }
            WriteOp::DeleteLayout(s) => {
                self.layouts.remove(&s);
            }
            WriteOp::PutPlacement(p) => {
                self.placements.insert(p.file_key.clone(), p);
            }
            WriteOp::DeletePlacement(k) => {
                self.placements.remove(&k);
            }
            WriteOp::PutVolumeGeometry(g) => {
                self.volume_geometry.insert(g.volume.clone(), g);
            }
            WriteOp::DeleteVolumeGeometry(v) => {
                self.volume_geometry.remove(&v);
            }
            // Device notifications exist only for the scsi class, which
            // this backend refuses outright — so there is nothing to
            // remember and nothing to forget.
            WriteOp::PutDeviceNotify(_) | WriteOp::DeleteDeviceNotify(..) => {}
            WriteOp::PutFhMapping(m) => {
                self.fh_mappings.insert(m.file_id, m);
            }
            WriteOp::DeleteFhMapping(id) => {
                self.fh_mappings.remove(&id);
            }
        }
    }

    async fn put_client(&self, c: &ClientRecord) -> StateBackendResult<()> {
        self.clients.insert(c.client_id, c.clone());
        Ok(())
    }

    async fn get_client(&self, client_id: u64) -> StateBackendResult<Option<ClientRecord>> {
        Ok(self.clients.get(&client_id).map(|r| r.clone()))
    }

    async fn list_clients(&self) -> StateBackendResult<Vec<ClientRecord>> {
        Ok(self.clients.iter().map(|r| r.clone()).collect())
    }

    async fn delete_client(&self, client_id: u64) -> StateBackendResult<()> {
        self.clients.remove(&client_id);
        Ok(())
    }

    async fn put_session(&self, s: &SessionRecord) -> StateBackendResult<()> {
        self.sessions.insert(s.session_id, s.clone());
        Ok(())
    }

    async fn get_session(&self, session_id: &[u8; 16]) -> StateBackendResult<Option<SessionRecord>> {
        Ok(self.sessions.get(session_id).map(|r| r.clone()))
    }

    async fn list_sessions(&self) -> StateBackendResult<Vec<SessionRecord>> {
        Ok(self.sessions.iter().map(|r| r.clone()).collect())
    }

    async fn delete_session(&self, session_id: &[u8; 16]) -> StateBackendResult<()> {
        self.sessions.remove(session_id);
        Ok(())
    }

    async fn put_stateid(&self, s: &StateIdRecord) -> StateBackendResult<()> {
        self.stateids.insert(s.other, s.clone());
        Ok(())
    }

    async fn get_stateid(&self, other: &[u8; 12]) -> StateBackendResult<Option<StateIdRecord>> {
        Ok(self.stateids.get(other).map(|r| r.clone()))
    }

    async fn list_stateids(&self) -> StateBackendResult<Vec<StateIdRecord>> {
        Ok(self.stateids.iter().map(|r| r.clone()).collect())
    }

    async fn delete_stateid(&self, other: &[u8; 12]) -> StateBackendResult<()> {
        self.stateids.remove(other);
        Ok(())
    }

    async fn put_lock(&self, l: &LockRecord) -> StateBackendResult<()> {
        self.locks.insert(l.other, l.clone());
        Ok(())
    }

    async fn get_lock(&self, other: &[u8; 12]) -> StateBackendResult<Option<LockRecord>> {
        Ok(self.locks.get(other).map(|r| r.clone()))
    }

    async fn list_locks(&self) -> StateBackendResult<Vec<LockRecord>> {
        Ok(self.locks.iter().map(|r| r.clone()).collect())
    }

    async fn delete_lock(&self, other: &[u8; 12]) -> StateBackendResult<()> {
        self.locks.remove(other);
        Ok(())
    }

    async fn put_layout(&self, l: &LayoutRecord) -> StateBackendResult<()> {
        self.layouts.insert(l.stateid, l.clone());
        Ok(())
    }

    async fn get_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<Option<LayoutRecord>> {
        Ok(self.layouts.get(stateid).map(|r| r.clone()))
    }

    async fn list_layouts(&self) -> StateBackendResult<Vec<LayoutRecord>> {
        Ok(self.layouts.iter().map(|r| r.clone()).collect())
    }

    async fn delete_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<()> {
        self.layouts.remove(stateid);
        Ok(())
    }

    async fn put_placement(&self, p: &PlacementRecord) -> StateBackendResult<()> {
        self.placements.insert(p.file_key.clone(), p.clone());
        Ok(())
    }

    async fn get_placement(&self, file_key: &str) -> StateBackendResult<Option<PlacementRecord>> {
        Ok(self.placements.get(file_key).map(|r| r.clone()))
    }

    async fn list_placements(&self) -> StateBackendResult<Vec<PlacementRecord>> {
        Ok(self.placements.iter().map(|r| r.clone()).collect())
    }

    async fn delete_placement(&self, file_key: &str) -> StateBackendResult<()> {
        self.placements.remove(file_key);
        Ok(())
    }

    async fn put_volume_geometry(&self, g: &VolumeGeometryRecord) -> StateBackendResult<()> {
        self.volume_geometry.insert(g.volume.clone(), g.clone());
        Ok(())
    }

    async fn get_volume_geometry(
        &self,
        volume: &str,
    ) -> StateBackendResult<Option<VolumeGeometryRecord>> {
        Ok(self.volume_geometry.get(volume).map(|r| r.clone()))
    }

    async fn list_volume_geometry(&self) -> StateBackendResult<Vec<VolumeGeometryRecord>> {
        Ok(self.volume_geometry.iter().map(|r| r.clone()).collect())
    }

    async fn delete_volume_geometry(&self, volume: &str) -> StateBackendResult<()> {
        self.volume_geometry.remove(volume);
        Ok(())
    }

    // ── Extent allocator: REFUSED on the memory backend, deliberately.
    // §8: extent durability is sqlite-only and must be STRONGER than the
    // rest of the state — an extent map lost while data survives is
    // F67's silent zeros, and a memory-backed map IS that loss, one
    // restart away. Block-class volumes fail loudly at provision time
    // on this backend, never at I/O time.

    async fn extent_register_volume(
        &self,
        _volume: &str,
        _size_ceiling: u64,
    ) -> StateBackendResult<Result<(), crate::state_backend::extent_alloc::ExtentAllocError>> {
        Err(StateBackendError::Storage(
            "block-class volumes require the durable sqlite backend \
             (extent maps must survive restart — design doc §8)"
                .into(),
        ))
    }

    async fn extent_expand_volume(
        &self,
        _volume: &str,
        _new_ceiling: u64,
    ) -> StateBackendResult<Result<u64, crate::state_backend::extent_alloc::ExtentAllocError>> {
        Err(StateBackendError::Storage(
            "block-class volumes require the durable sqlite backend \
             (extent maps must survive restart — design doc §8)"
                .into(),
        ))
    }

    async fn extent_volume_headroom(
        &self,
        _volume: &str,
    ) -> StateBackendResult<Result<u64, crate::state_backend::extent_alloc::ExtentAllocError>> {
        Err(StateBackendError::Storage(
            "block-class volumes require the durable sqlite backend \
             (extent maps must survive restart — design doc §8)"
                .into(),
        ))
    }

    async fn extent_committed_end(
        &self,
        _volume: &str,
        _file_id: u64,
    ) -> StateBackendResult<Result<u64, crate::state_backend::extent_alloc::ExtentAllocError>> {
        Err(StateBackendError::Storage(
            "block-class volumes require the durable sqlite backend \
             (extent maps must survive restart — design doc §8)"
                .into(),
        ))
    }

    async fn extent_grant(
        &self,
        _volume: &str,
        _file_id: u64,
        _client_id: u64,
        _logical_offset: u64,
        _length: u64,
        _fresh_only: bool,
    ) -> StateBackendResult<
        Result<
            Vec<crate::state_backend::extent_alloc::GrantedExtent>,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        Err(StateBackendError::Storage(
            "extent grant on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn extent_commit(
        &self,
        _volume: &str,
        _file_id: u64,
        _client_id: u64,
        _logical_offset: u64,
        _length: u64,
    ) -> StateBackendResult<Result<u64, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Err(StateBackendError::Storage(
            "extent commit on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn extent_layout_return(
        &self,
        _volume: &str,
        _file_id: u64,
        _client_id: u64,
        _logical_offset: u64,
        _length: u64,
    ) -> StateBackendResult<Result<usize, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Err(StateBackendError::Storage(
            "extent return on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn extent_reclaim_snapshot(
        &self,
        _volume: &str,
        _file_id: u64,
        _logical_offset: u64,
        _length: u64,
    ) -> StateBackendResult<Result<Vec<u64>, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Err(StateBackendError::Storage(
            "extent snapshot on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn extent_fence_client(
        &self,
        _volume: &str,
        _client_id: u64,
    ) -> StateBackendResult<Result<usize, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Err(StateBackendError::Storage(
            "extent fence on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn extent_reclaim_complete(
        &self,
        _volume: &str,
        _file_id: u64,
        _logical_offset: u64,
        _length: u64,
        _now_unix: i64,
    ) -> StateBackendResult<
        Result<
            crate::state_backend::extent_alloc::FreeOutcome,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        Err(StateBackendError::Storage(
            "extent reclaim on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn extent_drop_volume(
        &self,
        volume: &str,
    ) -> StateBackendResult<Result<u64, crate::state_backend::extent_alloc::ExtentAllocError>> {
        // Dropping rows that cannot exist is a clean no-op: DeleteVolume
        // calls this unconditionally, and memory-backed MDSes never had
        // a block-class volume to begin with (provision refuses).
        //
        // The seat is the exception, because seating is real here (see
        // the registry impls below) — and it goes with the volume for
        // the same reason it does in sqlite: a re-created namesake must
        // be seated afresh, never inherit an epoch.
        Ok(Ok(self.block_seats.remove(volume).map(|_| 1).unwrap_or(0)))
    }

    async fn extent_grant_read(
        &self,
        _volume: &str,
        _file_id: u64,
        _client_id: u64,
        _logical_offset: u64,
        _length: u64,
    ) -> StateBackendResult<
        Result<
            Vec<crate::state_backend::extent_alloc::GrantedExtent>,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        Err(StateBackendError::Storage(
            "read grant on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn block_host_admit(
        &self,
        _volume: &str,
        _client_id: u64,
        _host_nqn: &str,
        _now_unix: i64,
    ) -> StateBackendResult<Result<Vec<String>, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Err(StateBackendError::Storage(
            "block host admission on the memory backend — block-class volumes require sqlite"
                .into(),
        ))
    }

    async fn block_host_evict(
        &self,
        _volume: &str,
        _client_id: u64,
    ) -> StateBackendResult<
        Result<(Vec<String>, Vec<String>), crate::state_backend::extent_alloc::ExtentAllocError>,
    > {
        // Same shape as extent_drop_volume: the reclaim driver evicts
        // unconditionally after a fence, and there is nothing to evict
        // on a backend that can hold no block volumes.
        Ok(Ok((Vec::new(), Vec::new())))
    }

    async fn block_hosts(
        &self,
        _volume: &str,
    ) -> StateBackendResult<Result<Vec<String>, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Ok(Ok(Vec::new()))
    }

    async fn block_node_attach(
        &self,
        _volume: &str,
        _host_nqn: &str,
        _node_name: &str,
        _now_unix: i64,
    ) -> StateBackendResult<Result<Vec<String>, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Err(StateBackendError::Storage(
            "block node attach on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn block_node_detach(
        &self,
        _volume: &str,
        _host_nqn: &str,
    ) -> StateBackendResult<
        Result<(bool, Vec<String>), crate::state_backend::extent_alloc::ExtentAllocError>,
    > {
        // Detach replays are tolerated everywhere; a backend that can
        // hold no block volumes has nothing to detach.
        Ok(Ok((false, Vec::new())))
    }

    async fn device_notify_put(
        &self,
        _rec: &crate::state_backend::DeviceNotifyRecord,
    ) -> StateBackendResult<()> {
        Ok(())
    }

    async fn device_notify_list(&self, _volume: &str) -> StateBackendResult<Vec<(u64, u32)>> {
        // Empty is truthful: a block-class volume cannot exist here, so
        // no client can have cached one of its devices.
        Ok(Vec::new())
    }

    async fn device_notify_forget(
        &self,
        _volume: &str,
        _client_id: Option<u64>,
    ) -> StateBackendResult<()> {
        Ok(())
    }

    async fn block_initiators(
        &self,
    ) -> StateBackendResult<
        Result<
            Vec<crate::state_backend::extent_alloc::BlockInitiatorRow>,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        // Empty is the TRUTH here, not a shrug: every path that could
        // mint an initiator (create, attach, admit) refuses on this
        // backend, so a memory-backed MDS provably has none. The
        // roller reads an empty list as permission to roll, so this
        // must never become a stand-in for "don't know" — an MDS that
        // cannot be asked is an error, and the roller treats it as one.
        Ok(Ok(Vec::new()))
    }

    async fn block_fence_record(
        &self,
        _volume: &str,
        _client_id: u64,
        _now_unix: i64,
    ) -> StateBackendResult<Result<String, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Err(StateBackendError::Storage(
            "block fence record on the memory backend — block-class volumes require sqlite".into(),
        ))
    }

    async fn block_is_fenced(
        &self,
        _volume: &str,
        _client_id: u64,
    ) -> StateBackendResult<Result<bool, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        // A backend that can hold no block volumes fences no one.
        Ok(Ok(false))
    }

    async fn block_fenced_all(
        &self,
    ) -> StateBackendResult<
        Result<Vec<(String, u64)>, crate::state_backend::extent_alloc::ExtentAllocError>,
    > {
        Ok(Ok(Vec::new()))
    }

    async fn block_fence_delivered(
        &self,
        _volume: &str,
        _client_id: u64,
        _now_unix: i64,
    ) -> StateBackendResult<Result<bool, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        // No fence records exist here to mark (see block_fence_record).
        Ok(Ok(false))
    }

    async fn block_sweep_quarantine(
        &self,
        _volume: &str,
    ) -> StateBackendResult<
        Result<(u64, u64), crate::state_backend::extent_alloc::ExtentAllocError>,
    > {
        // Nothing is ever parked here: block volumes cannot exist on
        // this backend, so (0, 0) is TRUTHFUL — "no ranges, no bytes",
        // not a stand-in for "don't know".
        Ok(Ok((0, 0)))
    }

    async fn block_grant_clients(
        &self,
    ) -> StateBackendResult<
        Result<Vec<(String, u64)>, crate::state_backend::extent_alloc::ExtentAllocError>,
    > {
        Ok(Ok(Vec::new()))
    }

    async fn block_revoke_client(
        &self,
        _volume: &str,
        _client_id: u64,
    ) -> StateBackendResult<Result<u64, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        // No block volumes here (see block_fence_record) — nothing to
        // revoke, and the fence gate could never have been confirmed.
        Ok(Ok(0))
    }

    async fn block_unfence(
        &self,
        _volume: &str,
        _client_id: u64,
    ) -> StateBackendResult<Result<bool, crate::state_backend::extent_alloc::ExtentAllocError>>
    {
        Ok(Ok(false))
    }

    async fn put_fh_mapping(&self, m: &FhMappingRecord) -> StateBackendResult<()> {
        self.fh_mappings.insert(m.file_id, m.clone());
        Ok(())
    }

    async fn list_fh_mappings(&self) -> StateBackendResult<Vec<FhMappingRecord>> {
        Ok(self.fh_mappings.iter().map(|r| r.clone()).collect())
    }

    async fn delete_fh_mapping(&self, file_id: u64) -> StateBackendResult<()> {
        self.fh_mappings.remove(&file_id);
        Ok(())
    }

    async fn increment_instance_counter(&self) -> StateBackendResult<u64> {
        // SeqCst rather than Relaxed: callers (B.4 startup logic) treat
        // the post-increment value as a fence — every persisted record
        // written *after* the counter increment must observe the new
        // value. SeqCst makes that intuitive even though Relaxed would
        // suffice for the counter alone, because the counter's value
        // is effectively published through unrelated DashMap writes.
        Ok(self.instance_counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    async fn get_instance_counter(&self) -> StateBackendResult<u64> {
        Ok(self.instance_counter.load(Ordering::SeqCst))
    }

    async fn get_or_init_server_id(&self) -> StateBackendResult<u64> {
        // `OnceLock::get_or_init` runs the closure exactly once per
        // backend instance, even under concurrent first-call races —
        // no dupe writes, no mutex on the steady-state read path.
        // `rand::random::<u64>() | 1` keeps the value non-zero so a
        // caller treating zero as "uninitialised" still works.
        Ok(*self.server_id.get_or_init(|| rand::random::<u64>() | 1))
    }

    // ── target registry / serving seats ───────────────────────────────
    //
    // These four are REAL here, unlike the admission tables above, and
    // the difference is not laziness. An admission is state about a
    // block-class VOLUME, which cannot exist on this backend — so
    // "empty" is the truth and a write is an error. A seat is MDS-side
    // bookkeeping about which target composes a name; the block-export
    // reconciler's own unit tests provision through this backend, and a
    // registry that answered "no" there would either force those tests
    // onto sqlite or, worse, tempt a fallback-to-constructor path into
    // existence — which is the exact defect the registry deletes.

    async fn block_target_register(
        &self,
        target_id: &str,
        traddr: &str,
        trsvcid: u16,
        now_unix: i64,
    ) -> StateBackendResult<Result<(), crate::state_backend::extent_alloc::ExtentAllocError>> {
        use crate::state_backend::extent_alloc::BlockTargetRow;
        let registered = self
            .block_targets
            .get(target_id)
            .map(|r| r.registered_unix)
            .unwrap_or(now_unix);
        self.block_targets.insert(
            target_id.to_string(),
            BlockTargetRow {
                target_id: target_id.to_string(),
                traddr: traddr.to_string(),
                trsvcid,
                registered_unix: registered,
                updated_unix: now_unix,
            },
        );
        Ok(Ok(()))
    }

    async fn block_target_list(
        &self,
    ) -> StateBackendResult<
        Result<
            Vec<crate::state_backend::extent_alloc::BlockTargetRow>,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        let mut out: Vec<_> = self.block_targets.iter().map(|e| e.value().clone()).collect();
        out.sort_by(|a, b| a.target_id.cmp(&b.target_id));
        Ok(Ok(out))
    }

    async fn block_seat_volume(
        &self,
        volume: &str,
        composer: &str,
        now_unix: i64,
    ) -> StateBackendResult<
        Result<
            crate::state_backend::extent_alloc::BlockSeat,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        use crate::state_backend::extent_alloc::BlockSeat;
        // Insert-if-absent, matching the sqlite transaction exactly: a
        // standing seat is returned unchanged so the caller can refuse.
        let seat = self
            .block_seats
            .entry(volume.to_string())
            .or_insert_with(|| BlockSeat {
                volume: volume.to_string(),
                epoch: 1,
                composer: composer.to_string(),
                seated_unix: now_unix,
            })
            .clone();
        Ok(Ok(seat))
    }

    async fn block_volume_seat(
        &self,
        volume: &str,
    ) -> StateBackendResult<
        Result<
            Option<crate::state_backend::extent_alloc::BlockSeat>,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        Ok(Ok(self.block_seats.get(volume).map(|e| e.value().clone())))
    }

    async fn block_resolve_target(
        &self,
        volume: &str,
    ) -> StateBackendResult<
        Result<
            (
                crate::state_backend::extent_alloc::BlockSeat,
                crate::state_backend::extent_alloc::BlockTargetRow,
            ),
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        use crate::state_backend::extent_alloc::ExtentAllocError as E;
        let Some(seat) = self.block_seats.get(volume).map(|e| e.value().clone()) else {
            return Ok(Err(E::UnseatedVolume));
        };
        let Some(target) = self.block_targets.get(&seat.composer).map(|e| e.value().clone()) else {
            return Ok(Err(E::UnknownComposer { composer: seat.composer }));
        };
        Ok(Ok((seat, target)))
    }

    async fn block_seat_list(
        &self,
    ) -> StateBackendResult<
        Result<
            Vec<crate::state_backend::extent_alloc::BlockSeat>,
            crate::state_backend::extent_alloc::ExtentAllocError,
        >,
    > {
        let mut out: Vec<_> = self.block_seats.iter().map(|e| e.value().clone()).collect();
        out.sort_by(|a, b| a.volume.cmp(&b.volume));
        Ok(Ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::tests::{
        instance_counter_monotonic, put_overwrites, round_trip_all,
        server_id_stable_and_nonzero,
    };

    #[tokio::test]
    async fn memory_round_trip_all_records() {
        let b = MemoryBackend::new();
        round_trip_all(&b).await;
    }

    #[tokio::test]
    async fn memory_instance_counter_monotonic() {
        let b = MemoryBackend::new();
        instance_counter_monotonic(&b).await;
    }

    #[tokio::test]
    async fn memory_put_is_upsert() {
        let b = MemoryBackend::new();
        put_overwrites(&b).await;
    }

    #[tokio::test]
    async fn memory_server_id_stable_within_lifetime() {
        let b = MemoryBackend::new();
        server_id_stable_and_nonzero(&b).await;
    }

    /// Concurrent writers on different keys don't lose updates and
    /// don't deadlock. The DashMap shard count is the actual mechanism;
    /// this is just a regression sentinel for "did someone wrap it in
    /// a global Mutex".
    #[tokio::test]
    async fn memory_concurrent_writes_no_loss() {
        use std::sync::Arc;
        let b = Arc::new(MemoryBackend::new());
        let mut tasks = Vec::new();
        for i in 0..64u64 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                let c = ClientRecord {
                    client_id: i,
                    owner: format!("c{}", i).into_bytes(),
                    verifier: i,
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
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(b.list_clients().await.unwrap().len(), 64);
    }

    /// Counter increments are atomic across many concurrent calls — no
    /// duplicate values, no skipped values. Catches the obvious bug of
    /// using `load + 1; store` instead of `fetch_add`.
    #[tokio::test]
    async fn memory_instance_counter_atomic() {
        use std::sync::Arc;
        let b = Arc::new(MemoryBackend::new());
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                b.increment_instance_counter().await.unwrap()
            }));
        }
        let mut seen: Vec<u64> = Vec::new();
        for t in tasks {
            seen.push(t.await.unwrap());
        }
        seen.sort();
        assert_eq!(seen, (1..=32).collect::<Vec<u64>>());
        assert_eq!(b.get_instance_counter().await.unwrap(), 32);
    }
}
