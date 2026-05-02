// Session Management
//
// NFSv4.1+ uses sessions for exactly-once semantics and better recovery.
// Each session has:
// - Session ID (128-bit identifier)
// - Fore channel (client → server)
// - Back channel (server → client, for callbacks)
// - Slots (for request replay detection)
//
// Session Lifecycle:
// 1. Client performs EXCHANGE_ID → gets clientid
// 2. Client performs CREATE_SESSION → gets sessionid
// 3. Every COMPOUND starts with SEQUENCE (uses slot)
// 4. Server tracks slot state for exactly-once semantics

use super::super::protocol::SessionId;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info, warn};

/// Maximum slots per session (conservative default)
pub const MAX_SLOTS: u32 = 128;

/// Outcome of processing a SEQUENCE op against a session slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeqStatus {
    /// First time we've seen this `sequence_id` on this slot. The compound
    /// runs normally and the reply is cached against the slot when dispatch
    /// finishes.
    New,
    /// Exact resend (`sequence_id == slot.sequence_id`). The cached reply
    /// (if any) is in `cached`. RFC 8881 §15.1.10.4: server MUST return the
    /// cached reply byte-for-byte; if no reply is cached (the client
    /// retransmitted before the original returned), respond with
    /// `NFS4ERR_RETRY_UNCACHED_REP`.
    Replay { cached: Option<Vec<u8>> },
    /// `sequence_id` is anywhere other than `slot.sequence_id` or
    /// `slot.sequence_id + 1` — RFC 8881 §15.1.10.4 mandates
    /// `NFS4ERR_SEQ_MISORDERED`. We do *not* "resync"; that would defeat
    /// exactly-once semantics by silently accepting any seqid jump.
    Misordered,
}

/// Slot state (for exactly-once semantics)
#[derive(Debug, Clone)]
pub struct Slot {
    /// Slot ID
    pub slot_id: u32,

    /// Last sequence ID processed in this slot
    pub sequence_id: u32,

    /// Cached response (for replay detection)
    pub cached_response: Option<Vec<u8>>,
}

impl Slot {
    fn new(slot_id: u32) -> Self {
        Self {
            slot_id,
            sequence_id: 0,
            cached_response: None,
        }
    }
}

/// NFSv4.1 Session
#[derive(Debug, Clone)]
pub struct Session {
    /// Session ID
    pub session_id: SessionId,

    /// Client ID that owns this session
    pub client_id: u64,

    /// Sequence number (for CREATE_SESSION)
    pub sequence: u32,

    /// Session flags
    pub flags: u32,

    /// Fore channel attributes
    pub fore_chan_maxrequestsize: u32,
    pub fore_chan_maxresponsesize: u32,
    /// Maximum size of a reply that the server is allowed to cache for
    /// reply-cache replays on this session (negotiated via
    /// ca_maxresponsesize_cached in CREATE_SESSION). Used by SEQUENCE to
    /// emit `NFS4ERR_REP_TOO_BIG_TO_CACHE` when `cachethis` is set and the
    /// expected reply would exceed this.
    pub fore_chan_maxresponsesize_cached: u32,
    pub fore_chan_maxops: u32,

    /// Slots for exactly-once semantics
    pub slots: Vec<Slot>,

    /// Highest slot ID in use
    pub highest_slotid: u32,
}

impl Session {
    /// Create a new session
    pub fn new(
        session_id: SessionId,
        client_id: u64,
        sequence: u32,
        flags: u32,
        max_requests: u32,
        max_response: u32,
        max_response_cached: u32,
        max_ops: u32,
    ) -> Self {
        // Initialize slots
        let slot_count = MAX_SLOTS.min(128);
        let mut slots = Vec::with_capacity(slot_count as usize);
        for i in 0..slot_count {
            slots.push(Slot::new(i));
        }

        Self {
            session_id,
            client_id,
            sequence,
            flags,
            fore_chan_maxrequestsize: max_requests,
            fore_chan_maxresponsesize: max_response,
            fore_chan_maxresponsesize_cached: max_response_cached,
            fore_chan_maxops: max_ops,
            slots,
            highest_slotid: 0,
        }
    }

