// NFSv4 Locking Operations
//
// NFSv4 integrates byte-range locking directly into the protocol,
// eliminating the need for separate NLM (Network Lock Manager).
//
// Lock Types:
// - READ: Shared lock (multiple readers allowed)
// - WRITE: Exclusive lock (single writer, no readers)
//
// Lock Lifecycle:
// 1. OPEN file → get open-stateid
// 2. LOCK range → get lock-stateid (derived from open-stateid)
// 3. I/O operations use lock-stateid
// 4. LOCKU to release lock
// 5. CLOSE file
//
// Lock Conflict Resolution:
// - NFSv4 queues conflicting lock requests (blocking locks)
// - Client can test for conflicts with LOCKT (non-blocking)
//
// Zero-Copy Design:
// - Lock metadata only, no data transfer
// - Locks stored in memory (HashMap)
// - Fast conflict detection with range overlap checks

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::compound::CompoundContext;
use crate::nfs::v4::state::{StateManager, StateType};
use crate::state_backend::{LockRecord, StateBackend, WriteOp};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Lock types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    /// Shared read lock (READ_LT = 1)
    Read = 1,

    /// Exclusive write lock (WRITE_LT = 2)
    Write = 2,

    /// Read lock to be write lock (READW_LT = 3)
    ReadWrite = 3,

    /// Write lock to be read lock (WRITEW_LT = 4)
    WriteRead = 4,
}

/// Lock range
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockRange {
    pub offset: u64,
    /// Byte count, with [`LockRange::TO_EOF`] (all 1s) meaning "from
    /// `offset` to the end of file" — the encoding RFC 8881 §18.10.3
    /// defines and the one a Linux client actually puts on the wire.
    ///
    /// A wire length of **0 is invalid** and is refused with
    /// NFS4ERR_INVAL at the operation handlers. It is still tolerated as
    /// to-EOF by [`LockRange::end`] purely so rows persisted by an older
    /// build (which wrongly used 0 as the sentinel) keep the meaning
    /// they were stored with instead of collapsing to an empty range.
    pub length: u64,
}

/// Shared byte-range validation for LOCK / LOCKT / LOCKU.
///
/// RFC 8881 §18.10.3 (and §18.11.3 / §18.12.3, which defer to it): a
/// length of zero is invalid, and a length that is not all 1s must not
/// overflow when added to the offset. All 1s means "to EOF" and is
/// exempt. Returning `Err(status)` means the caller must answer with it.
fn validate_lock_range(offset: u64, length: u64, op: &str) -> Result<(), Nfs4Status> {
    if length == 0 {
        warn!("{}: zero-length byte range (offset={}) — NFS4ERR_INVAL", op, offset);
        return Err(Nfs4Status::Inval);
    }
    if length != LockRange::TO_EOF && offset.checked_add(length).is_none() {
        warn!("{}: byte range overflow (offset={}, length={})", op, offset, length);
        return Err(Nfs4Status::Inval);
    }
    Ok(())
}

impl LockRange {
    /// RFC 8881 §18.10.3: a length of all 1s means "to the end of file".
    pub const TO_EOF: u64 = u64::MAX;

    /// Exclusive end of the range, saturating at [`u64::MAX`].
    ///
    /// `offset + length` must never be a bare add: the to-EOF sentinel
    /// IS `u64::MAX`, so any non-zero offset overflows it — which is
    /// precisely the arithmetic that made the old code reject every
    /// "lock from offset N to EOF" request (nfstest_lock, 2026-08-23).
    fn end(&self) -> u64 {
        if self.length == Self::TO_EOF || self.length == 0 {
            u64::MAX
        } else {
            self.offset.saturating_add(self.length)
        }
    }

    /// Check if this range overlaps with another
    pub fn overlaps(&self, other: &LockRange) -> bool {
        self.offset < other.end() && other.offset < self.end()
    }

    /// Check if locks conflict (considering lock types)
    pub fn conflicts_with(&self, other: &LockRange, self_type: LockType, other_type: LockType) -> bool {
        // No overlap = no conflict
        if !self.overlaps(other) {
            return false;
        }

        // Both read locks = no conflict
        if matches!(self_type, LockType::Read) && matches!(other_type, LockType::Read) {
            return false;
        }

        // Any write lock = conflict
        true
    }
}

/// Active lock entry
#[derive(Debug, Clone)]
pub struct Lock {
    /// Lock stateid
    pub stateid: StateId,

    /// Client that owns this lock
    pub client_id: u64,

    /// Lock owner (within client)
    pub owner: Vec<u8>,

    /// File handle this lock is on
    pub filehandle: Vec<u8>,

    /// Lock type
    pub lock_type: LockType,

    /// Locked range
    pub range: LockRange,
}

impl Lock {
    fn to_record(&self) -> LockRecord {
        LockRecord {
            other: self.stateid.other,
            seqid: self.stateid.seqid,
            client_id: self.client_id,
            owner: self.owner.clone(),
            filehandle: self.filehandle.clone(),
            lock_type: self.lock_type as u32,
            offset: self.range.offset,
            length: self.range.length,
        }
    }

    /// Inverse of [`Lock::to_record`]. `None` on an unknown lock_type —
    /// the restore path skips (and logs) such rows rather than guessing
    /// a lock mode.
    fn from_record(r: &LockRecord) -> Option<Self> {
        let lock_type = match r.lock_type {
            1 => LockType::Read,
            2 => LockType::Write,
            3 => LockType::ReadWrite,
            4 => LockType::WriteRead,
            _ => return None,
        };
        Some(Lock {
            stateid: StateId { seqid: r.seqid, other: r.other },
            client_id: r.client_id,
            owner: r.owner.clone(),
            filehandle: r.filehandle.clone(),
            lock_type,
            range: LockRange { offset: r.offset, length: r.length },
        })
    }
}

/// Lock manager - tracks all active locks
///
/// LOCK-FREE DESIGN using DashMap:
/// - No global locks, only per-shard locks in DashMap
/// - Concurrent reads without blocking
/// - Lock-free lookups for read-heavy workloads
/// - Per-file lock tracking for fine-grained concurrency
/// The identity a lock stateid stands for. NFSv4's model is one lock
/// stateid per (client, lock-owner, file) covering a SET of ranges;
/// this table's entries are per-RANGE. The map below keeps a presented
/// stateid resolvable to its OWNER even after the specific range entry
/// it was minted for has been unlocked — without it, sqlite's ordinary
/// lock-PENDING / lock-SHARED / unlock-PENDING sequence orphans the
/// client's held stateid and every later lock op wedges in BadStateId
/// recovery.
#[derive(Debug, Clone)]
pub struct LockOwnerKey {
    pub client_id: u64,
    pub owner: Vec<u8>,
    /// The file the canonical stateid was minted for. NFSv4's lock
    /// stateid is per (lock-owner, file); the reverse lookup a reclaim
    /// needs ("which canonical stands for this owner on THIS file")
    /// is ambiguous without it.
    pub filehandle: Vec<u8>,
}

/// `lock_type` value marking a persisted row as a CANONICAL-stateid →
/// owner registration rather than a byte-range entry. Deliberately
/// outside the wire range (1..=4): an OLDER build restoring a DB that
/// carries these rows skips them with its unknown-lock_type warning
/// instead of misreading them as ranges — no schema bump, rollback
/// stays safe.
const OWNER_REGISTRATION_LOCK_TYPE: u32 = 0xFC;

/// The be64 counter inside a minted internal entry key
/// ([`LockManager::mint_entry_stateid`]'s `[0xFC,'l','k',0,be64(n)]`
/// layout), or `None` for any other key shape — canonical stateids,
/// synthetic test keys, rows from builds predating the
/// canonical/internal split.
fn minted_entry_counter(other: &[u8; 12]) -> Option<u64> {
    if other[..4] == [0xFC, b'l', b'k', 0] {
        Some(u64::from_be_bytes(other[4..12].try_into().expect("8 bytes")))
    } else {
        None
    }
}

pub struct LockManager {
    /// Active locks (stateid → lock)
    /// DashMap provides lock-free concurrent access
    locks: DashMap<[u8; 12], Lock>,  // Key is stateid.other

    /// Every client-visible lock stateid → its owner identity. Entries
    /// OUTLIVE the range entries (removed only with the client), so a
    /// client's held stateid keeps resolving between unlock and relock.
    /// The client-visible stateid is CANONICAL per owner: Linux's
    /// nfs4_update_lock_stateid refuses a reply whose `other` differs
    /// from the lock state it holds and silently RESTARTS the RPC — a
    /// server that mints a fresh stateid per exist-owner LOCK puts
    /// every locker (sqlite's first transaction, measured at 5.8M
    /// retries) into an infinite resend loop.
    stateid_owners: DashMap<[u8; 12], LockOwnerKey>,

    /// Mint for INTERNAL range-entry keys (never client-visible, never
    /// in the stateid table): an owner's ranges each need a distinct
    /// row key while the client sees only the canonical stateid.
    entry_key_counter: std::sync::atomic::AtomicU64,

    /// Locks by filehandle (for conflict detection)
    /// Enables per-file locking - only locks on same file conflict
    locks_by_fh: DashMap<Vec<u8>, Vec<[u8; 12]>>,

    /// Live range-entry count per client — the number the
    /// `max_locks_per_client` quota is checked against. Maintained at
    /// the ONLY two mutation sites (`insert_in_memory`, `remove_lock`),
    /// which every path — grant, trim/split, restore, client removal —
    /// funnels through.
    lock_counts: DashMap<u64, u64>,

    /// Persistence target. `None` (tests, `new()`) keeps the historical
    /// memory-only behavior; the server constructs with the shared
    /// state.db backend so locks survive an NFS server-pod restart the
    /// same way the lock STATEIDS always did. Mutations follow the
    /// spawn_persist pattern (in-memory first, fire-and-forget persist;
    /// see state_backend module docs for the accepted crash window).
    backend: Option<Arc<dyn StateBackend>>,

    /// `false` only when the server tried to restore lock state and the
    /// backend was unreadable (state LOST, not merely empty). In that
    /// degraded window, `handle_lock` refuses NEW locks for the grace
    /// period so a second client cannot grab a range whose pre-restart
    /// holder the server no longer knows about (RFC 8881 §9.6.3.1).
    /// Defaults true: a fresh volume or a clean restore has nothing to
    /// protect, and grace must not tax routine restarts.
    restored_clean: AtomicBool,
}

impl LockManager {
    /// Create a new lock manager (memory-only; tests and callers that
    /// don't need restart survival)
    pub fn new() -> Self {
        Self {
            locks: DashMap::new(),
            stateid_owners: DashMap::new(),
            entry_key_counter: std::sync::atomic::AtomicU64::new(1),
            locks_by_fh: DashMap::new(),
            lock_counts: DashMap::new(),
            backend: None,
            restored_clean: AtomicBool::new(true),
        }
    }

    /// Lock manager whose mutations mirror into `backend`; pair with
    /// [`LockManager::load_records`] at startup.
    pub fn with_backend(backend: Arc<dyn StateBackend>) -> Self {
        Self {
            locks: DashMap::new(),
            stateid_owners: DashMap::new(),
            entry_key_counter: std::sync::atomic::AtomicU64::new(1),
            locks_by_fh: DashMap::new(),
            lock_counts: DashMap::new(),
            backend: Some(backend),
            restored_clean: AtomicBool::new(true),
        }
    }

