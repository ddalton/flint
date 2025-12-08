//! High-Performance NLM Lock Manager
//!
//! Lock-free architecture using DashMap for concurrent access without global locks.
//!
//! ## Design
//! - DashMap<FileId, FileLocks> for lock-free per-file lock storage
//! - Each file has its own lock list (typically 0-2 locks)
//! - Lock conflict detection is O(n) where n = locks per file (small)
//! - No global locks = excellent scalability

use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, info};

/// Client identifier for lock ownership
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId {
    /// Client host address
    pub addr: String,
    /// Client process ID (opaque identifier)
    pub pid: u32,
}

/// Lock type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    /// Read lock (shared - multiple readers allowed)
    Read,
    /// Write lock (exclusive - only one writer)
    Write,
}

/// File lock entry
#[derive(Debug, Clone)]
pub struct Lock {
    /// Lock owner
    pub owner: ClientId,
    /// Lock type (read/write)
    pub lock_type: LockType,
    /// Byte range offset
    pub offset: u64,
    /// Byte range length (0 = to EOF)
    pub length: u64,
}

/// Lock conflict result
#[derive(Debug, PartialEq, Eq)]
pub enum LockResult {
    /// Lock granted
    Granted,
    /// Lock denied - conflicts with existing lock
    Denied,
    /// Lock already held by same owner
    AlreadyHeld,
}

/// Per-file lock state
#[derive(Debug, Default)]
struct FileLocks {
    /// List of active locks on this file
    locks: Vec<Lock>,
}

/// High-performance lock manager using DashMap
pub struct LockManager {
    /// File ID → locks mapping (lock-free concurrent access)
    locks: Arc<DashMap<u64, FileLocks>>,
}

impl LockManager {
    /// Create new lock manager
    pub fn new() -> Self {
        Self {
            locks: Arc::new(DashMap::new()),
        }
    }

    /// Test if a lock would conflict (NLM_TEST)
    pub fn test_lock(&self, file_id: u64, lock: &Lock) -> LockResult {
        // Fast path: no locks on file
        if !self.locks.contains_key(&file_id) {
            return LockResult::Granted;
        }

        // Check for conflicts
        if let Some(file_locks) = self.locks.get(&file_id) {
            self.check_conflicts(&lock, &file_locks.locks)
        } else {
            LockResult::Granted
        }
    }

    /// Acquire a lock (NLM_LOCK)
    pub fn lock(&self, file_id: u64, lock: Lock) -> LockResult {
        // Get or create file locks entry
        let mut entry = self.locks.entry(file_id).or_default();

        // Check for conflicts
        let result = self.check_conflicts(&lock, &entry.locks);

        if result == LockResult::Granted {
            // No conflicts - add lock
            entry.locks.push(lock.clone());
            debug!("Lock granted on file {}: {:?}", file_id, lock);
        } else {
            debug!("Lock denied on file {}: conflicts detected", file_id);
        }

        result
    }

    /// Release a lock (NLM_UNLOCK)
    pub fn unlock(&self, file_id: u64, owner: &ClientId, offset: u64, length: u64) -> bool {
        if let Some(mut entry) = self.locks.get_mut(&file_id) {
            let initial_count = entry.locks.len();

            // Remove locks matching owner and range
            entry.locks.retain(|l| {
                !(l.owner == *owner && l.offset == offset && l.length == length)
            });

            let removed = initial_count > entry.locks.len();

            if removed {
                debug!("Lock released on file {} by {:?}", file_id, owner);
            }

            // Clean up empty entries
            if entry.locks.is_empty() {
                drop(entry);
                self.locks.remove(&file_id);
            }

            removed
        } else {
            false
        }
    }

    /// Release all locks held by a client (on disconnect)
    pub fn unlock_all_for_client(&self, client: &ClientId) {
        let mut files_to_clean = Vec::new();

        // Iterate through all files and remove client's locks
        for mut entry in self.locks.iter_mut() {
            let file_id = *entry.key();
            let initial_count = entry.value().locks.len();

            entry.value_mut().locks.retain(|l| l.owner != *client);

            let removed = initial_count - entry.value().locks.len();
            if removed > 0 {
                info!("Released {} locks for client {:?} on file {}", removed, client, file_id);
            }

            if entry.value().locks.is_empty() {
                files_to_clean.push(file_id);
            }
        }

        // Clean up empty entries
        for file_id in files_to_clean {
            self.locks.remove(&file_id);
        }
    }

