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
use super::super::protocol::SessionId;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info, warn};

/// Persisted bits of a CREATE_SESSION response, kept on the Client so a
/// replay can return byte-identical fields. Held inline (not a reference
/// to an op-level type) to avoid a state→operations module dependency.
#[derive(Debug, Clone)]
pub struct CachedCreateSessionRes {
    pub sessionid: SessionId,
    pub sequence: u32,
    pub flags: u32,
    pub fore_max_request_size: u32,
    pub fore_max_response_size: u32,
    pub fore_max_response_size_cached: u32,
    pub fore_max_operations: u32,
    pub fore_max_requests: u32,
}

/// EXCHANGE_ID UPD_CONFIRMED_REC_A flag (RFC 8881 §18.35.3).
const EXCHGID4_FLAG_UPD_CONFIRMED_REC_A: u32 = 0x40000000;

/// Extract the clientid from an outcome variant — only meaningful for
/// the `NewUnconfirmed` / `ExistingConfirmed` arms; returns 0 for errors.
fn outcome_id(o: &ExchangeIdOutcome) -> u64 {
    match o {
        ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => *client_id,
        ExchangeIdOutcome::ExistingConfirmed { client_id, .. } => *client_id,
        _ => 0,
    }
}

/// Outcome of `ClientManager::exchange_id`. Maps directly onto the
/// "Server Action" rows of RFC 8881 §18.35.5 Table 2.
#[derive(Debug)]
pub enum ExchangeIdOutcome {
    /// A new clientid was created (or an existing record replaced). Caller
    /// MUST CREATE_SESSION before any state-using operation.
    NewUnconfirmed { client_id: u64, sequence_id: u32 },
    /// Existing confirmed record returned unchanged (renewal of an idempotent
    /// EXCHANGE_ID). Caller may continue using existing sessions.
    ExistingConfirmed { client_id: u64, sequence_id: u32 },
    /// UPD_CONFIRMED_REC_A was set but no confirmed record exists for this
    /// owner — RFC 8881 §18.35.5 Case 7.
    NoEnt,
    /// UPD_CONFIRMED_REC_A on a confirmed record where the verifier differs.
    /// RFC 8881 §18.35.5 Case 8.
    NotSame,
    /// UPD_CONFIRMED_REC_A on a confirmed record where the principal differs.
    /// RFC 8881 §18.35.5 Case 9.
    Perm,
}

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

    /// RPC-level principal that performed the EXCHANGE_ID. Used by
    /// RFC 8881 §18.35.5 to detect "another principal trying to use the
    /// same client owner" and apply NFS4ERR_PERM / replacement semantics.
    pub principal: Vec<u8>,

    /// `true` after the client has successfully completed CREATE_SESSION
    /// for this clientid (record is "confirmed"). Unconfirmed records have
    /// different §18.35.5 outcomes (always replace on duplicate
    /// EXCHANGE_ID, NOENT for UPD_CONFIRMED_REC_A).
    pub confirmed: bool,

    /// `csa_sequence` of the most recently *accepted* CREATE_SESSION on
    /// this clientid. `None` until the first CREATE_SESSION succeeds.
    /// RFC 8881 §18.36.4: a CREATE_SESSION with the same sequence as the
    /// last one is a replay; sequence + 1 is forward progress; anything
    /// else is `NFS4ERR_SEQ_MISORDERED`.
    pub last_cs_sequence: Option<u32>,

    /// Cached CREATE_SESSION reply bytes for the `last_cs_sequence` slot,
    /// returned verbatim on replay (RFC 8881 §15.1.10.4 exactly-once). Held
    /// as raw bytes so we don't have a circular type dependency between
    /// state::client and operations::session.
    pub cs_cached_reply: Option<bytes::Bytes>,
    /// Cached high-level CREATE_SESSION result for replay. Held in addition
    /// to `cs_cached_reply` because the dispatcher expects an
    /// `OperationResult::CreateSession` with structured fields, not raw
    /// bytes (CREATE_SESSION sits in a sole-op COMPOUND, so the
    /// SEQUENCE-style raw-bytes replay path doesn't apply).
    pub cs_cached_res: Option<CachedCreateSessionRes>,

    /// Initial CREATE_SESSION sequence ID. Returned by EXCHANGE_ID as
    /// `eir_sequenceid`; the first CREATE_SESSION on this clientid must
    /// carry exactly this value.
    pub initial_cs_sequence: u32,
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
        principal: Vec<u8>,
    ) -> Self {
        Self {
            client_id,
            owner,
            verifier,
            server_owner,
            server_scope,
            sequence_id: 0,
            flags,
            principal,
            confirmed: false,
            last_cs_sequence: None,
            cs_cached_reply: None,
            cs_cached_res: None,
            initial_cs_sequence: 0,
        }
    }
}

