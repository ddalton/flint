//! Integration test for DS SEQUENCE operation support
//!
//! This test verifies that the Data Server correctly handles NFSv4.1
//! SEQUENCE operations, which are required for parallel I/O.

#[cfg(test)]
mod ds_sequence_tests {
    use spdk_csi_driver::pnfs::ds::session::DsSessionManager;

    #[test]
    fn test_ds_session_manager_creation() {
        let mgr = DsSessionManager::new();
        assert_eq!(mgr.session_count(), 0);
    }

    #[test]
    fn test_sequence_basic() {
        let mgr = DsSessionManager::new();
        let sessionid = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
                        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];

        // First SEQUENCE should create session
        let result = mgr.handle_sequence(sessionid, 1, 0, 0).unwrap();
        assert_eq!(result.sessionid, sessionid);
        assert_eq!(result.sequenceid, 1);
        assert_eq!(result.slotid, 0);
        assert_eq!(result.target_highest_slotid, 127);
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_sequence_increment() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        // Sequential SEQUENCE operations
        for seq in 1..=10 {
            let result = mgr.handle_sequence(sessionid, seq, 0, 0).unwrap();
            assert_eq!(result.sequenceid, seq);
            assert_eq!(result.slotid, 0);
        }

        // Should still be one session
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_multiple_clients() {
        let mgr = DsSessionManager::new();
        
        let session1 = [1u8; 16];
        let session2 = [2u8; 16];
        let session3 = [3u8; 16];

        // Each client has its own session
        mgr.handle_sequence(session1, 1, 0, 0).unwrap();
        mgr.handle_sequence(session2, 1, 0, 0).unwrap();
        mgr.handle_sequence(session3, 1, 0, 0).unwrap();

        assert_eq!(mgr.session_count(), 3);

        // Each session tracks its own sequence
        let r1 = mgr.handle_sequence(session1, 5, 0, 0).unwrap();
        let r2 = mgr.handle_sequence(session2, 3, 0, 0).unwrap();
        
        assert_eq!(r1.sequenceid, 5);
        assert_eq!(r2.sequenceid, 3);
    }

    #[test]
    fn test_multiple_slots() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        // Use different slots
        let r0 = mgr.handle_sequence(sessionid, 1, 0, 1).unwrap();
        let r1 = mgr.handle_sequence(sessionid, 1, 1, 1).unwrap();
        let r2 = mgr.handle_sequence(sessionid, 1, 2, 2).unwrap();

        assert_eq!(r0.slotid, 0);
        assert_eq!(r1.slotid, 1);
        assert_eq!(r2.slotid, 2);

        // All in same session
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_invalid_slot() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        // Slot 128 is out of range (max is 127)
        let result = mgr.handle_sequence(sessionid, 1, 128, 0);
        assert!(result.is_err());
        
        // Error code should be NFS4ERR_BADSLOT (10053)
        assert_eq!(result.unwrap_err(), 10053);
    }

    #[test]
    fn test_status_flags() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        let result = mgr.handle_sequence(sessionid, 1, 0, 0).unwrap();
        
        // Status flags should be 0 (no special conditions)
        assert_eq!(result.status_flags, 0);
    }

    #[test]
    fn test_highest_slotid() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];

        let result = mgr.handle_sequence(sessionid, 1, 0, 0).unwrap();
        
        // highest_slotid should be 0 (we're only using slot 0)
        assert_eq!(result.highest_slotid, 0);
        
        // target_highest_slotid should be 127 (our max)
        assert_eq!(result.target_highest_slotid, 127);
    }

    #[test]
    fn test_session_persistence() {
        let mgr = DsSessionManager::new();
        let sessionid = [0xaa; 16];

        // Create session with sequence 1
        mgr.handle_sequence(sessionid, 1, 0, 0).unwrap();

        // Later sequence should work
        mgr.handle_sequence(sessionid, 100, 0, 0).unwrap();

        // Session should still exist
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_concurrent_sessions() {
        use std::sync::Arc;
        use std::thread;

        let mgr: Arc<DsSessionManager> = Arc::new(DsSessionManager::new());
        let mut handles = vec![];

        // Spawn multiple threads simulating concurrent clients
        for i in 0..10 {
            let mgr_clone: Arc<DsSessionManager> = Arc::clone(&mgr);
            let handle = thread::spawn(move || {
                let mut sessionid = [0u8; 16];
                sessionid[0] = i as u8;

                for seq in 1..=5 {
                    let result = mgr_clone.handle_sequence(sessionid, seq, 0, 0).unwrap();
                    assert_eq!(result.sequenceid, seq);
                }
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // Should have 10 sessions
        assert_eq!(mgr.session_count(), 10);
    }
}

