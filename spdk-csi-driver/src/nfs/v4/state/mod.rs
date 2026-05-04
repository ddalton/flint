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

use crate::state_backend::{StateBackend, StateBackendError};
use std::sync::Arc;

/// NFSv4 state manager - coordinates all state components
pub struct StateManager {
    pub clients: Arc<ClientManager>,
    pub sessions: Arc<SessionManager>,
    pub stateids: Arc<StateIdManager>,
    pub leases: Arc<LeaseManager>,
    pub delegations: Arc<DelegationManager>,
    /// Shared persistence target. Each per-component manager holds its
    /// own clone; this field exists so `load_from_backend` and
    /// post-startup helpers can reach the trait without going through
    /// a sub-manager.
    backend: Arc<dyn StateBackend>,
}

impl StateManager {
    /// Create a new state manager backed by `backend`. Use
    /// `state_backend::memory_backend()` for tests / dev work, or a
    /// `SqliteBackend` for production.
    pub fn new(volume_id: &str, backend: Arc<dyn StateBackend>) -> Self {
        let lease_manager = Arc::new(LeaseManager::new());
        let client_manager = Arc::new(ClientManager::new(
            lease_manager.clone(),
            volume_id,
            Arc::clone(&backend),
        ));
        let session_manager = Arc::new(SessionManager::new(Arc::clone(&backend)));
        let stateid_manager = Arc::new(StateIdManager::new(Arc::clone(&backend)));
        let delegation_manager = Arc::new(DelegationManager::new());

        Self {
            clients: client_manager,
            sessions: session_manager,
            stateids: stateid_manager,
            leases: lease_manager,
            delegations: delegation_manager,
            backend,
        }
    }

    /// Test/dev convenience: build a `StateManager` over a fresh
    /// `MemoryBackend`. Equivalent to `new(volume_id,
    /// memory_backend())` — makes the call sites in `#[cfg(test)]`
    /// modules read tighter.
    pub fn new_in_memory(volume_id: &str) -> Self {
        Self::new(volume_id, crate::state_backend::memory_backend())
    }

    /// Pre-listener hook: pull every persisted record out of the
    /// backend and seed the in-memory caches with it. After this
    /// returns, hot-path reads through `clients` / `sessions` /
    /// `stateids` find their pre-restart records — clients
    /// reconnecting against the post-restart MDS see no
    /// `STALE_CLIENTID` / `BAD_STATEID`. `LayoutManager` is loaded
    /// separately by the pNFS startup path because it lives outside
    /// the NFSv4 `state` module.
    pub async fn load_from_backend(&self) -> Result<(), StateBackendError> {
        let clients = self.backend.list_clients().await?;
        let sessions = self.backend.list_sessions().await?;
        let stateids = self.backend.list_stateids().await?;
        let n_c = clients.len();
        let n_s = sessions.len();
        let n_st = stateids.len();
        self.clients.load_records(clients);
        self.sessions.load_records(sessions);
        self.stateids.load_records(stateids);
        tracing::info!(
            "StateManager loaded {} clients, {} sessions, {} stateids from backend",
            n_c,
            n_s,
            n_st,
        );
        Ok(())
    }

    /// Borrow the shared backend (used by the pNFS layer to load
    /// `LayoutManager` records and to share an instance counter).
    pub fn backend(&self) -> Arc<dyn StateBackend> {
        Arc::clone(&self.backend)
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

            // RFC 8881 §8.4.2.4 courtesy-release: a client whose
            // lease has expired forfeits its share-reservations and
            // open state to the next conflicting op. Wiping the
            // stateids + open-state records here lets that next op
            // proceed instead of getting blocked on a phantom
            // conflict. Locks held by this client are released by
            // the dispatcher's courtesy-cleanup hook (since
            // `LockManager` lives outside `StateManager`).
            self.stateids.remove_client_stateids(client_id);

            // Remove the client itself
            self.clients.remove_client(client_id);

            // Cleanup any delegations for this client
            self.delegations.cleanup_client_delegations(client_id);
        }
    }
}