/// Outcome of `ClientManager::process_create_session_seq`. Lets the
/// CREATE_SESSION op handler distinguish "execute and cache" from "this
/// is a replay, return these fields verbatim" from "client's sequence
/// number is out of order".
#[derive(Debug)]
pub enum CreateSessionSeq {
    /// Forward-progress request — execute normally and call
    /// `record_create_session_reply()` when done.
    Execute,
    /// Exact replay of the previous CREATE_SESSION; return this cached
    /// structured response.
    Replay(CachedCreateSessionRes),
    /// Sequence number is neither last nor last+1 — RFC 8881 §18.36.4
    /// requires `NFS4ERR_SEQ_MISORDERED`.
    Misordered,
    /// Clientid does not exist — caller should return `NFS4ERR_STALE_CLIENTID`.
    StaleClientId,
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

    /// EXCHANGE_ID operation, implementing RFC 8881 §18.35.5.
    ///
    /// The state machine has nine cases keyed on:
    ///   * UPD_CONFIRMED_REC_A flag in `flags`
    ///   * whether an existing record for `owner` exists
    ///   * whether the existing record is `confirmed`
    ///   * whether the verifier matches the existing record's
    ///   * whether the principal matches the existing record's
    ///
    /// A simplified decision table (matching RFC 8881 §18.35.5 Table 2):
    ///
    ///   UPD?  Existing? Confirmed?  Verf?  Princ?   → Outcome
    ///   ----- --------- ----------- ------ ------   ----------
    ///   0     no        n/a         n/a    n/a      → NEW (create)
    ///   0     yes       no          n/a    n/a      → REPLACE (case 4)
    ///   0     yes       yes         eq     eq       → RENEW (case 1)
    ///   0     yes       yes         ne     eq       → REPLACE (case 5)
    ///   0     yes       yes         eq     ne       → REPLACE (case 9 alt)
    ///   0     yes       yes         ne     ne       → REPLACE (case 3)
    ///   1     no        n/a         n/a    n/a      → NoEnt   (case 7)
    ///   1     yes       no          n/a    n/a      → NoEnt   (case 7)
    ///   1     yes       yes         eq     eq       → UPDATE  (case 6)
    ///   1     yes       yes         ne     eq       → NotSame (case 8)
    ///   1     yes       yes         eq     ne       → Perm    (case 9)
    ///   1     yes       yes         ne     ne       → NotSame (case 8 alt)
    pub fn exchange_id(
        &self,
        owner: Vec<u8>,
        verifier: u64,
        flags: u32,
        principal: Vec<u8>,
    ) -> ExchangeIdOutcome {
        let upd = flags & EXCHGID4_FLAG_UPD_CONFIRMED_REC_A != 0;

        let existing = self.owner_to_id.get(&owner).as_deref().copied()
            .and_then(|id| self.clients.get(&id).map(|r| r.clone()));

        match (upd, existing) {
            // ── UPD_CONFIRMED_REC_A SET ────────────────────────────────────
            (true, None) => {
                debug!("EXCHANGE_ID upd: no existing record → NoEnt");
                ExchangeIdOutcome::NoEnt
            }
            (true, Some(c)) if !c.confirmed => {
                debug!("EXCHANGE_ID upd: record unconfirmed → NoEnt");
                ExchangeIdOutcome::NoEnt
            }
            (true, Some(c)) => {
                let verf_eq = c.verifier == verifier;
                let princ_eq = c.principal == principal;
                match (verf_eq, princ_eq) {
                    (true, true) => {
                        // Case 6 — confirmed update of existing record. Return
                        // the existing clientid; nothing to actually change yet.
                        debug!("EXCHANGE_ID upd: case 6 (idempotent), client {}", c.client_id);
                        ExchangeIdOutcome::ExistingConfirmed {
                            client_id: c.client_id,
                            sequence_id: c.sequence_id,
                        }
                    }
                    (false, _) => {
                        // Case 8 — verifier mismatch (with or without princ
                        // match). The principal-mismatch sub-case is not
                        // distinguished by the RFC; both are NOT_SAME.
                        warn!("EXCHANGE_ID upd: case 8 (verifier mismatch) → NOT_SAME");
                        ExchangeIdOutcome::NotSame
                    }
                    (true, false) => {
                        // Case 9 — verifier matches but principal does not.
                        warn!("EXCHANGE_ID upd: case 9 (princ mismatch) → PERM");
                        ExchangeIdOutcome::Perm
                    }
                }
            }

            // ── UPD_CONFIRMED_REC_A NOT SET ────────────────────────────────
            (false, None) => {
                let outcome = self.allocate_client(owner, verifier, flags, principal);
                info!("EXCHANGE_ID: new client {} (no existing record)", outcome_id(&outcome));
                outcome
            }
            (false, Some(c)) if !c.confirmed => {
                // Case 4 — replace the unconfirmed record. The old clientid
                // was never made usable (no CREATE_SESSION succeeded), so we
                // can drop it without disturbing any live state.
                debug!("EXCHANGE_ID: case 4 (replace unconfirmed) — drop client {}", c.client_id);
                let _ = self.remove_client_internal(c.client_id);
                self.allocate_client(owner, verifier, flags, principal)
            }
            (false, Some(c)) => {
                // Confirmed record. Distinguish the four (verf, princ) cases.
                let verf_eq = c.verifier == verifier;
                let princ_eq = c.principal == principal;
                match (verf_eq, princ_eq) {
                    (true, true) => {
                        // Case 1 — straightforward renewal. Caller can keep
                        // using existing sessions.
                        debug!("EXCHANGE_ID: case 1 (renewal), client {}", c.client_id);
                        ExchangeIdOutcome::ExistingConfirmed {
                            client_id: c.client_id,
                            sequence_id: c.sequence_id,
                        }
                    }
                    (false, true) => {
                        // Case 5 — same principal, fresh verifier (client
                        // rebooted). Allocate a *new* clientid; the OLD
                        // clientid's sessions remain valid until the new one
                        // is confirmed via CREATE_SESSION (we don't enforce
                        // that delay yet — pynfs's testNoUpdate101b checks
                        // the post-confirm BADSESSION; deferred for now).
                        info!("EXCHANGE_ID: case 5 (client reboot), replacing client {}", c.client_id);
                        let _ = self.remove_client_internal(c.client_id);
                        self.allocate_client(owner, verifier, flags, principal)
                    }
                    (true, false) => {
                        // Case 9 alt — verifier matches but a different
                        // principal is asking. Replace and warn.
                        warn!("EXCHANGE_ID: case 9 alt (princ change), replacing client {}", c.client_id);
                        let _ = self.remove_client_internal(c.client_id);
                        self.allocate_client(owner, verifier, flags, principal)
                    }
                    (false, false) => {
                        // Case 3 — wholly unrelated EXCHANGE_ID happens to
                        // collide on owner. Replace.
                        warn!("EXCHANGE_ID: case 3 (full mismatch), replacing client {}", c.client_id);
                        let _ = self.remove_client_internal(c.client_id);
                        self.allocate_client(owner, verifier, flags, principal)
                    }
                }
            }
        }
    }