    /// Seed the table from persisted records (startup restore). Rows
    /// with an unknown lock_type (schema from the future) are skipped
    /// loudly rather than guessed at.
    pub fn load_records(&self, records: Vec<LockRecord>) {
        let mut loaded = 0usize;
        let mut owners = 0usize;
        let mut max_minted: u64 = 0;
        for record in &records {
            if let Some(n) = minted_entry_counter(&record.other) {
                max_minted = max_minted.max(n);
            }
            if record.lock_type == OWNER_REGISTRATION_LOCK_TYPE {
                // A canonical-stateid → owner registration persisted by
                // `register_owner_stateid`. Without re-seeding this map,
                // every restart orphaned the client's held stateid: the
                // stateid TABLE validated it (persisted), but LOCKU and
                // exist-owner LOCK resolved no owner → BAD_STATEID,
                // while TEST_STATEID said Ok and FREE_STATEID said
                // LOCKS_HELD — non-converging recovery, and the restored
                // range rows denied the range to everyone forever.
                self.stateid_owners.insert(record.other, LockOwnerKey {
                    client_id: record.client_id,
                    owner: record.owner.clone(),
                    filehandle: record.filehandle.clone(),
                });
                owners += 1;
                continue;
            }
            match Lock::from_record(record) {
                Some(lock) => {
                    self.insert_in_memory(lock);
                    loaded += 1;
                }
                None => {
                    warn!(
                        "LockManager: skipping persisted lock with unknown lock_type {} (stateid {:02x?})",
                        record.lock_type, record.other
                    );
                }
            }
        }
        // Never re-mint a restored entry key. The counter restarts at 1
        // with the process, and every generation mints the SAME byte
        // keys — so without this bump, post-restart lock traffic
        // silently OVERWRITES restored locks in memory (plain DashMap
        // insert) and in the backend (`INSERT OR REPLACE`), re-losing
        // mutual exclusion for exactly the holders persistence exists
        // to protect. Same pattern as StateIdManager::load_records
        // bumping next_stateid past the max restored counter.
        if max_minted > 0 {
            self.entry_key_counter
                .fetch_max(max_minted + 1, Ordering::SeqCst);
        }
        if loaded > 0 || owners > 0 {
            info!(
                "LockManager restored {} byte-range locks and {} owner stateids from backend (entry keys resume past {})",
                loaded, owners, max_minted
            );
        }
    }

    /// The restore path found the backend unreadable: pre-restart lock
    /// state is LOST. New locks are refused during grace (see
    /// `restored_clean` docs).
    pub fn mark_restore_failed(&self) {
        self.restored_clean.store(false, Ordering::SeqCst);
    }

    /// The ONE lock-state bring-up path for every flint NFS front-end.
    ///
    /// Binds a manager to `backend` **and** restores the persisted lock
    /// table in a single call, so a front-end cannot obtain a
    /// LockManager without also obtaining its restore. That coupling is
    /// the whole point: construction and restore used to be two
    /// separate hooks wired into `NfsServer` only, and
    /// `MetadataServer` — the binary the flint-lite hub actually runs
    /// (`nfs_mds_main.rs` → `pnfs::mds::MetadataServer`) — built a bare
    /// `LockManager::new()` and never called `list_locks`. Because the
    /// STATEID table *was* persisted and EXCHANGE_ID answers
    /// CONFIRMED_R, a client reconnecting after any hub restart
    /// believed it still held a lock the server had silently forgotten,
    /// issued no reclaim, was never told, and a second client's
    /// conflicting LOCK was granted. Two writers, mutual exclusion
    /// gone, no error at either end.
    ///
    /// `state_lost` is the caller's "a prior state DB existed and could
    /// not be used" signal; combined with an unreadable `list_locks` it
    /// gates NEW locks for grace (`restored_clean`).
    pub async fn bring_up(
        backend: Arc<dyn StateBackend>,
        state_lost: bool,
    ) -> Arc<Self> {
        let mgr = Arc::new(Self::with_backend(Arc::clone(&backend)));
        if state_lost {
            // A prior state DB existed but was quarantined/unreadable:
            // the lock table went with it. Gate new locks in grace.
            mgr.mark_restore_failed();
        }
        match backend.list_locks().await {
            Ok(records) => mgr.load_records(records),
            Err(e) => {
                tracing::error!(
                    "NFSv4 lock restore failed ({}) — new locks gated for grace",
                    e
                );
                mgr.mark_restore_failed();
            }
        }
        mgr
    }

    /// `false` while running with known-lost lock state.
    pub fn restored_clean(&self) -> bool {
        self.restored_clean.load(Ordering::SeqCst)
    }

    fn insert_in_memory(&self, lock: Lock) {
        let stateid_key = lock.stateid.other;
        let fh_key = lock.filehandle.clone();
        self.stateid_owners.insert(stateid_key, LockOwnerKey {
            client_id: lock.client_id,
            owner: lock.owner.clone(),
            filehandle: lock.filehandle.clone(),
        });
        *self.lock_counts.entry(lock.client_id).or_insert(0) += 1;
        if let Some(prev) = self.locks.insert(stateid_key, lock) {
            // Never expected (entry keys are unique across generations
            // since the collision fix) — but a replaced row must not
            // leave its owner's count inflated forever.
            if let Some(mut n) = self.lock_counts.get_mut(&prev.client_id) {
                *n = n.saturating_sub(1);
            }
        }
        self.locks_by_fh
            .entry(fh_key)
            .or_insert_with(Vec::new)
            .push(stateid_key);
    }

    /// Resolve a presented lock stateid to the owner it stands for —
    /// via the long-lived owner map first, falling back to a live
    /// entry (covers restored rows from before the map existed).
    pub fn resolve_owner(&self, stateid: &StateId) -> Option<(u64, Vec<u8>)> {
        if let Some(o) = self.stateid_owners.get(&stateid.other) {
            return Some((o.client_id, o.owner.clone()));
        }
        self.locks
            .get(&stateid.other)
            .map(|l| (l.client_id, l.owner.clone()))
    }

    /// Live range entries this client holds (quota input).
    pub fn lock_count_for_client(&self, client_id: u64) -> u64 {
        self.lock_counts.get(&client_id).map(|n| *n).unwrap_or(0)
    }

    /// Mint an internal range-entry key. The 0xFC prefix keeps it out
    /// of any client-visible stateid space; these keys exist only as
    /// row identities in the lock table and its persistence.
    fn mint_entry_stateid(&self) -> StateId {
        let n = self
            .entry_key_counter
            .fetch_add(1, Ordering::Relaxed);
        let mut other = [0u8; 12];
        other[0] = 0xFC;
        other[1] = b'l';
        other[2] = b'k';
        other[4..12].copy_from_slice(&n.to_be_bytes());
        StateId { seqid: 1, other }
    }

    /// Register a client-visible (canonical) stateid as standing for
    /// an owner — the map consulted by [`LockManager::resolve_owner`].
    ///
    /// PERSISTED: the registration is mirrored into the backend as an
    /// [`OWNER_REGISTRATION_LOCK_TYPE`] row, because the canonical
    /// stateid itself survives a restart in the stateid table — a
    /// mapping that does not survive with it leaves the client holding
    /// a stateid that validates but resolves to nothing.
    pub fn register_owner_stateid(
        &self,
        other: [u8; 12],
        client_id: u64,
        owner: Vec<u8>,
        filehandle: Vec<u8>,
    ) {
        if let Some(backend) = &self.backend {
            backend.enqueue_write(WriteOp::PutLock(LockRecord {
                other,
                seqid: 0,
                client_id,
                owner: owner.clone(),
                filehandle: filehandle.clone(),
                lock_type: OWNER_REGISTRATION_LOCK_TYPE,
                offset: 0,
                length: 0,
            }));
        }
        self.stateid_owners
            .insert(other, LockOwnerKey { client_id, owner, filehandle });
    }

    /// The client-visible stateid registered for (`client_id`, `owner`)
    /// on `filehandle`, if any. Internal entry keys are skipped even
    /// though they sit in the same map: replying one of those is
    /// precisely the non-converging reclaim wedge this lookup exists to
    /// prevent — the stateid table cannot validate an internal key, so
    /// every subsequent op on it fails with BAD_STATEID and no recovery
    /// path.
    pub fn canonical_for_owner(
        &self,
        client_id: u64,
        owner: &[u8],
        filehandle: &[u8],
    ) -> Option<[u8; 12]> {
        self.stateid_owners.iter().find_map(|e| {
            let v = e.value();
            (minted_entry_counter(e.key()).is_none()
                && v.client_id == client_id
                && v.owner.as_slice() == owner
                && v.filehandle.as_slice() == filehandle)
                .then(|| *e.key())
        })
    }

    /// Carve `cut` out of every range this owner holds on `filehandle`
    /// — the shared engine of LOCKU and of an owner's own re-lock
    /// (upgrade/downgrade). Fully-covered entries are removed; partial
    /// overlaps shrink, and an unlock strictly inside a held range
    /// splits it in two (`mint` supplies stateids for the survivors).
    /// A cut that touches nothing is a no-op — POSIX unlock of an
    /// unheld range succeeds.
    pub fn trim_owner_range(
        &self,
        client_id: u64,
        owner: &[u8],
        filehandle: &[u8],
        cut: &LockRange,
    ) {
        let keys: Vec<[u8; 12]> = match self.locks_by_fh.get(filehandle) {
            Some(k) => k.value().clone(),
            None => return,
        };
        for key in keys {
            let (lock_type, held) = match self.locks.get(&key) {
                Some(e)
                    if e.client_id == client_id
                        && e.owner.as_slice() == owner
                        && e.range.overlaps(cut) =>
                {
                    (e.lock_type, e.range)
                }
                _ => continue,
            };
            // Ref dropped above (match arm ended) — safe to remove.
            self.remove_lock(&StateId { seqid: 0, other: key });

            // Left survivor: bytes of the held range before the cut.
            if held.offset < cut.offset {
                self.add_lock(Lock {
                    stateid: self.mint_entry_stateid(),
                    client_id,
                    owner: owner.to_vec(),
                    filehandle: filehandle.to_vec(),
                    lock_type,
                    range: LockRange {
                        offset: held.offset,
                        length: cut.offset - held.offset,
                    },
                });
            }
            // Right survivor: bytes after the cut (none if the cut
            // runs to EOF).
            if cut.length != 0 {
                let cut_end = cut.offset + cut.length;
                let survives = match held.length {
                    0 => true,
                    l => held.offset + l > cut_end,
                };
                if survives {
                    self.add_lock(Lock {
                        stateid: self.mint_entry_stateid(),
                        client_id,
                        owner: owner.to_vec(),
                        filehandle: filehandle.to_vec(),
                        lock_type,
                        range: LockRange {
                            offset: cut_end,
                            length: match held.length {
                                0 => 0,
                                l => held.offset + l - cut_end,
                            },
                        },
                    });
                }
            }
        }
    }

    /// Add a lock
    ///
    /// LOCK-FREE: DashMap handles concurrent inserts without global locks
    pub fn add_lock(&self, lock: Lock) {
        let record = lock.to_record();
        self.insert_in_memory(lock);

        if let Some(backend) = &self.backend {
            backend.enqueue_write(WriteOp::PutLock(record));
        }
    }