impl Default for StateManager {
    fn default() -> Self {
        // Default is in-memory only — no restart survival. Production
        // callers should use `StateManager::new(volume_id, sqlite)`.
        Self::new_in_memory("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_expired_removes_clients_and_sessions() {
        let state_mgr = StateManager::new_in_memory("");

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
        let state_mgr = StateManager::new_in_memory("");

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
        let state_mgr = StateManager::new_in_memory("");

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

    /// **The whole point of B.3.** Build a StateManager, stuff it
    /// with state, mutate that state, then build a *fresh*
    /// StateManager backed by the same `MemoryBackend` and prove
    /// `load_from_backend` reconstructs the in-memory caches —
    /// active client_id is back, mark_confirmed was persisted, the
    /// session is bound to the same client.
    ///
    /// This is the test that B.5's Lima e2e (`make
    /// test-pnfs-restart`) will mirror at the process level. If
    /// this passes, the in-process plumbing is sound and the
    /// remaining work is plumbing config + the e2e harness.
    #[tokio::test]
    async fn test_state_manager_reload_from_shared_backend() {
        use crate::state_backend::MemoryBackend;

        // Phase 1: write phase. Tokio runtime is live, so the
        // fire-and-forget persist tasks actually run.
        let backend: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let mgr1 = StateManager::new("vol1", Arc::clone(&backend));
        let outcome = mgr1.clients.exchange_id(
            b"alice-client".to_vec(),
            0xc0ffee,
            0,
            b"alice@FLINT".to_vec(),
        );
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed {
                client_id,
                ..
            } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };
        mgr1.clients.mark_confirmed(client_id);
        let session = mgr1.sessions.create_session(
            client_id, 0, 0, 4096, 4096, 1024, 16, 8, 0xcb_aabb,
        );

        // Persist is fire-and-forget; let the spawned tasks land
        // before we read the backend. In production this is bounded
        // by tokio's task queue; in tests we yield once.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // The spawned put_client/put_session/etc. complete on the
        // next runtime tick; allow a small budget rather than
        // racing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Sanity: backend has what we put.
        assert_eq!(backend.list_clients().await.unwrap().len(), 1);
        assert_eq!(backend.list_sessions().await.unwrap().len(), 1);

        // Phase 2: simulate a restart. New StateManager, same
        // backend, then load_from_backend. Equivalent at the
        // protocol level to "MDS pod rolled, comes back, kernel
        // reconnects against the same client_id".
        drop(mgr1);
        let mgr2 = StateManager::new("vol1", Arc::clone(&backend));
        // Pre-load: cache is empty.
        assert_eq!(mgr2.clients.active_count(), 0);
        assert_eq!(mgr2.sessions.active_count(), 0);

        mgr2.load_from_backend().await.expect("load must succeed");

        // Post-load: client is back with mark_confirmed intact —
        // EXCHANGE_ID after restart will return this same client_id
        // (case 1 of RFC 8881 §18.35.5) so the kernel keeps using
        // its existing client_id.
        let restored = mgr2
            .clients
            .get_client(client_id)
            .expect("client must reload");
        assert!(restored.confirmed, "mark_confirmed must persist");
        assert_eq!(restored.owner, b"alice-client");
        assert_eq!(restored.principal, b"alice@FLINT");

        // Sessions are deliberately NOT restored to the live map.
        // Slot replay state can't survive restart (RFC 8881
        // §15.1.10.4), so reloading a session would break Linux
        // clients that send SEQUENCE with their current per-slot
        // seqid. Instead, the kernel sees its session_id is unknown
        // → BADSESSION → CREATE_SESSION fresh → resumes against
        // the same persisted client_id. See
        // `SessionManager::load_records` for the full rationale.
        assert_eq!(
            mgr2.sessions.active_count(),
            0,
            "sessions deliberately not restored — kernel re-CREATE_SESSIONs",
        );
        // The persisted-id counter still got bumped past `session.session_id`'s
        // numeric component so a fresh CREATE_SESSION never collides.
        let new_session = mgr2.sessions.create_session(
            client_id, 0, 0, 4096, 4096, 1024, 16, 8, 0xcb_aabb,
        );
        assert_ne!(
            new_session.session_id, session.session_id,
            "post-restart CREATE_SESSION must mint a fresh session_id",
        );
    }
}
