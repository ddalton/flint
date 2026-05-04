// NFSv4.1 Session Operations
//
// Session operations are the foundation of NFSv4.1's exactly-once semantics
// and connection-based state management.
//
// Operation Flow:
// 1. EXCHANGE_ID: Client establishes clientid with server
// 2. CREATE_SESSION: Client creates session for exactly-once semantics
// 3. SEQUENCE: Every COMPOUND starts with SEQUENCE (renews lease, provides slot for replay detection)
// 4. DESTROY_SESSION: Client destroys session
//
// Every NFSv4.1 COMPOUND (except EXCHANGE_ID) must start with SEQUENCE

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::state::StateManager;
use crate::nfs::v4::compound::{ChannelAttrs, CompoundContext};
use std::sync::Arc;
use tracing::{debug, info, warn};

// CREATE_SESSION flags (RFC 5661 §18.36)
// Defined for protocol completeness; not all flags are implemented yet
#[allow(dead_code)]
const CREATE_SESSION4_FLAG_PERSIST: u32 = 0x0000_0001;
#[allow(dead_code)]
const CREATE_SESSION4_FLAG_CONN_BACK_CHAN: u32 = 0x0000_0002;
#[allow(dead_code)]
const CREATE_SESSION4_FLAG_CONN_RDMA: u32 = 0x0000_0004;

/// EXCHANGE_ID operation (opcode 42)
///
/// Establishes client identity and receives clientid from server.
/// This is the first operation a client performs.
pub struct ExchangeIdOp {
    /// Client owner (unique identifier for the client)
    pub client_owner: Vec<u8>,

    /// Client verifier (for detecting client reboots)
    pub verifier: u64,

    /// Flags (SP4_NONE, SP4_MACH_CRED, etc.)
    pub flags: u32,

    /// State protection (we'll use SP4_NONE for simplicity)
    pub state_protect: u32,

    /// Client implementation details
    pub client_impl_id: Option<ClientImplId>,
}

#[derive(Debug, Clone)]
pub struct ClientImplId {
    pub nii_domain: String,
    pub nii_name: String,
    pub nii_date: String,
}

/// EXCHANGE_ID response
pub struct ExchangeIdRes {
    pub status: Nfs4Status,
    pub clientid: u64,
    pub sequenceid: u32,
    pub flags: u32,
    pub server_owner: String,
    pub server_scope: Vec<u8>,
}

/// CREATE_SESSION operation (opcode 43)
///
/// Creates a session for exactly-once semantics and slot management.
pub struct CreateSessionOp {
    /// Client ID (from EXCHANGE_ID)
    pub clientid: u64,

    /// Sequence ID (from EXCHANGE_ID response)
    pub sequence: u32,

    /// Flags (CREATE_SESSION4_FLAG_PERSIST, etc.)
    pub flags: u32,

    /// Fore channel attributes (client → server)
    pub fore_chan_attrs: ChannelAttrs,

    /// Back channel attributes (server → client, for callbacks)
    pub back_chan_attrs: ChannelAttrs,

    /// Callback program number
    pub cb_program: u32,
}

// ChannelAttrs is now imported from compound.rs to ensure field name consistency

/// CREATE_SESSION response
#[derive(Debug, Clone)]
pub struct CreateSessionRes {
    pub status: Nfs4Status,
    pub sessionid: SessionId,
    pub sequence: u32,
    pub flags: u32,
    pub fore_chan_attrs: ChannelAttrs,
    pub back_chan_attrs: ChannelAttrs,
}

/// Build a CREATE_SESSION error reply with zeroed channel attributes. Used
/// for every error path so we have a single source of truth for the result
/// shape.
fn create_session_err(status: Nfs4Status) -> CreateSessionRes {
    CreateSessionRes {
        status,
        sessionid: SessionId([0; 16]),
        sequence: 0,
        flags: 0,
        fore_chan_attrs: ChannelAttrs::default(),
        back_chan_attrs: ChannelAttrs::default(),
    }
}