    /// Check if lock conflicts with existing locks
    fn check_conflicts(&self, new_lock: &Lock, existing: &[Lock]) -> LockResult {
        for existing_lock in existing {
            // Check if same owner already holds this exact lock
            if existing_lock.owner == new_lock.owner
                && existing_lock.offset == new_lock.offset
                && existing_lock.length == new_lock.length
            {
                return LockResult::AlreadyHeld;
            }

            // Check if ranges overlap
            if !self.ranges_overlap(
                new_lock.offset,
                new_lock.length,
                existing_lock.offset,
                existing_lock.length,
            ) {
                continue; // No overlap, check next lock
            }

            // Ranges overlap - check lock types
            match (new_lock.lock_type, existing_lock.lock_type) {
                (LockType::Read, LockType::Read) => {
                    // Shared locks can coexist
                    continue;
                }
                _ => {
                    // Write lock conflicts with any lock, or read conflicts with write
                    return LockResult::Denied;
                }
            }
        }

        LockResult::Granted
    }

    /// Check if two byte ranges overlap
    fn ranges_overlap(&self, off1: u64, len1: u64, off2: u64, len2: u64) -> bool {
        // Handle EOF locks (length = 0 means to end of file)
        let end1 = if len1 == 0 {
            u64::MAX
        } else {
            off1.saturating_add(len1)
        };

        let end2 = if len2 == 0 {
            u64::MAX
        } else {
            off2.saturating_add(len2)
        };

        // Ranges overlap if: start1 < end2 && start2 < end1
        off1 < end2 && off2 < end1
    }

    /// Get lock statistics (for debugging/monitoring)
    pub fn stats(&self) -> LockStats {
        let total_files = self.locks.len();
        let total_locks: usize = self.locks.iter().map(|e| e.value().locks.len()).sum();

        LockStats {
            files_with_locks: total_files,
            total_locks,
        }
    }
}

/// Lock manager statistics
#[derive(Debug)]
pub struct LockStats {
    pub files_with_locks: usize,
    pub total_locks: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(id: u32) -> ClientId {
        ClientId {
            addr: "127.0.0.1".to_string(),
            pid: id,
        }
    }

    #[test]
    fn test_no_conflict_different_ranges() {
        let lm = LockManager::new();
        let file_id = 1;

        let lock1 = Lock {
            owner: client(1),
            lock_type: LockType::Write,
            offset: 0,
            length: 100,
        };

        let lock2 = Lock {
            owner: client(2),
            lock_type: LockType::Write,
            offset: 200,
            length: 100,
        };

        assert_eq!(lm.lock(file_id, lock1), LockResult::Granted);
        assert_eq!(lm.lock(file_id, lock2), LockResult::Granted);
    }

    #[test]
    fn test_conflict_overlapping_writes() {
        let lm = LockManager::new();
        let file_id = 1;

        let lock1 = Lock {
            owner: client(1),
            lock_type: LockType::Write,
            offset: 0,
            length: 100,
        };

        let lock2 = Lock {
            owner: client(2),
            lock_type: LockType::Write,
            offset: 50,
            length: 100,
        };

        assert_eq!(lm.lock(file_id, lock1), LockResult::Granted);
        assert_eq!(lm.lock(file_id, lock2), LockResult::Denied);
    }

    #[test]
    fn test_shared_read_locks() {
        let lm = LockManager::new();
        let file_id = 1;

        let lock1 = Lock {
            owner: client(1),
            lock_type: LockType::Read,
            offset: 0,
            length: 100,
        };

        let lock2 = Lock {
            owner: client(2),
            lock_type: LockType::Read,
            offset: 50,
            length: 100,
        };

        assert_eq!(lm.lock(file_id, lock1), LockResult::Granted);
        assert_eq!(lm.lock(file_id, lock2), LockResult::Granted); // Shared lock OK
    }

    #[test]
    fn test_unlock() {
        let lm = LockManager::new();
        let file_id = 1;

        let lock = Lock {
            owner: client(1),
            lock_type: LockType::Write,
            offset: 0,
            length: 100,
        };

        assert_eq!(lm.lock(file_id, lock.clone()), LockResult::Granted);
        assert!(lm.unlock(file_id, &lock.owner, lock.offset, lock.length));

        // Should be able to lock again after unlock
        assert_eq!(lm.lock(file_id, lock), LockResult::Granted);
    }

    #[test]
    fn test_eof_lock() {
        let lm = LockManager::new();
        let file_id = 1;

        let eof_lock = Lock {
            owner: client(1),
            lock_type: LockType::Write,
            offset: 0,
            length: 0, // To EOF
        };

        let range_lock = Lock {
            owner: client(2),
            lock_type: LockType::Write,
            offset: 1000,
            length: 100,
        };

        assert_eq!(lm.lock(file_id, eof_lock), LockResult::Granted);
        assert_eq!(lm.lock(file_id, range_lock), LockResult::Denied); // EOF lock covers all
    }
}
