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
use crate::nfs::v4::operations::lockops::LockManager;
use crate::nfs::v4::compound::{ChannelAttrs, CompoundContext};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Fore-channel buffer caps this server advertises in CREATE_SESSION.
///
/// PINNED AT EXACTLY 1 MiB, AND THAT IS LOAD-BEARING IN A WAY THAT COSTS
/// THROUGHPUT. Linux's `nfs4_session_set_rwsize()` derives the mount's
/// rsize/wsize by SUBTRACTING its COMPOUND overhead
/// (`nfs41_maxread_overhead` / `nfs41_maxwrite_overhead`) from whatever the
/// server advertises here. So a client that asks for `rsize=1048576` — which
/// is exactly what the pNFS CSI driver's mount string asks for, see
/// `pnfs_csi::build_pnfs_mount_opts` — cannot get it, and settles for
/// 1047672 / 1047532. Measured on a real 6.1 client (runaw, 2026-08-01);
/// the shortfalls are 904 and 1044 bytes, matching the kernel's constants
/// to the byte.
///
/// The consequence is a DOUBLED RPC COUNT for 1 MiB direct I/O: each one
/// splits into a full-size RPC plus a runt, and on writes that runt is a
/// sub-page, non-page-aligned tail.
///
/// Raising these is the fix, but it resizes buffers for EVERY NFSv4.1
/// client and not just the pNFS path, so it is a deliberate change with its
/// own gate — not a constant to bump casually. They are `pub(crate)` so the
/// relationship can be asserted in a test instead of rediscovered from a
/// `mount` output; see
/// `pnfs_csi::mount_opts_tests::the_session_cap_holds_the_client_below_the_rsize_we_ask_for`.
pub(crate) const SERVER_MAX_REQUEST: u32 = 1024 * 1024;
pub(crate) const SERVER_MAX_RESPONSE: u32 = 1024 * 1024;

/// Ceiling on `ca_maxresponsesize_cached` — the bytes a client can DEMAND
/// the server pin per slot, for the life of the slot, by sending
/// `sa_cachethis`. This is the one channel attribute that is a standing
/// memory grant rather than a per-request buffer: slots × this value is
/// retained at steady state per session, and sessions are uncapped. Clamped
/// only to `SERVER_MAX_RESPONSE` (the old behavior), one CREATE_SESSION
/// could demand 128 slots × 1 MiB ≈ 128 MiB. Every reply a client
/// legitimately asks to cache is a small non-idempotent result (OPEN,
/// CLOSE, LOCK, RENAME...); knfsd caps its equivalent near 2 KiB and every
/// Linux client in the field mounts it, so 16 KiB is generous.
pub(crate) const SERVER_MAX_RESPONSE_CACHED: u32 = 16 * 1024;

/// Linux NFSv4.1 COMPOUND overheads subtracted from the advertised
/// fore-channel caps by `nfs4_session_set_rwsize()`. Recorded only so the
/// test can state the arithmetic; the kernel owns these values, so they are
/// test-only by construction — nothing in the server should ever branch on
/// a peer's internal constant.
#[cfg(test)]
pub(crate) const LINUX_NFS41_MAXREAD_OVERHEAD: u32 = 904;
#[cfg(test)]
pub(crate) const LINUX_NFS41_MAXWRITE_OVERHEAD: u32 = 1044;

// CREATE_SESSION flags (RFC 5661 §18.36)
// Defined for protocol completeness; not all flags are implemented yet
#[allow(dead_code)]
const CREATE_SESSION4_FLAG_PERSIST: u32 = 0x0000_0001;
/// Set in `csa_flags` by a client offering its connection as the session's
/// back channel, and echoed in `csr_flags` by a server that accepts it
/// (RFC 8881 §18.36.3). Both directions matter — see C9 in
/// `handle_create_session`.
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

    /// Credentials the client will accept on callback CALLs
    /// (`csa_sec_parms<>`). The server MUST pick one of these — see C8.
    pub cb_sec: Vec<crate::nfs::v4::compound::CallbackSecParms>,
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
    /// C9: `true` iff this reply echoed `CREATE_SESSION4_FLAG_CONN_BACK_CHAN`,
    /// i.e. we promised the client that callbacks will arrive on this
    /// connection. The dispatcher registers the back-channel writer off
    /// this field so the promise and the registration are one decision,
    /// not two predicates that can drift.
    pub back_chan_bound: bool,
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
        // A failed CREATE_SESSION binds nothing.
        back_chan_bound: false,
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
    /// The lock table, so the case-5 deferred cleanup can actually
    /// finish. Without it this handler could tear down a client's
    /// sessions, stateids, delegations and record while leaving its
    /// byte-range locks behind — and nothing could ever reap them,
    /// because the only reaper iterates EXPIRED LEASES and
    /// `remove_client` has already deleted the lease.
    lock_mgr: Option<Arc<LockManager>>,
}

impl SessionOperationHandler {
    /// Create a new session operation handler
    pub fn new(state_mgr: Arc<StateManager>) -> Self {
        Self { state_mgr, lock_mgr: None }
    }