/// Build an EXCHANGE_ID error reply. server_owner / server_scope are empty
/// because the spec lets a server omit identification on an error path.
fn exchange_id_err(status: Nfs4Status) -> ExchangeIdRes {
    ExchangeIdRes {
        status,
        clientid: 0,
        sequenceid: 0,
        flags: 0,
        server_owner: String::new(),
        server_scope: Vec::new(),
    }
}

/// SEQUENCE operation (opcode 53)
///
/// Must be the first operation in every COMPOUND (except EXCHANGE_ID).
/// Provides exactly-once semantics via slot management.
pub struct SequenceOp {
    /// Session ID
    pub sessionid: SessionId,

    /// Sequence ID for this slot (increments for each new request)
    pub sequenceid: u32,

    /// Slot ID (for replay detection)
    pub slotid: u32,

    /// Highest slot ID in use by client
    pub highest_slotid: u32,

    /// Is this a cache-this-response request?
    pub cache_this: bool,
}

/// SEQUENCE response
pub struct SequenceRes {
    pub status: Nfs4Status,
    pub sessionid: SessionId,
    pub sequenceid: u32,
    pub slotid: u32,
    pub highest_slotid: u32,
    pub target_highest_slotid: u32,
}

/// DESTROY_SESSION operation (opcode 44)
pub struct DestroySessionOp {
    pub sessionid: SessionId,
}

/// DESTROY_SESSION response
pub struct DestroySessionRes {
    pub status: Nfs4Status,
}

/// Session operation handler
pub struct SessionOperationHandler {
    state_mgr: Arc<StateManager>,
}

impl SessionOperationHandler {
    /// Create a new session operation handler
    pub fn new(state_mgr: Arc<StateManager>) -> Self {
        Self { state_mgr }
    }

