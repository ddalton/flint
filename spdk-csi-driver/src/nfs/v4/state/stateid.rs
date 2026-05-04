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
use crate::state_backend::{spawn_persist, StateBackend, StateIdRecord, StateTypeRecord};
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

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
    /// Snapshot the persisted bits of this state entry for the
    /// [`StateBackend`].
    pub(crate) fn to_record(&self) -> StateIdRecord {
        StateIdRecord {
            other: self.stateid.other,
            seqid: self.seqid,
            state_type: match self.state_type {
                StateType::Open => StateTypeRecord::Open,
                StateType::Lock => StateTypeRecord::Lock,
                StateType::Delegation => StateTypeRecord::Delegation,
            },
            client_id: self.client_id,
            filehandle: self.filehandle.clone(),
            revoked: self.revoked,
        }
    }

    /// Inverse of `to_record`. Used at startup by
    /// [`StateIdManager::load_records`] to repopulate the in-memory
    /// DashMap from a backend snapshot.
    pub(crate) fn from_record(r: StateIdRecord) -> Self {
        let stateid = StateId {
            seqid: r.seqid,
            other: r.other,
        };
        let state_type = match r.state_type {
            StateTypeRecord::Open => StateType::Open,
            StateTypeRecord::Lock => StateType::Lock,
            StateTypeRecord::Delegation => StateType::Delegation,
        };
        Self {
            stateid,
            state_type,
            client_id: r.client_id,
            seqid: r.seqid,
            filehandle: r.filehandle,
            revoked: r.revoked,
        }
    }

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

    /// Per-(client, owner, filehandle) open state (RFC 7530 §16.16):
    /// repeated OPENs by the same owner on the same file return the
    /// SAME `stateid.other` with `seqid` bumped, and the share-mask
    /// is updated. Keyed by `(client_id, owner_bytes, fh_bytes)`.
    /// **In-memory only** — not persisted because (a) restart drops
    /// in-flight open state anyway, (b) Linux clients reissue OPEN
    /// after restart via the BADSESSION recovery path, and (c)
    /// persisting open-owner records would require schema bump +
    /// boundary plumbing that's heavier than the win.
    open_states: DashMap<(u64, Vec<u8>, Vec<u8>), OpenState>,

    /// Index from filehandle bytes → list of `(client_id, owner)`
    /// pairs that have an open on it. Used by share-deny conflict
    /// detection (RFC 8881 §9.7) to scan all conflicting opens
    /// without iterating the full `open_states` map.
    opens_by_fh: DashMap<Vec<u8>, Vec<(u64, Vec<u8>)>>,

    /// Persistence target. See `client.rs` for the full rationale;
    /// stateids surviving restart is what prevents `BAD_STATEID` on
    /// the client's next WRITE after an MDS pod roll.
    backend: Arc<dyn StateBackend>,
}

/// Per-(client, owner, fh) open state. `share_access` /
/// `share_deny` are the bitmasks the client passed; `verifier` is
/// the EXCLUSIVE4 / EXCLUSIVE4_1 verifier captured on the original
/// create so a retry with a matching verifier returns the same
/// stateid (RFC 8881 §18.16.5).
#[derive(Debug, Clone)]
pub struct OpenState {
    pub stateid_other: [u8; 12],
    pub seqid: u32,
    pub share_access: u32,
    pub share_deny: u32,
    pub verifier: Option<u64>,
}

impl StateIdManager {
    /// Create a new stateid manager backed by `backend`.
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        info!("StateIdManager created");