    /// Check for lock conflicts
    ///
    /// LOCK-FREE: Uses per-file lock tracking for fine-grained concurrency
    /// Only checks locks on the same file, enabling concurrent ops on different files
    ///
    /// `exclude_owner` is the REQUESTER's (client_id, owner): that owner's
    /// own locks never conflict with its request — a write lock over its
    /// own read lock is an atomic upgrade (RFC 8881 §18.10), not a denial.
    /// Passing `None` (LOCKT-style probes) checks against every holder.
    pub fn check_conflicts(
        &self,
        filehandle: &[u8],
        range: &LockRange,
        lock_type: LockType,
        exclude_owner: Option<(u64, &[u8])>,
    ) -> Option<Lock> {
        // Get all locks on this filehandle (lock-free read)
        if let Some(lock_stateids) = self.locks_by_fh.get(filehandle) {
            for stateid_key in lock_stateids.value() {
                // Lock-free lookup
                if let Some(existing_lock) = self.locks.get(stateid_key) {
                    // The requester's own locks are upgrade material,
                    // never conflicts. (Before this exclusion, a lock
                    // owner's SH→EX upgrade was denied against its own
                    // read lock — sqlite's first CREATE TABLE wedged
                    // every client in blocking-retry forever.)
                    if let Some((cid, owner)) = exclude_owner {
                        if existing_lock.client_id == cid
                            && existing_lock.owner.as_slice() == owner
                        {
                            continue;
                        }
                    }

                    // Check for conflict
                    if range.conflicts_with(
                        &existing_lock.range,
                        lock_type,
                        existing_lock.lock_type,
                    ) {
                        return Some(existing_lock.clone());
                    }
                }
            }
        }

        None
    }

    /// Remove a lock
    ///
    /// LOCK-FREE: DashMap's remove is lock-free
    pub fn remove_lock(&self, stateid: &StateId) -> Option<Lock> {
        let stateid_key = stateid.other;

        // Remove from main map (lock-free)
        let lock = self.locks.remove(&stateid_key).map(|(_, lock)| lock);

        // Remove from filehandle index
        if let Some(ref lock) = lock {
            if let Some(mut n) = self.lock_counts.get_mut(&lock.client_id) {
                *n = n.saturating_sub(1);
            }
            if let Some(mut fh_locks) = self.locks_by_fh.get_mut(&lock.filehandle) {
                fh_locks.retain(|k| k != &stateid_key);
                if fh_locks.is_empty() {
                    drop(fh_locks); // Release borrow
                    self.locks_by_fh.remove(&lock.filehandle);
                }
            }
        }

        if lock.is_some() {
            if let Some(backend) = &self.backend {
                backend.enqueue_write(WriteOp::DeleteLock(stateid_key));
            }
        }

        lock
    }

    /// Get a lock
    ///
    /// LOCK-FREE: Lock-free read, no blocking on concurrent operations
    pub fn get_lock(&self, stateid: &StateId) -> Option<Lock> {
        self.locks.get(&stateid.other).map(|r| r.clone())
    }

    /// Find a lock identical in everything but stateid — the reclaim
    /// path's idempotency probe: a client re-LOCKing state the server
    /// already restored must get the restored lock back, not a
    /// self-conflict denial.
    pub fn find_matching(
        &self,
        client_id: u64,
        owner: &[u8],
        filehandle: &[u8],
        lock_type: LockType,
        range: &LockRange,
    ) -> Option<Lock> {
        let stateids = self.locks_by_fh.get(filehandle)?;
        for key in stateids.value() {
            if let Some(lock) = self.locks.get(key) {
                if lock.client_id == client_id
                    && lock.owner == owner
                    && lock.lock_type == lock_type
                    && lock.range == *range
                {
                    return Some(lock.clone());
                }
            }
        }
        None
    }

    /// Get all locks for a client
    ///
    /// LOCK-FREE: Iterates without holding global lock
    pub fn get_client_locks(&self, client_id: u64) -> Vec<Lock> {
        self.locks
            .iter()
            .filter(|entry| entry.value().client_id == client_id)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Remove all locks for a client
    ///
    /// LOCK-FREE: Each remove is independent, no global lock
    pub fn remove_client_locks(&self, client_id: u64) {
        // Collect stateids without holding lock
        let stateids: Vec<[u8; 12]> = self.locks
            .iter()
            .filter(|entry| entry.value().client_id == client_id)
            .map(|entry| *entry.key())
            .collect();

        // Remove each lock (each remove is lock-free)
        for stateid_key in stateids {
            let stateid = StateId {
                seqid: 0,
                other: stateid_key,
            };
            self.remove_lock(&stateid);
        }

        // The owner map outlives range entries by design; a departing
        // client is the one boundary where it must be purged too — and
        // since registrations are persisted, their rows go with them
        // (deleting an internal key here again is an idempotent no-op;
        // `remove_lock` above already enqueued those).
        let purged: Vec<[u8; 12]> = self
            .stateid_owners
            .iter()
            .filter(|e| e.value().client_id == client_id)
            .map(|e| *e.key())
            .collect();
        for key in &purged {
            self.stateid_owners.remove(key);
        }
        if let Some(backend) = &self.backend {
            for key in purged {
                backend.enqueue_write(WriteOp::DeleteLock(key));
            }
        }
    }
}

impl Default for LockManager {
    fn default() -> Self {
        Self::new()
    }
}

/// LOCK operation (opcode 12)
///
/// Acquires a byte-range lock on a file.
pub struct LockOp {
    /// Lock type
    pub locktype: LockType,

    /// Reclaim lock after server reboot?
    pub reclaim: bool,

    /// Byte range to lock
    pub offset: u64,
    pub length: u64,

    /// Open-stateid or lock-stateid (for lock renewal)
    pub stateid: StateId,

    /// Lock owner
    pub owner: Vec<u8>,

    /// Is this a new lock owner?
    pub new_lock_owner: bool,

    /// Sequence ID (for new lock owner)
    pub open_seqid: Option<u32>,
}

pub struct LockRes {
    pub status: Nfs4Status,

    /// Lock stateid (if successful)
    pub stateid: Option<StateId>,

    /// Denied lock (if conflict)
    pub denied: Option<LockDenied>,
}

#[derive(Debug, Clone)]
pub struct LockDenied {
    pub offset: u64,
    pub length: u64,
    pub locktype: LockType,
    /// Client that holds the conflicting lock — lock_owner4.clientid
    /// on the wire (RFC 8881 §18.10.2).
    pub client_id: u64,
    pub owner: Vec<u8>,
}

/// LOCKT operation (opcode 13)
///
/// Tests if a lock would succeed (without actually acquiring it).
pub struct LockTOp {
    pub locktype: LockType,
    pub offset: u64,
    pub length: u64,
    pub owner: Vec<u8>,
}

pub struct LockTRes {
    pub status: Nfs4Status,
    pub denied: Option<LockDenied>,
}

/// LOCKU operation (opcode 14)
///
/// Releases a byte-range lock.
pub struct LockUOp {
    pub locktype: LockType,
    pub seqid: u32,
    pub stateid: StateId,
    pub offset: u64,
    pub length: u64,
}

pub struct LockURes {
    pub status: Nfs4Status,
    pub stateid: Option<StateId>,
}

/// Lock operation handler
pub struct LockOperationHandler {
    state_mgr: Arc<StateManager>,
    lock_mgr: Arc<LockManager>,
}

impl LockOperationHandler {
    /// Create a new lock operation handler
    pub fn new(state_mgr: Arc<StateManager>, lock_mgr: Arc<LockManager>) -> Self {
        Self {
            state_mgr,
            lock_mgr,
        }
    }

    /// Handle LOCK operation
    pub fn handle_lock(
        &self,
        op: LockOp,
        ctx: &CompoundContext,
    ) -> LockRes {
        debug!("LOCK: locktype={:?}, offset={}, length={}, new_owner={}",
               op.locktype, op.offset, op.length, op.new_lock_owner);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return LockRes {
                    status: Nfs4Status::NoFileHandle,
                    stateid: None,
                    denied: None,
                };
            }
        };