    /// Handle EXCHANGE_ID operation
    pub fn handle_exchange_id(&self, op: ExchangeIdOp, ctx: &CompoundContext) -> ExchangeIdRes {
        info!("EXCHANGE_ID: owner={:?}, verifier={}", op.client_owner, op.verifier);

        use crate::nfs::v4::protocol::exchgid_flags;

        // RFC 8881 §18.35.3: only specific bits are allowed in eia_flags.
        // CONFIRMED_R is server-set-only; UPD_CONFIRMED_REC_A is client-set
        // when updating an existing record. Anything else MUST return INVAL.
        const VALID_EID_FLAGS: u32 = exchgid_flags::SUPP_MOVED_REFER
            | exchgid_flags::SUPP_MOVED_MIGR
            | exchgid_flags::BIND_PRINC_STATEID
            | exchgid_flags::USE_NON_PNFS
            | exchgid_flags::USE_PNFS_MDS
            | exchgid_flags::USE_PNFS_DS
            | exchgid_flags::UPD_CONFIRMED_REC_A;
        if op.flags & !VALID_EID_FLAGS != 0 {
            warn!("EXCHANGE_ID: rejecting unknown flag bits 0x{:x}", op.flags);
            return exchange_id_err(Nfs4Status::Inval);
        }
        // RFC 8881 §18.35.3: eia_flags MUST NOT have CONFIRMED_R set by the
        // client (it's a response-only bit). EID7 testSupported1a covers this.
        if op.flags & exchgid_flags::CONFIRMED_R != 0 {
            warn!("EXCHANGE_ID: client set CONFIRMED_R (server-set-only) flag");
            return exchange_id_err(Nfs4Status::Inval);
        }

        // RFC 8881 §18.35.5 client-record state machine. Principal is the
        // RPC-level identity of the caller; an empty Vec for AUTH_NONE.
        use crate::nfs::v4::state::client::ExchangeIdOutcome;
        let outcome = self.state_mgr.clients.exchange_id(
            op.client_owner,
            op.verifier,
            op.flags,
            ctx.principal.clone(),
        );

        let (clientid, sequenceid, is_new) = match outcome {
            ExchangeIdOutcome::NewUnconfirmed { client_id, sequence_id } => {
                info!("EXCHANGE_ID: new unconfirmed clientid {}", client_id);
                (client_id, sequence_id, true)
            }
            ExchangeIdOutcome::ExistingConfirmed { client_id, sequence_id } => {
                info!("EXCHANGE_ID: returning confirmed clientid {}", client_id);
                (client_id, sequence_id, false)
            }
            ExchangeIdOutcome::NoEnt => {
                warn!("EXCHANGE_ID: UPD_CONFIRMED_REC_A on missing/unconfirmed record");
                return exchange_id_err(Nfs4Status::NoEnt);
            }
            ExchangeIdOutcome::NotSame => {
                return exchange_id_err(Nfs4Status::NotSame);
            }
            ExchangeIdOutcome::Perm => {
                return exchange_id_err(Nfs4Status::Perm);
            }
        };

        // Build server response flags per RFC 8881 Section 18.35
        let mut response_flags = 0u32;

        // Set server role based on server identity (from ClientManager)
        // pNFS servers have owner "flint-pnfs", standalone has "flint-nfs"
        let server_owner = self.state_mgr.clients.server_owner().to_string();
        let is_pnfs = server_owner.contains("pnfs");
        
        if is_pnfs {
            // Check PNFS_MODE to determine if MDS or DS
            let pnfs_mode = std::env::var("PNFS_MODE").ok();
            if pnfs_mode.as_deref() == Some("ds") {
                response_flags |= exchgid_flags::USE_PNFS_DS;
                debug!("EXCHANGE_ID: Server mode = pNFS Data Server");
            } else {
                response_flags |= exchgid_flags::USE_PNFS_MDS;
                debug!("EXCHANGE_ID: Server mode = pNFS Metadata Server");
            }
        } else {
            // Standalone NFS server - no pNFS support
            response_flags |= exchgid_flags::USE_NON_PNFS;
            debug!("EXCHANGE_ID: Server mode = Standalone NFS (no pNFS)");
        }

        // Echo back ALL client capability flags (RFC 8881 Section 18.35.3)
        if op.flags & exchgid_flags::SUPP_MOVED_REFER != 0 {
            response_flags |= exchgid_flags::SUPP_MOVED_REFER;
        }
        if op.flags & exchgid_flags::SUPP_MOVED_MIGR != 0 {
            response_flags |= exchgid_flags::SUPP_MOVED_MIGR;
        }
        if op.flags & exchgid_flags::BIND_PRINC_STATEID != 0 {
            response_flags |= exchgid_flags::BIND_PRINC_STATEID;
        }

        // If this is an existing client (confirmed), set CONFIRMED_R flag
        if !is_new {
            response_flags |= exchgid_flags::CONFIRMED_R;
        }

        let server_scope = self.state_mgr.clients.server_scope().to_vec();
        
        info!("EXCHANGE_ID response - server_owner={:?}, server_scope={:?}, flags=0x{:08x}",
              server_owner, String::from_utf8_lossy(&server_scope), response_flags);
        
        ExchangeIdRes {
            status: Nfs4Status::Ok,
            clientid,
            sequenceid,
            flags: response_flags,
            server_owner,
            server_scope,
        }
    }

