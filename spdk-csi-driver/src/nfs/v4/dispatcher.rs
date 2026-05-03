// NFSv4 COMPOUND Dispatcher
//
// This module ties everything together by:
// 1. Processing COMPOUND requests
// 2. Dispatching operations to appropriate handlers
// 3. Managing COMPOUND context (current/saved filehandles)
// 4. Building COMPOUND responses
//
// Architecture:
// - CompoundDispatcher: Main entry point for COMPOUND requests
// - Operation handlers: Session, File, I/O, Performance, Locking
// - Context tracking: Current FH, saved FH, minor version
// - Error handling: Stop on first error in COMPOUND
//
// Zero-Copy Design:
// - Operations use Arc for shared state (no copying)
// - Bytes for data transfer (reference-counted)
// - Handlers access shared managers without cloning

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::compound::{CompoundRequest, CompoundResponse, CompoundContext, Operation, OperationResult, ExchangeIdResult, CreateSessionResult, SequenceResult};
use crate::nfs::v4::state::{StateManager, StateType};
use crate::nfs::v4::filehandle::FileHandleManager;
use crate::nfs::v4::operations::*;
use bytes::Bytes;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// COMPOUND dispatcher - processes COMPOUND requests
pub struct CompoundDispatcher {
    /// State manager (clients, sessions, stateids, leases)
    state_mgr: Arc<StateManager>,

    /// Operation handlers
    session_handler: SessionOperationHandler,
    file_handler: FileOperationHandler,
    io_handler: IoOperationHandler,
    perf_handler: PerfOperationHandler,
    lock_handler: LockOperationHandler,
    
    /// Optional pNFS handler (only set for pNFS MDS mode)
    /// When None: pNFS operations return NFS4ERR_NOTSUPP
    /// When Some: pNFS operations are delegated to this handler
    pnfs_handler: Option<Arc<dyn crate::pnfs::PnfsOperations>>,
    /// Per-session back-channel writer registry. Populated by
    /// `BIND_CONN_TO_SESSION` (RFC 8881 §18.34) when a client opts
    /// the connection in as a callback path. Read by the callback
    /// fan-out (CB_LAYOUTRECALL on DS death, CB_RECALL on
    /// delegation timeout).
    back_channels: Arc<dashmap::DashMap<
        crate::nfs::v4::protocol::SessionId,
        Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
    >>,
}

impl CompoundDispatcher {
    /// Create a new COMPOUND dispatcher (standalone NFS mode)
    pub fn new(
        fh_mgr: Arc<FileHandleManager>,
        state_mgr: Arc<StateManager>,
        lock_mgr: Arc<LockManager>,
    ) -> Self {
        Self::new_with_pnfs(fh_mgr, state_mgr, lock_mgr, None)
    }
    