        Self {
            next_stateid: AtomicU64::new(1),
            states: DashMap::new(),
            client_states: DashMap::new(),
            open_states: DashMap::new(),
            opens_by_fh: DashMap::new(),
            backend,
        }
    }

    /// Look up an existing open by (client, owner, fh). RFC 7530
    /// §16.16: returns Some when this open-owner already has state
    /// for this file; the dispatcher then bumps the existing
    /// stateid's seqid instead of allocating a new one.
    pub fn find_open(
        &self,
        client_id: u64,
        owner: &[u8],
        fh: &[u8],
    ) -> Option<OpenState> {
        self.open_states
            .get(&(client_id, owner.to_vec(), fh.to_vec()))
            .map(|e| e.clone())
    }

    /// RFC 8881 §9.7 share-reservation conflict check. Returns true
    /// if any *other* open on this filehandle has a deny mask that
    /// conflicts with `requested_access`, or holds an access bit
    /// that this caller's deny mask would prohibit.
    ///
    /// "Same (client, owner)" is exempt — repeated OPENs by the
    /// same owner upgrade their own share-mask without conflicting.
    pub fn share_conflict(
        &self,
        fh: &[u8],
        client_id: u64,
        owner: &[u8],
        requested_access: u32,
        requested_deny: u32,
    ) -> bool {
        let entries = match self.opens_by_fh.get(&fh.to_vec()) {
            Some(e) => e.clone(),
            None => return false,
        };
        for (other_cid, other_owner) in entries {
            if other_cid == client_id && other_owner.as_slice() == owner {
                continue; // same owner — no conflict
            }
            if let Some(state) = self
                .open_states
                .get(&(other_cid, other_owner.clone(), fh.to_vec()))
            {
                // existing's deny vs requested access:
                if state.share_deny & requested_access != 0 {
                    return true;
                }
                // existing's access vs our deny:
                if state.share_access & requested_deny != 0 {
                    return true;
                }
            }
        }
        false
    }

    /// Record-or-update an open. If an existing entry exists for the
    /// `(client_id, owner, fh)` triple, bump its seqid and merge the
    /// new share-mask in (RFC 7530 §16.16: subsequent OPENs upgrade
    /// the share semantics). Otherwise allocate a fresh stateid via
    /// the normal `allocate` path and stamp it into `open_states`.
    /// Returns the up-to-date `StateId` to send to the client.
    pub fn record_open(
        &self,
        client_id: u64,
        owner: Vec<u8>,
        fh: Vec<u8>,
        share_access: u32,
        share_deny: u32,
        verifier: Option<u64>,
    ) -> StateId {
        let key = (client_id, owner.clone(), fh.clone());
        if let Some(mut entry) = self.open_states.get_mut(&key) {
            entry.seqid = entry.seqid.wrapping_add(1);
            entry.share_access |= share_access;
            entry.share_deny |= share_deny;
            // Verifier on a follow-on open is preserved from the
            // original create; we only set it on first-create paths.
            let stateid = StateId {
                seqid: entry.seqid,
                other: entry.stateid_other,
            };
            // Mirror the seqid bump onto the master state map so
            // validate() agrees about the current seqid.
            if let Some(mut master) = self.states.get_mut(&entry.stateid_other) {
                master.seqid = entry.seqid;
                master.stateid.seqid = entry.seqid;
            }
            return stateid;
        }
        // Fresh allocation. Reuses the existing `allocate` path so
        // the master `states` map and `client_states` index stay
        // consistent with everything else.
        let stateid = self.allocate(StateType::Open, client_id, Some(fh.clone()));
        self.open_states.insert(
            key,
            OpenState {
                stateid_other: stateid.other,
                seqid: stateid.seqid,
                share_access,
                share_deny,
                verifier,
            },
        );
        self.opens_by_fh
            .entry(fh)
            .or_insert_with(Vec::new)
            .push((client_id, owner));
        stateid
    }

    /// EXCLUSIVE4 / EXCLUSIVE4_1 retry semantics: if any prior open
    /// on this filehandle was an exclusive create with `verifier`,
    /// return its OpenState. RFC 8881 §18.16.5: a retry with the
    /// same verifier returns the existing stateid; a different
    /// verifier on an existing file returns `NFS4ERR_EXIST`.
    pub fn find_exclusive_match(&self, fh: &[u8], verifier: u64) -> Option<OpenState> {
        let entries = self.opens_by_fh.get(&fh.to_vec()).map(|e| e.clone()).unwrap_or_default();
        for (cid, owner) in entries {
            if let Some(state) = self
                .open_states
                .get(&(cid, owner, fh.to_vec()))
            {
                if state.verifier == Some(verifier) {
                    return Some(state.clone());
                }
            }
        }
        None
    }

    /// Repopulate the in-memory DashMap from a backend snapshot.
    /// Bumps `next_stateid` past the highest persisted id so freshly
    /// allocated stateids never collide.
    pub fn load_records(&self, records: Vec<StateIdRecord>) {
        let mut max_counter: u64 = 0;
        for r in records {
            // Recover the numeric counter from the high 8 bytes of
            // `other` — `allocate` encodes `(counter, client_id_low)`
            // there.
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&r.other[0..8]);
            max_counter = max_counter.max(u64::from_be_bytes(buf));
            let cid = r.client_id;
            let other = r.other;
            self.states.insert(other, StateEntry::from_record(r));
            self.client_states
                .entry(cid)
                .or_insert_with(Vec::new)
                .push(other);
        }
        if max_counter >= self.next_stateid.load(Ordering::SeqCst) {
            self.next_stateid.store(max_counter + 1, Ordering::SeqCst);
        }
        info!(
            "StateIdManager loaded {} records from backend",
            self.states.len()
        );
    }

    fn persist(&self, e: &StateEntry) {
        let backend = Arc::clone(&self.backend);
        let record = e.to_record();
        spawn_persist(
            "stateid",
            move || async move { backend.put_stateid(&record).await },
        );
    }

    fn persist_delete(&self, other: [u8; 12]) {
        let backend = Arc::clone(&self.backend);
        spawn_persist(
            "stateid_delete",
            move || async move { backend.delete_stateid(&other).await },
        );
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

        // Persist before inserting so the boundary code in B.4 sees a
        // consistent snapshot if list_stateids is called concurrently
        // with allocations (it isn't today, but cheap insurance).
        self.persist(&entry);

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

    /// Validate a stateid for any state-using operation (WRITE, OPEN_DOWNGRADE,
    /// CLOSE, LOCK, …).
    ///
    /// RFC 8881 §16.2.3.1 / §8.2.2 admits four forms:
    ///   * `ANONYMOUS_STATEID` (all zeros)
    ///   * `READ_BYPASS_STATEID` (all 0xFF)
    ///   * "Current stateid" form: `seqid == 0` with a non-zero `other`. The
    ///     server MUST resolve this to the most recent seqid for that
    ///     `other`. Linux clients carry seqid=0 routinely after an OPEN
    ///     because the open-response seqid doesn't propagate to subsequent
    ///     ops in the same COMPOUND.
    ///   * Exact match of the server's current `seqid` for `other`.
    ///
    /// We do NOT accept `seqid - 1` here — that's a READ-only relaxation
    /// (see `validate_for_read`).
    pub fn validate(&self, stateid: &StateId) -> Result<(), String> {
        if stateid == &ANONYMOUS_STATEID {
            return Ok(());
        }
        if stateid == &READ_BYPASS_STATEID {
            return Ok(());
        }

        let entry = self.states.get(&stateid.other)
            .ok_or_else(|| "StateId not found".to_string())?;
        if entry.revoked {
            return Err("StateId revoked".to_string());
        }

        // "Current stateid" form: seqid=0 with a non-zero other resolves to
        // the latest seqid the server holds for this state.
        if stateid.seqid == 0 {
            return Ok(());
        }

        if stateid.seqid == entry.seqid {
            Ok(())
        } else {
            Err(format!("StateId sequence mismatch: expected {} (or 0 for current), got {}",
                       entry.seqid, stateid.seqid))
        }
    }

    /// Validate a stateid for READ.
    ///
    /// RFC 5661 §8.2.2 / §18.22: READ accepts a few stateid forms:
    ///   * `ANONYMOUS_STATEID` (all zeros) — anonymous read.
    ///   * `READ_BYPASS_STATEID` (all 0xFF) — bypass file locking checks.
    ///   * An open / lock / delegation stateid whose `seqid` matches the
    ///     server's current value, OR is the immediately preceding value
    ///     (the client may legitimately race a SETATTR/OPEN_DOWNGRADE
    ///     against READ — RFC 5661 §8.2.2 specifically allows the
    ///     "current" stateid OR `seqid - 1`).
    ///
    /// The previous implementation also accepted any unknown stateid with
    /// `seqid == 0` and any seqid mismatch with a `warn!()` — both are RFC
    /// violations that hide client bugs.
    pub fn validate_for_read(&self, stateid: &StateId) -> Result<(), String> {
        if stateid == &ANONYMOUS_STATEID {
            return Ok(());
        }
        if stateid == &READ_BYPASS_STATEID {
            return Ok(());
        }

        let entry = self.states.get(&stateid.other)
            .ok_or_else(|| "StateId not found".to_string())?;
        if entry.revoked {
            return Err("StateId revoked".to_string());
        }

        // Accept exact seqid, or seqid - 1 (a one-behind retransmit window).
        // saturating_sub avoids underflow when `entry.seqid == 0`; in that
        // case only an exact `seqid == 0` match is acceptable.
        let prev = entry.seqid.saturating_sub(1);
        if stateid.seqid == entry.seqid || stateid.seqid == prev {
            Ok(())
        } else {
            Err(format!(
                "StateId seqid mismatch: expected {} (or {}), got {}",
                entry.seqid, prev, stateid.seqid
            ))
        }
    }

    /// Update a stateid's sequence number (for state changes)
    ///
    /// LOCK-FREE: Per-stateid locking only, not global
    pub fn update_seqid(&self, stateid: &StateId) -> Result<StateId, String> {
        let result = if let Some(mut entry) = self.states.get_mut(&stateid.other) {
            if entry.revoked {
                return Err("Cannot update revoked stateid".to_string());
            }
            let new_id = entry.increment_seqid();
            let snap = entry.clone();
            drop(entry);
            self.persist(&snap);
            new_id
        } else {
            return Err("StateId not found".to_string());
        };
        Ok(result)
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
            let snap = entry.clone();
            drop(entry);
            self.persist(&snap);
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

            self.persist_delete(stateid.other);

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

    /// Remove all stateids for a client (cleanup). Also drops the
    /// client's open-state records and removes them from the
    /// `opens_by_fh` index so a fresh conflicting OPEN can succeed
    /// (RFC 8881 §8.4.2.4 courtesy-release semantics: when a
    /// client's lease expires, its share-reservations become
    /// courtesy state and MUST be released on conflict from another
    /// client).
    ///
    /// LOCK-FREE: Uses lock-free removal operations
    pub fn remove_client_stateids(&self, client_id: u64) {
        if let Some(state_list) = self.client_states.get(&client_id).map(|r| r.clone()) {
            let count = state_list.len();

            // Remove all stateids (each removal only locks its shard)
            for other in &state_list {
                self.states.remove(other);
                self.persist_delete(*other);
            }

            // Remove client mapping
            self.client_states.remove(&client_id);

            info!("Removed {} stateids for client {}", count, client_id);
        }

        // Open-state cleanup: every (client_id, owner, fh) entry
        // belonging to this client AND its index entries in
        // `opens_by_fh`. Iterating once and rebuilding is cheaper
        // than retain() since we touch both maps.
        let to_drop: Vec<(u64, Vec<u8>, Vec<u8>)> = self
            .open_states
            .iter()
            .filter(|e| e.key().0 == client_id)
            .map(|e| e.key().clone())
            .collect();
        for key in &to_drop {
            self.open_states.remove(key);
        }
        // Strip the client out of the by-fh index. If the resulting
        // entry is empty, drop the fh key too.
        let touched_fhs: Vec<Vec<u8>> = to_drop.iter().map(|k| k.2.clone()).collect();
        for fh in touched_fhs {
            if let Some(mut entry) = self.opens_by_fh.get_mut(&fh) {
                entry.retain(|(cid, _)| *cid != client_id);
                let now_empty = entry.is_empty();
                drop(entry);
                if now_empty {
                    self.opens_by_fh.remove(&fh);
                }
            }
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

// `StateIdManager` no longer has a `Default` impl: see `SessionManager`
// for the rationale (constructor now requires a backend).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stateid_allocation() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

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
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

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
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

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

    /// RFC 7530 §16.16: a follow-on OPEN by the same (client, owner)
    /// on the same filehandle returns the SAME `stateid.other` with
    /// `seqid` bumped, and merges the share-mask. Pynfs OPEN2
    /// asserts seqid=2 on the second OPEN.
    #[test]
    fn test_record_open_bumps_seqid_for_same_owner() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let owner = b"alice".to_vec();
        let fh = b"/path/to/file".to_vec();

        let s1 = mgr.record_open(1, owner.clone(), fh.clone(), 2 /*WRITE*/, 0, None);
        assert_eq!(s1.seqid, 1);

        let s2 = mgr.record_open(1, owner.clone(), fh.clone(), 1 /*READ*/, 0, None);
        assert_eq!(s2.seqid, 2);
        assert_eq!(s2.other, s1.other, "same owner+fh must reuse stateid.other");

        // The merged share-access should be WRITE | READ = BOTH (3).
        let st = mgr.find_open(1, &owner, &fh).unwrap();
        assert_eq!(st.share_access, 3);

        // validate() agrees about the latest seqid.
        assert!(mgr.validate(&s2).is_ok());
        // The old seqid is stale.
        assert!(mgr.validate(&s1).is_err());
    }

    /// RFC 8881 §9.7: an OPEN whose access bits collide with a
    /// concurrent owner's deny mask (or vice versa) returns
    /// SHARE_DENIED. Same (client, owner) is exempt — they upgrade
    /// their own share-mask. Pynfs COUR3 covers this.
    #[test]
    fn test_share_conflict_detection() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let fh = b"/path/to/file".to_vec();

        // Owner A opens with deny=WRITE.
        mgr.record_open(1, b"alice".to_vec(), fh.clone(), 1, 2, None);

        // Different (client, owner) trying to access WRITE — conflicts.
        assert!(mgr.share_conflict(&fh, 2, b"bob", 2 /*WRITE*/, 0));
        // Different owner accessing READ only — no conflict (deny mask
        // only covers WRITE).
        assert!(!mgr.share_conflict(&fh, 2, b"bob", 1 /*READ*/, 0));
        // Same (client, owner) is always exempt.
        assert!(!mgr.share_conflict(&fh, 1, b"alice", 2 /*WRITE*/, 0));

        // Symmetric conflict: a new owner deny=READ vs existing
        // access=READ is also a conflict.
        assert!(mgr.share_conflict(&fh, 2, b"bob", 0, 1 /*deny READ*/));
    }

    /// RFC 8881 §18.16.5: EXCLUSIVE4 retry with a matching verifier
    /// finds the existing open (idempotent retry). Different
    /// verifier on existing file → caller maps to EXIST.
    #[test]
    fn test_find_exclusive_match() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let fh = b"/excl/file".to_vec();

        mgr.record_open(1, b"alice".to_vec(), fh.clone(), 3, 0, Some(0xdead_beef));

        // Same verifier → matches.
        assert!(mgr.find_exclusive_match(&fh, 0xdead_beef).is_some());
        // Different verifier → no match (caller returns EXIST).
        assert!(mgr.find_exclusive_match(&fh, 0xc0ffee).is_none());
        // No prior exclusive open on a different fh.
        assert!(mgr.find_exclusive_match(b"/other", 0xdead_beef).is_none());
    }

    #[test]
    fn test_stateid_revocation() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

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
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let stateid = mgr.allocate(StateType::Open, 1, None);
        assert_eq!(mgr.active_count(), 1);

        mgr.remove(&stateid);

        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.validate(&stateid).is_err());
    }

    #[test]
    fn test_client_stateids() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

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
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

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
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        mgr.allocate(StateType::Open, 1, None);
        mgr.allocate(StateType::Open, 2, None);
        mgr.allocate(StateType::Lock, 1, None);

        assert_eq!(mgr.count_by_type(StateType::Open), 2);
        assert_eq!(mgr.count_by_type(StateType::Lock), 1);
        assert_eq!(mgr.count_by_type(StateType::Delegation), 0);
    }

    #[test]
    fn test_special_stateids() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        // Anonymous stateid should validate
        assert!(mgr.validate(&ANONYMOUS_STATEID).is_ok());

        // Read bypass stateid should validate
        assert!(mgr.validate(&READ_BYPASS_STATEID).is_ok());
    }
}