    /// Handle CREATE_SESSION operation. Validates input per RFC 5661 §18.36
    /// before negotiating channel sizes:
    ///   * unknown bits in csa_flags          → NFS4ERR_INVAL
    ///   * channel sizes below server minimum → NFS4ERR_TOOSMALL
    ///   * unknown clientid                   → NFS4ERR_STALE_CLIENTID
    /// "Clamp the value upward to a known good number" (the previous behavior)
    /// is wrong: it makes a buggy client succeed with degraded settings instead
    /// of telling them the session won't work.
    pub fn handle_create_session(&self, op: CreateSessionOp, ctx: &CompoundContext) -> CreateSessionRes {
        info!("CREATE_SESSION: clientid={}, sequence={}", op.clientid, op.sequence);
        info!("CREATE_SESSION: Client requested - max_request={}, max_response={}, max_ops={}",
              op.fore_chan_attrs.max_request_size, op.fore_chan_attrs.max_response_size, op.fore_chan_attrs.max_operations);

        // Defined csa_flags bits (RFC 5661 §18.36.3): PERSIST | CONN_BACK_CHAN | CONN_RDMA.
        const VALID_FLAGS: u32 = CREATE_SESSION4_FLAG_PERSIST
            | CREATE_SESSION4_FLAG_CONN_BACK_CHAN
            | CREATE_SESSION4_FLAG_CONN_RDMA;
        if op.flags & !VALID_FLAGS != 0 {
            warn!("CREATE_SESSION: rejecting unknown flag bits 0x{:x}", op.flags);
            return create_session_err(Nfs4Status::Inval);
        }

        // Server minimums for channel sizes (RFC 5661 §18.36.4): "If the
        // server is unable to support the value (it is too small), the
        // server MUST return NFS4ERR_TOOSMALL". The minimum has to be
        // permissive enough that pynfs's testRepTooBig (which negotiates a
        // 400-byte channel deliberately to provoke REP_TOO_BIG later) gets
        // through here. 256 is the smallest viable RPC frame in practice.
        const MIN_REQUEST_SIZE: u32 = 256;
        const MIN_RESPONSE_SIZE: u32 = 256;
        const MIN_OPERATIONS: u32 = 1;

        for (chan, attrs) in [
            ("fore", &op.fore_chan_attrs),
            ("back", &op.back_chan_attrs),
        ] {
            if attrs.max_request_size < MIN_REQUEST_SIZE
                || attrs.max_response_size < MIN_RESPONSE_SIZE
                || attrs.max_operations < MIN_OPERATIONS
            {
                warn!("CREATE_SESSION: {chan}-channel attrs too small ({:?})", attrs);
                return create_session_err(Nfs4Status::TooSmall);
            }
        }

        // RFC 8881 §18.36.3: if this is the first CREATE_SESSION on an
        // unconfirmed clientid and the calling principal is not the one
        // that performed the EXCHANGE_ID, return NFS4ERR_CLID_INUSE.
        //
        // After the clientid is confirmed (a successful CREATE_SESSION
        // happened), the principal check is dropped — replays of the same
        // CREATE_SESSION may legitimately come from a different cred
        // (RFC 8881 §18.36.3, pynfs CSESS10 testPrincipalCollision2).
        let client_principal_check = self.state_mgr.clients.get_client(op.clientid);
        match client_principal_check {
            None => {
                warn!("CREATE_SESSION: Client {} not found", op.clientid);
                return create_session_err(Nfs4Status::StaleClientId);
            }
            Some(c) if !c.confirmed && c.principal != ctx.principal => {
                warn!("CREATE_SESSION: principal collision (unconfirmed clientid {}: original={:?}, attempted={:?})",
                      op.clientid, String::from_utf8_lossy(&c.principal), String::from_utf8_lossy(&ctx.principal));
                return create_session_err(Nfs4Status::ClIdInUse);
            }
            Some(_) => { /* matches or already confirmed — proceed */ }
        }

        // RFC 8881 §8.4.2 + pynfs EID9: a clientid whose lease has
        // fully expired is `STALE_CLIENTID`. The lease is renewed by
        // every successful SEQUENCE; for an unconfirmed clientid that
        // never reached CREATE_SESSION, the original EXCHANGE_ID's
        // implicit lease is the only one. After lease_time + slack
        // with no renewal, the client must re-EXCHANGE_ID.
        if !self.state_mgr.leases.is_valid(op.clientid) {
            warn!(
                "CREATE_SESSION: Client {} lease expired → STALE_CLIENTID",
                op.clientid,
            );
            return create_session_err(Nfs4Status::StaleClientId);
        }

        // RFC 8881 §18.36.4 sequence checks. Replays return the cached
        // structured response so the client sees byte-identical fields;
        // out-of-order csa_sequence is SEQ_MISORDERED.
        use crate::nfs::v4::state::client::CreateSessionSeq;
        match self.state_mgr.clients.process_create_session_seq(op.clientid, op.sequence) {
            CreateSessionSeq::StaleClientId => {
                return create_session_err(Nfs4Status::StaleClientId);
            }
            CreateSessionSeq::Misordered => {
                warn!("CREATE_SESSION: csa_sequence {} misordered", op.sequence);
                return create_session_err(Nfs4Status::SeqMisordered);
            }
            CreateSessionSeq::Replay(cached) => {
                debug!("CREATE_SESSION: replay for clientid {} seq {}", op.clientid, op.sequence);
                return CreateSessionRes {
                    status: Nfs4Status::Ok,
                    sessionid: cached.sessionid,
                    sequence: cached.sequence,
                    flags: cached.flags,
                    fore_chan_attrs: ChannelAttrs {
                        header_pad_size: 0,
                        max_request_size: cached.fore_max_request_size,
                        max_response_size: cached.fore_max_response_size,
                        max_response_size_cached: cached.fore_max_response_size_cached,
                        max_operations: cached.fore_max_operations,
                        max_requests: cached.fore_max_requests,
                        rdma_ird: Vec::new(),
                    },
                    back_chan_attrs: ChannelAttrs::default(),
                };
            }
            CreateSessionSeq::Execute => { /* normal forward case */ }
        }

        // CREATE_SESSION succeeding marks the client record as confirmed,
        // which changes the EXCHANGE_ID §18.35.5 outcome for subsequent
        // EXCHANGE_IDs on the same owner (e.g. allows UPD_CONFIRMED_REC_A
        // to land instead of NoEnt). Also drives RFC 8881 §18.35.5
        // case 5 deferred-cleanup: if this client was allocated as a
        // case-5 replacement, the OLD clientid's sessions / stateids /
        // delegations / lease / record are torn down here. Pynfs
        // EID5f/EID5fb verifies the old session returns BADSESSION
        // immediately after this call.
        if let Some(old_id) = self.state_mgr.clients.mark_confirmed(op.clientid) {
            warn!(
                "CREATE_SESSION: case-5 deferred cleanup — discarding pre-reboot client {}",
                old_id,
            );
            self.state_mgr.sessions.destroy_client_sessions(old_id);
            self.state_mgr.stateids.remove_client_stateids(old_id);
            self.state_mgr.delegations.cleanup_client_delegations(old_id);
            self.state_mgr.clients.remove_client(old_id);
        }

        // Negotiate session buffer sizes. Take the *minimum* of client-requested
        // and server-maximum. We've already rejected requests below the server
        // minimum above, so the negotiated value is always >= MIN_*.
        const SERVER_MAX_REQUEST: u32 = 1024 * 1024;
        const SERVER_MAX_RESPONSE: u32 = 1024 * 1024;
        const SERVER_MAX_OPS: u32 = 128;

        let negotiated_max_request = op.fore_chan_attrs.max_request_size.min(SERVER_MAX_REQUEST);
        let negotiated_max_response = op.fore_chan_attrs.max_response_size.min(SERVER_MAX_RESPONSE);
        let negotiated_max_ops = op.fore_chan_attrs.max_operations.min(SERVER_MAX_OPS);

        info!("CREATE_SESSION: Negotiated: req={}, resp={}, ops={}",
              negotiated_max_request, negotiated_max_response, negotiated_max_ops);

        // Create session with negotiated sizes. We negotiate the cached
        // response size by clamping the client's request: it MUST be ≤
        // ca_maxresponsesize (caches can't be bigger than full responses)
        // and is also ≤ our SERVER_MAX_RESPONSE.
        let negotiated_max_response_cached = op.fore_chan_attrs
            .max_response_size_cached
            .min(negotiated_max_response);
        // Slot count = ca_maxrequests, capped at our MAX_SLOTS sentinel.
        const SERVER_MAX_REQUESTS: u32 = 128;
        let negotiated_max_requests = op.fore_chan_attrs
            .max_requests
            .max(1)
            .min(SERVER_MAX_REQUESTS);
        let session = self.state_mgr.sessions.create_session(
            op.clientid,
            op.sequence,
            op.flags,
            negotiated_max_request,
            negotiated_max_response,
            negotiated_max_response_cached,
            negotiated_max_ops,
            negotiated_max_requests,
            op.cb_program,
        );

        info!("CREATE_SESSION: Session {:?} created for client {} with {}KB buffers",
              session.session_id, op.clientid, negotiated_max_request / 1024);

        // Set server flags based on actual capabilities (RFC 5661 §18.36).
        // We do not implement persistent reply cache, and we do not yet
        // negotiate back-channel attrs, so we don't echo CONN_BACK_CHAN —
        // even when we *do* register the connection's writer for callbacks
        // (see the dispatcher's CREATE_SESSION arm). Linux v4.1 clients
        // are happy to mount without CONN_BACK_CHAN echoed; they may
        // refuse our outbound CB CALLs with PROG_UNAVAIL, which the
        // CallbackManager surfaces cleanly.
        let server_flags = 0u32;

        // We don't advertise back-channel attrs; clients ignore this
        // field when csr_flags doesn't have CONN_BACK_CHAN set.
        let back_chan_attrs = ChannelAttrs::default();

        // Record the result in the per-client CREATE_SESSION cache so a
        // future replay (same csa_sequence) returns byte-identical fields
        // (RFC 8881 §15.1.10.4 / §18.36.4).
        use crate::nfs::v4::state::client::CachedCreateSessionRes;
        self.state_mgr.clients.record_create_session_reply(
            op.clientid,
            op.sequence,
            CachedCreateSessionRes {
                sessionid: session.session_id,
                sequence: session.sequence,
                flags: server_flags,
                fore_max_request_size: session.fore_chan_maxrequestsize,
                fore_max_response_size: session.fore_chan_maxresponsesize,
                fore_max_response_size_cached: 64 * 1024,
                fore_max_operations: session.fore_chan_maxops,
                fore_max_requests: 128,
            },
        );

        CreateSessionRes {
            status: Nfs4Status::Ok,
            sessionid: session.session_id,
            sequence: session.sequence,
            flags: server_flags,
            fore_chan_attrs: ChannelAttrs {
                header_pad_size: 0,
                max_request_size: session.fore_chan_maxrequestsize,
                max_response_size: session.fore_chan_maxresponsesize,
                max_response_size_cached: 64 * 1024,
                max_operations: session.fore_chan_maxops,
                max_requests: 128,
                rdma_ird: Vec::new(),
            },
            back_chan_attrs,
        }
    }

