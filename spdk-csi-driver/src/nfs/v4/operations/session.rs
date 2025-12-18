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
use crate::nfs::v4::state::{StateManager, StateType};
use crate::nfs::v4::xdr::{Nfs4XdrEncoder, Nfs4XdrDecoder};
use crate::nfs::v4::compound::ChannelAttrs;
use bytes::{BytesMut, BufMut};
use std::sync::Arc;
use tracing::{debug, info, warn};

// CREATE_SESSION flags (RFC 5661 §18.36)
const CREATE_SESSION4_FLAG_PERSIST: u32 = 0x0000_0001;
const CREATE_SESSION4_FLAG_CONN_BACK_CHAN: u32 = 0x0000_0002;
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
pub struct CreateSessionRes {
    pub status: Nfs4Status,
    pub sessionid: SessionId,
    pub sequence: u32,
    pub flags: u32,
    pub fore_chan_attrs: ChannelAttrs,
    pub back_chan_attrs: ChannelAttrs,
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
    pub fn handle_exchange_id(&self, op: ExchangeIdOp) -> ExchangeIdRes {
        info!("EXCHANGE_ID: owner={:?}, verifier={}", op.client_owner, op.verifier);

        // Exchange client ID
        let (clientid, sequenceid, is_new) = self.state_mgr.clients.exchange_id(
            op.client_owner,
            op.verifier,
            op.flags,
        );

        if is_new {
            info!("EXCHANGE_ID: New client {} created", clientid);
        } else {
            info!("EXCHANGE_ID: Existing client {} returned", clientid);
        }

        // Build server response flags per RFC 8881 Section 18.35
        use crate::nfs::v4::protocol::exchgid_flags;
        let mut response_flags = 0u32;

        // Set server role - we're a non-pNFS server
        response_flags |= exchgid_flags::USE_NON_PNFS;

        // Support client capabilities we understand
        if op.flags & exchgid_flags::SUPP_MOVED_REFER != 0 {
            response_flags |= exchgid_flags::SUPP_MOVED_REFER;
        }
        if op.flags & exchgid_flags::SUPP_MOVED_MIGR != 0 {
            response_flags |= exchgid_flags::SUPP_MOVED_MIGR;
        }

        // If this is an existing client (confirmed), set CONFIRMED_R flag
        if !is_new {
            response_flags |= exchgid_flags::CONFIRMED_R;
        }

        ExchangeIdRes {
            status: Nfs4Status::Ok,
            clientid,
            sequenceid,
            flags: response_flags,
            server_owner: self.state_mgr.clients.server_owner().to_string(),
            server_scope: self.state_mgr.clients.server_scope().to_vec(),
        }
    }