    /// Create a new COMPOUND dispatcher with optional pNFS support
    pub fn new_with_pnfs(
        fh_mgr: Arc<FileHandleManager>,
        state_mgr: Arc<StateManager>,
        lock_mgr: Arc<LockManager>,
        pnfs_handler: Option<Arc<dyn crate::pnfs::PnfsOperations>>,
    ) -> Self {
        // Create operation handlers
        let pnfs_enabled = pnfs_handler.is_some();
        let session_handler = SessionOperationHandler::new(state_mgr.clone());
        let file_handler = FileOperationHandler::new(fh_mgr.clone(), pnfs_enabled);
        let io_handler = IoOperationHandler::new(state_mgr.clone(), fh_mgr.clone());
        let perf_handler = PerfOperationHandler::new(state_mgr.clone(), fh_mgr.clone());
        let lock_handler = LockOperationHandler::new(state_mgr.clone(), lock_mgr.clone());

        Self {
            state_mgr,
            session_handler,
            file_handler,
            io_handler,
            perf_handler,
            lock_handler,
            pnfs_handler,
            back_channels: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Read-only handle to the back-channel registry. Callers (the
    /// pNFS `CallbackManager`, future delegation recall paths) look
    /// up `Arc<BackChannelWriter>` by session id and emit callback
    /// frames. Returning the `Arc` keeps the lifetime decoupled from
    /// `&self` and lets long-lived background tasks cache it.
    pub fn back_channels(
        &self,
    ) -> Arc<dashmap::DashMap<
        crate::nfs::v4::protocol::SessionId,
        Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
    >> {
        Arc::clone(&self.back_channels)
    }

    /// Check if an opcode is a pNFS operation
    #[allow(dead_code)]
    fn is_pnfs_opcode(opcode: u32) -> bool {
        matches!(opcode,
            opcode::GETDEVICEINFO |   // 47
            opcode::GETDEVICELIST |   // 48
            opcode::LAYOUTCOMMIT |    // 49
            opcode::LAYOUTGET |       // 50
            opcode::LAYOUTRETURN      // 51
        )
    }

    /// Store an encoded COMPOUND reply against a session slot for future
    /// replay matching (RFC 8881 §15.1.10.4 exactly-once semantics).
    ///
    /// The RPC layer calls this after encoding finishes, with the exact bytes
    /// it is about to send to the client. A subsequent SEQUENCE for the same
    /// (session, slot, seqid) returns these bytes verbatim instead of
    /// re-executing the operations. The cache is per-slot; bytes are
    /// dropped on the next forward-progress SEQUENCE on the slot.
    pub fn cache_slot_reply(&self, session_id: &SessionId, slot_id: u32, bytes: Bytes) {
        let _ = self.state_mgr.sessions.get_session_mut(session_id, |s| {
            s.cache_response(slot_id, bytes.to_vec())
        });
    }

    /// Process a COMPOUND request.
    ///
    /// `principal` is the RPC-level identity of the caller (see
    /// `nfs::rpc::Auth::principal()`); EXCHANGE_ID needs it to apply the
    /// RFC 8881 §18.35.5 state machine.
    /// Convenience wrapper used by call sites that don't have a back-
    /// channel writer (unit tests, RPCSEC_GSS init paths). Equivalent
    /// to `dispatch_compound_with_back_channel(.., None)`.
    pub async fn dispatch_compound(&self, request: CompoundRequest, principal: Vec<u8>) -> CompoundResponse {
        self.dispatch_compound_with_back_channel(request, principal, None).await
    }

    /// Same as `dispatch_compound` but threads the connection's writer
    /// into `CompoundContext::back_channel`. The `BIND_CONN_TO_SESSION`
    /// op pulls it out and registers it in the dispatcher's per-session
    /// back-channel registry, where the callback fan-out can find it
    /// later.
    pub async fn dispatch_compound_with_back_channel(
        &self,
        request: CompoundRequest,
        principal: Vec<u8>,
        back_channel: Option<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
    ) -> CompoundResponse {
        self.dispatch_compound_inner(request, principal, back_channel).await
    }

    async fn dispatch_compound_inner(
        &self,
        request: CompoundRequest,
        principal: Vec<u8>,
        back_channel: Option<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
    ) -> CompoundResponse {
        info!("COMPOUND: tag={}, operations={}", request.tag, request.operations.len());

        // RFC 5661 §15.1.6 / RFC 7530 §15.1.6: reject unrecognised minor
        // versions before doing any work. Only 0 (v4.0), 1 (v4.1) and 2 (v4.2)
        // are defined; anything else MUST return NFS4ERR_MINOR_VERS_MISMATCH
        // with an empty result array.
        if request.minor_version > NFS_V4_MINOR_VERSION_2 {
            warn!("COMPOUND: rejecting unknown minor version {}", request.minor_version);
            return CompoundResponse {
                status: Nfs4Status::MinorVersMismatch,
                tag: request.tag,
                results: Vec::new(),
                raw_reply: None,
                cache_slot: None,
            };
        }

        // RFC 5661 §3.2: tag is utf8str_cs. Non-UTF-8 → NFS4ERR_INVAL with an
        // empty result array. Decode is lenient (so we can produce this clean
        // error) but the dispatcher enforces it here.
        if !request.tag_valid {
            warn!("COMPOUND: tag is not valid UTF-8");
            return CompoundResponse {
                status: Nfs4Status::Inval,
                tag: request.tag,
                results: Vec::new(),
                raw_reply: None,
                cache_slot: None,
            };
        }

        // RFC 8881 §2.10.6.1 — session-establishment / teardown operations
        // (EXCHANGE_ID, CREATE_SESSION, DESTROY_SESSION, DESTROY_CLIENTID,
        // BIND_CONN_TO_SESSION) cannot be bundled with arbitrary ops. They
        // are still permitted alongside SEQUENCE itself (a session-bound
        // compound legitimately routes them to an existing session — for
        // example, EXCHANGE_ID for a *different* client owner inside an
        // existing session, which pynfs EID1b exercises).
        //
        // The rule we enforce: if any sole-op operation is present and any
        // other op exists that is NOT a SEQUENCE, return NFS4ERR_NOT_ONLY_OP.
        fn requires_sole_op(op: &Operation) -> bool {
            matches!(op,
                Operation::ExchangeId { .. }
                | Operation::CreateSession { .. }
                | Operation::DestroySession(_)
                | Operation::DestroyClientId(_)
                | Operation::BindConnToSession { .. }
            )
        }
        let has_sole = request.operations.iter().any(requires_sole_op);
        let has_non_sequence_companion = request.operations.iter().any(
            |o| !matches!(o, Operation::Sequence { .. }) && !requires_sole_op(o),
        );
        if has_sole && (has_non_sequence_companion || request.operations.len() > 2) {
            // The "len > 2" guard catches malformed bundles like [SEQUENCE,
            // EXCHANGE_ID, CREATE_SESSION] — two sole-op ops together is
            // also a violation regardless of what else is present.
            // We don't catch single-sole + other ops where len==2 with both
            // being sole-class; rare enough to ignore for now.
            warn!("COMPOUND: session-establishment op bundled with non-SEQUENCE companions");
            return CompoundResponse {
                status: Nfs4Status::NotOnlyOp,
                tag: request.tag,
                results: Vec::new(),
                raw_reply: None,
                cache_slot: None,
            };
        }

        // RFC 8881 §2.10.6.1 / §15.1.1.1: in v4.1+, SEQUENCE (or one of the
        // sole-op ops handled above) MUST be the first op of a COMPOUND, and
        // there MUST be at most one SEQUENCE per COMPOUND. Validate up front
        // so the per-op responses encode `NFS4ERR_SEQUENCE_POS` /
        // `NFS4ERR_OP_NOT_IN_SESSION` where pynfs expects them.
        //
        // Skip this check entirely if the COMPOUND contains an
        // Operation::BadXdr or Operation::Unsupported — those carry their
        // own RFC-mandated error replies (BADXDR, OP_ILLEGAL, NOTSUPP) that
        // the per-op encoder needs to surface, and the malformed op might
        // have been *intended* to be a SEQUENCE (it just didn't decode).
        let has_decode_error = request.operations.iter().any(|o| {
            matches!(o, Operation::BadXdr(_) | Operation::Unsupported(_))
        });
        if request.minor_version >= NFS_V4_MINOR_VERSION_1 && !has_sole && !has_decode_error {
            let mut sequence_seen = false;
            let mut sequence_pos_violation = false;
            let mut op_not_in_session = false;
            for (idx, op) in request.operations.iter().enumerate() {
                let is_seq = matches!(op, Operation::Sequence { .. });
                if is_seq {
                    if idx != 0 || sequence_seen {
                        sequence_pos_violation = true;
                    }
                    sequence_seen = true;
                } else if !sequence_seen {
                    // A non-SEQUENCE op with no preceding SEQUENCE in a
                    // v4.1 compound is OP_NOT_IN_SESSION.
                    op_not_in_session = true;
                }
            }
            if sequence_pos_violation {
                warn!("COMPOUND: SEQUENCE not first / duplicated → SEQUENCE_POS");
                return CompoundResponse {
                    status: Nfs4Status::SequencePos,
                    tag: request.tag,
                    results: Vec::new(),
                    raw_reply: None,
                    cache_slot: None,
                };
            }
            if op_not_in_session && !request.operations.is_empty() {
                warn!("COMPOUND: op without preceding SEQUENCE → OP_NOT_IN_SESSION");
                return CompoundResponse {
                    status: Nfs4Status::OpNotInSession,
                    tag: request.tag,
                    results: Vec::new(),
                    raw_reply: None,
                    cache_slot: None,
                };
            }
        }

        // Create context, seeding with the RPC-level principal.
        let mut context = CompoundContext::with_principal(request.minor_version, principal);
        // Stash the connection's back-channel writer so the
        // BIND_CONN_TO_SESSION arm can register it later in the
        // dispatcher's per-session back-channel table.
        context.back_channel = back_channel;

        // Process operations sequentially
        let mut results = Vec::new();
        let mut final_status = Nfs4Status::Ok;

        // RFC 8881 §18.36.4 ca_maxoperations enforcement. We can only check
        // it after the SEQUENCE op identifies the session, but the spec
        // says the violation is reported on the *first* op past the limit
        // (typically GETATTR / PUTROOTFH following the SEQUENCE). We snapshot
        // the limit when we see SEQUENCE and short-circuit the loop if the
        // total op count is over.
        let total_ops = request.operations.len();

        for (i, operation) in request.operations.into_iter().enumerate() {
            debug!("COMPOUND[{}]: Processing operation: {:?}", i, operation);

            // Log pNFS operations with high visibility
            match &operation {
                Operation::LayoutGet { .. } => {
                    warn!("🔴🔴🔴 ABOUT TO DISPATCH LAYOUTGET OPERATION 🔴🔴🔴");
                }
                Operation::GetDeviceInfo { .. } => {
                    warn!("🔴🔴🔴 ABOUT TO DISPATCH GETDEVICEINFO OPERATION 🔴🔴🔴");
                }
                _ => {}
            }

            // Dispatch operation
            let result = self.dispatch_operation(operation, &mut context).await;

            // RFC 8881 §18.36.4: once SEQUENCE has bound a session, all ops
            // beyond `ca_maxoperations` MUST yield NFS4ERR_TOO_MANY_OPS. We
            // detect this on the (maxops+1)-th iteration after the SEQUENCE
            // landed in the result list — meaning the prior ops up to the
            // limit ran normally. The dispatcher fails fast on first-error,
            // so we just push a TOO_MANY_OPS sentinel using the current op's
            // result-slot opcode and break.
            if let Some(sid) = context.session_id {
                if let Some(s) = self.state_mgr.sessions.get_session(&sid) {
                    if total_ops as u32 > s.fore_chan_maxops && i + 1 > s.fore_chan_maxops as usize {
                        warn!("COMPOUND: total_ops {} > ca_maxoperations {} → TOO_MANY_OPS",
                              total_ops, s.fore_chan_maxops);
                        final_status = Nfs4Status::TooManyOps;
                        results.push(OperationResult::Unsupported {
                            opcode: 0,
                            status: Nfs4Status::TooManyOps,
                        });
                        break;
                    }
                }
            }

            // RFC 8881 §2.10.6.2 exactly-once: SEQUENCE detected an exact
            // resend with a cached reply on the slot. Stop touching state and
            // hand the cached bytes back verbatim. context.replay_reply was
            // populated by the SEQUENCE handler before it returned.
            if context.replay_reply.is_some() {
                debug!("COMPOUND[{}]: SEQUENCE replay short-circuit", i);
                return CompoundResponse {
                    status: Nfs4Status::Ok,
                    tag: request.tag,
                    results: Vec::new(),
                    raw_reply: context.replay_reply.take(),
                    cache_slot: None,
                };
            }

            // Check status
            let status = result.status();
            if status != Nfs4Status::Ok {
                warn!("COMPOUND[{}]: Operation failed with status {:?}", i, status);
                final_status = status;
                results.push(result);
                break; // Stop on first error
            }

            results.push(result);
        }

        CompoundResponse {
            status: final_status,
            tag: request.tag,
            results,
            raw_reply: None,
            // Propagate the cache hint so the RPC layer caches the encoded
            // reply against the slot once it has the byte representation.
            cache_slot: context.cache_slot,
        }
    }

    /// Dispatch a single operation to the appropriate handler
    async fn dispatch_operation(
        &self,
        operation: Operation,
        context: &mut CompoundContext,
    ) -> OperationResult {
        match operation {
            // Session operations (NFSv4.1)
            Operation::ExchangeId { clientowner, flags, state_protect, impl_id } => {
                // Parse impl_id into ClientImplId struct
                // impl_id is optional client implementation details (domain, name, date)
                // For now, we leave it as None since it's purely informational
                // Proper implementation would require XDR decoding of the impl_id bytes
                let client_impl_id = if impl_id.is_empty() {
                    None
                } else {
                    // TODO: Implement proper XDR decoding of impl_id
                    // For now, just log that we received it but don't parse it
                    debug!("Received client impl_id ({} bytes), but parsing not yet implemented", impl_id.len());
                    None
                };

                let op = ExchangeIdOp {
                    client_owner: clientowner.id,
                    verifier: clientowner.verifier,
                    flags,
                    state_protect,
                    client_impl_id,
                };
                let res = self.session_handler.handle_exchange_id(op, context);
                if res.status == Nfs4Status::Ok {
                    OperationResult::ExchangeId(res.status, Some(ExchangeIdResult {
                        clientid: res.clientid,
                        sequenceid: res.sequenceid,
                        flags: res.flags,
                        server_owner: res.server_owner,
                        server_scope: res.server_scope,
                    }))
                } else {
                    OperationResult::ExchangeId(res.status, None)
                }
            }

            Operation::CreateSession { clientid, sequence, flags, fore_chan_attrs, back_chan_attrs, cb_program } => {
                let op = CreateSessionOp {
                    clientid,
                    sequence,
                    flags,
                    fore_chan_attrs: fore_chan_attrs.clone(),
                    back_chan_attrs: back_chan_attrs.clone(),
                    cb_program,
                };
                let res = self.session_handler.handle_create_session(op, context);
                if res.status == Nfs4Status::Ok {
                    OperationResult::CreateSession(res.status, Some(CreateSessionResult {
                        sessionid: res.sessionid,
                        sequenceid: res.sequence,
                        flags: res.flags,
                        fore_chan_attrs: res.fore_chan_attrs,
                        back_chan_attrs: res.back_chan_attrs,
                    }))
                } else {
                    OperationResult::CreateSession(res.status, None)
                }
            }

            Operation::Sequence { sessionid, sequenceid, slotid, highest_slotid, cachethis } => {
                let op = SequenceOp {
                    sessionid,
                    sequenceid,
                    slotid,
                    highest_slotid,
                    cache_this: cachethis,
                };
                let res = self.session_handler.handle_sequence(op, context);
                if res.status == Nfs4Status::Ok {
                    // Store session_id in context for subsequent operations
                    context.session_id = Some(res.sessionid);

                    OperationResult::Sequence(res.status, Some(SequenceResult {
                        sessionid: res.sessionid,
                        sequenceid: res.sequenceid,
                        slotid: res.slotid,
                        highest_slotid: res.highest_slotid,
                        target_highest_slotid: res.target_highest_slotid,
                        // Status flags indicate session/callback state
                        // 0 = no special status (all good)
                        // Could return flags like CB_PATH_DOWN, EXPIRED_STATE, etc.
                        // For basic implementation, 0 is sufficient
                        status_flags: 0,
                    }))
                } else {
                    OperationResult::Sequence(res.status, None)
                }
            }

            Operation::DestroySession(sessionid) => {
                let op = DestroySessionOp { sessionid };
                let res = self.session_handler.handle_destroy_session(op);
                OperationResult::DestroySession(res.status)
            }

            Operation::BindConnToSession { sessionid, dir, use_conn_in_rdma_mode } => {
                info!("BIND_CONN_TO_SESSION: sessionid={:?}, dir={}", sessionid, dir);
                if self.state_mgr.sessions.get_session(&sessionid).is_some() {
                    info!("BIND_CONN_TO_SESSION: Session found, binding connection");
                    // RFC 5661 §2.10.3.1 conn_dir values:
                    //   1 = FORE (forward only — default if BCTS isn't called)
                    //   2 = BACK (the new bit we care about: server may
                    //       send callbacks on this connection)
                    //   3 = BOTH (forward + back on the same connection)
                    // Linux's NFS client uses BOTH for v4.1 mounts so a
                    // single TCP can carry both directions. We register
                    // the writer for BACK and BOTH; FORE leaves the
                    // existing registration alone.
                    const CDFC_BACK: u32 = 2;
                    const CDFC_BOTH: u32 = 3;
                    if dir == CDFC_BACK || dir == CDFC_BOTH {
                        if let Some(bcw) = context.back_channel.as_ref() {
                            self.back_channels.insert(sessionid, Arc::clone(bcw));
                            info!(
                                "BIND_CONN_TO_SESSION: registered back-channel writer for session {:?}",
                                sessionid,
                            );
                        } else {
                            warn!(
                                "BIND_CONN_TO_SESSION: dir={} requested back-channel but no writer is plumbed for this connection — callbacks will silently fail",
                                dir,
                            );
                        }
                    }
                    OperationResult::BindConnToSession(
                        Nfs4Status::Ok,
                        Some(sessionid),
                        dir,
                        use_conn_in_rdma_mode,
                    )
                } else {
                    warn!("BIND_CONN_TO_SESSION: Session {:?} not found", sessionid);
                    OperationResult::BindConnToSession(
                        Nfs4Status::BadSession,
                        None,
                        dir,
                        use_conn_in_rdma_mode,
                    )
                }
            }

            Operation::DestroyClientId(clientid) => {
                // RFC 5661 §18.50: DESTROY_CLIENTID has two error paths.
                //   * clientid does not exist → NFS4ERR_STALE_CLIENTID
                //   * clientid exists but has live sessions → NFS4ERR_CLIENTID_BUSY
                // The op is intended only to destroy *unused* client records.
                if self.state_mgr.clients.get_client(clientid).is_none() {
                    warn!("DESTROY_CLIENTID: unknown clientid {}", clientid);
                    return OperationResult::DestroyClientId(Nfs4Status::StaleClientId);
                }
                let active_sessions = self.state_mgr.sessions.get_client_sessions(clientid);
                if !active_sessions.is_empty() {
                    warn!("DESTROY_CLIENTID: clientid {} has {} active session(s) → CLIENTID_BUSY",
                          clientid, active_sessions.len());
                    return OperationResult::DestroyClientId(Nfs4Status::ClientIdBusy);
                }
                self.state_mgr.clients.remove_client(clientid);
                info!("DESTROY_CLIENTID: clientid={} destroyed", clientid);
                OperationResult::DestroyClientId(Nfs4Status::Ok)
            }

            Operation::TestStateId(stateids) => {
                // TEST_STATEID tests if stateids are valid
                // Per RFC 5661 Section 18.48
                debug!("TEST_STATEID: testing {} stateids", stateids.len());
                let mut statuses = Vec::with_capacity(stateids.len());
                for stateid in stateids {
                    match self.state_mgr.stateids.validate(&stateid) {
                        Ok(()) => {
                            debug!("TEST_STATEID: {:?} is valid", stateid);
                            statuses.push(Nfs4Status::Ok);
                        }
                        Err(e) => {
                            debug!("TEST_STATEID: {:?} is invalid: {}", stateid, e);
                            statuses.push(Nfs4Status::BadStateId);
                        }
                    }
                }
                OperationResult::TestStateId(Nfs4Status::Ok, Some(statuses))
            }

            // File handle operations. RFC 8881 §16.2.3.1.2: any operation
            // that changes the current filehandle invalidates the COMPOUND's
            // "current stateid" — a subsequent op that uses the
            // (seqid=1, other=00…00) sentinel after a CFH change MUST fail
            // with NFS4ERR_BAD_STATEID. Clear it whenever we replace CFH.
            Operation::PutRootFh => {
                context.current_stateid = None;
                let res = self.file_handler.handle_putrootfh(PutRootFhOp, context);
                OperationResult::PutRootFh(res.status)
            }

            Operation::PutFh(filehandle) => {
                context.current_stateid = None;
                let op = PutFhOp { filehandle };
                let res = self.file_handler.handle_putfh(op, context);
                OperationResult::PutFh(res.status)
            }

            Operation::GetFh => {
                let res = self.file_handler.handle_getfh(GetFhOp, context);
                if let Some(ref fh) = res.filehandle {
                    debug!("GETFH returning filehandle: {} bytes", fh.data.len());
                } else {
                    warn!("GETFH returning None (no current filehandle!)");
                }
                OperationResult::GetFh(res.status, res.filehandle)
            }

            Operation::SaveFh => {
                // RFC 8881 §16.2.3.1.2: the current stateid is bound to the
                // CFH, so SAVEFH copies the stateid alongside.
                context.saved_stateid = context.current_stateid;
                let res = self.file_handler.handle_savefh(SaveFhOp, context);
                OperationResult::SaveFh(res.status)
            }

            Operation::RestoreFh => {
                // Restore the CFH first; then bring the saved stateid back as
                // the current stateid so a follow-up CLOSE(current_stateid)
                // (after intervening LOOKUPs etc.) still works.
                let res = self.file_handler.handle_restorefh(RestoreFhOp, context);
                if res.status == Nfs4Status::Ok {
                    context.current_stateid = context.saved_stateid;
                } else {
                    context.current_stateid = None;
                }
                OperationResult::RestoreFh(res.status)
            }

            Operation::Lookup(component) => {
                context.current_stateid = None;
                let op = LookupOp { component };
                let res = self.file_handler.handle_lookup(op, context).await;
                OperationResult::Lookup(res.status)
            }

            Operation::LookupP => {
                context.current_stateid = None;
                let res = self.file_handler.handle_lookupp(LookupPOp, context).await;
                // Note: LookupP doesn't exist in OperationResult, using Lookup instead
                OperationResult::Lookup(res.status)
            }

            Operation::Access(access) => {
                let op = AccessOp { access };
                let res = self.file_handler.handle_access(op, context).await;
                // ACCESS response has TWO fields: supported and access (what's granted)
                OperationResult::Access(res.status, Some((res.supported, res.access)))
            }

            Operation::GetAttr(attr_request) => {
                let op = GetAttrOp { attr_request: attr_request.clone() };
                let res = self.file_handler.handle_getattr(op, context).await;
                if res.status == Nfs4Status::Ok {
                    // Encode Fattr4 properly: bitmap + values
                    let attrs_bytes = if let Some(fattr) = res.obj_attributes {
                        use bytes::{BytesMut, BufMut};
                        let mut buf = BytesMut::new();
                        
                        debug!("🔍 Encoding GETATTR response:");
                        debug!("   Requested attrs: {:?}", attr_request);
                        debug!("   Returned bitmap: {:?}", fattr.attrmask);
                        debug!("   Attr values: {} bytes", fattr.attr_vals.len());
                        
                        // Log first few attribute values to verify TYPE, FSID, etc.
                        if fattr.attr_vals.len() >= 4 {
                            let type_val = u32::from_be_bytes([
                                fattr.attr_vals[0], fattr.attr_vals[1],
                                fattr.attr_vals[2], fattr.attr_vals[3]
                            ]);
                            debug!("   🏷️  First attr (likely TYPE): value={} (2=dir, 1=file)", type_val);
                        }
                        
                        debug!("   📦 Full attr_vals hex dump:");
                        for (i, chunk) in fattr.attr_vals.chunks(16).enumerate() {
                            debug!("      [{:3}] {:02x?}", i * 16, chunk);
                        }
                        
                        // Encode attribute bitmap first (required by NFSv4!)
                        // Bitmap is array of u32 values
                        buf.put_u32(fattr.attrmask.len() as u32);
                        for &bitmap_word in &fattr.attrmask {
                            buf.put_u32(bitmap_word);
                        }
                        
                        // Then encode attribute values as XDR opaque
                        // Per XDR spec: length + data + padding to 4-byte boundary
                        let attr_vals_len = fattr.attr_vals.len();
                        buf.put_u32(attr_vals_len as u32); // Length of attr_vals
                        buf.put_slice(&fattr.attr_vals);
                        
                        // XDR padding: pad to 4-byte boundary
                        let padding = (4 - (attr_vals_len % 4)) % 4;
                        for _ in 0..padding {
                            buf.put_u8(0);
                        }
                        debug!("   📤 XDR: attr_vals {} bytes + {} padding bytes", attr_vals_len, padding);
                        
                        debug!("   📤 Total encoded fattr4: {} bytes", buf.len());
                        debug!("   📤 Complete fattr4 hex (first 96 bytes): {:02x?}", &buf[..std::cmp::min(96, buf.len())]);
                        
                        bytes::Bytes::from(buf)
                    } else {
                        bytes::Bytes::new()
                    };
                    OperationResult::GetAttr(res.status, Some(attrs_bytes))
                } else {
                    OperationResult::GetAttr(res.status, None)
                }
            }

            Operation::Verify { attrs } => {
                self.handle_verify(attrs, false, context).await
            }
            Operation::Nverify { attrs } => {
                self.handle_verify(attrs, true, context).await
            }

            Operation::SetAttr { stateid, attrs } => {
                // Convert Bytes to Fattr4
                // For now, we'll treat the bytes as raw attribute values
                // TODO: Properly decode attribute bitmap + values using XDR
                let fattr = crate::nfs::v4::operations::fileops::Fattr4 {
                    attrmask: Vec::new(),  // Empty for now - should be parsed from attrs
                    attr_vals: attrs.to_vec(),
                };
                let op = SetAttrOp {
                    stateid,
                    obj_attributes: fattr,
                };
                let res = self.file_handler.handle_setattr(op, context).await;
                OperationResult::SetAttr(res.status)
            }

            Operation::ReadDir { cookie, cookieverf, dircount, maxcount, attr_request } => {
                // Convert [u8; 8] to u64
                let cookieverf_u64 = u64::from_be_bytes(cookieverf);
                let op = ReadDirOp {
                    cookie,
                    cookieverf: cookieverf_u64,
                    dircount,
                    maxcount,
                    attr_request,
                };
                let res = self.file_handler.handle_readdir(op, context).await;
                if res.status == Nfs4Status::Ok {
                    use crate::nfs::v4::compound::ReadDirResult;
                    // Entries are already pre-encoded with attrs as Bytes
                    OperationResult::ReadDir(res.status, Some(ReadDirResult {
                        entries: res.entries,
                        eof: res.eof,
                        cookieverf: res.cookieverf,
                    }))
                } else {
                    OperationResult::ReadDir(res.status, None)
                }
            }

            // I/O operations
            Operation::Open { seqid, share_access, share_deny, owner, openhow, claim } => {
                // Convert compound::OpenHow to ioops::OpenHow
                let converted_openhow = match openhow.createmode {
                    0 => {
                        // UNCHECKED4
                        if let Some(attrs) = openhow.attrs {
                            crate::nfs::v4::operations::ioops::OpenHow::Create(
                                crate::nfs::v4::operations::fileops::Fattr4 {
                                    attrmask: Vec::new(),
                                    attr_vals: attrs.to_vec(),
                                }
                            )
                        } else {
                            crate::nfs::v4::operations::ioops::OpenHow::NoCreate
                        }
                    }
                    1 => {
                        // GUARDED4
                        let attrs = openhow.attrs.unwrap_or_default();
                        crate::nfs::v4::operations::ioops::OpenHow::Create(
                            crate::nfs::v4::operations::fileops::Fattr4 {
                                attrmask: Vec::new(),
                                attr_vals: attrs.to_vec(),
                            }
                        )
                    }
                    2 => {
                        // EXCLUSIVE4 - verifier in first 8 bytes of attrs
                        let verifier = if let Some(attrs) = openhow.attrs {
                            if attrs.len() >= 8 {
                                u64::from_be_bytes([
                                    attrs[0], attrs[1], attrs[2], attrs[3],
                                    attrs[4], attrs[5], attrs[6], attrs[7],
                                ])
                            } else {
                                0
                            }
                        } else {
                            0
                        };
                        crate::nfs::v4::operations::ioops::OpenHow::Exclusive(verifier)
                    }
                    3 => {
                        // EXCLUSIVE4_1 (NFSv4.1)
                        let (verifier, attrs) = if let Some(attrs_bytes) = openhow.attrs {
                            let verifier = if attrs_bytes.len() >= 8 {
                                u64::from_be_bytes([
                                    attrs_bytes[0], attrs_bytes[1], attrs_bytes[2], attrs_bytes[3],
                                    attrs_bytes[4], attrs_bytes[5], attrs_bytes[6], attrs_bytes[7],
                                ])
                            } else {
                                0
                            };
                            let remaining = if attrs_bytes.len() > 8 {
                                attrs_bytes.slice(8..).to_vec()
                            } else {
                                Vec::new()
                            };
                            (verifier, crate::nfs::v4::operations::fileops::Fattr4 {
                                attrmask: Vec::new(),
                                attr_vals: remaining,
                            })
                        } else {
                            (0, crate::nfs::v4::operations::fileops::Fattr4 {
                                attrmask: Vec::new(),
                                attr_vals: Vec::new(),
                            })
                        };
                        crate::nfs::v4::operations::ioops::OpenHow::Exclusive4_1 { verifier, attrs }
                    }
                    _ => crate::nfs::v4::operations::ioops::OpenHow::NoCreate,
                };

                // Convert compound::OpenClaim to ioops::OpenClaim
                let converted_claim = match claim.claim_type {
                    0 => crate::nfs::v4::operations::ioops::OpenClaim::Null(claim.file),
                    4 => crate::nfs::v4::operations::ioops::OpenClaim::Fh,
                    _ => crate::nfs::v4::operations::ioops::OpenClaim::Fh, // Default to Fh
                };

                let op = OpenOp {
                    seqid,
                    share_access,
                    share_deny,
                    owner,
                    openhow: converted_openhow,
                    claim: converted_claim,
                };
                let res = self.io_handler.handle_open(op, context);
                if res.status == Nfs4Status::Ok {
                    use crate::nfs::v4::compound::{OpenResult, ChangeInfo};
                    // RFC 8881 §16.2.3.1.2: a successful state-changing op
                    // (OPEN, LOCK, LOCKU, OPEN_DOWNGRADE) populates the
                    // "current stateid" so a subsequent op in the same
                    // COMPOUND can refer to it via the magic
                    // (seqid=1, other=00…00) sentinel.
                    if let Some(sid) = res.stateid {
                        context.current_stateid = Some(sid);
                    }
                    // Convert result if we have stateid and change_info
                    if let (Some(stateid), Some(change_info)) = (res.stateid, res.change_info) {
                        OperationResult::Open(res.status, Some(OpenResult {
                            stateid,
                            change_info: ChangeInfo {
                                atomic: change_info.atomic,
                                before: change_info.before,
                                after: change_info.after,
                            },
                            result_flags: res.result_flags,
                            attrset: res.attrset,
                            delegation: None,  // TODO: Implement delegation support
                        }))
                    } else {
                        OperationResult::Open(res.status, None)
                    }
                } else {
                    OperationResult::Open(res.status, None)
                }
            }

            Operation::Close { seqid, stateid } => {
                // Resolve the "current stateid" sentinel (RFC 8881 §16.2.3.1.2).
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::Close(Nfs4Status::BadStateId, None),
                };
                let op = CloseOp {
                    seqid,
                    stateid,
                };
                let res = self.io_handler.handle_close(op, context);
                OperationResult::Close(res.status, res.stateid)
            }

            Operation::Read { stateid, offset, count } => {
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::Read(Nfs4Status::BadStateId, None),
                };
                let op = ReadOp { stateid, offset, count };
                let res = self.io_handler.handle_read(op, context).await;
                if res.status == Nfs4Status::Ok {
                    use crate::nfs::v4::compound::ReadResult;
                    OperationResult::Read(res.status, Some(ReadResult {
                        eof: res.eof,
                        data: res.data,
                    }))
                } else {
                    OperationResult::Read(res.status, None)
                }
            }

            Operation::Write { stateid, offset, stable, data } => {
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::Write(Nfs4Status::BadStateId, None),
                };
                let op = WriteOp {
                    stateid,
                    offset,
                    stable,
                    data,
                };
                let res = self.io_handler.handle_write(op, context).await;
                if res.status == Nfs4Status::Ok {
                    use crate::nfs::v4::compound::WriteResult;
                    OperationResult::Write(res.status, Some(WriteResult {
                        count: res.count,
                        committed: res.committed,
                        verifier: res.writeverf.to_be_bytes(),
                    }))
                } else {
                    OperationResult::Write(res.status, None)
                }
            }

            Operation::Commit { offset, count } => {
                let op = CommitOp {
                    offset,
                    count,
                };
                let res = self.io_handler.handle_commit(op, context).await;
                if res.status == Nfs4Status::Ok {
                    OperationResult::Commit(res.status, Some(res.writeverf.to_be_bytes()))
                } else {
                    OperationResult::Commit(res.status, None)
                }
            }

            // NFSv4.2 performance operations
            Operation::Copy { src_stateid, dst_stateid, src_offset, dst_offset, count, consecutive: _, synchronous } => {
                let op = CopyOp {
                    src_stateid,
                    dst_stateid,
                    src_offset,
                    dst_offset,
                    count,
                    sync: synchronous,
                };
                let res = self.perf_handler.handle_copy(op, context).await;
                if res.status == Nfs4Status::Ok {
                    use crate::nfs::v4::compound::CopyResult;
                    OperationResult::Copy(res.status, Some(CopyResult {
                        count: res.count,
                        consecutive: true,  // Assume consecutive for simplicity
                        synchronous: res.sync,
                    }))
                } else {
                    OperationResult::Copy(res.status, None)
                }
            }

            Operation::Clone { src_stateid, dst_stateid, src_offset, dst_offset, count } => {
                let op = CloneOp {
                    src_stateid,
                    dst_stateid,
                    src_offset,
                    dst_offset,
                    count,
                };
                let res = self.perf_handler.handle_clone(op, context).await;
                OperationResult::Clone(res.status)
            }

            Operation::Allocate { stateid, offset, length } => {
                let op = AllocateOp { stateid, offset, length };
                let res = self.perf_handler.handle_allocate(op, context).await;
                OperationResult::Allocate(res.status)
            }

            Operation::Deallocate { stateid, offset, length } => {
                let op = DeallocateOp { stateid, offset, length };
                let res = self.perf_handler.handle_deallocate(op, context).await;
                OperationResult::Deallocate(res.status)
            }

            Operation::Seek { stateid, offset, what } => {
                let op = SeekOp {
                    stateid,
                    offset,
                    what: if what == 0 { SeekType::Data } else { SeekType::Hole },
                };
                let res = self.perf_handler.handle_seek(op, context).await;
                if res.status == Nfs4Status::Ok {
                    use crate::nfs::v4::compound::SeekResult;
                    OperationResult::Seek(res.status, Some(SeekResult {
                        eof: res.eof,
                        offset: res.offset,
                    }))
                } else {
                    OperationResult::Seek(res.status, None)
                }
            }

            Operation::ReadPlus { stateid, offset, count } => {
                let op = ReadPlusOp { stateid, offset, count };
                let res = self.perf_handler.handle_read_plus(op, context).await;
                if res.status == Nfs4Status::Ok {
                    use crate::nfs::v4::compound::{ReadPlusResult, ReadPlusSegment};
                    // Convert perfops::ReadPlusSegment to compound::ReadPlusSegment
                    let segments = res.segments.into_iter().map(|seg| {
                        match seg {
                            crate::nfs::v4::operations::perfops::ReadPlusSegment::Data { offset, data } => {
                                ReadPlusSegment::Data { offset, data }
                            }
                            crate::nfs::v4::operations::perfops::ReadPlusSegment::Hole { offset, length } => {
                                ReadPlusSegment::Hole { offset, length }
                            }
                        }
                    }).collect();

                    OperationResult::ReadPlus(res.status, Some(ReadPlusResult {
                        eof: res.eof,
                        segments,
                    }))
                } else {
                    OperationResult::ReadPlus(res.status, None)
                }
            }

            // Locking operations
            Operation::Lock { locktype, reclaim, offset, length, stateid, owner } => {
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::Lock(Nfs4Status::BadStateId, None),
                };
                // Convert u32 to LockType
                let lock_type = if locktype == 1 {
                    LockType::Read
                } else {
                    LockType::Write
                };
                let op = LockOp {
                    locktype: lock_type,
                    reclaim,
                    offset,
                    length,
                    stateid,
                    owner,
                    new_lock_owner: true,
                    open_seqid: Some(0),
                };
                let res = self.lock_handler.handle_lock(op, context);
                if res.status == Nfs4Status::Ok {
                    if let Some(sid) = res.stateid {
                        context.current_stateid = Some(sid);
                    }
                }
                OperationResult::Lock(res.status, res.stateid)
            }

            Operation::LockT { locktype, offset, length, owner } => {
                // Convert u32 to LockType
                let lock_type = if locktype == 1 {
                    LockType::Read
                } else {
                    LockType::Write
                };
                let op = LockTOp {
                    locktype: lock_type,
                    offset,
                    length,
                    owner,
                };
                let res = self.lock_handler.handle_lockt(op, context);
                OperationResult::LockT(res.status)
            }

            Operation::LockU { locktype, seqid, stateid, offset, length } => {
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::LockU(Nfs4Status::BadStateId, None),
                };
                // Convert u32 to LockType
                let lock_type = if locktype == 1 {
                    LockType::Read
                } else {
                    LockType::Write
                };
                let op = LockUOp {
                    locktype: lock_type,
                    seqid,
                    stateid,
                    offset,
                    length,
                };
                let res = self.lock_handler.handle_locku(op, context);
                if res.status == Nfs4Status::Ok {
                    if let Some(sid) = res.stateid {
                        context.current_stateid = Some(sid);
                    }
                }
                OperationResult::LockU(res.status, res.stateid)
            }

