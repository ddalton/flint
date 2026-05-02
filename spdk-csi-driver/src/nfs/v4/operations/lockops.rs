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
use dashmap::DashMap;
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
    pub length: u64,  // 0 means "to EOF"
}

impl LockRange {
    /// Check if this range overlaps with another
    pub fn overlaps(&self, other: &LockRange) -> bool {
        // Special case: length=0 means "to EOF"
        if self.length == 0 || other.length == 0 {
            // If either range goes to EOF, check if start positions allow overlap
            let self_end = if self.length == 0 { u64::MAX } else { self.offset + self.length };
            let other_end = if other.length == 0 { u64::MAX } else { other.offset + other.length };

            self.offset < other_end && other.offset < self_end
        } else {
            // Normal range overlap check
            let self_end = self.offset + self.length;
            let other_end = other.offset + other.length;

            self.offset < other_end && other.offset < self_end
        }
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

/// Lock manager - tracks all active locks
///
/// LOCK-FREE DESIGN using DashMap:
/// - No global locks, only per-shard locks in DashMap
/// - Concurrent reads without blocking
/// - Lock-free lookups for read-heavy workloads
/// - Per-file lock tracking for fine-grained concurrency
pub struct LockManager {
    /// Active locks (stateid → lock)
    /// DashMap provides lock-free concurrent access
    locks: DashMap<[u8; 12], Lock>,  // Key is stateid.other

    /// Locks by filehandle (for conflict detection)
    /// Enables per-file locking - only locks on same file conflict
    locks_by_fh: DashMap<Vec<u8>, Vec<[u8; 12]>>,
}

impl LockManager {
    /// Create a new lock manager
    pub fn new() -> Self {
        Self {
            locks: DashMap::new(),
            locks_by_fh: DashMap::new(),
        }
    }

    /// Add a lock
    ///
    /// LOCK-FREE: DashMap handles concurrent inserts without global locks
    pub fn add_lock(&self, lock: Lock) {
        let stateid_key = lock.stateid.other;
        let fh_key = lock.filehandle.clone();

        // Add to main lock map (lock-free insert)
        self.locks.insert(stateid_key, lock);

        // Add to filehandle index (lock-free update)
        self.locks_by_fh
            .entry(fh_key)
            .or_insert_with(Vec::new)
            .push(stateid_key);
    }

    /// Check for lock conflicts
    ///
    /// LOCK-FREE: Uses per-file lock tracking for fine-grained concurrency
    /// Only checks locks on the same file, enabling concurrent ops on different files
    pub fn check_conflicts(
        &self,
        filehandle: &[u8],
        range: &LockRange,
        lock_type: LockType,
        exclude_stateid: Option<StateId>,
    ) -> Option<Lock> {
        // Get all locks on this filehandle (lock-free read)
        if let Some(lock_stateids) = self.locks_by_fh.get(filehandle) {
            for stateid_key in lock_stateids.value() {
                // Skip the lock we're excluding (for lock upgrades)
                if let Some(ref exclude) = exclude_stateid {
                    if stateid_key == &exclude.other {
                        continue;
                    }
                }

                // Lock-free lookup
                if let Some(existing_lock) = self.locks.get(stateid_key) {
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
            if let Some(mut fh_locks) = self.locks_by_fh.get_mut(&lock.filehandle) {
                fh_locks.retain(|k| k != &stateid_key);
                if fh_locks.is_empty() {
                    drop(fh_locks); // Release borrow
                    self.locks_by_fh.remove(&lock.filehandle);
                }
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

        // Check if this is a lock reclaim (after server reboot)
        if op.reclaim {
            if !self.state_mgr.leases.in_grace_period() {
                return LockRes {
                    status: Nfs4Status::NoGrace,
                    stateid: None,
                    denied: None,
                };
            }
        }

        // RFC 5661 §18.10.3: `length == 0` is reserved to mean "lock from
        // offset to EOF". For any non-zero length, `offset + length` MUST not
        // overflow u64; if it does, the server MUST return NFS4ERR_INVAL.
        if op.length != 0 && op.offset.checked_add(op.length).is_none() {
            warn!("LOCK: byte range overflow (offset={}, length={})",
                  op.offset, op.length);
            return LockRes {
                status: Nfs4Status::Inval,
                stateid: None,
                denied: None,
            };
        }

        let range = LockRange {
            offset: op.offset,
            length: op.length,
        };

        // Check for conflicts
        if let Some(conflicting_lock) = self.lock_mgr.check_conflicts(
            &current_fh.data,
            &range,
            op.locktype,
            None,
        ) {
            warn!("LOCK: Conflict detected with existing lock");
            return LockRes {
                status: Nfs4Status::Denied,
                stateid: None,
                denied: Some(LockDenied {
                    offset: conflicting_lock.range.offset,
                    length: conflicting_lock.range.length,
                    locktype: conflicting_lock.lock_type,
                    owner: conflicting_lock.owner,
                }),
            };
        }

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

        let lock_stateid = self.state_mgr.stateids.allocate(
            StateType::Lock,
            client_id,
            Some(current_fh.data.clone()),
        );

        // Create lock entry
        let lock = Lock {
            stateid: lock_stateid,
            client_id,
            owner: op.owner,
            filehandle: current_fh.data.clone(),
            lock_type: op.locktype,
            range,
        };

        // Add to lock manager
        self.lock_mgr.add_lock(lock);

        info!("LOCK: Acquired {:?} lock on range {}+{}", op.locktype, op.offset, op.length);

        LockRes {
            status: Nfs4Status::Ok,
            stateid: Some(lock_stateid),
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
    pub fn handle_locku(
        &self,
        op: LockUOp,
        ctx: &CompoundContext,
    ) -> LockURes {
        debug!("LOCKU: offset={}, length={}", op.offset, op.length);

        // Check current filehandle
        if ctx.current_fh.is_none() {
            return LockURes {
                status: Nfs4Status::NoFileHandle,
                stateid: None,
            };
        }

        // Validate lock stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("LOCKU: Invalid stateid: {}", e);
            return LockURes {
                status: Nfs4Status::BadStateId,
                stateid: None,
            };
        }

        // Remove lock
        if self.lock_mgr.remove_lock(&op.stateid).is_some() {
            info!("LOCKU: Released lock on range {}+{}", op.offset, op.length);

            // Return updated stateid
            let new_stateid = StateId {
                seqid: op.stateid.seqid + 1,
                other: op.stateid.other,
            };

            LockURes {
                status: Nfs4Status::Ok,
                stateid: Some(new_stateid),
            }
        } else {
            warn!("LOCKU: Lock not found");
            LockURes {
                status: Nfs4Status::BadStateId,
                stateid: None,
            }
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
        let state_mgr = Arc::new(StateManager::new(""));
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
            .create_session(client_id, 0, 0, 1024 * 1024, 1024 * 1024, 64 * 1024, 8)
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
}