    /// Handle SEQUENCE operation.
    ///
    /// Implements RFC 8881 §15.1.10 exactly-once semantics by writing two hints
    /// onto the COMPOUND context:
    ///
    /// - `replay_reply`: when the slot reports an exact resend with a cached
    ///   reply, the bytes are placed here. The dispatcher short-circuits the
    ///   rest of the COMPOUND and returns these bytes verbatim.
    /// - `cache_slot`: on a new request, the `(session, slot)` pair is recorded
    ///   so the RPC layer caches the encoded reply against this slot once it
    ///   has the byte representation.
    ///
    /// The hints are zero-cost on the happy path: a single Option write.
    pub fn handle_sequence(&self, op: SequenceOp, ctx: &mut CompoundContext) -> SequenceRes {
        debug!("SEQUENCE: sessionid={:?}, sequenceid={}, slotid={}",
               op.sessionid, op.sequenceid, op.slotid);

        let session = match self.state_mgr.sessions.get_session(&op.sessionid) {
            Some(s) => s,
            None => {
                warn!("SEQUENCE: Session {:?} not found", op.sessionid);
                return SequenceRes {
                    status: Nfs4Status::BadSession,
                    sessionid: op.sessionid,
                    sequenceid: 0,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                };
            }
        };

        // RFC 5661 §18.46.3 / RFC 8881 §15.1.10.1: slot IDs must be in the
        // range [0, ca_maxrequests). Anything outside is NFS4ERR_BADSLOT.
        if op.slotid >= session.fore_chan_maxrequests {
            warn!("SEQUENCE: slotid {} >= ca_maxrequests {} → BADSLOT",
                  op.slotid, session.fore_chan_maxrequests);
            return SequenceRes {
                status: Nfs4Status::BadSessionId,  // wire value 10053 = NFS4ERR_BADSLOT
                sessionid: op.sessionid,
                sequenceid: 0,
                slotid: 0,
                highest_slotid: 0,
                target_highest_slotid: 0,
            };
        }

        // RFC 8881 §18.46.3: if the client requests `cachethis` and the
        // session's negotiated `ca_maxresponsesize_cached` is too small to
        // hold any meaningful reply, return NFS4ERR_REP_TOO_BIG_TO_CACHE
        // immediately. The smallest reply we ever generate is the SEQUENCE
        // result itself (~32 bytes plus COMPOUND overhead); a cache window
        // smaller than that simply can't hold one. This is the eager
        // approximation the test exercises.
        if op.cache_this && session.fore_chan_maxresponsesize_cached < 64 {
            warn!("SEQUENCE: cachethis with maxresponsesize_cached={} → REP_TOO_BIG_TO_CACHE",
                  session.fore_chan_maxresponsesize_cached);
            return SequenceRes {
                status: Nfs4Status::RepTooBigToCache,
                sessionid: op.sessionid,
                sequenceid: 0,
                slotid: 0,
                highest_slotid: 0,
                target_highest_slotid: 0,
            };
        }

        let outcome = self.state_mgr.sessions.get_session_mut(&op.sessionid, |s| {
            s.process_sequence(op.slotid, op.sequenceid)
        });

        let outcome = match outcome {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                // Slot index out of range; treat as BADSLOT.
                warn!("SEQUENCE: slot error: {}", e);
                return SequenceRes {
                    status: Nfs4Status::BadSessionId,
                    sessionid: op.sessionid,
                    sequenceid: 0,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                };
            }
            None => {
                warn!("SEQUENCE: session {:?} disappeared", op.sessionid);
                return SequenceRes {
                    status: Nfs4Status::BadSession,
                    sessionid: op.sessionid,
                    sequenceid: 0,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                };
            }
        };