    /// Process a SEQUENCE operation in a slot.
    pub fn process_sequence(&mut self, slot_id: u32, sequence_id: u32) -> Result<SeqStatus, String> {
        if slot_id >= self.slots.len() as u32 {
            return Err("Slot ID out of range".to_string());
        }

        let slot = &mut self.slots[slot_id as usize];

        debug!(
            "🔍 SEQUENCE processing: slot={}, client_seq={}, slot_seq={}, expecting={}",
            slot_id, sequence_id, slot.sequence_id, slot.sequence_id + 1
        );

        // First request after CREATE_SESSION: spec says first slot use must
        // be seqid=1 (slot starts at 0). Treat this as a new request.
        if slot.sequence_id == 0 && sequence_id == 1 {
            debug!("✅ SEQUENCE first request: slot={}, seq=1", slot_id);
            slot.sequence_id = 1;
            slot.cached_response = None;
            self.highest_slotid = self.highest_slotid.max(slot_id);
            return Ok(SeqStatus::New);
        }

        if sequence_id == slot.sequence_id {
            debug!("✅ SEQUENCE replay: slot={}, seq={}, cached={}",
                   slot_id, sequence_id, slot.cached_response.is_some());
            Ok(SeqStatus::Replay { cached: slot.cached_response.clone() })
        } else if sequence_id == slot.sequence_id.wrapping_add(1) {
            debug!("✅ SEQUENCE new request: slot={}, seq={} (was {})",
                   slot_id, sequence_id, slot.sequence_id);
            slot.sequence_id = sequence_id;
            // Clear last reply now; new reply will be cached when dispatch
            // completes. Leaving it set would let us return a *different*
            // operation's reply on a transient bug — tighter to drop early.
            slot.cached_response = None;
            self.highest_slotid = self.highest_slotid.max(slot_id);
            Ok(SeqStatus::New)
        } else {
            warn!("❌ SEQUENCE misordered: slot={}, expected {} or {}, got {}",
                  slot_id, slot.sequence_id, slot.sequence_id.wrapping_add(1), sequence_id);
            Ok(SeqStatus::Misordered)
        }
    }

    /// Cache a response in a slot (for replay detection)
    pub fn cache_response(&mut self, slot_id: u32, response: Vec<u8>) {
        if (slot_id as usize) < self.slots.len() {
            self.slots[slot_id as usize].cached_response = Some(response);
        }
    }

    /// Get cached response for a slot (for replay)
    pub fn get_cached_response(&self, slot_id: u32) -> Option<&Vec<u8>> {
        self.slots.get(slot_id as usize)?.cached_response.as_ref()
    }
}

/// Session manager - tracks all active sessions
/// LOCK-FREE DESIGN using DashMap:
/// - Concurrent session lookups without blocking
/// - Lock-free SEQUENCE operations (exactly-once semantics)
/// - Per-session granularity for high concurrency
pub struct SessionManager {
    /// Counter for generating session IDs (lock-free atomic)
    next_session_id: AtomicU64,

    /// Active sessions (session_id → session)
    /// DashMap enables lock-free concurrent access
    sessions: DashMap<SessionId, Session>,

    /// Client to session mapping (client_id → session_ids)
    /// Lock-free per-client session tracking
    client_sessions: DashMap<u64, Vec<SessionId>>,
}

impl SessionManager {
    /// Create a new session manager
    pub fn new() -> Self {
        info!("SessionManager created");

        Self {
            next_session_id: AtomicU64::new(1),
            sessions: DashMap::new(),
            client_sessions: DashMap::new(),
        }
    }

    /// Create a new session
    ///
    /// LOCK-FREE: Concurrent CREATE_SESSION operations use DashMap's per-shard locking
    pub fn create_session(
        &self,
        client_id: u64,
        sequence: u32,
        flags: u32,
        max_requests: u32,
        max_response: u32,
        max_response_cached: u32,
        max_ops: u32,
    ) -> Session {
        // Generate session ID (lock-free atomic increment)
        let session_id_num = self.next_session_id.fetch_add(1, Ordering::SeqCst);
        let mut session_id_bytes = [0u8; 16];
        session_id_bytes[0..8].copy_from_slice(&session_id_num.to_be_bytes());
        session_id_bytes[8..16].copy_from_slice(&client_id.to_be_bytes());
        let session_id = SessionId(session_id_bytes);

        let session = Session::new(
            session_id,
            client_id,
            sequence,
            flags,
            max_requests,
            max_response,
            max_response_cached,
            max_ops,
        );

        // LOCK-FREE: Direct DashMap inserts without global locks
        // Only locks the specific shard, not entire map
        self.sessions.insert(session_id, session.clone());
        self.client_sessions
            .entry(client_id)
            .or_insert_with(Vec::new)
            .push(session_id);

        info!("Session created: client={}, session_id={:?}", client_id, session_id);
        session
    }

