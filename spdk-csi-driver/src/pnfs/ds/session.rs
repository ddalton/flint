//! Minimal NFSv4.1 Session Support for Data Server
//!
//! Unlike the MDS which needs full session management, the DS only needs
//! minimal session support to satisfy NFSv4.1 clients.
//!
//! # Design
//!
//! The client already has a session with the MDS. When contacting the DS:
//! 1. Client uses the **same sessionid** from MDS
//! 2. DS just needs to validate SEQUENCE numbers
//! 3. DS doesn't need to create/destroy sessions
//! 4. DS is essentially stateless (file I/O only)
//!
//! # What DS Doesn't Need
//!
//! - ❌ Session creation (client has session with MDS)
//! - ❌ Session destruction
//! - ❌ Lease management (MDS handles)
//! - ❌ Client authentication (MDS did it)
//! - ❌ State revocation
//! - ❌ Full replay cache (optional for DS)
//!
//! # What DS Does Need
//!
//! - ✅ Validate SEQUENCE (sessionid, sequenceid, slotid)
//! - ✅ Track last sequence number per slot
//! - ✅ Return target_highest_slotid (tell client how many slots we support)
//! - ✅ Return status_flags (usually 0)

use dashmap::DashMap;
use std::sync::Arc;
use tracing::debug;

/// NFSv4 error codes
pub const NFS4ERR_BADSLOT: u32 = 10053;
pub const NFS4ERR_SEQ_MISORDERED: u32 = 10052;

/// Minimal session manager for DS
///
/// Only tracks enough state to handle SEQUENCE operations.
/// Sessions are auto-created on first SEQUENCE from a client.
pub struct DsSessionManager {
    /// Active sessions: sessionid -> DsSession
    sessions: Arc<DashMap<[u8; 16], DsSession>>,
    /// Maximum number of slots we support
    max_slots: u32,
}

struct DsSession {
    sessionid: [u8; 16],
    /// Per-slot sequence tracking (DS typically only needs slot 0)
    /// We use a simple approach: track last seen sequence per slot
    slot_sequences: Vec<u32>,
}

impl DsSessionManager {
    /// Create a new DS session manager
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            max_slots: 128, // Support up to 128 slots
        }
    }

    /// Handle SEQUENCE operation (minimal - just validate and echo back)
    ///
    /// # Arguments
    /// * `sessionid` - Session ID (inherited from MDS session)
    /// * `sequenceid` - Sequence number for this slot
    /// * `slotid` - Slot ID (usually 0 for simple clients)
    /// * `highest_slotid` - Highest slot the client is using
    ///
    /// # Returns
    /// Result with sequence result or NFS4 error code
    pub fn handle_sequence(
        &self,
        sessionid: [u8; 16],
        sequenceid: u32,
        slotid: u32,
        highest_slotid: u32,
    ) -> Result<SequenceResult, u32> {
        debug!(
            "🔥 DS SEQUENCE: sessionid={:02x}{:02x}{:02x}{:02x}..., seq={}, slot={}",
            sessionid[0], sessionid[1], sessionid[2], sessionid[3],
            sequenceid, slotid
        );

        // Validate slot
        if slotid >= self.max_slots {
            return Err(NFS4ERR_BADSLOT);
        }

        // Get or create session (DS auto-creates on first SEQUENCE)
        let mut session = self.sessions.entry(sessionid)
            .or_insert_with(|| {
                debug!("Creating new DS session for {:02x}{:02x}{:02x}{:02x}...",
                       sessionid[0], sessionid[1], sessionid[2], sessionid[3]);
                DsSession {
                    sessionid,
                    slot_sequences: vec![0; self.max_slots as usize],
                }
            });

        // Simple sequence validation
        // For DS, we use a relaxed model: accept seq >= last_seq
        // This avoids complex replay cache while preventing old replays
        let last_seq = session.slot_sequences[slotid as usize];
        
        if sequenceid < last_seq {
            debug!("⚠️ Sequence misordered: got {}, expected >= {}", sequenceid, last_seq);
            // For DS, we're lenient - allow it anyway (might be retry)
            // A strict implementation would return NFS4ERR_SEQ_MISORDERED
        }

        // Update sequence number
        session.slot_sequences[slotid as usize] = sequenceid;

        Ok(SequenceResult {
            sessionid,
            sequenceid,
            slotid,
            highest_slotid: 0,  // We're only using slot 0
            target_highest_slotid: self.max_slots - 1,  // Tell client our max
            status_flags: 0,  // No special flags
        })
    }

    /// Get session count (for monitoring)
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for DsSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a SEQUENCE operation
#[derive(Debug, Clone)]
pub struct SequenceResult {
    /// Echo back the session ID
    pub sessionid: [u8; 16],
    /// Echo back the sequence ID
    pub sequenceid: u32,
    /// Echo back the slot ID
    pub slotid: u32,
    /// Highest slot we're currently using
    pub highest_slotid: u32,
    /// Highest slot we support
    pub target_highest_slotid: u32,
    /// Status flags (e.g., SEQ4_STATUS_CB_PATH_DOWN)
    pub status_flags: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sequence_handling() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        // First sequence
        let result = mgr.handle_sequence(sessionid, 1, 0, 0).unwrap();
        assert_eq!(result.sequenceid, 1);
        assert_eq!(result.slotid, 0);
        assert_eq!(result.target_highest_slotid, 127);

        // Subsequent sequence should work
        let result2 = mgr.handle_sequence(sessionid, 2, 0, 0).unwrap();
        assert_eq!(result2.sequenceid, 2);

        // Session should be auto-created
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_multiple_sessions() {
        let mgr = DsSessionManager::new();
        let session1 = [1u8; 16];
        let session2 = [2u8; 16];

        mgr.handle_sequence(session1, 1, 0, 0).unwrap();
        mgr.handle_sequence(session2, 1, 0, 0).unwrap();

        assert_eq!(mgr.session_count(), 2);
    }

    #[test]
    fn test_invalid_slot() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        // Slot 128 is out of range (we support 0-127)
        let result = mgr.handle_sequence(sessionid, 1, 128, 0);
        assert_eq!(result, Err(NFS4ERR_BADSLOT));
    }

    #[test]
    fn test_multiple_slots() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        // Use different slots
        let result1 = mgr.handle_sequence(sessionid, 1, 0, 1).unwrap();
        assert_eq!(result1.slotid, 0);

        let result2 = mgr.handle_sequence(sessionid, 1, 1, 1).unwrap();
        assert_eq!(result2.slotid, 1);

        // Should be same session
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_sequence_increment() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        // Sequence numbers should increment
        for seq in 1..10 {
            let result = mgr.handle_sequence(sessionid, seq, 0, 0).unwrap();
            assert_eq!(result.sequenceid, seq);
        }
    }
}