    /// Mark a clientid as confirmed (called by CREATE_SESSION when the
    /// client successfully establishes its first session).
    pub fn mark_confirmed(&self, client_id: u64) {
        if let Some(mut c) = self.clients.get_mut(&client_id) {
            if !c.confirmed {
                debug!("Client {} now confirmed", client_id);
                c.confirmed = true;
            }
        }
    }

    /// Allocate a fresh unconfirmed client record. Internal helper used by
    /// `exchange_id` for both the "no existing record" and replacement paths.
    /// `eir_sequenceid` is returned to the client and becomes the value the
    /// first CREATE_SESSION on this clientid must carry as `csa_sequence`.
    fn allocate_client(
        &self,
        owner: Vec<u8>,
        verifier: u64,
        flags: u32,
        principal: Vec<u8>,
    ) -> ExchangeIdOutcome {
        let client_id = self.next_client_id.fetch_add(1, Ordering::SeqCst);
        // Pick a small non-zero sequence so a client that incorrectly sends
        // 0 still hits SEQ_MISORDERED. RFC 8881 §18.35.4 only requires that
        // we pick *some* initial value; clients echo it back on their first
        // CREATE_SESSION.
        let eir_sequenceid: u32 = 1;
        let mut client = Client::new(
            client_id,
            owner.clone(),
            verifier,
            self.server_owner.clone(),
            self.server_scope.clone(),
            flags,
            principal,
        );
        client.initial_cs_sequence = eir_sequenceid;
        self.clients.insert(client_id, client);
        self.owner_to_id.insert(owner, client_id);
        self.lease_manager.create_lease(client_id);
        ExchangeIdOutcome::NewUnconfirmed { client_id, sequence_id: eir_sequenceid }
    }

