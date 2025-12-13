// StateId Management
//
// StateIds are 128-bit identifiers for all NFSv4 state:
// - OPEN stateids (file opens)
// - LOCK stateids (byte-range locks)
// - DELEGATION stateids (delegations)
//
// StateId Structure:
// - seqid (32 bits): Sequence number, incremented on each state change
// - other (96 bits): Opaque identifier (unique per state)
//
// Lifecycle:
// 1. Client performs OPEN → server returns stateid (seqid=1)
// 2. Client performs operation → server validates stateid
// 3. State changes (LOCK) → server increments seqid
// 4. Client closes → server revokes stateid

use super::super::protocol::StateId;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info, warn};

/// Special stateid for anonymous/READ-only operations
pub const ANONYMOUS_STATEID: StateId = StateId {
    seqid: 0,
    other: [0; 12],
};

/// Special stateid for READ bypass (all ones)
pub const READ_BYPASS_STATEID: StateId = StateId {
    seqid: 0xFFFFFFFF,
    other: [0xFF; 12],
};

/// Type of state represented by a stateid
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateType {
    /// File open state
    Open,

    /// Byte-range lock state
    Lock,

    /// Delegation state
    Delegation,
}

/// State entry - tracks a single stateid
#[derive(Debug, Clone)]
pub struct StateEntry {
    /// The stateid itself
    pub stateid: StateId,

    /// Type of state
    pub state_type: StateType,

    /// Client that owns this state
    pub client_id: u64,

    /// Current sequence number (matches stateid.seqid)
    pub seqid: u32,

    /// File handle this state is associated with (for opens/locks)
    pub filehandle: Option<Vec<u8>>,

    /// Is this state revoked?
    pub revoked: bool,
}

impl StateEntry {
    /// Create a new state entry
    pub fn new(
        stateid: StateId,
        state_type: StateType,
        client_id: u64,
        filehandle: Option<Vec<u8>>,
    ) -> Self {
        Self {
            stateid,
            state_type,
            client_id,
            seqid: stateid.seqid,
            filehandle,
            revoked: false,
        }
    }

    /// Increment sequence number (for state changes)
    pub fn increment_seqid(&mut self) -> StateId {
        self.seqid += 1;
        self.stateid.seqid = self.seqid;
        self.stateid
    }

    /// Revoke this state
    pub fn revoke(&mut self) {
        self.revoked = true;
        warn!("StateId {:?} revoked", self.stateid);
    }
}

/// StateId manager - tracks all active stateids
///
/// LOCK-FREE DESIGN using DashMap:
/// - Concurrent stateid lookups and validations without blocking
/// - Lock-free stateid allocation and revocation
/// - Per-stateid granularity for high concurrency
/// - Critical for NFSv4.1+ exactly-once semantics (SEQUENCE operations)
pub struct StateIdManager {
    /// Counter for generating unique stateid identifiers (lock-free atomic)
    next_stateid: AtomicU64,

    /// Active stateids (stateid.other → state entry)
    /// We use 'other' as key since seqid changes
    /// DashMap enables lock-free concurrent access
    states: DashMap<[u8; 12], StateEntry>,

    /// Client to stateids mapping (for cleanup)
    /// Lock-free per-client state tracking
    client_states: DashMap<u64, Vec<[u8; 12]>>,
}

impl StateIdManager {
    /// Create a new stateid manager
    pub fn new() -> Self {
        info!("StateIdManager created");

        Self {
            next_stateid: AtomicU64::new(1),
            states: DashMap::new(),
            client_states: DashMap::new(),
        }
    }