    /// Give the handler the lock table. The dispatcher already owns one;
    /// this is what lets `handle_create_session` complete the case-5
    /// cascade instead of leaking the victim's locks.
    pub fn with_lock_manager(mut self, lock_mgr: Arc<LockManager>) -> Self {
        self.lock_mgr = Some(lock_mgr);
        self
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

        // SP4_SSV (spa_how=2) state protection needs SSV GSS machinery we
        // don't implement. RFC 8881 §2.10.8.3: a server that supports none
        // of the client's SSV algorithms returns ENCR_ALG_UNSUPP — the
        // honest answer here (previously the args didn't even decode and
        // clients saw BADXDR).
        if op.state_protect == 2 {
            warn!("EXCHANGE_ID: SP4_SSV requested — unsupported → ENCR_ALG_UNSUPP");
            return exchange_id_err(Nfs4Status::EncrAlgUnsupp);
        }

        // RFC 8881 §18.35.5 client-record state machine. Principal is the
        // RPC-level identity of the caller; an empty Vec for AUTH_NONE.
        // Client-table quota — checked only where the mint would GROW
        // the table: an unknown owner on the non-update path. A known
        // owner's EXCHANGE_ID renews/replaces/steals in place and MUST
        // pass even at the cap (refusing it would break a live mount),
        // and the UPD path never creates. DELAY, not a terminal error:
        // the courtesy sweep reclaims records from clients that stopped
        // renewing, so a legitimate newcomer succeeds on retry.
        if op.flags & exchgid_flags::UPD_CONFIRMED_REC_A == 0
            && !self.state_mgr.clients.owner_known(&op.client_owner)
        {
            let count = self.state_mgr.clients.client_count();
            let max = self.state_mgr.quotas.max_clients;
            if count >= max {
                warn!(
                    "EXCHANGE_ID: client table at quota ({count}/{max}) — refusing a NEW \
                     client with DELAY (FLINT_NFS_MAX_CLIENTS)"
                );
                return exchange_id_err(Nfs4Status::Delay);
            }
        }

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

        // A capability flag in the REPLY is a server PROMISE, not an
        // echo (RFC 8881 §18.35.3): SUPP_MOVED_REFER/MIGR say this
        // server may answer NFS4ERR_MOVED and serve fs_locations
        // referrals; BIND_PRINC_STATEID says stateids are
        // principal-bound and enforced. This server implements none of
        // those — fs_locations is a constant, nothing ever returns
        // MOVED, and stateids are not principal-checked — and the old
        // blanket echo advertised all three to any client that asked,
        // arming never-tested client recovery paths against a server
        // that cannot serve them. The reply is clamped to what the
        // server actually does; a client's flags express ITS support,
        // and declining them is the ordinary negotiated outcome.

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
                    // C9: a replay must reproduce the ORIGINAL reply, so
                    // the back-channel attrs come from the cache too.
                    // Returning `default()` here while `cached.flags`
                    // carries CONN_BACK_CHAN would hand the client a
                    // 1 MB back channel it never offered — EINVAL, and
                    // the mount fails on the retry path only. That is
                    // precisely the kind of bug that hides for months.
                    back_chan_attrs: ChannelAttrs {
                        header_pad_size: 0,
                        max_request_size: cached.back_max_request_size,
                        max_response_size: cached.back_max_response_size,
                        max_response_size_cached: cached.back_max_response_size_cached,
                        max_operations: cached.back_max_operations,
                        max_requests: cached.back_max_requests,
                        rdma_ird: Vec::new(),
                    },
                    // The connection this replay arrived on may differ
                    // from the original's; if the cached reply promised
                    // a back channel, bind this one too.
                    back_chan_bound: cached.flags & CREATE_SESSION4_FLAG_CONN_BACK_CHAN != 0,
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
            // LOCKS BEFORE THE CLIENT RECORD. `remove_client` drops the
            // lease, and the only production caller of
            // `remove_client_locks` iterates `get_expired_clients()` —
            // which reads the lease map. Anything still holding a lock
            // after `remove_client` can therefore never be collected by
            // anything, survives a restart (locks are persisted and
            // re-seeded at startup), and denies its range to every other
            // client forever, naming a clientid the server can no longer
            // resolve. The holder cannot release it either: its lock
            // stateid went with `remove_client_stateids` above.
            if let Some(locks) = &self.lock_mgr {
                locks.remove_client_locks(old_id);
            } else {
                warn!(
                    "CREATE_SESSION: case-5 cleanup of client {} has NO lock manager — \
                     any locks it held are now unreachable",
                    old_id,
                );
            }
            self.state_mgr.clients.remove_client(old_id);
        }

        // Per-client session quota. Each session owns a slot table whose
        // reply cache is a standing memory grant (bounded per session,
        // but multiplied here without limit before this check — Linux
        // uses ONE session per client). Checked after replay/seq
        // handling so a §18.36.4 replay of an earlier success is never
        // refused. DELAY: DESTROY_SESSION and the courtesy sweep free
        // slots, so a client cycling sessions succeeds on retry.
        {
            let held = self.state_mgr.sessions.session_count_for_client(op.clientid);
            let max = self.state_mgr.quotas.max_sessions_per_client;
            if held >= max {
                warn!(
                    "CREATE_SESSION: client {} at session quota ({held}/{max}) — DELAY \
                     (FLINT_NFS_MAX_SESSIONS_PER_CLIENT)",
                    op.clientid
                );
                return create_session_err(Nfs4Status::Delay);
            }
        }