    /// Internal client removal that doesn't touch logging / leases differently
    /// from the public API. Used during EXCHANGE_ID record replacement.
    fn remove_client_internal(&self, client_id: u64) -> Option<Client> {
        if let Some((_, client)) = self.clients.remove(&client_id) {
            self.owner_to_id.remove(&client.owner);
            self.lease_manager.remove_lease(client_id);
            Some(client)
        } else {
            None
        }
    }

    /// Get client by ID
    ///
    /// LOCK-FREE: Concurrent reads don't block
    pub fn get_client(&self, client_id: u64) -> Option<Client> {
        self.clients.get(&client_id).map(|r| r.clone())
    }

    /// Update client sequence ID (legacy helper — retained for callers that
    /// only need a monotonic counter, not the full §18.36.4 state machine).
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

    /// Process the `csa_sequence` field of a CREATE_SESSION op, applying
    /// the RFC 8881 §18.36.4 sequence rules:
    ///
    ///   Initial state (no CREATE_SESSION yet):
    ///     csa_sequence == initial_cs_sequence  →  Execute
    ///     anything else                        →  Misordered
    ///
    ///   After at least one accepted CREATE_SESSION at sequence S:
    ///     csa_sequence == S                    →  Replay (return cached)
    ///     csa_sequence == S + 1                →  Execute (forward)
    ///     anything else                        →  Misordered
    pub fn process_create_session_seq(&self, client_id: u64, csa_sequence: u32) -> CreateSessionSeq {
        let client = match self.clients.get(&client_id) {
            Some(c) => c,
            None => return CreateSessionSeq::StaleClientId,
        };

        match client.last_cs_sequence {
            None => {
                if csa_sequence == client.initial_cs_sequence {
                    CreateSessionSeq::Execute
                } else {
                    CreateSessionSeq::Misordered
                }
            }
            Some(last) => {
                if csa_sequence == last {
                    match &client.cs_cached_res {
                        Some(res) => CreateSessionSeq::Replay(res.clone()),
                        // Should not happen — we only set last_cs_sequence
                        // alongside cs_cached_res. Treat defensively as
                        // misordered so we don't double-execute.
                        None => CreateSessionSeq::Misordered,
                    }
                } else if csa_sequence == last.wrapping_add(1) {
                    CreateSessionSeq::Execute
                } else {
                    CreateSessionSeq::Misordered
                }
            }
        }
    }

    /// Record the result of a successful CREATE_SESSION so a future replay
    /// at the same `csa_sequence` returns byte-identical fields.
    pub fn record_create_session_reply(
        &self,
        client_id: u64,
        csa_sequence: u32,
        cached: CachedCreateSessionRes,
    ) {
        if let Some(mut client) = self.clients.get_mut(&client_id) {
            client.last_cs_sequence = Some(csa_sequence);
            client.cs_cached_res = Some(cached);
        }
    }