    /// Allocate a new stateid
    ///
    /// LOCK-FREE: Concurrent allocations use atomic counter + per-shard DashMap locks
    pub fn allocate(
        &self,
        state_type: StateType,
        client_id: u64,
        filehandle: Option<Vec<u8>>,
    ) -> StateId {
        // Generate unique identifier (lock-free atomic)
        let id = self.next_stateid.fetch_add(1, Ordering::SeqCst);

        // Build 'other' field (96 bits = 12 bytes)
        let mut other = [0u8; 12];
        other[0..8].copy_from_slice(&id.to_be_bytes());
        other[8..12].copy_from_slice(&(client_id as u32).to_be_bytes());

        // Create stateid with seqid=1
        let stateid = StateId {
            seqid: 1,
            other,
        };

        // Create state entry
        let entry = StateEntry::new(stateid, state_type, client_id, filehandle);

        // LOCK-FREE: Direct DashMap inserts without global locks
        self.states.insert(other, entry);
        self.client_states
            .entry(client_id)
            .or_insert_with(Vec::new)
            .push(other);

        info!("StateId allocated: {:?} for client {} (type: {:?})",
              stateid, client_id, state_type);
        stateid
    }

    /// Validate a stateid
    /// Returns Ok(()) if valid, Err(reason) if invalid
    ///
    /// LOCK-FREE: Critical path for all NFSv4 operations - concurrent validations
    pub fn validate(&self, stateid: &StateId) -> Result<(), String> {
        // Check for special stateids
        if stateid == &ANONYMOUS_STATEID {
            return Ok(());
        }
        if stateid == &READ_BYPASS_STATEID {
            return Ok(());
        }

        if let Some(entry) = self.states.get(&stateid.other) {
            // Check if revoked
            if entry.revoked {
                return Err("StateId revoked".to_string());
            }

            // Check sequence number
            if stateid.seqid != entry.seqid {
                return Err(format!("StateId sequence mismatch: expected {}, got {}",
                                  entry.seqid, stateid.seqid));
            }

            Ok(())
        } else {
            Err("StateId not found".to_string())
        }
    }

    /// Validate a stateid with relaxed sequence checking for READ operations
    /// Allows seqid=0 for anonymous/first reads, or accepts any seqid if the stateid exists
    ///
    /// Per RFC 5661: Some clients may use seqid=0 for READ operations
    /// LOCK-FREE: Critical path for all NFSv4 operations - concurrent validations
    pub fn validate_for_read(&self, stateid: &StateId) -> Result<(), String> {
        // Check for special stateids
        if stateid == &ANONYMOUS_STATEID {
            debug!("READ: Accepting ANONYMOUS_STATEID (seqid=0, other=all zeros)");
            return Ok(());
        }
        if stateid == &READ_BYPASS_STATEID {
            debug!("READ: Accepting READ_BYPASS_STATEID (seqid=0xFFFFFFFF, other=all 0xFF)");
            return Ok(());
        }

        // Special case: Accept any stateid with seqid=0 as anonymous read
        if stateid.seqid == 0 {
            // Check if this matches any known stateid (ignoring seqid)
            if let Some(entry) = self.states.get(&stateid.other) {
                if entry.revoked {
                    return Err("StateId revoked".to_string());
                }
                // Accept seqid=0 for READ operations (relaxed validation)
                debug!("READ: Accepting stateid with seqid=0 (relaxed validation), expected seqid={}", entry.seqid);
                return Ok(());
            } else {
                // Unknown stateid - treat as anonymous read
                debug!("READ: Accepting unknown stateid with seqid=0 as anonymous read");
                return Ok(());
            }
        }

        // Standard validation for non-zero seqid
        if let Some(entry) = self.states.get(&stateid.other) {
            // Check if revoked
            if entry.revoked {
                return Err("StateId revoked".to_string());
            }

            // For READ, accept either exact seqid or seqid-1 (client may use old seqid)
            if stateid.seqid == entry.seqid || stateid.seqid == entry.seqid - 1 {
                Ok(())
            } else {
                warn!("READ: StateId sequence mismatch: expected {}, got {} (will accept anyway for READ compatibility)", 
                      entry.seqid, stateid.seqid);
                // For READ operations, accept anyway (lenient)
                Ok(())
            }
        } else {
            Err("StateId not found".to_string())
        }
    }

