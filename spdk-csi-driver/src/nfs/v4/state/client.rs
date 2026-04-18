// Client Management
//
// Tracks NFSv4 clients. Each client is identified by a clientid (u64).
// Clients are established via EXCHANGE_ID operation.
//
// Client Lifecycle:
// 1. Client sends EXCHANGE_ID → server assigns clientid
// 2. Client creates session → CREATE_SESSION
// 3. Client performs operations → maintains lease
// 4. Client idle → lease expires → cleanup
//
// We use a simple counter for client IDs (incrementing u64)

use super::lease::LeaseManager;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info};

/// Client information
#[derive(Debug, Clone)]
pub struct Client {
    /// Client ID (assigned by server)
    pub client_id: u64,

    /// Client owner (from EXCHANGE_ID)
    pub owner: Vec<u8>,

    /// Client verifier (for detecting reboots)
    pub verifier: u64,

    /// Server owner (our identifier)
    pub server_owner: String,

    /// Server scope (our scope identifier)
    pub server_scope: Vec<u8>,

    /// Sequence ID (for CREATE_SESSION)
    pub sequence_id: u32,

    /// Flags from EXCHANGE_ID
    pub flags: u32,
}

impl Client {
    /// Create a new client
    pub fn new(
        client_id: u64,
        owner: Vec<u8>,
        verifier: u64,
        server_owner: String,
        server_scope: Vec<u8>,
        flags: u32,
    ) -> Self {
        Self {
            client_id,
            owner,
            verifier,
            server_owner,
            server_scope,
            sequence_id: 0,
            flags,
        }
    }
}

/// Client manager - tracks all connected clients
///
/// LOCK-FREE DESIGN using DashMap:
/// - Concurrent client lookups without blocking
/// - Lock-free client registration (EXCHANGE_ID)
/// - Per-client granularity, no global contention
pub struct ClientManager {
    /// Next client ID to assign (lock-free atomic)
    next_client_id: AtomicU64,

    /// Active clients (client_id → client)
    /// DashMap enables lock-free concurrent access
    clients: DashMap<u64, Client>,

    /// Client owner to client ID mapping (for reboot detection)
    /// Lock-free lookups for reconnecting clients
    owner_to_id: DashMap<Vec<u8>, u64>,

    /// Lease manager (for creating leases)
    lease_manager: Arc<LeaseManager>,

    /// Server owner (our identifier)
    server_owner: String,

    /// Server scope (our scope identifier)
    server_scope: Vec<u8>,
}

impl ClientManager {
    /// Create a new client manager
    /// `volume_id` ensures each NFS server instance has a unique NFSv4 server_owner,
    /// preventing the Linux kernel from treating separate NFS pods as trunked paths
    /// to the same server (which causes cross-volume data corruption).
    pub fn new(lease_manager: Arc<LeaseManager>, volume_id: &str) -> Self {
        // Determine server mode: standalone NFS vs pNFS (MDS/DS)
        // PNFS_MODE can be: "standalone", "mds", "ds"
        // If not set, assume standalone mode (safer default for flint-nfs-server)
        let pnfs_mode = std::env::var("PNFS_MODE").ok();
        let is_pnfs = pnfs_mode.as_deref() == Some("mds") || pnfs_mode.as_deref() == Some("ds");

        // Server identifiers: different for pNFS vs standalone
        // IMPORTANT: Each standalone NFS server MUST have a unique server_owner
        // to prevent NFSv4 trunking detection from merging connections.
        let server_owner = if is_pnfs {
            "flint-pnfs".to_string()
        } else if volume_id.is_empty() {
            "flint-nfs".to_string()
        } else {
            format!("flint-nfs-{}", volume_id)
        };

        // Read server_scope from environment (allows MDS vs DS differentiation)
        let server_scope = if is_pnfs {
            std::env::var("PNFS_SERVER_SCOPE")
                .unwrap_or_else(|_| "flint-pnfs-mds".to_string())
        } else if volume_id.is_empty() {
            "flint-nfs-standalone".to_string()
        } else {
            format!("flint-nfs-{}", volume_id)
        }.into_bytes();

        info!("ClientManager created - mode={:?}, server_owner={}, server_scope={}", 
              pnfs_mode.as_deref().unwrap_or("standalone"),
              server_owner, String::from_utf8_lossy(&server_scope));

        Self {
            next_client_id: AtomicU64::new(1),
            clients: DashMap::new(),
            owner_to_id: DashMap::new(),
            lease_manager,
            server_owner,
            server_scope,
        }
    }