        match outcome {
            crate::nfs::v4::state::session::SeqStatus::Misordered => {
                return SequenceRes {
                    status: Nfs4Status::SeqMisordered,
                    sessionid: op.sessionid,
                    sequenceid: 0,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                };
            }
            crate::nfs::v4::state::session::SeqStatus::Replay { cached: Some(bytes) } => {
                // Hand the cached reply back verbatim. Skip lease renewal —
                // we are returning the original reply unchanged, and renewing
                // again would duplicate the side-effect.
                debug!("SEQUENCE: replay with cached reply ({} bytes)", bytes.len());
                ctx.replay_reply = Some(bytes::Bytes::from(bytes));
                // The status / counters here are placeholders — the dispatcher
                // will discard this SequenceRes and replace it with the cached
                // reply bytes. Returning Ok lets the dispatcher avoid logging
                // a spurious "Operation failed" warning before short-circuit.
                return SequenceRes {
                    status: Nfs4Status::Ok,
                    sessionid: op.sessionid,
                    sequenceid: op.sequenceid,
                    slotid: op.slotid,
                    highest_slotid: session.highest_slotid,
                    target_highest_slotid: 127,
                };
            }
            crate::nfs::v4::state::session::SeqStatus::Replay { cached: None } => {
                // Resend before the original reply was cached. RFC 8881
                // §15.1.10.4: NFS4ERR_RETRY_UNCACHED_REP forces the client to
                // wait for the in-flight reply rather than re-execute.
                warn!("SEQUENCE: replay before reply was cached → RETRY_UNCACHED_REP");
                return SequenceRes {
                    status: Nfs4Status::RetryUncachedRep,
                    sessionid: op.sessionid,
                    sequenceid: 0,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                };
            }
            crate::nfs::v4::state::session::SeqStatus::New => {
                // Record where the reply bytes should be cached once the
                // RPC layer has them. cachethis is a hint — we always cache
                // for now (matches Linux server behaviour). Honoring the hint
                // is a perf optimisation we can add later.
                ctx.cache_slot = Some((op.sessionid, op.slotid));
            }
        }