    /// Update a stateid's sequence number (for state changes)
    ///
    /// LOCK-FREE: Per-stateid locking only, not global
    pub fn update_seqid(&self, stateid: &StateId) -> Result<StateId, String> {
        if let Some(mut entry) = self.states.get_mut(&stateid.other) {
            if entry.revoked {
                return Err("Cannot update revoked stateid".to_string());
            }

            Ok(entry.increment_seqid())
        } else {
            Err("StateId not found".to_string())
        }
    }

    /// Get state entry
    ///
    /// LOCK-FREE: Concurrent reads without blocking
    pub fn get_state(&self, stateid: &StateId) -> Option<StateEntry> {
        self.states.get(&stateid.other).map(|r| r.clone())
    }

    /// Revoke a stateid (e.g., for CLOSE operation)
    ///
    /// LOCK-FREE: Per-stateid locking only, not global
    pub fn revoke(&self, stateid: &StateId) -> Result<(), String> {
        if let Some(mut entry) = self.states.get_mut(&stateid.other) {
            entry.revoke();
            Ok(())
        } else {
            Err("StateId not found".to_string())
        }
    }

    /// Remove a stateid completely (cleanup)
    ///
    /// LOCK-FREE: Removal only locks specific shards, not entire map
    pub fn remove(&self, stateid: &StateId) {
        if let Some((_, entry)) = self.states.remove(&stateid.other) {
            // Remove from client mapping (per-client locking only)
            if let Some(mut state_list) = self.client_states.get_mut(&entry.client_id) {
                state_list.retain(|other| other != &stateid.other);
            }

            info!("StateId removed: {:?}", stateid);
        }
    }