    /// Exchange client ID (EXCHANGE_ID operation)
    /// Returns (client_id, sequence_id, is_new_client)
    ///
    /// LOCK-FREE: Concurrent EXCHANGE_ID operations don't block each other
    pub fn exchange_id(
        &self,
        owner: Vec<u8>,
        verifier: u64,
        flags: u32,
    ) -> (u64, u32, bool) {
        // Check if client already exists (lock-free read)
        if let Some(&existing_id) = self.owner_to_id.get(&owner).as_deref() {
            if let Some(client) = self.clients.get(&existing_id) {
                // Check verifier to detect reboot
                if client.verifier == verifier {
                    // Same client, same verifier - return existing ID
                    debug!("EXCHANGE_ID: existing client {} (same verifier)", existing_id);
                    return (existing_id, client.sequence_id, false);
                } else {
                    // Same owner, different verifier - client rebooted
                    info!("EXCHANGE_ID: client rebooted (owner match, verifier mismatch)");
                    // Fall through to create new client
                }
            }
        }

        // Assign new client ID (lock-free atomic)
        let client_id = self.next_client_id.fetch_add(1, Ordering::SeqCst);

        let client = Client::new(
            client_id,
            owner.clone(),
            verifier,
            self.server_owner.clone(),
            self.server_scope.clone(),
            flags,
        );

        // Store client (lock-free inserts)
        self.clients.insert(client_id, client);
        self.owner_to_id.insert(owner, client_id);

        // Create lease
        self.lease_manager.create_lease(client_id);

        info!("EXCHANGE_ID: new client {} created", client_id);
        (client_id, 0, true)
    }

    /// Get client by ID
    ///
    /// LOCK-FREE: Concurrent reads don't block
    pub fn get_client(&self, client_id: u64) -> Option<Client> {
        self.clients.get(&client_id).map(|r| r.clone())
    }

    /// Update client sequence ID
    ///
    /// LOCK-FREE: Per-client locking, not global
    pub fn update_sequence(&self, client_id: u64) -> Result<u32, String> {
        if let Some(mut client) = self.clients.get_mut(&client_id) {
            client.sequence_id += 1;
            Ok(client.sequence_id)
        } else {
            Err("Client not found".to_string())
        }
    }

    /// Remove a client
    ///
    /// LOCK-FREE: Removal doesn't block other operations
    pub fn remove_client(&self, client_id: u64) {
        if let Some((_, client)) = self.clients.remove(&client_id) {
            // Remove from owner map
            self.owner_to_id.remove(&client.owner);

            // Remove lease
            self.lease_manager.remove_lease(client_id);

            info!("Client {} removed", client_id);
        }
    }

    /// Get active client count
    ///
    /// LOCK-FREE: Count without blocking concurrent operations
    pub fn active_count(&self) -> usize {
        self.clients.len()
    }

    /// Get server owner
    pub fn server_owner(&self) -> &str {
        &self.server_owner
    }

    /// Get server scope
    pub fn server_scope(&self) -> &[u8] {
        &self.server_scope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exchange_id_new_client() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let owner = b"client1".to_vec();
        let verifier = 12345;

        let (client_id, seq_id, is_new) = client_mgr.exchange_id(owner, verifier, 0);

        assert_eq!(client_id, 1);
        assert_eq!(seq_id, 0);
        assert!(is_new);
        assert_eq!(client_mgr.active_count(), 1);
    }

    #[test]
    fn test_exchange_id_existing_client() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let owner = b"client1".to_vec();
        let verifier = 12345;

        // First exchange
        let (id1, _, _) = client_mgr.exchange_id(owner.clone(), verifier, 0);

        // Second exchange with same owner and verifier
        let (id2, _, is_new) = client_mgr.exchange_id(owner, verifier, 0);

        assert_eq!(id1, id2);
        assert!(!is_new);
        assert_eq!(client_mgr.active_count(), 1);
    }

    #[test]
    fn test_client_reboot() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let owner = b"client1".to_vec();
        let verifier1 = 12345;
        let verifier2 = 67890;

        // First exchange
        let (id1, _, _) = client_mgr.exchange_id(owner.clone(), verifier1, 0);

        // Second exchange with different verifier (reboot)
        let (id2, _, is_new) = client_mgr.exchange_id(owner, verifier2, 0);

        assert_ne!(id1, id2);
        assert!(is_new);
    }

    #[test]
    fn test_sequence_update() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let (client_id, _, _) = client_mgr.exchange_id(b"client1".to_vec(), 12345, 0);

        let seq1 = client_mgr.update_sequence(client_id).unwrap();
        let seq2 = client_mgr.update_sequence(client_id).unwrap();

        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2);
    }

    #[test]
    fn test_client_removal() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let (client_id, _, _) = client_mgr.exchange_id(b"client1".to_vec(), 12345, 0);

        assert_eq!(client_mgr.active_count(), 1);

        client_mgr.remove_client(client_id);

        assert_eq!(client_mgr.active_count(), 0);
        assert!(client_mgr.get_client(client_id).is_none());
    }
}