    /// Return the principal of the client that performed the EXCHANGE_ID
    /// for this clientid. Used by CREATE_SESSION's principal-collision
    /// check (RFC 8881 §18.36.3 returns NFS4ERR_CLID_INUSE if a different
    /// principal tries to create a session on this clientid).
    pub fn get_principal(&self, client_id: u64) -> Option<Vec<u8>> {
        self.clients.get(&client_id).map(|c| c.principal.clone())
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

    use super::*;

    fn new_id(o: ExchangeIdOutcome) -> u64 {
        match o {
            ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            ExchangeIdOutcome::ExistingConfirmed { client_id, .. } => client_id,
            other => panic!("expected client id, got {:?}", other),
        }
    }

    #[test]
    fn test_exchange_id_new_client() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let outcome = client_mgr.exchange_id(b"client1".to_vec(), 12345, 0, b"princ".to_vec());
        // initial_cs_sequence is now 1 (small non-zero, so a client that
        // sends 0 still gets SEQ_MISORDERED).
        assert!(matches!(outcome, ExchangeIdOutcome::NewUnconfirmed { client_id: 1, sequence_id: 1 }));
        assert_eq!(client_mgr.active_count(), 1);
    }

    #[test]
    fn test_exchange_id_replaces_unconfirmed() {
        // Until CREATE_SESSION confirms a client, RFC 8881 §18.35.5 case 4
        // says a duplicate EXCHANGE_ID MUST replace the record (new clientid).
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let id1 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec()));
        let id2 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec()));
        assert_ne!(id1, id2, "unconfirmed records MUST be replaced");
        assert_eq!(client_mgr.active_count(), 1);
    }

    #[test]
    fn test_exchange_id_renews_confirmed() {
        // After CREATE_SESSION marks the client confirmed, an idempotent
        // EXCHANGE_ID returns the existing clientid (case 1).
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let id1 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec()));
        client_mgr.mark_confirmed(id1);
        let id2 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec()));
        assert_eq!(id1, id2, "confirmed + idempotent → renewal");
    }

    #[test]
    fn test_exchange_id_principal_mismatch_replaces() {
        // Confirmed record + same verifier but different principal →
        // replace (case 9 alt).
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let id1 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"alice".to_vec()));
        client_mgr.mark_confirmed(id1);
        let id2 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"bob".to_vec()));
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_exchange_id_upd_no_record_returns_noent() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let outcome = client_mgr.exchange_id(
            b"c".to_vec(), 1, EXCHGID4_FLAG_UPD_CONFIRMED_REC_A, b"p".to_vec()
        );
        assert!(matches!(outcome, ExchangeIdOutcome::NoEnt));
    }

    #[test]
    fn test_exchange_id_upd_unconfirmed_returns_noent() {
        // EXCHANGE_ID with UPD on an unconfirmed record → NoEnt (case 7).
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let _ = client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec());
        let outcome = client_mgr.exchange_id(
            b"c".to_vec(), 1, EXCHGID4_FLAG_UPD_CONFIRMED_REC_A, b"p".to_vec()
        );
        assert!(matches!(outcome, ExchangeIdOutcome::NoEnt));
    }

    #[test]
    fn test_exchange_id_upd_verifier_mismatch_returns_not_same() {
        // Confirmed + UPD + different verifier → NotSame (case 8).
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let id1 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec()));
        client_mgr.mark_confirmed(id1);
        let outcome = client_mgr.exchange_id(
            b"c".to_vec(), 2, EXCHGID4_FLAG_UPD_CONFIRMED_REC_A, b"p".to_vec()
        );
        assert!(matches!(outcome, ExchangeIdOutcome::NotSame));
    }

    #[test]
    fn test_exchange_id_upd_princ_mismatch_returns_perm() {
        // Confirmed + UPD + same verifier + different principal → Perm
        // (case 9).
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let id1 = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"alice".to_vec()));
        client_mgr.mark_confirmed(id1);
        let outcome = client_mgr.exchange_id(
            b"c".to_vec(), 1, EXCHGID4_FLAG_UPD_CONFIRMED_REC_A, b"bob".to_vec()
        );
        assert!(matches!(outcome, ExchangeIdOutcome::Perm));
    }

    #[test]
    fn test_sequence_update() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let id = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec()));
        assert_eq!(client_mgr.update_sequence(id).unwrap(), 1);
        assert_eq!(client_mgr.update_sequence(id).unwrap(), 2);
    }

    #[test]
    fn test_client_removal() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = ClientManager::new(lease_mgr, "test-vol");

        let id = new_id(client_mgr.exchange_id(b"c".to_vec(), 1, 0, b"p".to_vec()));
        assert_eq!(client_mgr.active_count(), 1);
        client_mgr.remove_client(id);
        assert_eq!(client_mgr.active_count(), 0);
        assert!(client_mgr.get_client(id).is_none());
    }
}