        // Negotiate session buffer sizes. Take the *minimum* of client-requested
        // and server-maximum. We've already rejected requests below the server
        // minimum above, so the negotiated value is always >= MIN_*.
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
            .min(negotiated_max_response)
            .min(SERVER_MAX_RESPONSE_CACHED);
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
            // C8: pick a credential the CLIENT said it would accept.
            // AUTH_SYS is preferred when offered — it is what Linux
            // advertises and what it will actually honour; AUTH_NONE is
            // used only when explicitly offered or when nothing was.
            // RPCSEC_GSS is recognised but not emittable, so it is
            // skipped rather than selected.
            op.cb_sec
                .iter()
                .find(|p| matches!(p, crate::nfs::v4::compound::CallbackSecParms::Sys { .. }))
                .or_else(|| {
                    op.cb_sec
                        .iter()
                        .find(|p| matches!(p, crate::nfs::v4::compound::CallbackSecParms::None))
                })
                .cloned(),
            // The COMPOUND's own minor version. Every callback on this
            // session must carry it: Linux looks its client up by
            // (address, sessionid, MINOR VERSION), so a 4.2 mount sent
            // a minorversion=1 callback answers BADSESSION and never
            // reaches the callback op at all.
            ctx.minor_version,
        );

        // C8 evidence, logged rather than assumed: what the client will
        // actually ACCEPT on a callback. The pre-C8 code hardcoded
        // AUTH_NONE on the belief this "matches Linux client behaviour";
        // that belief was never measured, and a Linux 6.1 client denied
        // the RPC. Print the offer so the next person does not have to
        // guess either.
        info!(
            "CREATE_SESSION: client offered {} callback credential(s): {:?} → selected {:?}",
            op.cb_sec.len(),
            op.cb_sec,
            session.cb_cred,
        );

        info!("CREATE_SESSION: Session {:?} created for client {} with {}KB buffers",
              session.session_id, op.clientid, negotiated_max_request / 1024);

        // C9: accept the back channel, and SAY SO.
        //
        // RFC 8881 §18.36.3: when the client sets CONN_BACK_CHAN in
        // csa_flags and the server agrees to use the connection as the
        // session's back channel, the server MUST set CONN_BACK_CHAN in
        // csr_flags. This is not decorative. A Linux client records the
        // connection as a back channel only if the flag comes back
        // (`nfs4_session_set_rwsize` / `nfs41_set_server_capabilities`
        // path); without it, a callback arriving on that connection is
        // answered with BADSESSION at CB_SEQUENCE — which is exactly
        // what the F65 live drill on runas caught, one layer below the
        // AUTH_NONE denial that C8 fixed. Linux sends *zero*
        // BIND_CONN_TO_SESSION on a fresh v4.1 mount, so this flag is
        // the only signal it ever gets.
        //
        // We echo it only when it is TRUE: the client asked for it AND
        // this COMPOUND arrived on a connection we can actually write
        // callbacks back down (`ctx.back_channel`). A unit-test or
        // in-process dispatch has no writer, so it gets no flag. The
        // dispatcher registers the writer off this same decision
        // (`back_chan_bound`) rather than recomputing the predicate, so
        // "we said yes" and "we can send" cannot drift apart.
        let back_chan_bound = op.flags & CREATE_SESSION4_FLAG_CONN_BACK_CHAN != 0
            && ctx.back_channel.is_some();

        let server_flags = if back_chan_bound {
            CREATE_SESSION4_FLAG_CONN_BACK_CHAN
        } else {
            0u32
        };

        // Echo the client's offered back-channel attrs VERBATIM.
        //
        // This is not laziness — it is the only choice that is
        // unconditionally safe against Linux's
        // `nfs4_verify_back_channel_attrs`, which fails the mount with
        // EINVAL if the returned max_rqst_sz is *greater*, max_resp_sz
        // *lesser*, or max_resp_sz_cached / max_ops / max_reqs
        // *greater* than what it sent. Equality satisfies all five at
        // once. Note what this means for the old code: had it set the
        // flag while still returning `ChannelAttrs::default()` (1 MB
        // request size vs Linux's PAGE_SIZE offer), the mount itself
        // would have failed. The flag and the attrs had to change
        // together.
        let back_chan_attrs = if back_chan_bound {
            op.back_chan_attrs.clone()
        } else {
            ChannelAttrs::default()
        };

        // A CB_LAYOUTRECALL COMPOUND is CB_SEQUENCE + CB_LAYOUTRECALL —
        // two ops. Linux offers ca_maxoperations=2 exactly, so we sit on
        // the limit by design. Anything less and no recall we send can
        // ever be legal; say so at bind time rather than discovering it
        // as an opaque callback failure under a truncate.
        if back_chan_bound && back_chan_attrs.max_operations < 2 {
            warn!(
                "CREATE_SESSION: back channel accepted but ca_maxoperations={} < 2 — \
                 CB_SEQUENCE+CB_LAYOUTRECALL will not fit; recalls to this client will fail",
                back_chan_attrs.max_operations,
            );
        }

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
                // A replay must reproduce the ORIGINAL reply byte for
                // byte (§18.36.4) — store what the reply actually said,
                // not constants that merely happened to match it.
                fore_max_request_size: session.fore_chan_maxrequestsize,
                fore_max_response_size: session.fore_chan_maxresponsesize,
                fore_max_response_size_cached: session.fore_chan_maxresponsesize_cached,
                fore_max_operations: session.fore_chan_maxops,
                fore_max_requests: session.fore_chan_maxrequests,
                back_max_request_size: back_chan_attrs.max_request_size,
                back_max_response_size: back_chan_attrs.max_response_size,
                back_max_response_size_cached: back_chan_attrs.max_response_size_cached,
                back_max_operations: back_chan_attrs.max_operations,
                back_max_requests: back_chan_attrs.max_requests,
            },
        );

        CreateSessionRes {
            status: Nfs4Status::Ok,
            back_chan_bound,
            sessionid: session.session_id,
            sequence: session.sequence,
            flags: server_flags,
            fore_chan_attrs: ChannelAttrs {
                header_pad_size: 0,
                max_request_size: session.fore_chan_maxrequestsize,
                max_response_size: session.fore_chan_maxresponsesize,
                // The values the session actually ENFORCES, not constants:
                // hardcoding 64 KiB / 128 here advertised a cached window
                // and a slot range the session had not necessarily granted
                // — SEQUENCE answers BADSLOT from `fore_chan_maxrequests`,
                // and the Linux client refuses fore-channel attrs larger
                // than its own offer.
                max_response_size_cached: session.fore_chan_maxresponsesize_cached,
                max_operations: session.fore_chan_maxops,
                max_requests: session.fore_chan_maxrequests,
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
                    highest_slotid: session.fore_chan_maxrequests.saturating_sub(1),
                    target_highest_slotid: session.fore_chan_maxrequests.saturating_sub(1),
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
                // RPC layer has them — ONLY when the client asked
                // (sa_cachethis). "We always cache" was the largest
                // per-session memory term in the server: a session
                // streaming 1 MiB READ replies retained slots × 1 MiB
                // (up to ~128 MiB) at steady state, pinning bytes for
                // replies whose cachethis=false explicitly said the
                // client will never replay them expecting content. A
                // replay of an uncached slot is already answered by the
                // Replay{cached: None} arm above — RETRY_UNCACHED_REP,
                // RFC 8881 §2.10.6.1.3's exact contract for a request
                // sent with cachethis=false.
                if op.cache_this {
                    ctx.cache_slot = Some((op.sessionid, op.slotid));
                }
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
            // RFC 8881 §18.46.3: sr_highest_slotid is the highest slot id the
            // server will currently ACCEPT (ca_maxrequests - 1), not the
            // highest seen so far. Echoing the highest-seen value (0 on a
            // fresh session) makes the Linux client shrink its slot table to
            // ONE slot — and then self-deadlock after a server restart, when
            // update_open_stateid holds the OPEN's slot while issuing a
            // nested synchronous TEST_STATEID that waits for that same slot.
            highest_slotid: session.fore_chan_maxrequests.saturating_sub(1),
            target_highest_slotid: session.fore_chan_maxrequests.saturating_sub(1),
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
    // ---- C9: back-channel acceptance must be ANNOUNCED ----------------
    //
    // These tests exist because the pre-C9 server registered a
    // back-channel writer internally and never set CONN_BACK_CHAN in
    // csr_flags. Everything server-side looked healthy — the session was
    // in the registry, the recall was built, the RPC went out — and every
    // CB_LAYOUTRECALL died at the client's CB_SEQUENCE with BADSESSION,
    // because the client had never been told that connection was a back
    // channel. Linux sends no BIND_CONN_TO_SESSION on a v4.1 mount, so
    // the flag is the entire handshake.
    //
    // Following the C8 lesson: assert what RFC 8881 §18.36.3 and the
    // Linux client REQUIRE, never what flint currently emits. A test that
    // restates the implementation cannot fail when the implementation is
    // the bug — the old `assert_eq!(flavor, AuthFlavor::Null)` proved it.

    /// A verbatim transcription of the Linux client's
    /// `nfs4_verify_back_channel_attrs` (fs/nfs/nfs4proc.c). If this
    /// returns Err, a real Linux mount fails with EINVAL.
    ///
    /// Note the asymmetry — max_resp_sz must not come back SMALLER,
    /// everything else must not come back LARGER. Guessing at "reasonable"
    /// server values trips one side or the other; echoing the offer is
    /// the only response that satisfies all five without knowing which
    /// direction each check runs.
    fn linux_verify_back_channel(
        sent: &super::ChannelAttrs,
        rcvd: &super::ChannelAttrs,
        flags: u32,
    ) -> Result<(), String> {
        if flags & super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN == 0 {
            return Ok(()); // client skips the checks entirely
        }
        if rcvd.max_request_size > sent.max_request_size {
            return Err(format!(
                "max_rqst_sz {} > offered {}",
                rcvd.max_request_size, sent.max_request_size
            ));
        }
        if rcvd.max_response_size < sent.max_response_size {
            return Err(format!(
                "max_resp_sz {} < offered {}",
                rcvd.max_response_size, sent.max_response_size
            ));
        }
        if rcvd.max_response_size_cached > sent.max_response_size_cached {
            return Err(format!(
                "max_resp_sz_cached {} > offered {}",
                rcvd.max_response_size_cached, sent.max_response_size_cached
            ));
        }
        if rcvd.max_operations > sent.max_operations {
            return Err(format!(
                "max_ops {} > offered {}",
                rcvd.max_operations, sent.max_operations
            ));
        }
        if rcvd.max_requests > sent.max_requests {
            return Err(format!(
                "max_reqs {} > offered {}",
                rcvd.max_requests, sent.max_requests
            ));
        }
        Ok(())
    }

    /// The back-channel attrs a Linux 6.1 client actually offers:
    /// PAGE_SIZE request/response, zero cached, 2 ops (exactly
    /// CB_SEQUENCE + one callback op), 1 slot.
    fn linux_bc_offer() -> super::ChannelAttrs {
        super::ChannelAttrs {
            header_pad_size: 0,
            max_request_size: 4096,
            max_response_size: 4096,
            max_response_size_cached: 0,
            max_operations: 2,
            max_requests: 1,
            rdma_ird: Vec::new(),
        }
    }

    /// Bring up a loopback TCP pair and hand back a real
    /// `BackChannelWriter` for the server side, plus the halves that must
    /// stay alive for the connection to remain open.
    async fn loopback_writer() -> (
        std::sync::Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
        tokio::net::TcpStream,
        tokio::net::tcp::OwnedReadHalf,
    ) {
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (connect, accept) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let (server_stream, _) = accept.unwrap();
        let client_stream = connect.unwrap();
        let (server_read, server_write) = server_stream.into_split();
        let writer = crate::nfs::v4::back_channel::BackChannelWriter::new(
            tokio::io::BufWriter::with_capacity(4096, server_write),
        );
        (writer, client_stream, server_read)
    }

    /// Drive EXCHANGE_ID + CREATE_SESSION with the given csa_flags and
    /// back-channel offer, optionally on a connection that has a real
    /// back-channel writer.
    async fn create_session_with(
        csa_flags: u32,
        bc_offer: super::ChannelAttrs,
        with_writer: bool,
    ) -> (super::CreateSessionRes, super::ChannelAttrs) {
        use super::*;
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr);

        let ex = handler.handle_exchange_id(
            ExchangeIdOp {
                client_owner: b"c9-client".to_vec(),
                verifier: 1,
                flags: 0,
                state_protect: 0,
                client_impl_id: None,
            },
            &CompoundContext::new(1),
        );

        let mut ctx = CompoundContext::new(1);
        // Keep the connection halves alive for the duration of the call.
        let _keepalive = if with_writer {
            let (w, client_stream, server_read) = loopback_writer().await;
            ctx.back_channel = Some(w);
            Some((client_stream, server_read))
        } else {
            None
        };

        let res = handler.handle_create_session(
            CreateSessionOp {
                clientid: ex.clientid,
                sequence: ex.sequenceid,
                flags: csa_flags,
                fore_chan_attrs: ChannelAttrs::default(),
                back_chan_attrs: bc_offer.clone(),
                cb_program: 0x4000_0000,
                cb_sec: Vec::new(),
            },
            &ctx,
        );
        (res, bc_offer)
    }

    /// THE C9 REGRESSION. A client that offers its connection as a back
    /// channel, to a server that can write callbacks down it, must be
    /// TOLD the offer was accepted. Without this bit the client answers
    /// every CB_LAYOUTRECALL with BADSESSION and a truncate can never
    /// fence a stale layout holder.
    #[tokio::test]
    async fn accepted_back_channel_is_echoed_in_csr_flags() {
        let (res, _) = create_session_with(
            super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
            linux_bc_offer(),
            true,
        )
        .await;
        assert_eq!(res.status, super::Nfs4Status::Ok);
        assert_ne!(
            res.flags & super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
            0,
            "server accepted the back channel but did not echo CONN_BACK_CHAN — \
             the client will reject callbacks with BADSESSION (C9)",
        );
        assert!(
            res.back_chan_bound,
            "csr_flags promised a back channel, so the writer must be registered",
        );
    }

    /// The echoed attrs must survive the client's own validation. This is
    /// the test that catches the tempting half-fix: set the flag, leave
    /// `csr_back_chan_attrs` at `ChannelAttrs::default()` (1 MB). That
    /// combination doesn't just break recalls — `max_rqst_sz 1048576 >
    /// offered 4096` fails the check and the MOUNT itself returns EINVAL.
    #[tokio::test]
    async fn echoed_back_chan_attrs_pass_the_linux_client_check() {
        let (res, offered) = create_session_with(
            super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
            linux_bc_offer(),
            true,
        )
        .await;
        linux_verify_back_channel(&offered, &res.back_chan_attrs, res.flags)
            .expect("Linux client would reject csr_back_chan_attrs");

        // And prove the helper has teeth: the pre-C9 default would fail.
        assert!(
            linux_verify_back_channel(
                &offered,
                &super::ChannelAttrs::default(),
                super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
            )
            .is_err(),
            "the verifier must reject a 1 MB back channel against a 4 KiB offer, \
             otherwise it proves nothing",
        );
    }

    /// A back channel offered over a range of client attrs — including
    /// odd but legal ones — is always echoed compatibly. Guards against a
    /// future "clamp to server maximums" change quietly reintroducing the
    /// EINVAL, which would only ever show up as a mount failure on real
    /// hardware.
    #[tokio::test]
    async fn any_offer_is_echoed_compatibly() {
        for offer in [
            linux_bc_offer(),
            super::ChannelAttrs {
                header_pad_size: 0,
                max_request_size: 256,
                max_response_size: 256,
                max_response_size_cached: 0,
                max_operations: 2,
                max_requests: 1,
                rdma_ird: Vec::new(),
            },
            super::ChannelAttrs {
                header_pad_size: 0,
                max_request_size: 1 << 20,
                max_response_size: 1 << 20,
                max_response_size_cached: 4096,
                max_operations: 16,
                max_requests: 4,
                rdma_ird: Vec::new(),
            },
        ] {
            let (res, offered) = create_session_with(
                super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
                offer,
                true,
            )
            .await;
            assert_eq!(res.status, super::Nfs4Status::Ok);
            linux_verify_back_channel(&offered, &res.back_chan_attrs, res.flags)
                .unwrap_or_else(|e| panic!("offer {:?} echoed incompatibly: {e}", offered));
        }
    }

    /// Never promise what we cannot deliver. With no writer for this
    /// connection there is nothing to send callbacks down, so the flag
    /// must stay clear even though the client asked — a client that
    /// believes it has a back channel and never gets a recall is strictly
    /// worse off than one that knows it has none.
    #[tokio::test]
    async fn no_writer_means_no_promise() {
        let (res, _) = create_session_with(
            super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
            linux_bc_offer(),
            false,
        )
        .await;
        assert_eq!(res.status, super::Nfs4Status::Ok);
        assert_eq!(
            res.flags & super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
            0,
            "echoed CONN_BACK_CHAN with no writer to deliver callbacks on",
        );
        assert!(!res.back_chan_bound);
    }

    /// A client that never asked must never be handed a back channel;
    /// RFC 8881 §18.36.3 makes csr_flags a response to csa_flags, not an
    /// independent server assertion.
    #[tokio::test]
    async fn unrequested_back_channel_is_not_offered() {
        let (res, _) = create_session_with(0, linux_bc_offer(), true).await;
        assert_eq!(res.status, super::Nfs4Status::Ok);
        assert_eq!(res.flags & super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN, 0);
        assert!(!res.back_chan_bound);
    }

    /// `back_chan_bound` is the single source of truth the dispatcher
    /// binds on. If it could ever be true while the flag is clear (or
    /// vice versa) the server would be back to registering a channel it
    /// never announced — the exact C9 shape.
    #[tokio::test]
    async fn bound_flag_and_csr_flag_cannot_disagree() {
        for (csa, writer) in [
            (super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN, true),
            (super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN, false),
            (0, true),
            (0, false),
        ] {
            let (res, _) = create_session_with(csa, linux_bc_offer(), writer).await;
            assert_eq!(
                res.back_chan_bound,
                res.flags & super::CREATE_SESSION4_FLAG_CONN_BACK_CHAN != 0,
                "back_chan_bound={} disagrees with csr_flags=0x{:x} (csa=0x{:x}, writer={})",
                res.back_chan_bound,
                res.flags,
                csa,
                writer,
            );
        }
    }

    /// A CREATE_SESSION replay must reproduce the original reply — RFC
    /// 8881 §15.1.10.4. Both halves matter: replaying the flag without
    /// the cached attrs would give the client a back channel it never
    /// offered, and only on the retry path, where nobody looks.
    #[tokio::test]
    async fn replay_reproduces_flags_and_back_chan_attrs() {
        use super::*;
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr);
        let ex = handler.handle_exchange_id(
            ExchangeIdOp {
                client_owner: b"c9-replay".to_vec(),
                verifier: 1,
                flags: 0,
                state_protect: 0,
                client_impl_id: None,
            },
            &CompoundContext::new(1),
        );

        let (w, _client_stream, _server_read) = loopback_writer().await;
        let mut ctx = CompoundContext::new(1);
        ctx.back_channel = Some(w);

        let offer = linux_bc_offer();
        let mk = || CreateSessionOp {
            clientid: ex.clientid,
            sequence: ex.sequenceid,
            flags: CREATE_SESSION4_FLAG_CONN_BACK_CHAN,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: offer.clone(),
            cb_program: 0x4000_0000,
            cb_sec: Vec::new(),
        };

        let first = handler.handle_create_session(mk(), &ctx);
        assert_eq!(first.status, Nfs4Status::Ok);
        assert_ne!(first.flags & CREATE_SESSION4_FLAG_CONN_BACK_CHAN, 0);

        // Same csa_sequence → replay.
        let replay = handler.handle_create_session(mk(), &ctx);
        assert_eq!(replay.status, Nfs4Status::Ok);
        assert_eq!(replay.sessionid, first.sessionid, "replay minted a new session");
        assert_eq!(replay.flags, first.flags, "replay lost csr_flags");
        assert_eq!(
            replay.back_chan_attrs.max_request_size,
            first.back_chan_attrs.max_request_size,
        );
        assert_eq!(
            replay.back_chan_attrs.max_response_size,
            first.back_chan_attrs.max_response_size,
        );
        assert_eq!(
            replay.back_chan_attrs.max_operations,
            first.back_chan_attrs.max_operations,
        );
        assert_eq!(
            replay.back_chan_attrs.max_requests,
            first.back_chan_attrs.max_requests,
        );
        assert!(replay.back_chan_bound, "replay dropped the back-channel binding");
        linux_verify_back_channel(&offer, &replay.back_chan_attrs, replay.flags)
            .expect("Linux client would reject the REPLAYED csr_back_chan_attrs");
    }

    /// A CB_LAYOUTRECALL COMPOUND is CB_SEQUENCE + CB_LAYOUTRECALL. If a
    /// client's ca_maxoperations were below 2 we could never send one;
    /// Linux offers exactly 2, so this documents that we sit on the limit
    /// deliberately rather than by luck.
    #[test]
    fn linux_back_channel_offer_just_fits_a_layout_recall() {
        const OPS_IN_A_RECALL: u32 = 2;
        assert!(
            linux_bc_offer().max_operations >= OPS_IN_A_RECALL,
            "a recall does not fit in the back channel Linux offers",
        );
    }

    /// C8 selection. The server MUST pick a credential the client
    /// offered (RFC 8881 §18.36.3); AUTH_SYS is preferred because it is
    /// what Linux advertises, and RPCSEC_GSS must never be selected —
    /// flint cannot emit it, so choosing it would guarantee a DENIED.
    #[test]
    fn selects_a_credential_the_client_actually_offered() {
        use crate::nfs::v4::compound::CallbackSecParms as P;
        let sys = P::Sys {
            stamp: 1,
            machinename: "host".into(),
            uid: 0,
            gid: 0,
            gids: vec![],
        };
        let pick = |offer: Vec<P>| -> Option<P> {
            offer
                .iter()
                .find(|p| matches!(p, P::Sys { .. }))
                .or_else(|| offer.iter().find(|p| matches!(p, P::None)))
                .cloned()
        };

        // AUTH_SYS wins when offered, whatever the order.
        assert_eq!(pick(vec![sys.clone()]), Some(sys.clone()));
        assert_eq!(pick(vec![P::None, sys.clone()]), Some(sys.clone()));
        assert_eq!(pick(vec![P::Gss, sys.clone()]), Some(sys.clone()));
        // AUTH_NONE only when it is what was offered.
        assert_eq!(pick(vec![P::None]), Some(P::None));
        // Nothing usable: GSS alone, or an empty offer, must NOT select
        // GSS — degrade to AUTH_NONE rather than send what we cannot build.
        assert_eq!(pick(vec![P::Gss]), None);
        assert_eq!(pick(vec![]), None);
    }

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
            cb_sec: Vec::new(),
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
            cb_sec: Vec::new(),
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
        // cachethis=false → NO cache hint. The reply bytes must not be
        // retained: pinning them regardless (the old "cachethis is a
        // hint — we always cache") held slots × 1 MiB per session at
        // steady state from ordinary READ traffic, ~128 MiB against a
        // 128Mi pod request. The client that sets false has declared it
        // will never replay this request expecting content.
        assert_eq!(
            ctx.cache_slot, None,
            "a cachethis=false reply must not be pinned in the slot cache"
        );
        assert!(ctx.replay_reply.is_none());

        // cachethis=true → the hint is recorded for the RPC layer.
        let mut ctx_cached = CompoundContext::new(1);
        let res2 = handler.handle_sequence(
            SequenceOp {
                sessionid: create_res.sessionid,
                sequenceid: 2,
                slotid: 0,
                highest_slotid: 0,
                cache_this: true,
            },
            &mut ctx_cached,
        );
        assert_eq!(res2.status, Nfs4Status::Ok);
        assert_eq!(ctx_cached.cache_slot, Some((create_res.sessionid, 0)));
        // sr_highest_slotid must advertise the full negotiated slot range
        // (ca_maxrequests - 1), not the highest slot used so far. A Linux
        // client shrinks its slot table to sr_highest_slotid + 1, and a
        // one-slot table self-deadlocks post-restart when the OPEN path
        // issues a nested TEST_STATEID while still holding its own slot.
        assert_eq!(res.highest_slotid, 127);
        assert_eq!(res.target_highest_slotid, 127);
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
            cb_sec: Vec::new(),
        };
        let create_res = handler.handle_create_session(create_op, &CompoundContext::new(1));

        // First SEQUENCE
        let seq_op1 = SequenceOp {
            sessionid: create_res.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: true,
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
            cache_this: true,
        };
        let mut ctx2 = CompoundContext::new(1);
        let res2 = handler.handle_sequence(seq_op2, &mut ctx2);
        assert_eq!(res2.status, Nfs4Status::Ok);
        assert_eq!(
            ctx2.replay_reply.as_ref().map(|b| b.as_ref()),
            Some(&[0xDE, 0xAD, 0xBE, 0xEFu8][..])
        );
    }

    /// RFC 8881 §2.10.6.1.3: the replay of a request sent with
    /// cachethis=false is answered NFS4ERR_RETRY_UNCACHED_REP — never a
    /// re-execution, never stale bytes. With the hint honored this is
    /// the STANDING replay path for most Linux traffic, not just the
    /// resend-before-reply-cached race it was written for.
    #[test]
    fn uncached_replay_gets_retry_uncached_rep() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr.clone());
        let ex = handler.handle_exchange_id(
            ExchangeIdOp {
                client_owner: b"uncached-client".to_vec(),
                verifier: 1,
                flags: 0,
                state_protect: 0,
                client_impl_id: None,
            },
            &CompoundContext::new(1),
        );
        let cs = handler.handle_create_session(
            CreateSessionOp {
                clientid: ex.clientid,
                sequence: ex.sequenceid,
                flags: 0,
                fore_chan_attrs: ChannelAttrs::default(),
                back_chan_attrs: ChannelAttrs::default(),
                cb_program: 0,
                cb_sec: Vec::new(),
            },
            &CompoundContext::new(1),
        );

        let mut ctx = CompoundContext::new(1);
        let seq = SequenceOp {
            sessionid: cs.sessionid,
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cache_this: false,
        };
        assert_eq!(handler.handle_sequence(seq, &mut ctx).status, Nfs4Status::Ok);
        // Nothing was cached (no hint, no RPC-layer store). Replay:
        let mut ctx2 = CompoundContext::new(1);
        let replay = handler.handle_sequence(
            SequenceOp {
                sessionid: cs.sessionid,
                sequenceid: 1,
                slotid: 0,
                highest_slotid: 0,
                cache_this: false,
            },
            &mut ctx2,
        );
        assert_eq!(replay.status, Nfs4Status::RetryUncachedRep);
        assert!(ctx2.replay_reply.is_none());
    }

    /// The cached-response window is the one channel attribute that is a
    /// standing MEMORY GRANT (slots × window, per session, for the
    /// session's life), so the server clamps it to
    /// SERVER_MAX_RESPONSE_CACHED — and both the genuine reply and a
    /// §18.36.4 replay must state the value the session actually
    /// enforces. The old reply hardcoded 64 KiB / 128 slots regardless
    /// of negotiation.
    #[tokio::test]
    async fn create_session_clamps_the_cached_window_and_replays_it_identically() {
        use super::*;
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr);
        let ex = handler.handle_exchange_id(
            ExchangeIdOp {
                client_owner: b"clamp-client".to_vec(),
                verifier: 1,
                flags: 0,
                state_protect: 0,
                client_impl_id: None,
            },
            &CompoundContext::new(1),
        );
        let greedy_offer = ChannelAttrs {
            header_pad_size: 0,
            max_request_size: SERVER_MAX_REQUEST,
            max_response_size: SERVER_MAX_RESPONSE,
            // The abuse shape: demand the whole response size be
            // cacheable, on a reduced slot table.
            max_response_size_cached: SERVER_MAX_RESPONSE,
            max_operations: 8,
            max_requests: 64,
            rdma_ird: Vec::new(),
        };
        let mk_op = || CreateSessionOp {
            clientid: ex.clientid,
            sequence: ex.sequenceid,
            flags: 0,
            fore_chan_attrs: greedy_offer.clone(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
            cb_sec: Vec::new(),
        };
        let res = handler.handle_create_session(mk_op(), &CompoundContext::new(1));
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(
            res.fore_chan_attrs.max_response_size_cached, SERVER_MAX_RESPONSE_CACHED,
            "a 1 MiB cached-window demand must be clamped — 128 slots × 1 MiB is a \
             ~128 MiB standing grant from one CREATE_SESSION"
        );
        assert_eq!(
            res.fore_chan_attrs.max_requests, 64,
            "the reply must advertise the slot range the session enforces (BADSLOT \
             comes from the negotiated value, not from the hardcoded 128)"
        );

        // The replay (same csa_sequence) must reproduce the reply.
        let replay = handler.handle_create_session(mk_op(), &CompoundContext::new(1));
        assert_eq!(replay.status, Nfs4Status::Ok);
        assert_eq!(
            replay.fore_chan_attrs.max_response_size_cached,
            res.fore_chan_attrs.max_response_size_cached
        );
        assert_eq!(replay.fore_chan_attrs.max_requests, res.fore_chan_attrs.max_requests);
    }

    /// B6, client-table quota: a NEW co_ownerid is refused with DELAY
    /// at the cap; a KNOWN owner must still renew (refusing it breaks a
    /// live mount); freed capacity re-admits — the DELAY is honest.
    #[test]
    fn exchange_id_quota_refuses_new_clients_but_renews_known_ones() {
        use crate::nfs::v4::state::StateQuotas;
        let q = StateQuotas {
            max_clients: 2,
            max_sessions_per_client: 16,
            max_stateids_per_client: 65536,
            max_locks_per_client: 65536,
        };
        let state_mgr = Arc::new(StateManager::new_in_memory_with_quotas("", q));
        let handler = SessionOperationHandler::new(state_mgr.clone());
        let ex = |owner: &[u8]| {
            handler.handle_exchange_id(
                ExchangeIdOp {
                    client_owner: owner.to_vec(),
                    verifier: 1,
                    flags: 0,
                    state_protect: 0,
                    client_impl_id: None,
                },
                &CompoundContext::new(1),
            )
        };
        assert_eq!(ex(b"c1").status, Nfs4Status::Ok);
        let c2 = ex(b"c2");
        assert_eq!(c2.status, Nfs4Status::Ok);
        assert_eq!(
            ex(b"c3").status,
            Nfs4Status::Delay,
            "a THIRD client identity must be refused at max_clients=2 — the table \
             was unboundedly mintable by any unauthenticated peer"
        );
        assert_eq!(
            ex(b"c1").status,
            Nfs4Status::Ok,
            "a KNOWN owner's EXCHANGE_ID must pass at the cap — it renews in place, \
             and refusing it would break a live mount"
        );
        state_mgr.clients.remove_client(c2.clientid);
        assert_eq!(
            ex(b"c3").status,
            Nfs4Status::Ok,
            "freed capacity must re-admit — DELAY promised the client a retry works"
        );
    }

    /// B6, per-client session quota: the slot-cache grant is bounded per
    /// session, and this bounds the multiplier.
    #[test]
    fn create_session_quota_refuses_the_seventeenth_session() {
        use crate::nfs::v4::state::StateQuotas;
        let q = StateQuotas {
            max_clients: 4096,
            max_sessions_per_client: 1,
            max_stateids_per_client: 65536,
            max_locks_per_client: 65536,
        };
        let state_mgr = Arc::new(StateManager::new_in_memory_with_quotas("", q));
        let handler = SessionOperationHandler::new(state_mgr);
        let ex = handler.handle_exchange_id(
            ExchangeIdOp {
                client_owner: b"one-session".to_vec(),
                verifier: 1,
                flags: 0,
                state_protect: 0,
                client_impl_id: None,
            },
            &CompoundContext::new(1),
        );
        let mk = |seq: u32| CreateSessionOp {
            clientid: ex.clientid,
            sequence: seq,
            flags: 0,
            fore_chan_attrs: ChannelAttrs::default(),
            back_chan_attrs: ChannelAttrs::default(),
            cb_program: 0,
            cb_sec: Vec::new(),
        };
        let first = handler.handle_create_session(mk(ex.sequenceid), &CompoundContext::new(1));
        assert_eq!(first.status, Nfs4Status::Ok);
        let second =
            handler.handle_create_session(mk(ex.sequenceid.wrapping_add(1)), &CompoundContext::new(1));
        assert_eq!(
            second.status,
            Nfs4Status::Delay,
            "a second session at max_sessions_per_client=1 must be refused — each \
             session multiplies the slot-cache grant"
        );
    }

    /// A reply capability flag is a server PROMISE. The old handler
    /// echoed SUPP_MOVED_REFER / SUPP_MOVED_MIGR / BIND_PRINC_STATEID
    /// back to any client that asked, advertising referral, migration,
    /// and principal-bound-stateid machinery that does not exist —
    /// fs_locations is a constant, nothing ever answers NFS4ERR_MOVED,
    /// and stateids are not principal-checked. A client told its server
    /// does referrals keeps recovery paths armed that this server can
    /// never satisfy.
    #[test]
    fn exchange_id_reply_never_advertises_unimplemented_capabilities() {
        use crate::nfs::v4::protocol::exchgid_flags;
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr);
        const UNIMPLEMENTED: u32 = exchgid_flags::SUPP_MOVED_REFER
            | exchgid_flags::SUPP_MOVED_MIGR
            | exchgid_flags::BIND_PRINC_STATEID;
        let res = handler.handle_exchange_id(
            ExchangeIdOp {
                client_owner: b"cap-client".to_vec(),
                verifier: 1,
                flags: UNIMPLEMENTED,
                state_protect: 0,
                client_impl_id: None,
            },
            &CompoundContext::new(1),
        );
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(
            res.flags & UNIMPLEMENTED,
            0,
            "the reply advertised referral/migration/principal-binding support the \
             server does not implement (reply flags = 0x{:x})",
            res.flags
        );
    }

    /// Store-time backstop: a reply larger than the negotiated cached
    /// window is not pinned (the client broke its own §2.10.6.1.1
    /// obligation; the ops already ran, so the bounded-memory answer is
    /// an uncached slot whose replay gets RETRY_UNCACHED_REP).
    #[test]
    fn oversize_cached_reply_is_refused_not_pinned() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = SessionOperationHandler::new(state_mgr.clone());
        let ex = handler.handle_exchange_id(
            ExchangeIdOp {
                client_owner: b"oversize-client".to_vec(),
                verifier: 1,
                flags: 0,
                state_protect: 0,
                client_impl_id: None,
            },
            &CompoundContext::new(1),
        );
        let cs = handler.handle_create_session(
            CreateSessionOp {
                clientid: ex.clientid,
                sequence: ex.sequenceid,
                flags: 0,
                fore_chan_attrs: ChannelAttrs::default(),
                back_chan_attrs: ChannelAttrs::default(),
                cb_program: 0,
                cb_sec: Vec::new(),
            },
            &CompoundContext::new(1),
        );
        let over = vec![0u8; super::SERVER_MAX_RESPONSE_CACHED as usize + 1];
        let under = vec![1u8, 2, 3];
        let (stored_over, stored_under) = state_mgr
            .sessions
            .get_session_mut(&cs.sessionid, |s| {
                s.cache_response(0, over);
                let a = s.get_cached_response(0).is_some();
                s.cache_response(0, under);
                let b = s.get_cached_response(0).is_some();
                (a, b)
            })
            .unwrap();
        assert!(!stored_over, "an oversize reply must not be pinned in the slot cache");
        assert!(stored_under, "a within-window reply must cache normally");
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
            cb_sec: Vec::new(),
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
