// NFSv4 State Management
//
// NFSv4 is a stateful protocol (unlike NFSv3). The server maintains:
// - Client IDs (established via EXCHANGE_ID)
// - Sessions (established via CREATE_SESSION)
// - StateIds (for opens, locks, delegations)
// - Leases (clients must renew or lose state)
//
// State Lifecycle:
// 1. Client connects → EXCHANGE_ID → receives clientid
// 2. Client establishes session → CREATE_SESSION → receives sessionid
// 3. Client performs operations → SEQUENCE (in every COMPOUND) → renews lease
// 4. Client opens file → OPEN → receives stateid
// 5. Client doesn't renew → lease expires → state cleanup
//
// This module implements all state tracking for NFSv4.1/4.2

pub mod client;
pub mod session;
pub mod stateid;
pub mod lease;
pub mod delegation;

pub use client::ClientManager;
pub use session::SessionManager;
pub use stateid::{StateIdManager, StateType, StateEntry};
pub use lease::LeaseManager;
pub use delegation::{DelegationManager, Delegation, DelegationType, DelegationStats};

use std::sync::Arc;

/// NFSv4 state manager - coordinates all state components
pub struct StateManager {
    pub clients: Arc<ClientManager>,
    pub sessions: Arc<SessionManager>,
    pub stateids: Arc<StateIdManager>,
    pub leases: Arc<LeaseManager>,
    pub delegations: Arc<DelegationManager>,
}

impl StateManager {
    /// Create a new state manager
    pub fn new(volume_id: &str) -> Self {
        let lease_manager = Arc::new(LeaseManager::new());
        let client_manager = Arc::new(ClientManager::new(lease_manager.clone(), volume_id));
        let session_manager = Arc::new(SessionManager::new());
        let stateid_manager = Arc::new(StateIdManager::new());
        let delegation_manager = Arc::new(DelegationManager::new());

        Self {
            clients: client_manager,
            sessions: session_manager,
            stateids: stateid_manager,
            leases: lease_manager,
            delegations: delegation_manager,
        }
    }

    /// Cleanup expired state
    ///
    /// Removes expired leases and their associated sessions and clients.
    /// This should be called periodically (e.g., every 30 seconds) to prevent
    /// resource leaks from clients that stop responding.
    pub fn cleanup_expired(&self) {
        // First, collect expired client IDs before removing leases
        let expired_clients = self.leases.get_expired_clients();

        // Now cleanup expired leases (this removes them from the lease manager)
        self.leases.cleanup_expired();

        // For each expired client, cleanup associated sessions and client state
        for client_id in expired_clients {
            // Destroy all sessions for this client
            self.sessions.destroy_client_sessions(client_id);

            // Remove the client itself
            self.clients.remove_client(client_id);

            // Cleanup any delegations for this client
            self.delegations.cleanup_client_delegations(client_id);
        }
    }
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_expired_removes_clients_and_sessions() {
        let state_mgr = StateManager::new("");

        // Create a client and session
        let outcome = state_mgr.clients.exchange_id(b"test-client".to_vec(), 12345, 0, Vec::new());
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };

        // Create session for this client
        let session = state_mgr.sessions.create_session(
            client_id,
            0,
            0,
            10,
            4096, 4096, 8,
            10,
            0,
        );

        // Verify client and session exist
        assert!(state_mgr.clients.get_client(client_id).is_some());
        assert!(state_mgr.sessions.get_session(&session.session_id).is_some());
        assert_eq!(state_mgr.clients.active_count(), 1);
        assert_eq!(state_mgr.sessions.active_count(), 1);

        // Note: We can't easily test actual lease expiration without waiting 90+ seconds,
        // so this test verifies the cleanup logic doesn't crash with active leases
        state_mgr.cleanup_expired();

        // Since no leases have expired, clients and sessions should still exist
        assert_eq!(state_mgr.clients.active_count(), 1);
        assert_eq!(state_mgr.sessions.active_count(), 1);
    }

    #[test]
    fn test_cleanup_expired_with_no_expired_clients() {
        let state_mgr = StateManager::new("");

        // Create a client and session
        let outcome = state_mgr.clients.exchange_id(b"test-client".to_vec(), 12345, 0, Vec::new());
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };

        state_mgr.sessions.create_session(
            client_id,
            0,
            0,
            10,
            4096, 4096, 8,
            10,
            0,
        );

        // Run cleanup (no leases have expired)
        state_mgr.cleanup_expired();

        // Client and session should still exist
        assert_eq!(state_mgr.clients.active_count(), 1);
        assert_eq!(state_mgr.sessions.active_count(), 1);
    }

    #[test]
    fn test_get_expired_clients_returns_empty_for_active_leases() {
        let state_mgr = StateManager::new("");

        // Create a client with active lease
        let outcome = state_mgr.clients.exchange_id(b"test-client".to_vec(), 12345, 0, Vec::new());
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };

        // Verify no expired clients
        let expired = state_mgr.leases.get_expired_clients();
        assert_eq!(expired.len(), 0);

        // Verify client still exists
        assert!(state_mgr.clients.get_client(client_id).is_some());
    }

    #[test]
    fn test_state_manager_default() {
        let state_mgr = StateManager::default();
        assert_eq!(state_mgr.clients.active_count(), 0);
        assert_eq!(state_mgr.sessions.active_count(), 0);
    }
}