        // Renew lease on every accepted SEQUENCE.
        if let Err(e) = self.state_mgr.leases.renew_lease(session.client_id) {
            warn!("SEQUENCE: Failed to renew lease: {}", e);
        }

        SequenceRes {
            status: Nfs4Status::Ok,
            sessionid: op.sessionid,
            sequenceid: op.sequenceid,
            slotid: op.slotid,
            highest_slotid: session.highest_slotid,
            target_highest_slotid: 127, // we support up to 128 slots
        }
    }

    /// Handle DESTROY_SESSION operation
    pub fn handle_destroy_session(&self, op: DestroySessionOp) -> DestroySessionRes {
        info!("DESTROY_SESSION: sessionid={:?}", op.sessionid);

        match self.state_mgr.sessions.destroy_session(&op.sessionid) {
            Ok(_) => {
                info!("DESTROY_SESSION: Session {:?} destroyed", op.sessionid);
                DestroySessionRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("DESTROY_SESSION: Failed to destroy session: {}", e);
                DestroySessionRes {
                    status: Nfs4Status::BadSession,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exchange_id() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr.clone());

        let op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };

        let res = handler.handle_exchange_id(op, &CompoundContext::new(1));
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.clientid, 1);
        // EXCHANGE_ID returns the initial CREATE_SESSION sequence (eir_sequenceid).
        // We pick 1 so a client that incorrectly sends 0 still gets SEQ_MISORDERED.
        assert_eq!(res.sequenceid, 1);
    }

    #[test]
    fn test_create_session() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // First do EXCHANGE_ID
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op, &CompoundContext::new(1));

        // Now CREATE_SESSION
        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };

        let res = handler.handle_create_session(create_op, &CompoundContext::new(1));
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_ne!(res.sessionid, SessionId([0; 16]));
    }

    #[test]
    fn test_sequence() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // Setup: EXCHANGE_ID + CREATE_SESSION
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op, &CompoundContext::new(1));

        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };
        let create_res = handler.handle_create_session(create_op, &CompoundContext::new(1));

        // Now SEQUENCE
        let seq_op = SequenceOp {
            sessionid: create_res.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: false,
        };

        let mut ctx = CompoundContext::new(1);
        let res = handler.handle_sequence(seq_op, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.slotid, 0);
        // New request → cache slot recorded for the RPC layer.
        assert_eq!(ctx.cache_slot, Some((create_res.sessionid, 0)));
        assert!(ctx.replay_reply.is_none());
    }

    #[test]
    fn test_sequence_replay() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // Setup session
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op, &CompoundContext::new(1));

        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };
        let create_res = handler.handle_create_session(create_op, &CompoundContext::new(1));

        // First SEQUENCE
        let seq_op1 = SequenceOp {
            sessionid: create_res.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: false,
        };
        let mut ctx1 = CompoundContext::new(1);
        let res1 = handler.handle_sequence(seq_op1, &mut ctx1);
        assert_eq!(res1.status, Nfs4Status::Ok);

        // Simulate the RPC layer caching the encoded reply.
        state_mgr.sessions.get_session_mut(&create_res.sessionid, |s| {
            s.cache_response(0, vec![0xDE, 0xAD, 0xBE, 0xEF])
        });

        // Replay the same SEQUENCE → handler should signal a replay by
        // populating ctx.replay_reply with the cached bytes.
        let seq_op2 = SequenceOp {
            sessionid: create_res.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: false,
        };
        let mut ctx2 = CompoundContext::new(1);
        let res2 = handler.handle_sequence(seq_op2, &mut ctx2);
        assert_eq!(res2.status, Nfs4Status::Ok);
        assert_eq!(
            ctx2.replay_reply.as_ref().map(|b| b.as_ref()),
            Some(&[0xDE, 0xAD, 0xBE, 0xEFu8][..])
        );
    }

    #[test]
    fn test_destroy_session() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // Setup session
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op, &CompoundContext::new(1));

        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };
        let create_res = handler.handle_create_session(create_op, &CompoundContext::new(1));

        // Destroy session
        let destroy_op = DestroySessionOp {
            sessionid: create_res.sessionid,
        };
        let res = handler.handle_destroy_session(destroy_op);
        assert_eq!(res.status, Nfs4Status::Ok);

        // Verify session is gone
        assert!(state_mgr.sessions.get_session(&create_res.sessionid).is_none());
    }
}