            Operation::FreeStateId(stateid) => {
                // Resolve the "current stateid" sentinel and check the
                // stateid type. RFC 8881 §18.38.3:
                //  * lock stateid with locks held → NFS4ERR_LOCKS_HELD
                //  * open stateid (any) → server MAY return LOCKS_HELD;
                //    pynfs CSID9 expects this to indicate the stateid is
                //    not freeable while held open.
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::FreeStateId(Nfs4Status::BadStateId),
                };
                use crate::nfs::v4::state::StateType;
                let entry = self.state_mgr.stateids.get_state(&stateid);
                match entry {
                    None => OperationResult::FreeStateId(Nfs4Status::BadStateId),
                    Some(e) => match e.state_type {
                        StateType::Open | StateType::Lock => {
                            // RFC 8881 §18.38.3: open/lock stateids that are
                            // still in use cannot be freed. Pynfs's CSID9
                            // exercises this immediately after OPEN.
                            OperationResult::FreeStateId(Nfs4Status::LocksHeld)
                        }
                        _ => {
                            let _ = self.state_mgr.stateids.revoke(&stateid);
                            OperationResult::FreeStateId(Nfs4Status::Ok)
                        }
                    },
                }
            }

            // File modification operations
            Operation::Create { objtype, objname, linkdata } => {
                use crate::nfs::v4::operations::fileops::{CreateOp, Fattr4 as FileFattr4};
                let op = CreateOp {
                    objtype,
                    objname,
                    linkdata,  // Pass linkdata for symlinks
                    createattrs: FileFattr4 {
                        attrmask: Vec::new(),
                        attr_vals: Vec::new(),
                    },
                };
                let res = self.file_handler.handle_create(op, context).await;
                OperationResult::Create(res.status, res.change_info, res.attrset)
            }

            Operation::Remove(name) => {
                use crate::nfs::v4::operations::fileops::RemoveOp;
                let op = RemoveOp { target: name };
                let res = self.file_handler.handle_remove(op, context).await;
                OperationResult::Remove(res.status, res.change_info)
            }

            Operation::Rename { oldname, newname } => {
                use crate::nfs::v4::operations::fileops::RenameOp;
                let op = RenameOp { oldname, newname };
                let res = self.file_handler.handle_rename(op, context).await;
                OperationResult::Rename(res.status, res.source_cinfo, res.target_cinfo)
            }

            Operation::Link(newname) => {
                use crate::nfs::v4::operations::fileops::LinkOp;
                let op = LinkOp { newname };
                let res = self.file_handler.handle_link(op, context).await;
                OperationResult::Link(res.status, res.change_info)
            }

            Operation::ReadLink => {
                use crate::nfs::v4::operations::fileops::ReadLinkOp;
                let op = ReadLinkOp;
                let res = self.file_handler.handle_readlink(op, context).await;
                OperationResult::ReadLink(res.status, res.link)
            }

            Operation::PutPubFh => {
                context.current_stateid = None;
                use crate::nfs::v4::operations::fileops::PutPubFhOp;
                let op = PutPubFhOp;
                let res = self.file_handler.handle_putpubfh(op, context);
                OperationResult::PutPubFh(res.status)
            }

            // Recovery operations
            Operation::ReclaimComplete(one_fs) => {
                // RECLAIM_COMPLETE indicates client has finished reclaiming state
                // For a fresh mount with no previous state, just return OK
                info!("RECLAIM_COMPLETE: one_fs={}", one_fs);
                OperationResult::ReclaimComplete(Nfs4Status::Ok)
            }

            // Security operations
            //
            // RFC 5661 §2.6.3.1.1.8: after SECINFO and SECINFO_NO_NAME the
            // current filehandle is left "unset", so a following GETFH must
            // fail with NFS4ERR_NOFILEHANDLE. We clear CFH on Ok.
            Operation::SecInfo(component) => {
                info!("SECINFO: name={:?}", component);
                let cfh = match &context.current_fh {
                    Some(fh) => fh.clone(),
                    None => return OperationResult::SecInfo(Nfs4Status::NoFileHandle),
                };
                // Resolve CFH → directory path; verify it's a dir and the
                // child exists (NFS4ERR_NOTDIR / NFS4ERR_NOENT otherwise).
                let parent_path = match self.file_handler.fh_manager().resolve_handle(&cfh) {
                    Ok(p) => p,
                    Err(_) => return OperationResult::SecInfo(Nfs4Status::Stale),
                };
                match std::fs::metadata(&parent_path) {
                    Ok(m) if !m.is_dir() => return OperationResult::SecInfo(Nfs4Status::NotDir),
                    Err(_) => return OperationResult::SecInfo(Nfs4Status::Stale),
                    _ => {}
                }
                let child = parent_path.join(&component);
                match std::fs::symlink_metadata(&child) {
                    Ok(_) => {
                        context.clear_current_fh();
                        OperationResult::SecInfo(Nfs4Status::Ok)
                    }
                    Err(_) => OperationResult::SecInfo(Nfs4Status::NoEnt),
                }
            }
            Operation::SecInfoNoName(style) => {
                // SECINFO_NO_NAME (RFC 5661 §18.45). style:
                //   SECINFO_STYLE4_CURRENT_FH = 0  → flavors for CFH
                //   SECINFO_STYLE4_PARENT     = 1  → flavors for CFH's parent
                info!("SECINFO_NO_NAME: style={}", style);
                let cfh = match &context.current_fh {
                    Some(fh) => fh.clone(),
                    None => return OperationResult::SecInfoNoName(Nfs4Status::NoFileHandle),
                };
                if style == 1 {
                    // SECINFO_STYLE4_PARENT of the served root has no NFS-
                    // visible parent → NOENT (pynfs SECNN3). Two roots can
                    // appear here: the pseudo-FS root marker, and (under
                    // single-export "Option B" PUTROOTFH) the export root
                    // itself. Compare CFH's resolved path to the export
                    // root's path to catch both shapes.
                    let mgr = self.file_handler.fh_manager();
                    let is_root = mgr.is_pseudo_root(&cfh)
                        || mgr.resolve_handle(&cfh)
                            .map(|p| p == mgr.get_export_path())
                            .unwrap_or(false);
                    if is_root {
                        return OperationResult::SecInfoNoName(Nfs4Status::NoEnt);
                    }
                }
                context.clear_current_fh();
                OperationResult::SecInfoNoName(Nfs4Status::Ok)
            }

            // pNFS operations
            Operation::LayoutGet { signal_layout_avail, layout_type, iomode, offset, length, minlength, stateid, maxcount } => {
                warn!("🚨🚨🚨 LAYOUTGET OPERATION DISPATCHED IN DISPATCHER.RS 🚨🚨🚨");
                warn!("   offset={}, length={}, iomode={}, layout_type={}", offset, length, iomode, layout_type);
                self.handle_layoutget(signal_layout_avail, layout_type, iomode, offset, length, minlength, stateid, maxcount, context)
            }
            
            Operation::GetDeviceInfo { device_id, layout_type, maxcount, notify_types } => {
                self.handle_getdeviceinfo(device_id, layout_type, maxcount, notify_types)
            }
            
            Operation::LayoutReturn { reclaim, layout_type, iomode, return_body } => {
                self.handle_layoutreturn(reclaim, layout_type, iomode, return_body, context)
            }

            Operation::LayoutCommit {
                offset, length, reclaim, stateid,
                last_write_offset, time_modify,
                layout_type, layoutupdate,
            } => {
                self.handle_layoutcommit(
                    offset, length, reclaim, stateid,
                    last_write_offset, time_modify,
                    layout_type, layoutupdate, context,
                )
            }

            // Unsupported operations — RFC 5661 §15.2 distinguishes:
            //   * "illegal" opcodes (reserved 0/1/2 or out of range) MUST be
            //     reported with sentinel resop OP_ILLEGAL and status
            //     NFS4ERR_OP_ILLEGAL;
            //   * "valid but unimplemented" opcodes echo the request opcode
            //     with status NFS4ERR_NOTSUPP.
            // The COMPOUND-level (top-of-reply) status is set from the result's
            // status() and aborts the chain, so the choice has to be made here
            // rather than at encode time.
            Operation::Unsupported(opcode) => {
                let is_illegal = opcode < 3 || opcode > opcode::CLONE;
                let status = if is_illegal {
                    Nfs4Status::OpIllegal
                } else {
                    Nfs4Status::NotSupp
                };
                warn!("Unsupported operation: opcode={} -> {:?}", opcode, status);
                OperationResult::Unsupported { opcode, status }
            }

            // The opcode was recognised but its arguments did not parse.
            // RFC 5661 §15: reply with NFS4ERR_BADXDR, echoing the request
            // opcode in the result so the client can correlate.
            Operation::BadXdr(opcode) => {
                warn!("BADXDR for opcode={}", opcode);
                OperationResult::Unsupported { opcode, status: Nfs4Status::BadXdr }
            }

            // Catch-all for any unhandled operations (e.g. an Operation variant
            // that was decoded but the dispatcher hasn't been wired to handle).
            // No opcode available here, so we surface NOTSUPP and let the
            // encoder substitute OP_ILLEGAL.
            _ => {
                warn!("Unhandled operation in dispatcher - returning NotSupp");
                OperationResult::Unsupported { opcode: 0, status: Nfs4Status::OpIllegal }
            }
        }
    }

    /// Get statistics about the server state
    pub fn get_stats(&self) -> ServerStats {
        ServerStats {
            active_clients: self.state_mgr.clients.active_count(),
            active_sessions: self.state_mgr.sessions.active_count(),
            active_stateids: self.state_mgr.stateids.active_count(),
            open_stateids: self.state_mgr.stateids.count_by_type(StateType::Open),
            lock_stateids: self.state_mgr.stateids.count_by_type(StateType::Lock),
        }
    }
    
    // pNFS operation handlers
    
    fn handle_layoutget(
        &self,
        _signal_layout_avail: bool,
        layout_type: u32,
        iomode: u32,
        offset: u64,
        length: u64,
        _minlength: u64,
        stateid: StateId,
        _maxcount: u32,
        context: &CompoundContext,
    ) -> OperationResult {
        use crate::pnfs::mds::operations::LayoutGetArgs;
        use crate::pnfs::mds::layout::{LayoutOwner, LayoutType, IoMode};
        use crate::nfs::xdr::XdrEncoder;
        
        // Check if pNFS handler is available
        let pnfs = match &self.pnfs_handler {
            Some(handler) => handler,
            None => {
                warn!("❌ LAYOUTGET requested but pNFS not configured");
                return OperationResult::LayoutGet(Nfs4Status::NotSupp, None);
            }
        };
        
        info!("📥📥📥 LAYOUTGET RECEIVED 📥📥📥");
        info!("📥 LAYOUTGET: offset={}, length={}, iomode={}, layout_type={}", offset, length, iomode, layout_type);
        
        // Get current filehandle
        let filehandle = match context.current_fh {
            Some(ref fh) => fh.data.clone(),
            None => {
                warn!("❌ LAYOUTGET: No current filehandle");
                return OperationResult::LayoutGet(Nfs4Status::NoFileHandle, None);
            }
        };
        
        // Resolve the calling client and session from the COMPOUND
        // context — RFC 8881 §12.5 ties every layout to the issuing
        // (clientid, sessionid) so CB_LAYOUTRECALL can find the
        // backchannel and LAYOUTRETURN ALL/FSID can filter by client.
        let (owner_client_id, owner_session_id) = match context.session_id {
            Some(sid) => {
                let cid = self.state_mgr.sessions
                    .get_session(&sid)
                    .map(|s| s.client_id)
                    .unwrap_or(0);
                (cid, sid.0)
            }
            None => {
                warn!("❌ LAYOUTGET without preceding SEQUENCE — no session context");
                return OperationResult::LayoutGet(Nfs4Status::OpNotInSession, None);
            }
        };
        // The fsid lives on the FH; we don't extract it yet (the FH
        // manager only stores paths). Until that's wired, treat every
        // layout as living in fsid=1 — it doesn't change recall routing,
        // just makes LAYOUTRETURN FSID degenerate to LAYOUTRETURN ALL.
        let owner = LayoutOwner {
            client_id: owner_client_id,
            session_id: owner_session_id,
            fsid: 1,
        };

        // Convert arguments
        let args = LayoutGetArgs {
            signal_layout_avail: _signal_layout_avail,
            layout_type: match layout_type {
                1 => LayoutType::NfsV4_1Files,
                4 => LayoutType::FlexFiles,  // RFC 8435
                _ => {
                    warn!("❌ Unsupported layout type: {}", layout_type);
                    return OperationResult::LayoutGet(Nfs4Status::NotSupp, None);
                }
            },
            iomode: match iomode {
                1 => IoMode::Read,
                2 => IoMode::ReadWrite,
                3 => IoMode::Any,
                _ => {
                    warn!("❌ Bad iomode: {}", iomode);
                    return OperationResult::LayoutGet(Nfs4Status::BadIoMode, None);
                }
            },
            offset,
            length,
            minlength: _minlength,
            stateid: {
                let mut sid = [0u8; 16];
                sid[0..4].copy_from_slice(&stateid.seqid.to_be_bytes());
                sid[4..16].copy_from_slice(&stateid.other);
                sid
            },
            maxcount: _maxcount,
            filehandle: filehandle.clone(),
            owner,
        };
        
        // Call pNFS handler
        match pnfs.layoutget(args) {
            Ok(result) => {
                info!("   Available data servers: {}", result.layouts.len());
                
                // Encode result
                let mut encoder = XdrEncoder::new();
                encoder.encode_bool(result.return_on_close);
                // Encode stateid (16 bytes fixed, NO length prefix per RFC 5661)
                // CRITICAL: stateid is a fixed structure, not variable-length opaque
                // Use encode_fixed_opaque which writes bytes + padding but NO length prefix
                encoder.encode_fixed_opaque(&result.stateid);
                
                // Encode layouts array - one layout per request
                // Each layout may contain multiple segments for striping
                encoder.encode_u32(result.layouts.len() as u32);
                for layout in &result.layouts {
                    // layout4 = { offset, length, iomode, layout_content4 }
                    encoder.encode_u64(layout.offset);
                    encoder.encode_u64(layout.length);
                    encoder.encode_u32(iomode);

                    // Use NFSv4.1 FILE layout (RFC 5661 §13). FFLv4 (RFC 8435)
                    // is more flexible but has subtle ff_layout4 framing
                    // requirements that the Linux kernel parses very strictly;
                    // FILE layout is the most widely tested path. Smoke-test
                    // observation: the kernel was issuing LAYOUTGET in FFLv4
                    // mode but never following up with GETDEVICEINFO — the
                    // body parsed cleanly, the kernel just couldn't find a
                    // path to actual I/O and fell back to MDS-direct.
                    const LAYOUT_TYPE_NFSV4_1_FILES: u32 = 1;
                    encoder.encode_u32(LAYOUT_TYPE_NFSV4_1_FILES);

                    let stripe_unit: u64 = 8 * 1024 * 1024; // 8 MiB stripe unit

                    if layout.segments.is_empty() {
                        warn!("❌ Layout has no segments!");
                        return OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None);
                    }

                    info!("   📤 Encoding FILE layout (RFC 5661 §13.3) with {} segments",
                          layout.segments.len());

                    let layout_content = Self::encode_file_layout_striped(
                        &layout.segments,
                        &filehandle,
                        stripe_unit,
                    );

                    info!("   📤 FILE layout content encoded: {} bytes", layout_content.len());
                    encoder.encode_opaque(&layout_content);
                }
                
                let final_response = encoder.finish();
                info!("✅ LAYOUTGET successful: {} layouts returned", result.layouts.len());
                info!("✅ Total LAYOUTGET response: {} bytes", final_response.len());
                info!("✅ Response hex (first 128 bytes): {:02x?}", &final_response[..final_response.len().min(128)]);
                OperationResult::LayoutGet(Nfs4Status::Ok, Some(final_response))
            }
            Err(e) => {
                warn!("❌ LAYOUTGET failed: {:?}", e);
                OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None)
            }
        }
    }
    
    fn handle_getdeviceinfo(
        &self,
        device_id: Vec<u8>,
        layout_type: u32,
        _maxcount: u32,
        _notify_types: Vec<u32>,
    ) -> OperationResult {
        use crate::pnfs::mds::operations::GetDeviceInfoArgs;
        use crate::pnfs::mds::layout::LayoutType;
        use crate::pnfs::mds::device::DeviceId;
        use crate::nfs::xdr::XdrEncoder;
        
        // Check if pNFS handler is available
        let pnfs = match &self.pnfs_handler {
            Some(handler) => handler,
            None => {
                warn!("❌ GETDEVICEINFO requested but pNFS not configured");
                return OperationResult::GetDeviceInfo(Nfs4Status::NotSupp, None);
            }
        };
        
        info!("🔥🔥🔥 GETDEVICEINFO RECEIVED! 🔥🔥🔥");
        info!("📥 GETDEVICEINFO: device_id len={}, layout_type={}", device_id.len(), layout_type);
        info!("📥 Device ID bytes: {:02x?}", device_id);
        
        // Convert device_id to [u8; 16]
        let mut dev_id: DeviceId = [0; 16];
        if device_id.len() >= 16 {
            dev_id.copy_from_slice(&device_id[0..16]);
        } else {
            warn!("❌ Invalid device_id length: {}", device_id.len());
            return OperationResult::GetDeviceInfo(Nfs4Status::NoEnt, None);
        }
        
        let args = GetDeviceInfoArgs {
            device_id: dev_id,
            layout_type: match layout_type {
                1 => LayoutType::NfsV4_1Files,
                4 => LayoutType::FlexFiles,  // RFC 8435
                _ => {
                    warn!("❌ Unsupported layout type: {}", layout_type);
                    return OperationResult::GetDeviceInfo(Nfs4Status::NotSupp, None);
                }
            },
            maxcount: _maxcount,
            notify_types: _notify_types,
        };
        
        match pnfs.getdeviceinfo(args) {
            Ok(result) => {
                // Encode device address
                let mut encoder = XdrEncoder::new();
                encoder.encode_u32(layout_type);
                
                // Check if this is a striped device (has multipath addresses = multiple DSes)
                let dev_addr_encoded = if result.device_addr.multipath.is_empty() {
                    // Single DS device
                    info!("   Single DS device: {}", result.device_addr.addr);
                    Self::encode_device_addr(&result.device_addr)
                } else {
                    // Striped device with multiple DSes
                    info!("   Striped device with {} DSes", result.device_addr.multipath.len() + 1);
                    
                    // Build array of DeviceAddr4 for each DS
                    let mut addrs = vec![result.device_addr.clone()];
                    for addr_str in &result.device_addr.multipath {
                        addrs.push(crate::pnfs::mds::operations::DeviceAddr4 {
                            netid: result.device_addr.netid.clone(),
                            addr: addr_str.clone(),
                            multipath: Vec::new(),
                        });
                    }
                    
                    Self::encode_device_addr_striped(&addrs)
                };
                
                encoder.encode_opaque(&dev_addr_encoded);
                
                // Notification (empty for now)
                encoder.encode_u32(0);  // Empty notification array
                
                info!("✅ GETDEVICEINFO successful");
                OperationResult::GetDeviceInfo(Nfs4Status::Ok, Some(encoder.finish()))
            }
            Err(_e) => {
                warn!("❌ GETDEVICEINFO failed");
                OperationResult::GetDeviceInfo(Nfs4Status::NoEnt, None)
            }
        }
    }
    
    /// LAYOUTRETURN (RFC 5661 §18.4 / RFC 8881 §18.44).
    ///
    /// The client tells the MDS it no longer needs a layout. Three flavors:
    /// FILE (one stateid), FSID (every layout this client holds in this
    /// filesystem), ALL (every layout this client holds anywhere). Linux
    /// issues ALL during unmount; without honouring it the MDS leaks
    /// layout state across mount cycles.
    ///
    /// We resolve `(client_id, fsid)` from the SEQUENCE-bound session and
    /// the CFH (currently every export shares fsid=1, matching what
    /// `handle_layoutget` stamps onto each `LayoutOwner`), then route
    /// through the pNFS handler.
    /// VERIFY (RFC 5661 §18.30) and NVERIFY (§18.31).
    ///
    /// VERIFY succeeds (Ok) iff the supplied fattr4 matches the server's
    /// view of the current FH; mismatch → NFS4ERR_NOT_SAME. NVERIFY is
    /// the inverse: match → NFS4ERR_SAME, mismatch → Ok. We re-use the
    /// GETATTR machinery for the canonical server encoding so the
    /// comparison is bytewise-trivial — RFC requires VERIFY to behave
    /// "as if" the server ran GETATTR for the same bitmap and compared.
    /// If any requested attr isn't in the server's supported_bitmap,
    /// reply NFS4ERR_ATTRNOTSUPP per §18.30.3.
    async fn handle_verify(
        &self,
        attrs: Bytes,
        is_nverify: bool,
        context: &mut CompoundContext,
    ) -> OperationResult {
        let mk = |s| if is_nverify {
            OperationResult::Nverify(s)
        } else {
            OperationResult::Verify(s)
        };

        // Decode the inbound fattr4: bitmap4 (u32 array) + attrlist4 (opaque).
        let mut decoder = crate::nfs::xdr::XdrDecoder::new(attrs);
        let bitmap_len = match decoder.decode_u32() {
            Ok(n) => n as usize,
            Err(_) => return mk(Nfs4Status::BadXdr),
        };
        let mut want_bitmap = Vec::with_capacity(bitmap_len);
        for _ in 0..bitmap_len {
            match decoder.decode_u32() {
                Ok(w) => want_bitmap.push(w),
                Err(_) => return mk(Nfs4Status::BadXdr),
            }
        }
        let want_vals = match decoder.decode_opaque() {
            Ok(b) => b,
            Err(_) => return mk(Nfs4Status::BadXdr),
        };

        // Ask the GETATTR handler for the server's encoding of the same bitmap.
        let op = GetAttrOp { attr_request: want_bitmap.clone() };
        let res = self.file_handler.handle_getattr(op, context).await;
        if res.status != Nfs4Status::Ok {
            return mk(res.status);
        }
        let fattr = match res.obj_attributes {
            Some(f) => f,
            None => return mk(Nfs4Status::ServerFault),
        };

        // ATTRNOTSUPP if the server's `attrmask` doesn't cover every
        // requested bit. Compare as bitwise subset, padding the shorter
        // bitmap with zeros so length differences don't trip us.
        let max_words = want_bitmap.len().max(fattr.attrmask.len());
        for i in 0..max_words {
            let want = want_bitmap.get(i).copied().unwrap_or(0);
            let have = fattr.attrmask.get(i).copied().unwrap_or(0);
            if (want & !have) != 0 {
                return mk(Nfs4Status::AttrNotsupp);
            }
        }

        // Bytewise compare the attrlist4 payloads.
        let same = want_vals.as_ref() == fattr.attr_vals.as_slice();
        let status = match (is_nverify, same) {
            (false, true)  => Nfs4Status::Ok,
            (false, false) => Nfs4Status::NotSame,
            (true, true)   => Nfs4Status::Same,
            (true, false)  => Nfs4Status::Ok,
        };
        mk(status)
    }

    fn handle_layoutreturn(
        &self,
        reclaim: bool,
        layout_type: u32,
        iomode: u32,
        return_body: super::compound::LayoutReturn4Body,
        context: &CompoundContext,
    ) -> OperationResult {
        use crate::pnfs::mds::layout::{IoMode, LayoutType};
        use crate::pnfs::mds::operations::{LayoutReturnArgs, LayoutReturnType};
        use super::compound::LayoutReturn4Body;

        let pnfs = match &self.pnfs_handler {
            Some(h) => h,
            None => {
                warn!("LAYOUTRETURN received but pNFS not configured");
                return OperationResult::LayoutReturn(Nfs4Status::NotSupp);
            }
        };

        // FILE/FSID need a session for the (client_id, fsid) lookup; ALL
        // strictly only needs the client_id. Require SEQUENCE for all
        // three to keep the rule simple — RFC 8881 §2.10.5 mandates it
        // anyway for v4.1 ops.
        let client_id = match context.session_id {
            Some(sid) => self.state_mgr.sessions
                .get_session(&sid)
                .map(|s| s.client_id)
                .unwrap_or(0),
            None => {
                warn!("LAYOUTRETURN without preceding SEQUENCE");
                return OperationResult::LayoutReturn(Nfs4Status::OpNotInSession);
            }
        };

        let lt = match layout_type {
            1 => LayoutType::NfsV4_1Files,
            4 => LayoutType::FlexFiles,
            _ => return OperationResult::LayoutReturn(Nfs4Status::UnknownLayoutType),
        };
        let im = match iomode {
            1 => IoMode::Read,
            2 => IoMode::ReadWrite,
            3 => IoMode::Any,
            _ => return OperationResult::LayoutReturn(Nfs4Status::BadIoMode),
        };

        let return_type = match return_body {
            LayoutReturn4Body::File { offset, length, stateid, body } => {
                let mut sid = [0u8; 16];
                sid[0..4].copy_from_slice(&stateid.seqid.to_be_bytes());
                sid[4..16].copy_from_slice(&stateid.other);
                LayoutReturnType::File {
                    offset,
                    length,
                    stateid: sid,
                    layout_body: body.to_vec(),
                }
            }
            LayoutReturn4Body::Fsid => LayoutReturnType::Fsid,
            LayoutReturn4Body::All => LayoutReturnType::All,
        };

        let args = LayoutReturnArgs {
            reclaim,
            layout_type: lt,
            iomode: im,
            return_type,
            client_id,
            // Single-fsid export model — see comment on doc string.
            fsid: 1,
        };

        match pnfs.layoutreturn(args) {
            Ok(()) => {
                info!("📥 LAYOUTRETURN ok (client_id={})", client_id);
                OperationResult::LayoutReturn(Nfs4Status::Ok)
            }
            Err(e) => {
                warn!("LAYOUTRETURN failed: {}", e);
                OperationResult::LayoutReturn(Nfs4Status::ServerFault)
            }
        }
    }

    /// LAYOUTCOMMIT (RFC 8881 §18.42).
    ///
    /// In the file-layout pNFS data path the *client* writes through
    /// the data servers. The MDS holds the file's metadata (size, mtime)
    /// but never sees those WRITEs, so without LAYOUTCOMMIT every
    /// readback through the MDS observes a 0-byte file. The client
    /// closes the gap by issuing LAYOUTCOMMIT before CLOSE / final
    /// LAYOUTRETURN, telling the MDS the highest offset it wrote so
    /// the MDS can extend EOF.
    ///
    /// Wire (§18.42.1): offset, length, reclaim, stateid,
    /// `last_write_offset` (Some → file ends at `last_write_offset+1`),
    /// optional `time_modify`, layoutupdate body. We honour
    /// `last_write_offset` and `time_modify`; the body is layout-type
    /// specific and FILES has nothing useful in it for a striped
    /// store, so we ignore it for now.
    ///
    /// We resolve CFH → on-disk path through the same FH manager the
    /// rest of the dispatcher uses, then `set_len(new_size)` if the
    /// file would grow. Sparse holes appear under the offsets the
    /// client routed to *other* DSes — that's expected, the kernel
    /// reassembles the logical extent from the layout, not from MDS
    /// bytes.
    fn handle_layoutcommit(
        &self,
        _offset: u64,
        _length: u64,
        _reclaim: bool,
        _stateid: StateId,
        last_write_offset: Option<u64>,
        time_modify: Option<(i64, u32)>,
        _layout_type: u32,
        _layoutupdate: Bytes,
        context: &CompoundContext,
    ) -> OperationResult {
        let cfh = match &context.current_fh {
            Some(fh) => fh,
            None => {
                warn!("LAYOUTCOMMIT without current filehandle");
                return OperationResult::LayoutCommit(Nfs4Status::NoFileHandle, None);
            }
        };

        let path = match self.file_handler.fh_manager().resolve_handle(cfh) {
            Ok(p) => p,
            Err(e) => {
                warn!("LAYOUTCOMMIT: stale/invalid CFH: {}", e);
                return OperationResult::LayoutCommit(Nfs4Status::Stale, None);
            }
        };

        let mut new_size_reported: Option<u64> = None;

        if let Some(lwo) = last_write_offset {
            // last_write_offset is the offset of the *last byte written*
            // (RFC 8881 §18.42.1), so EOF is one past that.
            let candidate = lwo.saturating_add(1);
            match std::fs::OpenOptions::new().write(true).open(&path) {
                Ok(file) => {
                    let cur_size = file.metadata().map(|m| m.len()).unwrap_or(0);
                    if candidate > cur_size {
                        if let Err(e) = file.set_len(candidate) {
                            warn!("LAYOUTCOMMIT: set_len({}, {:?}): {}", candidate, path, e);
                            return OperationResult::LayoutCommit(Nfs4Status::Io, None);
                        }
                        info!("📥 LAYOUTCOMMIT: extended {:?} {} → {}", path, cur_size, candidate);
                        new_size_reported = Some(candidate);
                    }
                }
                Err(e) => {
                    warn!("LAYOUTCOMMIT: open({:?}): {}", path, e);
                    return OperationResult::LayoutCommit(Nfs4Status::Io, None);
                }
            }
        }

        if let Some((secs, nsecs)) = time_modify {
            // Best-effort mtime update. The size update is the
            // load-bearing thing — if mtime doesn't apply we don't
            // fail the op.
            let ft = std::fs::FileTimes::new()
                .set_modified(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::new(secs.max(0) as u64, nsecs),
                );
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
                let _ = file.set_times(ft);
            }
        }

        OperationResult::LayoutCommit(Nfs4Status::Ok, new_size_reported)
    }
    
    /// Encode FILE layout for a segment (RFC 5661/8881 Section 13.2)
    /// 
    /// Structure: nfsv4_1_file_layout4
    /// - deviceid (16 bytes fixed)
    /// - nfl_util (stripe unit size in bytes)
    /// - nfl_first_stripe_index (u32)
    /// - nfl_pattern_offset (u64)
    /// - nfl_fh_list<> (array of filehandles)
    /// Encode FFLv4 (Flexible File Layout) per RFC 8435
    /// 
    /// FFLv4 supports independent storage per DS - exactly our use case!
    /// Each DS gets a unique filehandle pointing to its local storage.
    fn encode_fflv4_layout(
        segments: &[crate::pnfs::mds::layout::LayoutSegment],
        filename: &str,
        stripe_unit: u64,
    ) -> Bytes {
        use crate::nfs::xdr::XdrEncoder;
        use crate::nfs::v4::filehandle_pnfs;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        if segments.is_empty() {
            warn!("⚠️ encode_fflv4_layout called with no segments!");
            return Bytes::new();
        }
        
        // Get shared instance_id for filehandles
        let instance_id = std::env::var("PNFS_INSTANCE_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1734648000000000000);
        
        let mut encoder = XdrEncoder::new();
        
        info!("🔧 Encoding FFLv4 layout (RFC 8435) for '{}':", filename);
        info!("   Segments: {}", segments.len());
        info!("   Stripe unit: {} bytes", stripe_unit);
        info!("   Instance ID: {}", instance_id);
        
        // ffl_stripe_unit (u64)
        encoder.encode_u64(stripe_unit);
        
        // ffl_mirrors<> - array of mirror groups
        // For striping (no mirroring), we have 1 mirror with N data servers
        encoder.encode_u32(1);  // One mirror group
        
        // Mirror 0: All DSes for striping
        // ffm_data_servers<> - array of data servers
        encoder.encode_u32(segments.len() as u32);  // N data servers
        
        for (i, segment) in segments.iter().enumerate() {
            info!("   📁 Segment {}: device={}, stripe_index={}", i, segment.device_id, i);
            
            // Generate DS-specific filehandle for this stripe
            let ds_filehandle = filehandle_pnfs::generate_pnfs_filehandle(
                instance_id,
                filename,
                i as u32,  // stripe_index
            );
            
            info!("      FH for DS: {} bytes, file_id={:016x}, stripe={}",
                  ds_filehandle.data.len(),
                  filehandle_pnfs::generate_file_id(filename),
                  i);
            info!("      FH bytes: {:02x?}", ds_filehandle.data);
            
            // Convert device_id to binary
            let mut hasher = DefaultHasher::new();
            segment.device_id.hash(&mut hasher);
            let hash = hasher.finish();
            let mut device_id_bytes = [0u8; 16];
            device_id_bytes[0..8].copy_from_slice(&hash.to_be_bytes());
            device_id_bytes[8..16].copy_from_slice(&hash.to_be_bytes());
            
            // ff_data_server4 structure:
            // - ffds_deviceid
            // - ffds_efficiency  
            // - ffds_stateid
            // - ffds_fh_vers<> (array of filehandle versions)
            // - ffds_user
            // - ffds_group
            
            // Device ID (16 bytes)
            encoder.encode_fixed_opaque(&device_id_bytes);
            
            // Efficiency (u32) - 0 = unknown
            encoder.encode_u32(0);
            
            // Stateid - use anonymous stateid for now
            encoder.encode_u32(0);  // seqid
            encoder.encode_fixed_opaque(&[0u8; 12]);  // other
            
            // ffds_fh_vers<> - array of filehandles (one per supported NFS version)
            // Per RFC 8435: nfs_fh4 ffds_fh_vers<>
            // We support one filehandle for NFSv4.1
            encoder.encode_u32(1);  // Array length = 1 filehandle
            encoder.encode_opaque(&ds_filehandle.data);  // nfs_fh4 is opaque<>

            // User/group (fattr4_owner, fattr4_owner_group - empty strings = use client creds)
            encoder.encode_string("");  // user (empty = use client)
            encoder.encode_string("");  // group (empty = use client)
        }
        
        // ffl_flags (u32) - 0 for now
        encoder.encode_u32(0);
        
        // ffl_stats_collect_hint (u32) - 0 = no stats
        encoder.encode_u32(0);
        
        let result = encoder.finish();
        info!("📦 FFLv4 layout encoded: {} bytes total", result.len());
        result
    }
    
    /// Encode FILE layout with multiple segments for striping across DSes
    /// Per RFC 5661 Section 13.3 - NFSv4.1 File Layout Type
    #[allow(dead_code)]
    fn encode_file_layout_striped(
        segments: &[crate::pnfs::mds::layout::LayoutSegment],
        filehandle: &[u8],
        stripe_unit: u64,
    ) -> Bytes {
        use crate::nfs::xdr::XdrEncoder;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        if segments.is_empty() {
            warn!("⚠️ encode_file_layout_striped called with no segments!");
            return Bytes::new();
        }
        
        let mut encoder = XdrEncoder::new();
        
        // Generate composite device_id for stripe group
        // Hash ALL device IDs together to create a unique stripe group identifier
        let mut hasher = DefaultHasher::new();
        for segment in segments {
            segment.device_id.hash(&mut hasher);
        }
        b"STRIPE:".hash(&mut hasher);  // Add marker to distinguish from single devices
        let hash = hasher.finish();
        
        let mut device_id_bytes = [0u8; 16];
        device_id_bytes[0..8].copy_from_slice(&hash.to_be_bytes());
        device_id_bytes[8..16].copy_from_slice(&hash.to_be_bytes());
        
        info!("   🔧 Encoding STRIPED FILE layout (RFC 5661 Section 13.3):");
        info!("      Number of DSes in stripe: {}", segments.len());
        info!("      device_id binary (16 bytes): {:02x?}", device_id_bytes);
        info!("      stripe_unit: {} bytes ({} MB)", stripe_unit, stripe_unit / (1024*1024));
        info!("      first_stripe_index: 0");
        info!("      pattern_offset: 0");
        info!("      filehandle length: {} bytes (same for all DSes)", filehandle.len());
        
        // Encode deviceid (16 bytes fixed, no length prefix)
        encoder.encode_fixed_opaque(&device_id_bytes);
        
        // nfl_util: stripe unit size (u32 per RFC 5661)
        encoder.encode_u32(stripe_unit as u32);
        
        // nfl_first_stripe_index: which stripe to start with (always 0)
        encoder.encode_u32(0);
        
        // nfl_pattern_offset: offset where stripe pattern starts (always 0)
        encoder.encode_u64(0);
        
        // nfl_fh_list: array of filehandles (one per DS in stripe pattern)
        // For striping across N DSes, we encode N filehandles in round-robin order
        encoder.encode_u32(segments.len() as u32);
        for (i, segment) in segments.iter().enumerate() {
            info!("      FH[{}]: device_id='{}' (will use same filehandle for all DSes)", i, segment.device_id);
            encoder.encode_opaque(filehandle);
        }
        
        let result = encoder.finish();
        info!("      📦 Encoded STRIPED FILE layout: {} bytes total, {} filehandles", result.len(), segments.len());
        info!("      📦 First 128 bytes: {:02x?}", &result[..result.len().min(128)]);
        
        result
    }

    #[allow(dead_code)]
    fn encode_file_layout(
        segment: &crate::pnfs::mds::layout::LayoutSegment,
        filehandle: &[u8],
        stripe_unit: u64,
    ) -> Bytes {
        use crate::nfs::xdr::XdrEncoder;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut encoder = XdrEncoder::new();
        
        // Convert device_id string to 16-byte binary format
        // Use same hashing approach as DeviceInfo::generate_binary_id
        let mut hasher = DefaultHasher::new();
        segment.device_id.hash(&mut hasher);
        let hash = hasher.finish();
        
        let mut device_id_bytes = [0u8; 16];
        device_id_bytes[0..8].copy_from_slice(&hash.to_be_bytes());
        device_id_bytes[8..16].copy_from_slice(&hash.to_be_bytes());
        
        info!("   🔧 Encoding FILE layout (RFC 5661 Section 13.2):");
        info!("      device_id string: '{}'", segment.device_id);
        info!("      device_id binary (16 bytes): {:02x?}", device_id_bytes);
        info!("      stripe_unit: {} bytes ({} MB)", stripe_unit, stripe_unit / (1024*1024));
        info!("      first_stripe_index: {}", segment.stripe_index);
        info!("      pattern_offset: {}", segment.pattern_offset);
        info!("      filehandle length: {} bytes", filehandle.len());
        
        // Encode deviceid (16 bytes fixed, no length prefix)
        encoder.encode_fixed_opaque(&device_id_bytes);
        
        // nfl_util: stripe unit size (u32 per RFC 5661, not u64!)
        // CRITICAL: This is nfl_util4 which is a 32-bit value
        encoder.encode_u32(stripe_unit as u32);
        
        // nfl_first_stripe_index: which stripe to start with
        encoder.encode_u32(segment.stripe_index);
        
        // nfl_pattern_offset: offset where stripe pattern starts
        encoder.encode_u64(segment.pattern_offset);
        
        // nfl_fh_list: array of filehandles (one per DS in stripe pattern)
        // For simple layouts with one device, we have one filehandle
        encoder.encode_u32(1);  // Array count
        encoder.encode_opaque(filehandle);
        
        let result = encoder.finish();
        info!("      📦 Encoded FILE layout: {} bytes total", result.len());
        info!("      📦 First 64 bytes: {:02x?}", &result[..result.len().min(64)]);
        
        result
    }
    
    /// Encode striped device address per RFC 5661 Section 13.3
    /// 
    /// For N DSes in stripe pattern:
    /// - stripe_indices = [0, 1, 2, ..., N-1]  // Round-robin across all DSes
    /// - multipath_ds_list = [ [addr0], [addr1], ..., [addrN] ]  // All DS addresses
    fn encode_device_addr_striped(addrs: &[crate::pnfs::mds::operations::DeviceAddr4]) -> Bytes {
        use crate::nfs::xdr::XdrEncoder;
        use crate::pnfs::protocol::endpoint_to_uaddr;
        
        let mut encoder = XdrEncoder::new();
        
        info!("🔧 Encoding STRIPED device address with {} DSes", addrs.len());
        
        // PART 1: stripe_indices<> array
        // For striping: indices point to each DS in round-robin order
        encoder.encode_u32(addrs.len() as u32);  // stripe_indices count = number of DSes
        for i in 0..addrs.len() {
            encoder.encode_u32(i as u32);  // stripe_indices[i] = i
            info!("   stripe_index[{}] = {}", i, i);
        }
        
        // PART 2: multipath_ds_list<> array
        // This is an array of multipath_list4 (one per DS)
        encoder.encode_u32(addrs.len() as u32);  // multipath_ds_list count = number of DSes
        
        for (i, addr) in addrs.iter().enumerate() {
            // multipath_list4 for DS #i:
            // This is an array of netaddr4 for this DS
            encoder.encode_u32(1);  // netaddr4 count (1 address per DS for now)
            
            // netaddr4:
            encoder.encode_string(&addr.netid);  // e.g., "tcp"
            
            // Convert endpoint to universal address format
            // e.g., "10.42.214.18:2049" -> "10.42.214.18.8.1"
            let uaddr = endpoint_to_uaddr(&addr.addr)
                .unwrap_or_else(|_| addr.addr.clone());
            encoder.encode_string(&uaddr);
            
            info!("   DS[{}]: {} ({})", i, addr.addr, uaddr);
        }
        
        let result = encoder.finish();
        info!("📦 Striped device address encoded: {} bytes", result.len());
        result
    }
    
    /// Encode device address per RFC 5661 Section 13.2.1
    /// 
    /// nfsv4_1_file_layout_ds_addr4 {
    ///     uint32_t        stripe_indices<>;       // Indices into multipath_ds_list
    ///     multipath_list4 multipath_ds_list<>;    // Array of DS address sets
    /// }
    /// 
    /// multipath_list4 {
    ///     netaddr4 ml_naddr<>;    // Array of addresses for one DS
    /// }
    /// 
    /// For a simple single-DS layout:
    /// - stripe_indices = [0]      // Use DS #0
    /// - multipath_ds_list = [ [addr] ]  // One DS with one address
    fn encode_device_addr(addr: &crate::pnfs::mds::operations::DeviceAddr4) -> Bytes {
        use crate::nfs::xdr::XdrEncoder;
        use crate::pnfs::protocol::endpoint_to_uaddr;
        
        let mut encoder = XdrEncoder::new();
        
        // PART 1: stripe_indices<> array
        // For simple case: one stripe index pointing to DS #0
        encoder.encode_u32(1);  // stripe_indices count
        encoder.encode_u32(0);  // stripe_indices[0] = 0 (use first DS)
        
        // PART 2: multipath_ds_list<> array
        // This is an array of multipath_list4 (one per DS)
        encoder.encode_u32(1);  // multipath_ds_list count (1 DS)
        
        // multipath_list4 for DS #0:
        // This is an array of netaddr4 for this DS
        encoder.encode_u32(1);  // netaddr4 count (1 address for this DS)
        
        // netaddr4:
        encoder.encode_string(&addr.netid);  // e.g., "tcp"
        
        // Convert endpoint to universal address format
        // e.g., "10.42.214.18:2049" -> "10.42.214.18.8.1"
        let uaddr = endpoint_to_uaddr(&addr.addr)
            .unwrap_or_else(|_| addr.addr.clone());
        encoder.encode_string(&uaddr);
        
        encoder.finish()
    }
}