    /// Handle CREATE_SESSION operation
    pub fn handle_create_session(&self, op: CreateSessionOp) -> CreateSessionRes {
        info!("CREATE_SESSION: clientid={}, sequence={}", op.clientid, op.sequence);

        // Verify client exists
        if self.state_mgr.clients.get_client(op.clientid).is_none() {
            warn!("CREATE_SESSION: Client {} not found", op.clientid);
            return CreateSessionRes {
                status: Nfs4Status::StaleClientId,
                sessionid: SessionId([0; 16]),
                sequence: 0,
                flags: 0,
                fore_chan_attrs: ChannelAttrs {
                    header_pad_size: 0,
                    max_request_size: 0,
                    max_response_size: 0,
                    max_response_size_cached: 0,
                    max_operations: 0,
                    max_requests: 0,
                },
                back_chan_attrs: ChannelAttrs {
                    header_pad_size: 0,
                    max_request_size: 0,
                    max_response_size: 0,
                    max_response_size_cached: 0,
                    max_operations: 0,
                    max_requests: 0,
                },
            };
        }

        // Update client sequence
        if let Err(e) = self.state_mgr.clients.update_sequence(op.clientid) {
            warn!("CREATE_SESSION: Failed to update sequence: {}", e);
            return CreateSessionRes {
                status: Nfs4Status::SeqMisordered,
                sessionid: SessionId([0; 16]),
                sequence: 0,
                flags: 0,
                fore_chan_attrs: ChannelAttrs {
                    header_pad_size: 0,
                    max_request_size: 0,
                    max_response_size: 0,
                    max_response_size_cached: 0,
                    max_operations: 0,
                    max_requests: 0,
                },
                back_chan_attrs: ChannelAttrs {
                    header_pad_size: 0,
                    max_request_size: 0,
                    max_response_size: 0,
                    max_response_size_cached: 0,
                    max_operations: 0,
                    max_requests: 0,
                },
            };
        }

        // Negotiate session buffer sizes
        // Use the MINIMUM of what client requested and our server maximums
        // Server maximums: 1MB for requests/responses (standard for modern NFS)
        const SERVER_MAX_REQUEST: u32 = 1 * 1024 * 1024;  // 1MB
        const SERVER_MAX_RESPONSE: u32 = 1 * 1024 * 1024; // 1MB
        const SERVER_MAX_OPS: u32 = 128;
        
        let negotiated_max_request = op.fore_chan_attrs.max_request_size.min(SERVER_MAX_REQUEST).max(1024);
        let negotiated_max_response = op.fore_chan_attrs.max_response_size.min(SERVER_MAX_RESPONSE).max(1024);
        let negotiated_max_ops = op.fore_chan_attrs.max_operations.min(SERVER_MAX_OPS).max(8);
        
        info!("CREATE_SESSION: Negotiated buffers: req={}, resp={}, ops={}", 
              negotiated_max_request, negotiated_max_response, negotiated_max_ops);
        
        // Create session with negotiated sizes
        let session = self.state_mgr.sessions.create_session(
            op.clientid,
            op.sequence,
            op.flags,
            negotiated_max_request,
            negotiated_max_response,
            negotiated_max_ops,
        );

        info!("CREATE_SESSION: Session {:?} created for client {}",
              session.session_id, op.clientid);

        // Set server flags based on actual capabilities (RFC 5661 §18.36)
        // We do not support persistent reply cache or backchannel callbacks yet,
        // so advertise none to avoid client-side EINVAL during mount negotiation.
        let server_flags = 0u32;

        // If we are not offering a backchannel, RFC 5661 says csr_flags MUST NOT
        // set CREATE_SESSION4_FLAG_CONN_BACK_CHAN and the backchannel attrs should
        // be zeroed so the client knows callbacks are unavailable.
        let back_chan_attrs = ChannelAttrs {
            header_pad_size: 0,
            max_request_size: 0,
            max_response_size: 0,
            max_response_size_cached: 0,
            max_operations: 0,
            max_requests: 0,
        };
        
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
            },
            back_chan_attrs,
        }
    }

    /// Handle SEQUENCE operation
    pub fn handle_sequence(&self, op: SequenceOp) -> SequenceRes {
        debug!("SEQUENCE: sessionid={:?}, sequenceid={}, slotid={}",
               op.sessionid, op.sequenceid, op.slotid);

        // Get session
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

        // Process sequence in slot
        let is_new_request = match self.state_mgr.sessions.get_session_mut(&op.sessionid, |s| {
            s.process_sequence(op.slotid, op.sequenceid)
        }) {
            Some(Ok(is_new)) => is_new,
            Some(Err(e)) => {
                warn!("SEQUENCE: Error processing sequence: {}", e);
                return SequenceRes {
                    status: Nfs4Status::SeqMisordered,
                    sessionid: op.sessionid,
                    sequenceid: 0,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                };
            }
            None => {
                warn!("SEQUENCE: Session {:?} disappeared", op.sessionid);
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

        if !is_new_request {
            debug!("SEQUENCE: Replay detected, returning cached response");
            // TODO: Return cached response from slot
        }

        // Renew lease
        if let Err(e) = self.state_mgr.leases.renew_lease(session.client_id) {
            warn!("SEQUENCE: Failed to renew lease: {}", e);
        }

        SequenceRes {
            status: Nfs4Status::Ok,
            sessionid: op.sessionid,
            sequenceid: op.sequenceid,
            slotid: op.slotid,
            highest_slotid: session.highest_slotid,
            target_highest_slotid: 127, // We support up to 128 slots
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
    use crate::nfs::v4::state::LeaseManager;

    #[test]
    fn test_exchange_id() {
        let lease_mgr = Arc::new(LeaseManager::new());
        let state_mgr = Arc::new(StateManager::new());
        let handler = SessionOperationHandler::new(state_mgr.clone());

        let op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };

        let res = handler.handle_exchange_id(op);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.clientid, 1);
        assert_eq!(res.sequenceid, 0);
    }

    #[test]
    fn test_create_session() {
        let state_mgr = Arc::new(StateManager::new());
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // First do EXCHANGE_ID
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op);

        // Now CREATE_SESSION
        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };

        let res = handler.handle_create_session(create_op);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_ne!(res.sessionid, SessionId([0; 16]));
    }

    #[test]
    fn test_sequence() {
        let state_mgr = Arc::new(StateManager::new());
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // Setup: EXCHANGE_ID + CREATE_SESSION
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op);

        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };
        let create_res = handler.handle_create_session(create_op);

        // Now SEQUENCE
        let seq_op = SequenceOp {
            sessionid: create_res.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: false,
        };

        let res = handler.handle_sequence(seq_op);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.slotid, 0);
    }

    #[test]
    fn test_sequence_replay() {
        let state_mgr = Arc::new(StateManager::new());
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // Setup session
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op);

        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };
        let create_res = handler.handle_create_session(create_op);

        // First SEQUENCE
        let seq_op1 = SequenceOp {
            sessionid: create_res.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: false,
        };
        let res1 = handler.handle_sequence(seq_op1);
        assert_eq!(res1.status, Nfs4Status::Ok);

        // Replay same sequence (should succeed)
        let seq_op2 = SequenceOp {
            sessionid: create_res.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: false,
        };
        let res2 = handler.handle_sequence(seq_op2);
        assert_eq!(res2.status, Nfs4Status::Ok);
    }

    #[test]
    fn test_destroy_session() {
        let state_mgr = Arc::new(StateManager::new());
        let handler = SessionOperationHandler::new(state_mgr.clone());

        // Setup session
        let exchange_op = ExchangeIdOp {
            client_owner: b"test-client".to_vec(),
            verifier: 12345,
            flags: 0,
            state_protect: 0,
            client_impl_id: None,
        };
        let exchange_res = handler.handle_exchange_id(exchange_op);

        let create_op = CreateSessionOp {
            clientid: exchange_res.clientid,
            sequence: exchange_res.sequenceid,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
        };
        let create_res = handler.handle_create_session(create_op);

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