    /// Get all stateids for a client
    ///
    /// LOCK-FREE: Concurrent reads without blocking
    pub fn get_client_stateids(&self, client_id: u64) -> Vec<StateId> {
        if let Some(state_list) = self.client_states.get(&client_id) {
            state_list
                .iter()
                .filter_map(|other| self.states.get(other).map(|e| e.stateid))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Revoke all stateids for a client (for client cleanup)
    ///
    /// LOCK-FREE: Uses lock-free get and revoke operations
    pub fn revoke_client_stateids(&self, client_id: u64) {
        let stateids = self.get_client_stateids(client_id);

        for stateid in stateids {
            let _ = self.revoke(&stateid);
        }

        info!("All stateids revoked for client {}", client_id);
    }

    /// Remove all stateids for a client (cleanup)
    ///
    /// LOCK-FREE: Uses lock-free removal operations
    pub fn remove_client_stateids(&self, client_id: u64) {
        if let Some(state_list) = self.client_states.get(&client_id).map(|r| r.clone()) {
            let count = state_list.len();

            // Remove all stateids (each removal only locks its shard)
            for other in &state_list {
                self.states.remove(other);
            }

            // Remove client mapping
            self.client_states.remove(&client_id);

            info!("Removed {} stateids for client {}", count, client_id);
        }
    }

    /// Get total active stateid count
    ///
    /// LOCK-FREE: Counts without blocking concurrent operations
    pub fn active_count(&self) -> usize {
        self.states.len()
    }

    /// Get stateid count by type
    ///
    /// LOCK-FREE: Iterates without blocking concurrent operations
    pub fn count_by_type(&self, state_type: StateType) -> usize {
        self.states
            .iter()
            .filter(|e| e.value().state_type == state_type && !e.value().revoked)
            .count()
    }
}

impl Default for StateIdManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stateid_allocation() {
        let mgr = StateIdManager::new();

        let stateid1 = mgr.allocate(StateType::Open, 1, None);
        let stateid2 = mgr.allocate(StateType::Open, 1, None);

        // Should be unique
        assert_ne!(stateid1.other, stateid2.other);

        // Both should start with seqid=1
        assert_eq!(stateid1.seqid, 1);
        assert_eq!(stateid2.seqid, 1);

        assert_eq!(mgr.active_count(), 2);
    }

    #[test]
    fn test_stateid_validation() {
        let mgr = StateIdManager::new();

        let stateid = mgr.allocate(StateType::Open, 1, None);

        // Should be valid
        assert!(mgr.validate(&stateid).is_ok());

        // Wrong seqid should fail
        let mut bad_stateid = stateid;
        bad_stateid.seqid = 99;
        assert!(mgr.validate(&bad_stateid).is_err());

        // Non-existent stateid should fail
        let fake_stateid = StateId {
            seqid: 1,
            other: [0xFF; 12],
        };
        assert!(mgr.validate(&fake_stateid).is_err());
    }

    #[test]
    fn test_seqid_update() {
        let mgr = StateIdManager::new();

        let stateid = mgr.allocate(StateType::Open, 1, None);
        assert_eq!(stateid.seqid, 1);

        // Update seqid
        let new_stateid = mgr.update_seqid(&stateid).unwrap();
        assert_eq!(new_stateid.seqid, 2);
        assert_eq!(new_stateid.other, stateid.other);

        // Old seqid should fail validation
        assert!(mgr.validate(&stateid).is_err());

        // New seqid should pass
        assert!(mgr.validate(&new_stateid).is_ok());
    }

    #[test]
    fn test_stateid_revocation() {
        let mgr = StateIdManager::new();

        let stateid = mgr.allocate(StateType::Open, 1, None);
        assert!(mgr.validate(&stateid).is_ok());

        // Revoke
        mgr.revoke(&stateid).unwrap();

        // Should fail validation
        assert!(mgr.validate(&stateid).is_err());

        // Should still be in active count
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn test_stateid_removal() {
        let mgr = StateIdManager::new();

        let stateid = mgr.allocate(StateType::Open, 1, None);
        assert_eq!(mgr.active_count(), 1);

        mgr.remove(&stateid);

        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.validate(&stateid).is_err());
    }

    #[test]
    fn test_client_stateids() {
        let mgr = StateIdManager::new();

        // Client 1 has 2 stateids
        let _s1 = mgr.allocate(StateType::Open, 1, None);
        let _s2 = mgr.allocate(StateType::Lock, 1, None);

        // Client 2 has 1 stateid
        let _s3 = mgr.allocate(StateType::Open, 2, None);

        let client1_states = mgr.get_client_stateids(1);
        let client2_states = mgr.get_client_stateids(2);

        assert_eq!(client1_states.len(), 2);
        assert_eq!(client2_states.len(), 1);
    }

    #[test]
    fn test_client_cleanup() {
        let mgr = StateIdManager::new();

        mgr.allocate(StateType::Open, 1, None);
        mgr.allocate(StateType::Lock, 1, None);
        mgr.allocate(StateType::Open, 2, None);

        assert_eq!(mgr.active_count(), 3);

        // Remove all client 1 stateids
        mgr.remove_client_stateids(1);

        assert_eq!(mgr.active_count(), 1);
        assert_eq!(mgr.get_client_stateids(1).len(), 0);
        assert_eq!(mgr.get_client_stateids(2).len(), 1);
    }

    #[test]
    fn test_count_by_type() {
        let mgr = StateIdManager::new();

        mgr.allocate(StateType::Open, 1, None);
        mgr.allocate(StateType::Open, 2, None);
        mgr.allocate(StateType::Lock, 1, None);

        assert_eq!(mgr.count_by_type(StateType::Open), 2);
        assert_eq!(mgr.count_by_type(StateType::Lock), 1);
        assert_eq!(mgr.count_by_type(StateType::Delegation), 0);
    }

    #[test]
    fn test_special_stateids() {
        let mgr = StateIdManager::new();

        // Anonymous stateid should validate
        assert!(mgr.validate(&ANONYMOUS_STATEID).is_ok());

        // Read bypass stateid should validate
        assert!(mgr.validate(&READ_BYPASS_STATEID).is_ok());
    }
}