    /// Get a session by ID
    ///
    /// LOCK-FREE: Concurrent reads without blocking other operations
    pub fn get_session(&self, session_id: &SessionId) -> Option<Session> {
        self.sessions.get(session_id).map(|r| r.clone())
    }

    /// Get a mutable session (for updating slot state)
    ///
    /// LOCK-FREE: Per-session locking only, not global
    /// Multiple sessions can be updated concurrently
    pub fn get_session_mut<F, R>(&self, session_id: &SessionId, f: F) -> Option<R>
    where
        F: FnOnce(&mut Session) -> R,
    {
        self.sessions.get_mut(session_id).map(|mut r| f(&mut r))
    }

    /// Destroy a session
    ///
    /// LOCK-FREE: Removal only locks specific shards, not entire map
    pub fn destroy_session(&self, session_id: &SessionId) -> Result<(), String> {
        if let Some((_, session)) = self.sessions.remove(session_id) {
            // Remove from client mapping (per-client locking only)
            if let Some(mut session_ids) = self.client_sessions.get_mut(&session.client_id) {
                session_ids.retain(|id| id != session_id);
            }

            info!("Session destroyed: {:?}", session_id);
            Ok(())
        } else {
            Err("Session not found".to_string())
        }
    }

    /// Get all sessions for a client
    ///
    /// LOCK-FREE: Concurrent reads without blocking
    pub fn get_client_sessions(&self, client_id: u64) -> Vec<SessionId> {
        self.client_sessions
            .get(&client_id)
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// Destroy all sessions for a client
    ///
    /// LOCK-FREE: Uses lock-free get and destroy operations
    pub fn destroy_client_sessions(&self, client_id: u64) {
        let session_ids = self.get_client_sessions(client_id);
        for session_id in session_ids {
            let _ = self.destroy_session(&session_id);
        }
    }

    /// Get active session count
    ///
    /// LOCK-FREE: Counts without blocking concurrent operations
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let mgr = SessionManager::new();
        let session = mgr.create_session(1, 0, 0, 1024, 1024, 1024, 16);

        assert_eq!(session.client_id, 1);
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn test_sequence_processing() {
        let mgr = SessionManager::new();
        let session = mgr.create_session(1, 0, 0, 1024, 1024, 1024, 16);

        // First request on slot 0
        let result = mgr.get_session_mut(&session.session_id, |s| {
            s.process_sequence(0, 1)
        });
        assert_eq!(result, Some(Ok(SeqStatus::New)));

        // Cache a fake reply, then replay → cached bytes returned.
        mgr.get_session_mut(&session.session_id, |s| s.cache_response(0, vec![0xAA]));
        let result = mgr.get_session_mut(&session.session_id, |s| {
            s.process_sequence(0, 1)
        });
        assert_eq!(result, Some(Ok(SeqStatus::Replay { cached: Some(vec![0xAA]) })));

        // Next sequence id → new request, cached reply cleared.
        let result = mgr.get_session_mut(&session.session_id, |s| {
            s.process_sequence(0, 2)
        });
        assert_eq!(result, Some(Ok(SeqStatus::New)));

        // Non-monotonic jump → misordered, no resync.
        let result = mgr.get_session_mut(&session.session_id, |s| {
            s.process_sequence(0, 99)
        });
        assert_eq!(result, Some(Ok(SeqStatus::Misordered)));
    }

    #[test]
    fn test_session_destruction() {
        let mgr = SessionManager::new();
        let session = mgr.create_session(1, 0, 0, 1024, 1024, 1024, 16);
        let session_id = session.session_id;

        assert_eq!(mgr.active_count(), 1);

        mgr.destroy_session(&session_id).unwrap();

        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.get_session(&session_id).is_none());
    }

    #[test]
    fn test_client_sessions() {
        let mgr = SessionManager::new();
        let _session1 = mgr.create_session(1, 0, 0, 1024, 1024, 1024, 16);
        let _session2 = mgr.create_session(1, 1, 0, 1024, 1024, 1024, 16);
        let _session3 = mgr.create_session(2, 0, 0, 1024, 1024, 1024, 16);

        let client1_sessions = mgr.get_client_sessions(1);
        assert_eq!(client1_sessions.len(), 2);

        let client2_sessions = mgr.get_client_sessions(2);
        assert_eq!(client2_sessions.len(), 1);

        // Destroy all client 1 sessions
        mgr.destroy_client_sessions(1);
        assert_eq!(mgr.active_count(), 1);
    }
}