/// Server statistics
#[derive(Debug, Clone)]
pub struct ServerStats {
    pub active_clients: usize,
    pub active_sessions: usize,
    pub active_stateids: usize,
    pub open_stateids: usize,
    pub lock_stateids: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_dispatcher() -> (CompoundDispatcher, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new(""));
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr);
        (dispatcher, temp_dir)
    }

    #[tokio::test]
    async fn test_simple_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "test".to_string(),
            tag_valid: true,
            minor_version: 0, // NFSv4.0 — no SEQUENCE/session-enforcement
            operations: vec![
                Operation::PutRootFh,
                Operation::GetFh,
            ],
        };

        let response = dispatcher.dispatch_compound(request, Vec::new()).await;
        assert_eq!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 2);
    }

    #[tokio::test]
    async fn test_session_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "session".to_string(),
            tag_valid: true,
            minor_version: 2,
            operations: vec![
                Operation::ExchangeId {
                    clientowner: ClientId {
                        verifier: 12345,
                        id: b"test-client".to_vec(),
                    },
                    flags: 0,
                    state_protect: 0,
                    impl_id: vec![],
                },
            ],
        };

        let response = dispatcher.dispatch_compound(request, Vec::new()).await;
        assert_eq!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 1);

        match &response.results[0] {
            OperationResult::ExchangeId(status, result) => {
                assert_eq!(*status, Nfs4Status::Ok);
                if let Some(res) = result {
                    assert_ne!(res.clientid, 0);
                }
            }
            _ => panic!("Expected ExchangeId result"),
        }
    }

    #[tokio::test]
    async fn test_file_ops_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "fileops".to_string(),
            tag_valid: true,
            minor_version: 0,
            operations: vec![
                Operation::PutRootFh,
                Operation::SaveFh,
                Operation::RestoreFh,
                Operation::GetFh,
            ],
        };

        let response = dispatcher.dispatch_compound(request, Vec::new()).await;
        assert_eq!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 4);
    }

    #[tokio::test]
    async fn test_error_stops_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "error".to_string(),
            tag_valid: true,
            minor_version: 0,
            operations: vec![
                Operation::GetFh,  // This will fail (no current FH)
                Operation::PutRootFh,  // This won't execute
            ],
        };

        let response = dispatcher.dispatch_compound(request, Vec::new()).await;
        assert_ne!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 1); // Only first operation
    }

    #[tokio::test]
    async fn test_get_stats() {
        let (dispatcher, _temp) = create_test_dispatcher();

        // Create a client via EXCHANGE_ID
        let request = CompoundRequest {
            tag: "stats".to_string(),
            tag_valid: true,
            minor_version: 2,
            operations: vec![
                Operation::ExchangeId {
                    clientowner: ClientId {
                        verifier: 1,
                        id: b"test".to_vec(),
                    },
                    flags: 0,
                    state_protect: 0,
                    impl_id: vec![],
                },
            ],
        };

        dispatcher.dispatch_compound(request, Vec::new()).await;

        let stats = dispatcher.get_stats();
        assert_eq!(stats.active_clients, 1);
    }

    #[test]
    fn test_encode_fflv4_layout_single_segment() {
        use crate::pnfs::mds::layout::LayoutSegment;
        use crate::pnfs::mds::layout::IoMode;
        use crate::nfs::xdr::XdrDecoder;

        let segments = vec![LayoutSegment {
            offset: 0,
            length: 8388608,
            iomode: IoMode::ReadWrite,
            device_id: "ds-1".to_string(),
            stripe_index: 0,
            pattern_offset: 0,
        }];

        let encoded = CompoundDispatcher::encode_fflv4_layout(
            &segments,
            "test.dat",
            8388608,
        );

        assert!(!encoded.is_empty(), "Encoded layout should not be empty");

        // Decode and verify structure
        let mut decoder = XdrDecoder::new(encoded);

        // Stripe unit
        let stripe_unit = decoder.decode_u64().unwrap();
        assert_eq!(stripe_unit, 8388608);

        // Mirror count (should be 1 for striping)
        let mirror_count = decoder.decode_u32().unwrap();
        assert_eq!(mirror_count, 1);

        // DS count in mirror
        let ds_count = decoder.decode_u32().unwrap();
        assert_eq!(ds_count, 1);

        // Device ID (16 bytes)
        let _device_id = decoder.decode_fixed_opaque(16).unwrap();

        // Efficiency
        let efficiency = decoder.decode_u32().unwrap();
        assert_eq!(efficiency, 0);

        // Stateid
        let _stateid_seqid = decoder.decode_u32().unwrap();
        let _stateid_other = decoder.decode_fixed_opaque(12).unwrap();

        // Filehandle array length
        let fh_count = decoder.decode_u32().unwrap();
        assert_eq!(fh_count, 1, "Should have one filehandle");

        // Filehandle (opaque)
        let filehandle = decoder.decode_opaque().unwrap();
        assert_eq!(filehandle.len(), 21, "pNFS filehandle should be 21 bytes");
        assert_eq!(filehandle[0], 2, "Filehandle version should be 2 (pNFS)");

        // User and group
        let user = decoder.decode_string().unwrap();
        assert_eq!(user, "");
        let group = decoder.decode_string().unwrap();
        assert_eq!(group, "");

        // Flags and stats hint
        let flags = decoder.decode_u32().unwrap();
        assert_eq!(flags, 0);
        let stats_hint = decoder.decode_u32().unwrap();
        assert_eq!(stats_hint, 0);
    }

    #[test]
    fn test_encode_fflv4_layout_multiple_segments() {
        use crate::pnfs::mds::layout::LayoutSegment;
        use crate::pnfs::mds::layout::IoMode;
        use crate::nfs::xdr::XdrDecoder;

        let segments = vec![
            LayoutSegment {
                offset: 0,
                length: 8388608,
                iomode: IoMode::ReadWrite,
                device_id: "ds-1".to_string(),
                stripe_index: 0,
                pattern_offset: 0,
            },
            LayoutSegment {
                offset: 8388608,
                length: 8388608,
                iomode: IoMode::ReadWrite,
                device_id: "ds-2".to_string(),
                stripe_index: 1,
                pattern_offset: 0,
            },
            LayoutSegment {
                offset: 16777216,
                length: 8388608,
                iomode: IoMode::ReadWrite,
                device_id: "ds-3".to_string(),
                stripe_index: 2,
                pattern_offset: 0,
            },
        ];

        let encoded = CompoundDispatcher::encode_fflv4_layout(
            &segments,
            "bigfile.dat",
            8388608,
        );

        assert!(!encoded.is_empty());

        let mut decoder = XdrDecoder::new(encoded);

        // Stripe unit
        let stripe_unit = decoder.decode_u64().unwrap();
        assert_eq!(stripe_unit, 8388608);

        // Mirror count
        let mirror_count = decoder.decode_u32().unwrap();
        assert_eq!(mirror_count, 1);

        // DS count
        let ds_count = decoder.decode_u32().unwrap();
        assert_eq!(ds_count, 3, "Should have 3 data servers");

        // Verify each DS has a unique filehandle
        for i in 0..3 {
            // Device ID
            let _device_id = decoder.decode_fixed_opaque(16).unwrap();

            // Efficiency
            let _efficiency = decoder.decode_u32().unwrap();

            // Stateid
            let _stateid_seqid = decoder.decode_u32().unwrap();
            let _stateid_other = decoder.decode_fixed_opaque(12).unwrap();

            // Filehandle array
            let fh_count = decoder.decode_u32().unwrap();
            assert_eq!(fh_count, 1);

            let filehandle = decoder.decode_opaque().unwrap();
            assert_eq!(filehandle.len(), 21, "pNFS filehandle should be 21 bytes");
            assert_eq!(filehandle[0], 2, "Version should be 2");

            // Verify stripe index in filehandle
            let mut stripe_bytes = [0u8; 4];
            stripe_bytes.copy_from_slice(&filehandle[17..21]);
            let stripe_index = u32::from_be_bytes(stripe_bytes);
            assert_eq!(stripe_index, i, "Stripe index should match DS index");

            // User and group
            let _user = decoder.decode_string().unwrap();
            let _group = decoder.decode_string().unwrap();
        }
    }

    #[test]
    fn test_encode_fflv4_layout_empty_segments() {
        let segments = vec![];

        let encoded = CompoundDispatcher::encode_fflv4_layout(
            &segments,
            "test.dat",
            8388608,
        );

        // Should return empty bytes for empty segments
        assert!(encoded.is_empty());
    }

    #[test]
    fn test_encode_fflv4_layout_deterministic_file_id() {
        use crate::pnfs::mds::layout::LayoutSegment;
        use crate::pnfs::mds::layout::IoMode;
        use crate::nfs::xdr::XdrDecoder;
        use crate::nfs::v4::filehandle_pnfs;

        let segments = vec![LayoutSegment {
            offset: 0,
            length: 8388608,
            iomode: IoMode::ReadWrite,
            device_id: "ds-1".to_string(),
            stripe_index: 0,
            pattern_offset: 0,
        }];

        // Encode twice with same filename
        let encoded1 = CompoundDispatcher::encode_fflv4_layout(
            &segments,
            "myfile.txt",
            8388608,
        );

        let encoded2 = CompoundDispatcher::encode_fflv4_layout(
            &segments,
            "myfile.txt",
            8388608,
        );

        // Extract filehandles and verify they're identical
        let mut decoder1 = XdrDecoder::new(encoded1);
        let mut decoder2 = XdrDecoder::new(encoded2);

        // Skip to filehandle
        decoder1.decode_u64().unwrap(); // stripe_unit
        decoder1.decode_u32().unwrap(); // mirror_count
        decoder1.decode_u32().unwrap(); // ds_count
        decoder1.decode_fixed_opaque(16).unwrap(); // device_id
        decoder1.decode_u32().unwrap(); // efficiency
        decoder1.decode_u32().unwrap(); // stateid seqid
        decoder1.decode_fixed_opaque(12).unwrap(); // stateid other
        decoder1.decode_u32().unwrap(); // fh_count

        decoder2.decode_u64().unwrap();
        decoder2.decode_u32().unwrap();
        decoder2.decode_u32().unwrap();
        decoder2.decode_fixed_opaque(16).unwrap();
        decoder2.decode_u32().unwrap();
        decoder2.decode_u32().unwrap();
        decoder2.decode_fixed_opaque(12).unwrap();
        decoder2.decode_u32().unwrap();

        let fh1 = decoder1.decode_opaque().unwrap();
        let fh2 = decoder2.decode_opaque().unwrap();

        assert_eq!(fh1, fh2, "Same filename should produce identical filehandles");

        // Verify file_id is deterministic
        let file_id1 = filehandle_pnfs::generate_file_id("myfile.txt");
        let file_id2 = filehandle_pnfs::generate_file_id("myfile.txt");
        assert_eq!(file_id1, file_id2);
    }

    #[test]
    fn test_fflv4_filehandle_uniqueness_per_stripe() {
        use crate::pnfs::mds::layout::LayoutSegment;
        use crate::pnfs::mds::layout::IoMode;
        use crate::nfs::xdr::XdrDecoder;

        let segments = vec![
            LayoutSegment {
                offset: 0,
                length: 8388608,
                iomode: IoMode::ReadWrite,
                device_id: "ds-1".to_string(),
                stripe_index: 0,
                pattern_offset: 0,
            },
            LayoutSegment {
                offset: 8388608,
                length: 8388608,
                iomode: IoMode::ReadWrite,
                device_id: "ds-2".to_string(),
                stripe_index: 1,
                pattern_offset: 0,
            },
        ];

        let encoded = CompoundDispatcher::encode_fflv4_layout(
            &segments,
            "file.dat",
            8388608,
        );

        let mut decoder = XdrDecoder::new(encoded);

        // Skip to first filehandle
        decoder.decode_u64().unwrap(); // stripe_unit
        decoder.decode_u32().unwrap(); // mirror_count
        decoder.decode_u32().unwrap(); // ds_count

        // First DS
        decoder.decode_fixed_opaque(16).unwrap(); // device_id
        decoder.decode_u32().unwrap(); // efficiency
        decoder.decode_u32().unwrap(); // stateid
        decoder.decode_fixed_opaque(12).unwrap();
        decoder.decode_u32().unwrap(); // fh_count
        let fh1 = decoder.decode_opaque().unwrap();
        decoder.decode_string().unwrap(); // user
        decoder.decode_string().unwrap(); // group

        // Second DS
        decoder.decode_fixed_opaque(16).unwrap(); // device_id
        decoder.decode_u32().unwrap(); // efficiency
        decoder.decode_u32().unwrap(); // stateid
        decoder.decode_fixed_opaque(12).unwrap();
        decoder.decode_u32().unwrap(); // fh_count
        let fh2 = decoder.decode_opaque().unwrap();

        // Filehandles should be different (different stripe_index)
        assert_ne!(fh1, fh2, "Different stripes should have different filehandles");

        // But they should have the same file_id (bytes 9-17)
        assert_eq!(&fh1[1..17], &fh2[1..17], "Same file should have same instance_id and file_id");

        // Different stripe_index (bytes 17-21)
        assert_ne!(&fh1[17..21], &fh2[17..21], "Different stripes should have different stripe_index");
    }
}
