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
    pub fn new() -> Self {
        let lease_manager = Arc::new(LeaseManager::new());
        let client_manager = Arc::new(ClientManager::new(lease_manager.clone()));
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
    pub fn cleanup_expired(&self) {
        self.leases.cleanup_expired();
        // TODO: Remove expired clients and sessions
    }
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
}