        // Validate stateid (open-stateid or existing lock-stateid)
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("LOCK: Invalid stateid: {}", e);
            return LockRes {
                status: Nfs4Status::BadStateId,
                stateid: None,
                denied: None,
            };
        }

        // RFC 8881 §18.10.3: "If the length is zero, or if a length that
        // is not all 1s would overflow the maximum 64-bit unsigned integer
        // value, the server MUST return NFS4ERR_INVAL."
        //
        // ALL 1s (`LockRange::TO_EOF`) is the to-EOF sentinel and is
        // exempt from the overflow check; ZERO is invalid. This used to
        // be stated exactly backwards — 0 was treated as to-EOF and
        // all-1s went through `checked_add`, which overflows for every
        // offset > 0. A Linux client asking to lock from any non-zero
        // offset to EOF (the plain `fcntl` l_len=0 idiom) therefore got
        // NFS4ERR_INVAL, while offset 0 worked because 0 + MAX does not
        // overflow. Found by nfstest_lock, 2026-08-23.
        if let Err(status) = validate_lock_range(op.offset, op.length, "LOCK") {
            return LockRes { status, stateid: None, denied: None };
        }

        let range = LockRange {
            offset: op.offset,
            length: op.length,
        };

        // Resolve the owning client from the SEQUENCE-set session id. Without
        // this, every client's locks were tagged to a hardcoded `client_id=1`,
        // which made multi-client RWX scenarios silently share lock state and
        // caused one client's lease expiry to wipe everyone else's locks.
        let client_id = match ctx.session_id.and_then(|sid|
            self.state_mgr.sessions.get_session(&sid).map(|s| s.client_id)
        ) {
            Some(id) => id,
            None => {
                warn!("LOCK: no session in context, returning NFS4ERR_BAD_SESSION");
                return LockRes {
                    status: Nfs4Status::BadSession,
                    stateid: None,
                    denied: None,
                };
            }
        };

        // Lock reclaim (client detected a server reboot). Only legal in
        // grace; and if the server RESTORED this exact lock from the
        // backend, hand the restored stateid back instead of letting the
        // reclaim self-conflict below.
        if op.reclaim {
            if !self.state_mgr.leases.in_grace_period() {
                return LockRes {
                    status: Nfs4Status::NoGrace,
                    stateid: None,
                    denied: None,
                };
            }
            if let Some(existing) = self.lock_mgr.find_matching(
                client_id,
                &op.owner,
                &current_fh.data,
                op.locktype,
                &range,
            ) {
                // What to hand back depends on the row's generation:
                // - A LEGACY row (pre canonical/internal split): its key
                //   IS the client-visible stateid — return it unchanged.
                // - An INTERNAL entry key: the stateid table cannot
                //   validate it, so replying it (the old behavior) wedged
                //   every subsequent op on the "granted" stateid in
                //   BAD_STATEID. Reply the owner's CANONICAL stateid,
                //   restored alongside the range rows — keeping the
                //   client's `other` even when the stateid-table row is
                //   missing (the same convention as the exist-owner arm:
                //   a reply whose `other` differs from what the client
                //   holds is silently refused and resent forever).
                // - An internal key with NO canonical anywhere (a DB
                //   written before registrations were persisted): mint
                //   and register a fresh one — legal here because a
                //   new_lock_owner reclaim ESTABLISHES lock state rather
                //   than updating held state.
                let reply = if minted_entry_counter(&existing.stateid.other).is_none() {
                    existing.stateid
                } else {
                    match self
                        .lock_mgr
                        .canonical_for_owner(client_id, &op.owner, &current_fh.data)
                    {
                        Some(other) => self
                            .state_mgr
                            .stateids
                            .update_seqid(&StateId { seqid: 0, other })
                            .unwrap_or(StateId { seqid: 1, other }),
                        None => {
                            let sid = self.state_mgr.stateids.allocate(
                                StateType::Lock,
                                client_id,
                                Some(current_fh.data.clone()),
                            );
                            self.lock_mgr.register_owner_stateid(
                                sid.other,
                                client_id,
                                op.owner.clone(),
                                current_fh.data.clone(),
                            );
                            sid
                        }
                    }
                };
                info!("LOCK: reclaim matched restored lock; returning canonical stateid");
                return LockRes {
                    status: Nfs4Status::Ok,
                    stateid: Some(reply),
                    denied: None,
                };
            }
        } else if self.lock_mgr.lock_count_for_client(client_id)
            >= self.state_mgr.quotas.max_locks_per_client as u64
        {
            // Per-client range-entry quota (B6): each granted range is a
            // DashMap entry plus a persisted state.db row, mintable by
            // an unauthenticated peer without bound. Reclaims are exempt
            // (they re-establish counted state); DELAY because LOCKU and
            // lease expiry free capacity.
            warn!(
                "LOCK: client {} at lock quota ({}) — DELAY (FLINT_NFS_MAX_LOCKS_PER_CLIENT)",
                client_id,
                self.state_mgr.quotas.max_locks_per_client
            );
            return LockRes {
                status: Nfs4Status::Delay,
                stateid: None,
                denied: None,
            };
        } else if !self.lock_mgr.restored_clean()
            && self.state_mgr.leases.in_grace_period()
        {
            // RFC 8881 §9.6.3.1: the server restarted WITHOUT its lock
            // state (backend unreadable) — a new lock granted now could
            // stomp a pre-restart holder we no longer know about. Refuse
            // new locks until grace ends; reclaims (above) still work.
            // A clean restore never takes this branch: the restored
            // table makes conflict detection authoritative again.
            warn!("LOCK: refusing new lock during degraded grace (lock state lost at restart)");
            return LockRes {
                status: Nfs4Status::Grace,
                stateid: None,
                denied: None,
            };
        }

        // exist_lock_owner4: op.stateid is a LOCK stateid and the wire
        // carries no owner bytes — the owner map is the authority (it
        // outlives individual range entries, so a stateid whose ranges
        // were all unlocked still resolves). An unresolvable stateid
        // is stale; refuse it rather than granting under an empty
        // owner identity.
        let resolved_owner = if op.new_lock_owner {
            None
        } else {
            match self.lock_mgr.resolve_owner(&op.stateid) {
                Some((cid, owner)) if cid == client_id => Some(owner),
                Some(_) => {
                    warn!("LOCK: lock stateid belongs to another client");
                    return LockRes {
                        status: Nfs4Status::BadStateId,
                        stateid: None,
                        denied: None,
                    };
                }
                None => {
                    warn!("LOCK: exist_lock_owner stateid resolves to no owner");
                    return LockRes {
                        status: Nfs4Status::BadStateId,
                        stateid: None,
                        denied: None,
                    };
                }
            }
        };
        let owner_bytes: Vec<u8> =
            resolved_owner.unwrap_or_else(|| op.owner.clone());

        // Check for conflicts — excluding the requester's own locks:
        // an owner re-locking its own range is an atomic upgrade or
        // downgrade, never a self-denial (the sqlite-wedge bug).
        if let Some(conflicting_lock) = self.lock_mgr.check_conflicts(
            &current_fh.data,
            &range,
            op.locktype,
            Some((client_id, owner_bytes.as_slice())),
        ) {
            warn!("LOCK: Conflict detected with existing lock");
            return LockRes {
                status: Nfs4Status::Denied,
                stateid: None,
                denied: Some(LockDenied {
                    offset: conflicting_lock.range.offset,
                    length: conflicting_lock.range.length,
                    locktype: conflicting_lock.lock_type,
                    client_id: conflicting_lock.client_id,
                    owner: conflicting_lock.owner,
                }),
            };
        }

        // An existing owner re-locking over its own ranges is an atomic
        // upgrade/downgrade (RFC 8881 §18.10): carve the requested
        // range out of the owner's own entries first, then grant one
        // clean entry of the requested type over it. Without the carve,
        // the pre-upgrade read lock survives as a phantom that denies
        // every future writer.
        //
        // THE REPLY STATEID IS THE OWNER'S CANONICAL ONE — the same
        // `other` the client presented, seqid advanced through the
        // stateid table. Linux's nfs4_update_lock_stateid REFUSES a
        // reply whose `other` differs from the lock state it holds and
        // silently restarts the RPC: a fresh stateid per exist-owner
        // LOCK is an infinite resend loop (sqlite's first transaction,
        // measured at 5.8M LOCKs before diagnosis), not a protocol
        // liberty. Range entries therefore carry internal keys; only
        // the canonical stateid is ever client-visible.
        let reply_stateid = if op.new_lock_owner {
            let canonical = self.state_mgr.stateids.allocate(
                StateType::Lock,
                client_id,
                Some(current_fh.data.clone()),
            );
            self.lock_mgr.register_owner_stateid(
                canonical.other,
                client_id,
                owner_bytes.clone(),
                current_fh.data.clone(),
            );
            canonical
        } else {
            self.lock_mgr.trim_owner_range(
                client_id,
                &owner_bytes,
                &current_fh.data,
                &range,
            );
            match self.state_mgr.stateids.update_seqid(&op.stateid) {
                Ok(sid) => sid,
                // A restored-from-backend canonical may predate the
                // stateid table row; keep the client's `other` and
                // advance its presented seqid rather than minting a
                // stateid it would refuse.
                Err(_) => StateId {
                    seqid: op.stateid.seqid.wrapping_add(1),
                    other: op.stateid.other,
                },
            }
        };

        // Create lock entry (internal key — never client-visible)
        let lock = Lock {
            stateid: self.lock_mgr.mint_entry_stateid(),
            client_id,
            owner: owner_bytes,
            filehandle: current_fh.data.clone(),
            lock_type: op.locktype,
            range,
        };

        // Add to lock manager
        self.lock_mgr.add_lock(lock);

        debug!("LOCK: Acquired {:?} lock on range {}+{}", op.locktype, op.offset, op.length);

        LockRes {
            status: Nfs4Status::Ok,
            stateid: Some(reply_stateid),
            denied: None,
        }
    }

    /// Handle LOCKT operation (test lock)
    pub fn handle_lockt(
        &self,
        op: LockTOp,
        ctx: &CompoundContext,
    ) -> LockTRes {
        debug!("LOCKT: locktype={:?}, offset={}, length={}",
               op.locktype, op.offset, op.length);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return LockTRes {
                    status: Nfs4Status::NoFileHandle,
                    denied: None,
                };
            }
        };

        // Same range rules as LOCK (RFC 8881 §18.11.3 defers to §18.10.3):
        // zero length invalid, all-1s means to-EOF and is exempt from the
        // overflow check.
        if let Err(status) = validate_lock_range(op.offset, op.length, "LOCKT") {
            return LockTRes { status, denied: None };
        }

        let range = LockRange {
            offset: op.offset,
            length: op.length,
        };

        // Check for conflicts (without acquiring)
        if let Some(conflicting_lock) = self.lock_mgr.check_conflicts(
            &current_fh.data,
            &range,
            op.locktype,
            None,
        ) {
            debug!("LOCKT: Would conflict");
            return LockTRes {
                status: Nfs4Status::Denied,
                denied: Some(LockDenied {
                    offset: conflicting_lock.range.offset,
                    length: conflicting_lock.range.length,
                    locktype: conflicting_lock.lock_type,
                    client_id: conflicting_lock.client_id,
                    owner: conflicting_lock.owner,
                }),
            };
        }

        debug!("LOCKT: No conflict");

        LockTRes {
            status: Nfs4Status::Ok,
            denied: None,
        }
    }

    /// Handle LOCKU operation (unlock)
    ///
    /// The stateid names the OWNER, the (offset, length) names the
    /// range — NFSv4's model, not "delete the entry behind the
    /// stateid". The kernel client holds ONE lock stateid per owner
    /// and unlocks ranges through it; releasing whichever single
    /// entry the stateid happened to be minted for (the previous
    /// behavior) freed the wrong range the moment an owner held more
    /// than one — sqlite's unlock-PENDING dropped its SHARED lock.
    pub fn handle_locku(
        &self,
        op: LockUOp,
        ctx: &CompoundContext,
    ) -> LockURes {
        debug!("LOCKU: offset={}, length={}", op.offset, op.length);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh.clone(),
            None => {
                return LockURes {
                    status: Nfs4Status::NoFileHandle,
                    stateid: None,
                };
            }
        };

        // Validate lock stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("LOCKU: Invalid stateid: {}", e);
            return LockURes {
                status: Nfs4Status::BadStateId,
                stateid: None,
            };
        }

        let (client_id, owner) = match self.lock_mgr.resolve_owner(&op.stateid) {
            Some(o) => o,
            None => {
                warn!("LOCKU: stateid resolves to no lock owner");
                return LockURes {
                    status: Nfs4Status::BadStateId,
                    stateid: None,
                };
            }
        };

        // Same range rules as LOCK (RFC 8881 §18.12.3 defers to §18.10.3).
        // An unvalidated LOCKU is worse than an unvalidated LOCKT: `cut`
        // drives `trim_owner_range`, so a zero-length or overflowing
        // range would silently trim nothing (or the wrong span) while
        // still answering NFS4_OK — the client would believe it had
        // released a lock it still holds.
        if let Err(status) = validate_lock_range(op.offset, op.length, "LOCKU") {
            return LockURes { status, stateid: None };
        }

        let cut = LockRange {
            offset: op.offset,
            length: op.length,
        };
        self.lock_mgr.trim_owner_range(
            client_id,
            &owner,
            &current_fh.data,
            &cut,
        );
        debug!("LOCKU: Released range {}+{} for owner", op.offset, op.length);

        // Advance the seqid through the stateid table — the copy
        // validate() checks on the client's next presentation. The
        // old reply (op.seqid+1, table untouched) meant a second
        // LOCKU/LOCK under the same owner failed strict validation.
        let new_stateid = match self.state_mgr.stateids.update_seqid(&op.stateid) {
            Ok(sid) => sid,
            Err(_) => StateId {
                seqid: op.stateid.seqid.wrapping_add(1),
                other: op.stateid.other,
            },
        };

        LockURes {
            status: Nfs4Status::Ok,
            stateid: Some(new_stateid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::filehandle::FileHandleManager;
    use tempfile::TempDir;

    fn create_test_handler() -> (LockOperationHandler, Arc<FileHandleManager>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let lock_mgr = Arc::new(LockManager::new());
        let handler = LockOperationHandler::new(state_mgr.clone(), lock_mgr);
        (handler, fh_mgr, temp_dir)
    }

    fn create_test_stateid(handler: &LockOperationHandler, client_id: u64) -> StateId {
        handler.state_mgr.stateids.allocate(StateType::Open, client_id, None)
    }

    /// Set up a session for `client_id` and return the SessionId so a test
    /// can populate `CompoundContext::session_id`. The LOCK handler now
    /// resolves client_id from the session id rather than hardcoding 1.
    fn create_test_session(handler: &LockOperationHandler, client_id: u64) -> SessionId {
        handler.state_mgr.sessions
            .create_session(client_id, 0, 0, 1024 * 1024, 1024 * 1024, 64 * 1024, 8, 8, 0, None, 1)
            .session_id
    }

    #[test]
    fn test_lock_range_overlap() {
        let range1 = LockRange { offset: 0, length: 100 };
        let range2 = LockRange { offset: 50, length: 100 };
        let range3 = LockRange { offset: 200, length: 100 };

        assert!(range1.overlaps(&range2));
        assert!(range2.overlaps(&range1));
        assert!(!range1.overlaps(&range3));
    }

    #[test]
    fn test_lock_range_eof() {
        let range1 = LockRange { offset: 100, length: 0 }; // 100 to EOF
        let range2 = LockRange { offset: 200, length: 50 };
        let range3 = LockRange { offset: 50, length: 40 };  // 50-90

        assert!(range1.overlaps(&range2)); // EOF range overlaps 200
        assert!(!range1.overlaps(&range3)); // EOF range starts at 100, doesn't overlap 50-90
    }

    #[test]
    fn test_lock_conflicts() {
        let range1 = LockRange { offset: 0, length: 100 };
        let range2 = LockRange { offset: 50, length: 100 };

        // Read + Read = no conflict
        assert!(!range1.conflicts_with(&range2, LockType::Read, LockType::Read));

        // Read + Write = conflict
        assert!(range1.conflicts_with(&range2, LockType::Read, LockType::Write));

        // Write + Write = conflict
        assert!(range1.conflicts_with(&range2, LockType::Write, LockType::Write));
    }

    #[test]
    fn test_lock_acquire() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let open_stateid = create_test_stateid(&handler, 1);

        let op = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid,
            owner: b"test-owner".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let res = handler.handle_lock(op, &ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
        assert!(res.denied.is_none());
    }

    #[test]
    fn test_lock_conflict() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let open_stateid = create_test_stateid(&handler, 1);

        // First lock
        let op1 = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid,
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let res1 = handler.handle_lock(op1, &ctx);
        assert_eq!(res1.status, Nfs4Status::Ok);

        // Conflicting lock
        let open_stateid2 = create_test_stateid(&handler, 2);
        let op2 = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 512,   // Overlaps with first lock
            length: 1024,
            stateid: open_stateid2,
            owner: b"owner2".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let res2 = handler.handle_lock(op2, &ctx);
        assert_eq!(res2.status, Nfs4Status::Denied);
        assert!(res2.denied.is_some());
    }

    /// The sqlite wedge: an owner holding a READ lock upgrades to WRITE
    /// on the same range. Before the fix the conflict check counted the
    /// owner's own read lock and denied — the kernel's blocking-lock
    /// path then retried forever, D-stating the caller. The upgrade
    /// must succeed IN PLACE: same stateid `other`, advanced seqid.
    #[test]
    fn upgrade_read_to_write_by_own_owner_succeeds_in_place() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let open_stateid = create_test_stateid(&handler, 1);
        let sh = handler.handle_lock(LockOp {
            locktype: LockType::Read,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: open_stateid,
            owner: b"sqlite-owner".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(sh.status, Nfs4Status::Ok);
        let sh_sid = sh.stateid.unwrap();

        // exist_lock_owner4: lock stateid, no owner bytes on the wire.
        let up = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: sh_sid,
            owner: Vec::new(),
            new_lock_owner: false,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(up.status, Nfs4Status::Ok, "own-owner upgrade must never self-conflict");
        // Linux's nfs4_update_lock_stateid refuses a reply whose `other`
        // differs from the held lock stateid and RESTARTS the RPC — the
        // canonical stateid is a client contract, not a nicety.
        let up_sid = up.stateid.unwrap();
        assert_eq!(up_sid.other, sh_sid.other,
            "exist-owner LOCK must return the canonical stateid (same other)");
        assert!(up_sid.seqid > sh_sid.seqid, "and advance its seqid");

        // The write lock must now be real: another client's read is denied.
        let mut ctx2 = CompoundContext::new(0);
        ctx2.session_id = Some(create_test_session(&handler, 2));
        ctx2.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        let rd = handler.handle_lock(LockOp {
            locktype: LockType::Read,
            reclaim: false,
            offset: 100,
            length: 10,
            stateid: create_test_stateid(&handler, 2),
            owner: b"other-owner".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx2);
        assert_eq!(rd.status, Nfs4Status::Denied, "upgraded WRITE lock must exclude readers");
    }

    /// After an upgrade, ONE LOCKU must free the whole claim — a second
    /// surviving entry (the pre-upgrade read lock) would be an immortal
    /// phantom that denies every future writer on the file.
    #[test]
    fn upgrade_leaves_no_phantom_lock_after_unlock() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let sh = handler.handle_lock(LockOp {
            locktype: LockType::Read,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: create_test_stateid(&handler, 1),
            owner: b"sqlite-owner".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx);
        let up = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: sh.stateid.unwrap(),
            owner: Vec::new(),
            new_lock_owner: false,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(up.status, Nfs4Status::Ok);
        let up_sid = up.stateid.unwrap();

        let un = handler.handle_locku(LockUOp {
            locktype: LockType::Write,
            seqid: 0,
            stateid: up_sid,
            offset: 0,
            length: 512,
        }, &ctx);
        assert_eq!(un.status, Nfs4Status::Ok);

        // A different client's WRITE on the range must now succeed:
        // nothing of the original claim may survive.
        let mut ctx2 = CompoundContext::new(0);
        ctx2.session_id = Some(create_test_session(&handler, 2));
        ctx2.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        let wr = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: create_test_stateid(&handler, 2),
            owner: b"other-owner".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx2);
        assert_eq!(wr.status, Nfs4Status::Ok,
            "a phantom pre-upgrade lock survived the unlock");
    }

    /// Same (client, owner) exclusion must NOT leak across owners: a
    /// different owner on the SAME client still conflicts.
    #[test]
    fn same_client_different_owner_still_conflicts() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let a = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: create_test_stateid(&handler, 1),
            owner: b"owner-a".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(a.status, Nfs4Status::Ok);

        let b = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: create_test_stateid(&handler, 1),
            owner: b"owner-b".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(b.status, Nfs4Status::Denied,
            "two owners on one client must still exclude each other");
    }

    /// The sqlite wedge, part 2 — the exact shape that survived the
    /// self-conflict fix: an owner holds TWO ranges (PENDING byte +
    /// SHARED range in sqlite's terms) and unlocks ONE of them through
    /// its current stateid. The old LOCKU deleted "the entry behind
    /// the stateid" — the WRONG range — and the owner's next lock op
    /// found its stateid orphaned. LOCKU must free exactly the named
    /// range, keep the other, and keep the stateid usable.
    #[test]
    fn unlock_frees_the_named_range_not_the_stateids_entry() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        // Range A ("PENDING byte"), new owner.
        let a = handler.handle_lock(LockOp {
            locktype: LockType::Read,
            reclaim: false,
            offset: 1000,
            length: 1,
            stateid: create_test_stateid(&handler, 1),
            owner: b"sqlite-owner".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(a.status, Nfs4Status::Ok);

        // Range B ("SHARED range"), same owner via the returned stateid.
        let b = handler.handle_lock(LockOp {
            locktype: LockType::Read,
            reclaim: false,
            offset: 2000,
            length: 510,
            stateid: a.stateid.unwrap(),
            owner: Vec::new(),
            new_lock_owner: false,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(b.status, Nfs4Status::Ok);
        let held = b.stateid.unwrap();

        // Unlock range A through the CURRENT stateid (whose entry is B).
        let un = handler.handle_locku(LockUOp {
            locktype: LockType::Read,
            seqid: 0,
            stateid: held,
            offset: 1000,
            length: 1,
        }, &ctx);
        assert_eq!(un.status, Nfs4Status::Ok);
        let held2 = un.stateid.unwrap();

        // Range A must be free, range B must still be held.
        let mut ctx2 = CompoundContext::new(0);
        ctx2.session_id = Some(create_test_session(&handler, 2));
        ctx2.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        let wr_a = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 1000,
            length: 1,
            stateid: create_test_stateid(&handler, 2),
            owner: b"other".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx2);
        assert_eq!(wr_a.status, Nfs4Status::Ok, "the unlocked range must be free");
        let wr_b = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 2100,
            length: 10,
            stateid: create_test_stateid(&handler, 2),
            owner: b"other".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx2);
        assert_eq!(wr_b.status, Nfs4Status::Denied, "the still-held range must deny");

        // And the owner's stateid must remain usable for its next lock.
        let c = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 3000,
            length: 8,
            stateid: held2,
            owner: Vec::new(),
            new_lock_owner: false,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(c.status, Nfs4Status::Ok, "owner stateid orphaned by the unlock");
    }

    /// Unlocking the middle of a held range splits it: both edges stay
    /// locked, the hole is free.
    #[test]
    fn unlock_inside_a_range_splits_it() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let a = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 300,
            stateid: create_test_stateid(&handler, 1),
            owner: b"owner".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(a.status, Nfs4Status::Ok);

        let un = handler.handle_locku(LockUOp {
            locktype: LockType::Write,
            seqid: 0,
            stateid: a.stateid.unwrap(),
            offset: 100,
            length: 100,
        }, &ctx);
        assert_eq!(un.status, Nfs4Status::Ok);

        let mut ctx2 = CompoundContext::new(0);
        ctx2.session_id = Some(create_test_session(&handler, 2));
        ctx2.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        let probe = |off: u64, len: u64| {
            handler.handle_lock(LockOp {
                locktype: LockType::Write,
                reclaim: false,
                offset: off,
                length: len,
                stateid: create_test_stateid(&handler, 2),
                owner: b"other".to_vec(),
                new_lock_owner: true,
                open_seqid: Some(0),
            }, &ctx2).status
        };
        assert_eq!(probe(100, 100), Nfs4Status::Ok, "the hole must be free");
        assert_eq!(probe(0, 50), Nfs4Status::Denied, "left edge must stay locked");
        assert_eq!(probe(250, 50), Nfs4Status::Denied, "right edge must stay locked");
    }

    /// exist_lock_owner4 with a stateid that has no live lock entry is
    /// stale — BadStateId, never a grant under an empty owner identity.
    #[test]
    fn exist_owner_without_lock_entry_is_bad_stateid() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let res = handler.handle_lock(LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 512,
            stateid: create_test_stateid(&handler, 1), // open stateid, no lock entry
            owner: Vec::new(),
            new_lock_owner: false,
            open_seqid: Some(0),
        }, &ctx);
        assert_eq!(res.status, Nfs4Status::BadStateId);
    }

    #[test]
    fn test_lock_shared() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        // Two read locks should not conflict
        let open_stateid1 = create_test_stateid(&handler, 1);
        let op1 = LockOp {
            locktype: LockType::Read,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid1,
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let res1 = handler.handle_lock(op1, &ctx);
        assert_eq!(res1.status, Nfs4Status::Ok);

        let open_stateid2 = create_test_stateid(&handler, 2);
        let op2 = LockOp {
            locktype: LockType::Read,
            reclaim: false,
            offset: 512,
            length: 1024,
            stateid: open_stateid2,
            owner: b"owner2".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let res2 = handler.handle_lock(op2, &ctx);
        assert_eq!(res2.status, Nfs4Status::Ok); // Should succeed
    }

    #[test]
    fn test_lockt() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let open_stateid = create_test_stateid(&handler, 1);

        // Acquire a lock
        let lock_op = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid,
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        handler.handle_lock(lock_op, &ctx);

        // Test for conflict
        let test_op = LockTOp {
            locktype: LockType::Write,
            offset: 512,
            length: 1024,
            owner: b"owner2".to_vec(),
        };

        let res = handler.handle_lockt(test_op, &ctx);
        assert_eq!(res.status, Nfs4Status::Denied);
        assert!(res.denied.is_some());
    }

    #[test]
    fn test_locku() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let open_stateid = create_test_stateid(&handler, 1);

        // Acquire a lock
        let lock_op = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid,
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let lock_res = handler.handle_lock(lock_op, &ctx);
        let lock_stateid = lock_res.stateid.unwrap();

        // Release the lock
        let unlock_op = LockUOp {
            locktype: LockType::Write,
            seqid: 0,
            stateid: lock_stateid,
            offset: 0,
            length: 1024,
        };

        let res = handler.handle_locku(unlock_op, &ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
    }

    #[test]
    fn test_lock_after_unlock() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let open_stateid1 = create_test_stateid(&handler, 1);

        // Acquire lock
        let lock_op = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid1,
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let lock_res = handler.handle_lock(lock_op, &ctx);
        let lock_stateid = lock_res.stateid.unwrap();

        // Release lock
        let unlock_op = LockUOp {
            locktype: LockType::Write,
            seqid: 0,
            stateid: lock_stateid,
            offset: 0,
            length: 1024,
        };

        handler.handle_locku(unlock_op, &ctx);

        // Now another client should be able to lock
        let open_stateid2 = create_test_stateid(&handler, 2);
        let lock_op2 = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid2,
            owner: b"owner2".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };

        let res2 = handler.handle_lock(lock_op2, &ctx);
        assert_eq!(res2.status, Nfs4Status::Ok);
    }

    // ── Lock persistence (restart survival) ─────────────────────────────
    //
    // The lock STATEIDS always survived a restart (StateIdRecord); these
    // pin that the lock TABLE now does too — the pre-fix behavior was a
    // post-restart server that validated the client's lock stateid while
    // silently enforcing nothing (a second client could take a
    // conflicting lock the first still believed it held).

    use crate::state_backend::{memory_backend, StateBackend};

    /// spawn_persist is fire-and-forget; give the spawned puts/deletes a
    /// bounded window to land in the backend.
    async fn settle(backend: &Arc<dyn StateBackend>, want: usize) {
        for _ in 0..200 {
            if backend.list_locks().await.unwrap().len() == want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn mk_lock(other: u8, client_id: u64, offset: u64, length: u64) -> Lock {
        Lock {
            stateid: StateId { seqid: 1, other: [other; 12] },
            client_id,
            owner: format!("owner-{}", client_id).into_bytes(),
            filehandle: b"/data/file".to_vec(),
            lock_type: LockType::Write,
            range: LockRange { offset, length },
        }
    }

    /// The to-EOF sentinel is all 1s, NOT zero (RFC 8881 §18.10.3), and
    /// `offset + TO_EOF` must never be a bare add.
    ///
    /// Shipped behaviour until 2026-08-23: a Linux client locking from a
    /// non-zero offset to EOF sends length = all 1s, `checked_add`
    /// overflowed, and the server answered NFS4ERR_INVAL — so plain
    /// `fcntl(F_SETLK)` with `l_len = 0` at any non-zero offset failed
    /// with EINVAL while offset 0 worked. Found by nfstest_lock;
    /// reproduced against a local-ext4 control at offsets 1/4096/8192.
    #[test]
    fn lock_to_eof_is_all_ones_and_never_overflows() {
        // The wire-level validator: all-1s is legal at ANY offset.
        for off in [0u64, 1, 4096, 8192, u64::MAX - 1] {
            assert!(
                validate_lock_range(off, LockRange::TO_EOF, "LOCK").is_ok(),
                "lock-to-EOF must be accepted at offset {off}"
            );
        }
        // Zero length is invalid per the RFC...
        assert_eq!(
            validate_lock_range(0, 0, "LOCK"),
            Err(Nfs4Status::Inval),
            "a zero length must be NFS4ERR_INVAL"
        );
        // ...and a genuine (non-sentinel) overflow still is too.
        assert_eq!(
            validate_lock_range(u64::MAX - 4, 16, "LOCK"),
            Err(Nfs4Status::Inval),
            "offset+length overflow must be NFS4ERR_INVAL"
        );
        assert!(validate_lock_range(4096, 4096, "LOCK").is_ok());

        // The range model: a to-EOF lock must actually reach EOF, and
        // computing its end must not overflow.
        let to_eof = LockRange { offset: 8192, length: LockRange::TO_EOF };
        assert_eq!(to_eof.end(), u64::MAX);
        // It conflicts with anything at or past its offset...
        assert!(to_eof.overlaps(&LockRange { offset: 1 << 40, length: 8 }));
        assert!(to_eof.overlaps(&LockRange { offset: 8192, length: 1 }));
        // ...and with nothing before it. This is the anti-vacuity half:
        // an `end()` that saturated everything to u64::MAX would make
        // the previous asserts pass while breaking this one.
        assert!(!to_eof.overlaps(&LockRange { offset: 0, length: 8192 }));
        // Two to-EOF locks at different offsets always overlap.
        assert!(to_eof.overlaps(&LockRange { offset: 0, length: LockRange::TO_EOF }));
        // Legacy rows persisted with 0 keep their to-EOF meaning.
        let legacy = LockRange { offset: 100, length: 0 };
        assert_eq!(legacy.end(), u64::MAX);
        assert!(!legacy.overlaps(&LockRange { offset: 0, length: 100 }));
    }

    /// `bring_up` is the shared path both front-ends use: it must bind
    /// the backend AND restore in one call. Proves the restore half —
    /// a `bring_up` that bound the backend but skipped `list_locks`
    /// (the hub's shipped behaviour) fails here.
    #[tokio::test(flavor = "multi_thread")]
    async fn bring_up_binds_and_restores_in_one_call() {
        let backend = memory_backend();

        // Generation 1 persists a lock through the shared path.
        let mgr1 = LockManager::bring_up(Arc::clone(&backend), false).await;
        mgr1.add_lock(mk_lock(7, 99, 0, 4096));
        settle(&backend, 1).await;
        drop(mgr1);

        // Generation 2 must come back holding it, with no separate
        // load_records call by the caller.
        let mgr2 = LockManager::bring_up(Arc::clone(&backend), false).await;
        assert!(mgr2.restored_clean(), "a clean restore must not gate new locks");
        let restored = mgr2
            .get_lock(&StateId { seqid: 0, other: [7; 12] })
            .expect("bring_up must restore the persisted lock table");
        assert_eq!(restored.client_id, 99);
        // Mutual exclusion is authoritative again — the anti-vacuity
        // half: a manager that "restored" an empty table would find no
        // conflict here and this assert is what catches it.
        assert!(
            mgr2.check_conflicts(
                b"/data/file",
                &LockRange { offset: 100, length: 8 },
                LockType::Write,
                None
            )
            .is_some(),
            "restored lock must still deny a conflicting range"
        );

        // state_lost gates new locks even when the table reads clean.
        let mgr3 = LockManager::bring_up(Arc::clone(&backend), true).await;
        assert!(!mgr3.restored_clean(), "state_lost must gate new locks for grace");
    }

    /// Anti-drift guard. Both NFS front-ends MUST obtain their
    /// LockManager from `bring_up`; a bare `LockManager::new()` in
    /// production code means a memory-only lock table whose loss is
    /// SILENT (the stateids persist, EXCHANGE_ID answers CONFIRMED_R,
    /// the client never reclaims and is never told). That is exactly
    /// what `pnfs/mds/server.rs` — the binary flint-lite runs — shipped
    /// with. A behavioural test cannot catch the revert because
    /// `MetadataServer::new` is far too heavy to construct here, so
    /// this asserts on the call sites directly, the way the F24
    /// guard-discipline lint above does.
    #[test]
    fn front_ends_build_their_lock_manager_through_bring_up() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // The two production front-ends, and the mains that arm them.
        let front_ends = [
            base.join("pnfs").join("mds").join("server.rs"),
            base.join("nfs").join("server_v4.rs"),
        ];
        let mut violations = Vec::new();
        let mut saw_bring_up = 0usize;
        for f in &front_ends {
            let text = std::fs::read_to_string(f)
                .unwrap_or_else(|e| panic!("front-end source unreadable {}: {e}", f.display()));
            // Convention: unit-test modules sit at the END behind cfg(test).
            let prod = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text[..],
            };
            if prod.contains("LockManager::bring_up") {
                saw_bring_up += 1;
            }
            for (n, line) in prod.lines().enumerate() {
                if line.contains("LockManager::new()") {
                    violations.push(format!("{}:{}: {}", f.display(), n + 1, line.trim()));
                }
            }
        }
        // Anti-vacuity: if the walk found nothing at all, the paths are
        // wrong and the lint would pass by not looking.
        assert_eq!(
            saw_bring_up,
            front_ends.len(),
            "expected every front-end to call LockManager::bring_up; found {saw_bring_up}. \
             Either a front-end regressed or this lint's paths are stale"
        );
        assert!(
            violations.is_empty(),
            "NFS front-end builds a memory-only LockManager — locks will be silently \
             lost on every restart while the stateid still validates. Use \
             `LockManager::bring_up(backend, state_lost).await`:\n{}",
            violations.join("\n")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn locks_survive_a_manager_generation() {
        let backend = memory_backend();

        // Generation 1: grant two locks, release one.
        let mgr1 = LockManager::with_backend(Arc::clone(&backend));
        mgr1.add_lock(mk_lock(1, 42, 0, 1024));
        mgr1.add_lock(mk_lock(2, 43, 4096, 0));
        settle(&backend, 2).await;
        mgr1.remove_lock(&StateId { seqid: 1, other: [2; 12] });
        settle(&backend, 1).await;
        assert_eq!(backend.list_locks().await.unwrap().len(), 1);
        drop(mgr1);

        // Generation 2: restart. The restored table enforces the
        // surviving lock and has forgotten the released one.
        let mgr2 = LockManager::with_backend(Arc::clone(&backend));
        mgr2.load_records(backend.list_locks().await.unwrap());
        assert!(mgr2.restored_clean());

        let restored = mgr2
            .get_lock(&StateId { seqid: 0, other: [1; 12] })
            .expect("lock must survive the restart");
        assert_eq!(restored.client_id, 42);
        assert_eq!(restored.range, LockRange { offset: 0, length: 1024 });

        // Conflict detection is authoritative again: the range the
        // restored lock covers is denied to another owner...
        assert!(mgr2
            .check_conflicts(b"/data/file", &LockRange { offset: 512, length: 100 }, LockType::Write, None)
            .is_some());
        // ...and the released lock's range is free.
        assert!(mgr2
            .check_conflicts(b"/data/file", &LockRange { offset: 8192, length: 100 }, LockType::Write, None)
            .is_none());
    }

    /// B6, per-client lock quota: granted range entries are refused with
    /// DELAY at the cap, and LOCKU frees capacity — each entry is memory
    /// plus a persisted state.db row, mintable without bound before.
    #[test]
    fn lock_quota_refuses_at_cap_and_locku_frees_capacity() {
        use crate::nfs::v4::state::StateQuotas;
        let q = StateQuotas {
            max_clients: 4096,
            max_sessions_per_client: 16,
            max_stateids_per_client: 65536,
            max_locks_per_client: 1,
        };
        let temp = TempDir::new().unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        let state_mgr = Arc::new(StateManager::new_in_memory_with_quotas("", q));
        let handler = LockOperationHandler::new(state_mgr, Arc::new(LockManager::new()));
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 9));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        let mk = |off: u64, owner: &[u8], sid: StateId, new_owner: bool| LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: off,
            length: 16,
            stateid: sid,
            owner: owner.to_vec(),
            new_lock_owner: new_owner,
            open_seqid: Some(0),
        };
        let open_sid = create_test_stateid(&handler, 9);
        let first = handler.handle_lock(mk(0, b"o1", open_sid, true), &ctx);
        assert_eq!(first.status, Nfs4Status::Ok);
        let canonical = first.stateid.unwrap();

        let open_sid2 = create_test_stateid(&handler, 9);
        assert_eq!(
            handler.handle_lock(mk(4096, b"o2", open_sid2, true), &ctx).status,
            Nfs4Status::Delay,
            "a second range entry at max_locks_per_client=1 must be refused"
        );

        let unlocked = handler.handle_locku(
            LockUOp {
                locktype: LockType::Write,
                seqid: canonical.seqid,
                stateid: canonical,
                offset: 0,
                length: 16,
            },
            &ctx,
        );
        assert_eq!(unlocked.status, Nfs4Status::Ok);
        let open_sid3 = create_test_stateid(&handler, 9);
        assert_eq!(
            handler.handle_lock(mk(8192, b"o3", open_sid3, true), &ctx).status,
            Nfs4Status::Ok,
            "LOCKU must free quota capacity — the counter tracks removals"
        );
    }

    /// A production-minted internal entry key, generation-independent.
    fn entry_key(n: u64) -> [u8; 12] {
        let mut other = [0u8; 12];
        other[0] = 0xFC;
        other[1] = b'l';
        other[2] = b'k';
        other[4..12].copy_from_slice(&n.to_be_bytes());
        other
    }

    /// THE ENTRY-KEY COLLISION: `entry_key_counter` restarts at 1 with
    /// the process, and `load_records` used to leave it there — so the
    /// first post-restart mint produced the exact byte key of a
    /// restored row still held by a pre-restart client, and both the
    /// DashMap insert and sqlite's `INSERT OR REPLACE` silently
    /// replaced that lock with the new one. Mutual exclusion lost
    /// again, through the restore path 7abc0a5 added to protect it.
    /// (StateIdManager::load_records always bumped its counter; this
    /// was the one mint that didn't.)
    #[tokio::test(flavor = "multi_thread")]
    async fn restored_entry_keys_are_never_reminted() {
        let backend = memory_backend();

        // Generation 1: alice holds two production-minted locks on
        // file A across the restart.
        let mgr1 = LockManager::with_backend(Arc::clone(&backend));
        for (off, len) in [(0u64, 4096u64), (8192, 4096)] {
            mgr1.add_lock(Lock {
                stateid: mgr1.mint_entry_stateid(),
                client_id: 1,
                owner: b"alice".to_vec(),
                filehandle: b"/file-A".to_vec(),
                lock_type: LockType::Write,
                range: LockRange { offset: off, length: len },
            });
        }
        settle(&backend, 2).await;
        drop(mgr1);

        // Generation 2: restart, then ordinary new lock traffic from a
        // different client on a different file.
        let mgr2 = LockManager::bring_up(Arc::clone(&backend), false).await;
        let minted = mgr2.mint_entry_stateid();
        assert!(
            minted.other != entry_key(1) && minted.other != entry_key(2),
            "a post-restart mint re-issued a restored entry key — the restored lock \
             would be silently overwritten in memory AND in the backend"
        );
        mgr2.add_lock(Lock {
            stateid: minted,
            client_id: 2,
            owner: b"bob".to_vec(),
            filehandle: b"/file-B".to_vec(),
            lock_type: LockType::Write,
            range: LockRange { offset: 1 << 40, length: 4096 },
        });
        settle(&backend, 3).await;

        // Alice's first range is still guarded on file A...
        let conflict = mgr2
            .check_conflicts(b"/file-A", &LockRange { offset: 100, length: 8 }, LockType::Write, None)
            .expect("the restored lock must still deny its range after new lock traffic");
        assert_eq!(conflict.owner, b"alice".to_vec());
        assert_eq!(conflict.filehandle, b"/file-A".to_vec());

        // ...and its durable row still names file A (INSERT OR REPLACE
        // under a colliding key destroyed exactly this).
        let rows = backend.list_locks().await.unwrap();
        assert_eq!(rows.len(), 3, "the new lock must be a NEW row, not a replacement");
        let row1 = rows
            .iter()
            .find(|r| r.other == entry_key(1))
            .expect("the restored row must still exist under its original key");
        assert_eq!(row1.filehandle, b"/file-A".to_vec());
        assert_eq!(row1.owner, b"alice".to_vec());
    }

    /// THE CANONICAL ORPHAN, map half: the client-visible lock stateid
    /// survives a restart in the STATEID table, so it validates — but
    /// the stateid→owner map was memory-only, so LOCKU and exist-owner
    /// LOCK resolved no owner and answered BAD_STATEID forever, while
    /// the restored range rows denied the range to every other client.
    /// Registrations are now persisted and restored.
    #[tokio::test(flavor = "multi_thread")]
    async fn canonical_lock_stateid_survives_a_restart() {
        let backend = memory_backend();
        let canonical = [0xAB_u8; 12];

        let mgr1 = LockManager::with_backend(Arc::clone(&backend));
        mgr1.register_owner_stateid(canonical, 42, b"owner-42".to_vec(), b"/db".to_vec());
        mgr1.add_lock(Lock {
            stateid: mgr1.mint_entry_stateid(),
            client_id: 42,
            owner: b"owner-42".to_vec(),
            filehandle: b"/db".to_vec(),
            lock_type: LockType::Write,
            range: LockRange { offset: 0, length: 1024 },
        });
        settle(&backend, 2).await;
        drop(mgr1);

        let mgr2 = LockManager::bring_up(Arc::clone(&backend), false).await;
        assert_eq!(
            mgr2.resolve_owner(&StateId { seqid: 1, other: canonical }),
            Some((42, b"owner-42".to_vec())),
            "the canonical stateid must still resolve to its owner after a restart"
        );
        // The reverse lookup returns the canonical, never the internal
        // entry key that shares the same owner in the map.
        assert_eq!(
            mgr2.canonical_for_owner(42, b"owner-42", b"/db"),
            Some(canonical)
        );
    }

    /// THE CANONICAL ORPHAN, end to end: LOCK → hub restart → LOCKU
    /// through the canonical the client was handed, then an exist-owner
    /// re-LOCK through it — sqlite's ordinary sequence around any
    /// rollout that catches a transaction in flight.
    #[tokio::test(flavor = "multi_thread")]
    async fn locku_succeeds_via_the_canonical_stateid_after_a_restart() {
        let backend = memory_backend();
        let temp = TempDir::new().unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        let root = fh_mgr.get_root_fh().unwrap();

        // Generation 1: the client takes a lock through the real
        // handler path and is handed its canonical stateid.
        let canonical = {
            let state = Arc::new(StateManager::new("vol", Arc::clone(&backend)));
            let locks = LockManager::bring_up(Arc::clone(&backend), false).await;
            let h = LockOperationHandler::new(state, locks);
            let mut ctx = CompoundContext::new(0);
            ctx.session_id = Some(create_test_session(&h, 7));
            ctx.current_fh = Some(root.clone());
            let open_sid = create_test_stateid(&h, 7);
            let res = h.handle_lock(
                LockOp {
                    locktype: LockType::Write,
                    reclaim: false,
                    offset: 0,
                    length: 1024,
                    stateid: open_sid,
                    owner: b"sqlite".to_vec(),
                    new_lock_owner: true,
                    open_seqid: Some(0),
                },
                &ctx,
            );
            assert_eq!(res.status, Nfs4Status::Ok);
            res.stateid.unwrap()
        };
        settle(&backend, 2).await; // range entry + owner registration

        // Generation 2: restart — fresh managers over the same backend,
        // a fresh session (sessions are deliberately not restored).
        let state = Arc::new(StateManager::new("vol", Arc::clone(&backend)));
        state.load_from_backend(false).await.unwrap();
        let locks = LockManager::bring_up(Arc::clone(&backend), false).await;
        let h = LockOperationHandler::new(state, locks);
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&h, 7));
        ctx.current_fh = Some(root);

        // Pre-fix: validate() passed (the stateid table IS persisted)
        // but resolve_owner found nothing → BAD_STATEID, with no
        // recovery path (TEST_STATEID said Ok, FREE_STATEID said
        // LOCKS_HELD) and the restored rows denying the range forever.
        let unlocked = h.handle_locku(
            LockUOp {
                locktype: LockType::Write,
                seqid: canonical.seqid,
                stateid: canonical,
                offset: 0,
                length: 1024,
            },
            &ctx,
        );
        assert_eq!(
            unlocked.status,
            Nfs4Status::Ok,
            "LOCKU through the canonical stateid must survive a hub restart"
        );

        // exist_lock_owner4 carries no owner bytes on the wire — the
        // restored map is the only authority for this re-lock.
        let relock = h.handle_lock(
            LockOp {
                locktype: LockType::Write,
                reclaim: false,
                offset: 0,
                length: 512,
                stateid: unlocked.stateid.unwrap(),
                owner: Vec::new(),
                new_lock_owner: false,
                open_seqid: None,
            },
            &ctx,
        );
        assert_eq!(
            relock.status,
            Nfs4Status::Ok,
            "exist-owner LOCK via the restored canonical must succeed"
        );
    }

    /// THE CANONICAL ORPHAN, reclaim half: the reclaim-match arm used
    /// to reply with the restored INTERNAL entry key — a stateid the
    /// stateid table has never seen, so the "successfully reclaimed"
    /// lock failed validation on its very next use.
    #[tokio::test(flavor = "multi_thread")]
    async fn reclaim_replies_the_canonical_stateid_not_the_internal_key() {
        let backend = memory_backend();
        let temp = TempDir::new().unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        let root = fh_mgr.get_root_fh().unwrap();

        let canonical = {
            let state = Arc::new(StateManager::new("vol", Arc::clone(&backend)));
            let locks = LockManager::bring_up(Arc::clone(&backend), false).await;
            let h = LockOperationHandler::new(state, locks);
            let mut ctx = CompoundContext::new(0);
            ctx.session_id = Some(create_test_session(&h, 7));
            ctx.current_fh = Some(root.clone());
            let open_sid = create_test_stateid(&h, 7);
            let res = h.handle_lock(
                LockOp {
                    locktype: LockType::Write,
                    reclaim: false,
                    offset: 0,
                    length: 1024,
                    stateid: open_sid,
                    owner: b"sqlite".to_vec(),
                    new_lock_owner: true,
                    open_seqid: Some(0),
                },
                &ctx,
            );
            assert_eq!(res.status, Nfs4Status::Ok);
            res.stateid.unwrap()
        };
        settle(&backend, 2).await;

        // Restart; the fresh LeaseManager opens its grace window, in
        // which the client reclaims the lock it still holds.
        let state = Arc::new(StateManager::new("vol", Arc::clone(&backend)));
        state.load_from_backend(false).await.unwrap();
        let locks = LockManager::bring_up(Arc::clone(&backend), false).await;
        let h = LockOperationHandler::new(state, locks);
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&h, 7));
        ctx.current_fh = Some(root);
        let open_sid = create_test_stateid(&h, 7);
        let res = h.handle_lock(
            LockOp {
                locktype: LockType::Write,
                reclaim: true,
                offset: 0,
                length: 1024,
                stateid: open_sid,
                owner: b"sqlite".to_vec(),
                new_lock_owner: true,
                open_seqid: Some(0),
            },
            &ctx,
        );
        assert_eq!(res.status, Nfs4Status::Ok, "reclaim of a restored lock must succeed");
        let sid = res.stateid.unwrap();
        assert!(
            minted_entry_counter(&sid.other).is_none(),
            "reclaim replied an internal entry key — unusable in every subsequent op"
        );
        assert_eq!(
            sid.other, canonical.other,
            "the reclaim reply must be the owner's canonical stateid"
        );
        assert!(
            h.state_mgr.stateids.validate(&sid).is_ok(),
            "the reclaim reply must validate for subsequent I/O"
        );
    }


    /// MANY CLUSTERS, ONE HUB: the case-5 clientid steal leaks the
    /// victim's byte-range locks, and nothing can ever reap them.
    ///
    /// This is not a hypothetical collision. On NFSv4.1+ the Linux
    /// client's co_ownerid is `"Linux NFSv4.<minor> <nodename>"` and
    /// NOTHING ELSE — no address, no cluster, no uniquifier unless
    /// `nfs.nfs4_unique_id` is set on the node. A fleet that runs one
    /// agent manifest in every cluster therefore presents ONE identity
    /// from all of them. Captured on the wire from two kind clusters
    /// mounting one hub, both sent the same 19 bytes:
    ///
    ///     Linux NFSv4.2 agent
    ///
    /// `ClientManager::exchange_id` keys on those bytes alone
    /// (`owner_to_id.get(&owner)`), so the second cluster reads as the
    /// first REBOOTING: RFC 8881 §18.35.5 case 5. On the newcomer's
    /// CREATE_SESSION the incumbent is torn down —
    /// `destroy_client_sessions`, `remove_client_stateids`,
    /// `cleanup_client_delegations`, `remove_client`.
    ///
    /// `remove_client_locks` is NOT in that list, and it cannot be:
    /// `SessionOperationHandler` holds only a `StateManager` and has no
    /// reference to the `LockManager` at all.
    ///
    /// What makes it permanent rather than merely untidy is the reaper.
    /// `remove_client_locks` has exactly one production caller —
    /// `courtesy_release_expired`, which iterates
    /// `leases.get_expired_clients()`. `remove_client` has already
    /// dropped the victim's lease, so the victim's id can never appear
    /// in that list again. The rows are persisted and re-seeded at every
    /// startup, so a hub restart does not clear them either.
    ///
    /// The asserts below are the three separable claims. Each would pass
    /// on its own for an innocent reason, which is why all three are
    /// here: locks survive the steal, the reaper cannot see them, and a
    /// THIRD client with a distinct identity — nothing to do with the
    /// collision — is denied the range forever.
    /// MANY CLUSTERS, ONE HUB: a client displaced by a co_ownerid
    /// collision must not leave its locks behind.
    ///
    /// On NFSv4.1+ the Linux client's co_ownerid is
    /// `Linux NFSv4.<minor> <nodename>` and NOTHING else — no address,
    /// no cluster, no uniquifier unless `nfs.nfs4_unique_id` is set on
    /// the node. A fleet running one agent manifest in every cluster
    /// presents ONE identity from all of them. Captured on the wire from
    /// two kind clusters mounting one hub, both sent the same 19 bytes:
    /// `Linux NFSv4.2 agent`. `ClientManager::exchange_id` keys on those
    /// bytes alone and must — RFC 8881 §18.35.5 requires a server to
    /// treat an identical co_ownerid as the same client returning.
    /// Flint cannot refuse the collision. It can decline to lose state
    /// over it.
    ///
    /// This drives the REAL `SessionOperationHandler` rather than
    /// re-typing its cascade, because a test that reproduces the
    /// implementation cannot fail when the implementation is the bug —
    /// and an earlier draft of this test did exactly that, passing
    /// happily against the defect it was written to catch.
    ///
    /// It also mounts the way the driver actually mounts: `nconnect=2`,
    /// so TWO EXCHANGE_IDs arrive before any CREATE_SESSION. That is
    /// load-bearing. The first takes the case-5 arm and records the
    /// obligation; the second finds an unconfirmed record and takes case
    /// 4. If case 4 does not carry the obligation forward, the client
    /// that finally confirms owes no cleanup, the incumbent is never
    /// discarded, and this test would pass for entirely the wrong
    /// reason. Pynfs EID5f misses this because it uses one connection.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_displaced_client_does_not_leave_its_locks_behind() {
        use crate::nfs::v4::operations::session::SessionOperationHandler;
        use crate::nfs::v4::compound::ChannelAttrs;
        use crate::nfs::v4::operations::{CreateSessionOp, ExchangeIdOp};

        let state = Arc::new(StateManager::new_in_memory(""));
        let locks = Arc::new(LockManager::new());
        let handler =
            SessionOperationHandler::new(Arc::clone(&state)).with_lock_manager(Arc::clone(&locks));

        let owner = b"Linux NFSv4.2 agent".to_vec();
        let eid = |verifier: u64| ExchangeIdOp {
            client_owner: owner.clone(),
            verifier,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let confirm = |clientid: u64, sequence: u32| CreateSessionOp {
            clientid,
            sequence,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0x4000_0000,
            cb_sec: Vec::new(),
        };

        // Cluster B's agent mounts and takes an exclusive lock.
        let a = handler.handle_exchange_id(eid(111), &CompoundContext::new(1));
        handler.handle_create_session(confirm(a.clientid, a.sequenceid), &CompoundContext::new(1));
        let victim = a.clientid;
        locks.add_lock(mk_lock(1, victim, 0, 1024));
        assert_eq!(locks.get_client_locks(victim).len(), 1, "victim holds its lock");

        // Cluster C's agent mounts: same owner bytes, its own boot
        // verifier, TWO connections.
        // Connection 1 of the trunking probe. Its outcome is deliberately
        // unused: what matters is that it FIRED, because it is the one that
        // takes on the case-5 obligation the next connection must carry.
        let _c1 = handler.handle_exchange_id(eid(222), &CompoundContext::new(2));
        let c2 = handler.handle_exchange_id(eid(222), &CompoundContext::new(2));
        assert_ne!(c2.clientid, victim, "the newcomer must get its own clientid");
        // The connection that actually confirms is the LAST one.
        handler.handle_create_session(confirm(c2.clientid, c2.sequenceid), &CompoundContext::new(2));

        // The obligation survived the case-4 replacement, so the
        // incumbent was actually discarded.
        assert!(
            state.clients.get_client(victim).is_none(),
            "the case-5 obligation must survive an nconnect trunking probe — the incumbent \
             is still present, so CREATE_SESSION discharged nothing (client.rs case 4 must \
             carry `pending_replaces` forward)",
        );
        assert!(
            state.clients.get_client(c2.clientid).is_some(),
            "the newcomer must be confirmed and live",
        );

        // ANTI-VACUITY, before the claim under test: this manager can
        // report a conflict at all, and an unrelated range is free — so
        // a later "granted" cannot come from a manager that grants
        // everything.
        locks.add_lock(mk_lock(9, c2.clientid, 65536, 1024));
        assert!(
            locks
                .check_conflicts(b"/data/file", &LockRange { offset: 65536, length: 1024 },
                                 LockType::Write, None)
                .is_some(),
            "a live client's lock must still be reported as a conflict",
        );
        assert!(
            locks
                .check_conflicts(b"/data/file", &LockRange { offset: 8192, length: 1024 },
                                 LockType::Write, None)
                .is_none(),
            "an untouched range must be free — otherwise this test proves nothing",
        );

        // THE REQUIREMENT: the displaced client's locks went with the
        // rest of its state. Left behind they would be UNREAPABLE —
        // `remove_client` drops the lease and the only caller of
        // `remove_client_locks` iterates expired LEASES — persisted and
        // re-seeded across restarts, and refused to every other client
        // forever, naming a clientid the server cannot resolve.
        assert_eq!(
            locks.get_client_locks(victim).len(),
            0,
            "a displaced client's locks must be released with the rest of its state",
        );

        // The consequence for the fleet: a third agent, in a third
        // cluster, with no part in the collision, gets the range.
        let verdict = locks.check_conflicts(
            b"/data/file",
            &LockRange { offset: 0, length: 1024 },
            LockType::Write,
            None,
        );
        if let Some(ref d) = verdict {
            assert!(
                state.clients.get_client(d.client_id).is_some(),
                "DENIED by clientid {}, which the server cannot resolve — a phantom lock",
                d.client_id,
            );
        }
        assert!(verdict.is_none(), "an innocent third client must be granted the range");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_lock_wipe_deletes_persisted_records() {
        let backend = memory_backend();
        let mgr = LockManager::with_backend(Arc::clone(&backend));
        mgr.add_lock(mk_lock(1, 42, 0, 100));
        mgr.add_lock(mk_lock(2, 42, 200, 100));
        mgr.add_lock(mk_lock(3, 99, 400, 100));
        settle(&backend, 3).await;

        // Lease expiry path: the dispatcher's courtesy-cleanup calls this.
        mgr.remove_client_locks(42);
        settle(&backend, 1).await;

        let left = backend.list_locks().await.unwrap();
        assert_eq!(left.len(), 1, "only the other client's lock remains");
        assert_eq!(left[0].client_id, 99);
    }

    #[test]
    fn degraded_grace_gates_new_locks_but_not_reclaims() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        let open_stateid = create_test_stateid(&handler, 1);

        // Restart with LOST lock state (unreadable backend). A fresh
        // LeaseManager is inside its grace window by construction.
        handler.lock_mgr.mark_restore_failed();
        assert!(handler.state_mgr.leases.in_grace_period());

        let new_lock = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: open_stateid,
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };
        let res = handler.handle_lock(new_lock, &ctx);
        assert_eq!(
            res.status,
            Nfs4Status::Grace,
            "new locks must wait out grace when pre-restart lock state is lost"
        );

        // A reclaim in the same window is the recovery path — it grants.
        let reclaim = LockOp {
            locktype: LockType::Write,
            reclaim: true,
            offset: 0,
            length: 1024,
            stateid: create_test_stateid(&handler, 1),
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };
        let res = handler.handle_lock(reclaim, &ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
    }

    #[test]
    fn clean_restore_never_gates_new_locks() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.session_id = Some(create_test_session(&handler, 1));
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        // restored_clean defaults true (fresh volume / clean restore):
        // grace must not tax routine restarts.
        assert!(handler.state_mgr.leases.in_grace_period());
        let op = LockOp {
            locktype: LockType::Write,
            reclaim: false,
            offset: 0,
            length: 1024,
            stateid: create_test_stateid(&handler, 1),
            owner: b"owner1".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };
        assert_eq!(handler.handle_lock(op, &ctx).status, Nfs4Status::Ok);
    }

    #[test]
    fn reclaim_of_a_restored_lock_returns_it_instead_of_self_conflicting() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let session_id = create_test_session(&handler, 7);
        ctx.session_id = Some(session_id);
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        let root_fh = ctx.current_fh.as_ref().unwrap().data.clone();

        // Simulate a restart-restored lock for client 7.
        let restored_stateid = StateId { seqid: 3, other: [9; 12] };
        handler.lock_mgr.load_records(vec![crate::state_backend::LockRecord {
            other: restored_stateid.other,
            seqid: restored_stateid.seqid,
            client_id: 7,
            owner: b"owner7".to_vec(),
            filehandle: root_fh,
            lock_type: 2, // WRITE_LT
            offset: 0,
            length: 4096,
        }]);

        // The client reclaims the same lock: it must get the restored
        // stateid back, not a Denied from colliding with itself.
        let reclaim = LockOp {
            locktype: LockType::Write,
            reclaim: true,
            offset: 0,
            length: 4096,
            stateid: create_test_stateid(&handler, 7),
            owner: b"owner7".to_vec(),
            new_lock_owner: true,
            open_seqid: Some(0),
        };
        let res = handler.handle_lock(reclaim, &ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.stateid, Some(restored_stateid));
    }
}
