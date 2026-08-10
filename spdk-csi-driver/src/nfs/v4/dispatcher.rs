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
/// The MDS-local stub a fallback op resolved to (F66): `file_key` is the
/// export-relative key the pNFS handler pins placements under, `path`
/// the local stub file — the SIZE authority for proxied reads' hole
/// resolution and the set_len target after extending proxied writes.
struct StubTarget {
    file_key: String,
    path: std::path::PathBuf,
}

pub struct CompoundDispatcher {
    /// State manager (clients, sessions, stateids, leases)
    state_mgr: Arc<StateManager>,

    /// Operation handlers
    session_handler: SessionOperationHandler,
    file_handler: FileOperationHandler,
    io_handler: IoOperationHandler,
    perf_handler: PerfOperationHandler,
    lock_handler: LockOperationHandler,
    /// Held alongside `lock_handler` so the dispatcher can drive
    /// courtesy-release of expired clients' locks at COMPOUND entry
    /// (RFC 8881 §8.4.2.4). The handler keeps its own `Arc` clone
    /// for the LOCK / LOCKU / LOCKT op paths.
    lock_mgr: Arc<LockManager>,
    
    /// Optional pNFS handler (only set for pNFS MDS mode)
    /// When None: pNFS operations return NFS4ERR_NOTSUPP
    /// When Some: pNFS operations are delegated to this handler
    pnfs_handler: Option<Arc<dyn crate::pnfs::PnfsOperations>>,

    /// F68a data-path meter, cached from the pNFS handler at
    /// construction. None ⇔ not an MDS ⇔ nothing to meter.
    f68a: Option<Arc<crate::pnfs::mds::f68a_meter::DataPathMeter>>,
    /// Per-session back-channel writer registry. Populated by
    /// `BIND_CONN_TO_SESSION` (RFC 8881 §18.34) when a client opts
    /// the connection in as a callback path. Read by the callback
    /// fan-out (CB_LAYOUTRECALL on DS death, CB_RECALL on
    /// delegation timeout).
    back_channels: Arc<dashmap::DashMap<
        crate::nfs::v4::protocol::SessionId,
        Vec<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
    >>,

    /// Which TCP connections are bound to which session (RFC 8881 §2.10.3.1).
    /// The per-connection `BackChannelWriter` Arc doubles as the connection's
    /// identity (one writer per connection, held alive by the connection
    /// handler); Weak refs keep dead connections from pinning memory or
    /// recycling pointer identity while still registered. Bound at
    /// CREATE_SESSION / SEQUENCE (implicit under SP4_NONE) /
    /// BIND_CONN_TO_SESSION; checked by DESTROY_SESSION, which must reject
    /// unbound connections with NFS4ERR_CONN_NOT_BOUND_TO_SESSION (§18.37.3).
    session_bound_conns: dashmap::DashMap<
        crate::nfs::v4::protocol::SessionId,
        Vec<std::sync::Weak<crate::nfs::v4::back_channel::BackChannelWriter>>,
    >,
}

/// One pnfs_scsi_layout4 extent as encoded on the wire (RFC 8154
/// §2.3.2). States: READ_WRITE_DATA=0, READ_DATA=1, INVALID_DATA=2,
/// NONE_DATA=3.
struct ScsiSegment {
    file_offset: u64,
    length: u64,
    storage_offset: u64,
    state: u32,
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
        // Create operation handlers. The io handler comes first so the
        // file handler can share its open-fd view (F17b: GETATTR serves
        // renamed-over-but-open files via fstat instead of STALE).
        let pnfs_enabled = pnfs_handler.is_some();
        let session_handler = SessionOperationHandler::new(state_mgr.clone());
        let io_handler = IoOperationHandler::new(state_mgr.clone(), fh_mgr.clone());
        let file_handler = FileOperationHandler::new(fh_mgr.clone(), pnfs_enabled)
            .with_pnfs_handler(pnfs_handler.clone())
            .with_open_files(io_handler.open_file_view());
        let perf_handler = PerfOperationHandler::new_with_pnfs(
            state_mgr.clone(),
            fh_mgr.clone(),
            pnfs_handler.clone(),
        );
        let lock_handler = LockOperationHandler::new(state_mgr.clone(), lock_mgr.clone());

        Self {
            state_mgr,
            session_handler,
            file_handler,
            io_handler,
            perf_handler,
            lock_handler,
            lock_mgr,
            f68a: pnfs_handler.as_ref().and_then(|p| p.f68a_meter()),
            pnfs_handler,
            back_channels: Arc::new(dashmap::DashMap::new()),
            session_bound_conns: dashmap::DashMap::new(),
        }
    }

    /// Record that the compound's connection is bound to `sid`. No-op for
    /// call sites without a connection writer (unit tests, GSS init).
    fn bind_conn_to_session(
        &self,
        sid: crate::nfs::v4::protocol::SessionId,
        ctx: &CompoundContext,
    ) {
        let Some(bcw) = ctx.back_channel.as_ref() else { return };
        let mut entry = self.session_bound_conns.entry(sid).or_default();
        entry.retain(|w| w.strong_count() > 0);
        if !entry.iter().any(|w| w.as_ptr() == Arc::as_ptr(bcw)) {
            entry.push(Arc::downgrade(bcw));
        }
    }

    /// Whether the compound's connection is bound to `sid`. Compounds with
    /// no connection writer (unit tests, GSS init) are treated as bound.
    fn conn_bound_to_session(
        &self,
        sid: &crate::nfs::v4::protocol::SessionId,
        ctx: &CompoundContext,
    ) -> bool {
        let Some(bcw) = ctx.back_channel.as_ref() else { return true };
        self.session_bound_conns
            .get(sid)
            .map(|v| v.iter().any(|w| w.as_ptr() == Arc::as_ptr(bcw)))
            .unwrap_or(false)
    }

    /// Read-only handle to the back-channel registry. Callers (the
    /// pNFS `CallbackManager`, future delegation recall paths) look
    /// up `Arc<BackChannelWriter>` by session id and emit callback
    /// frames. Returning the `Arc` keeps the lifetime decoupled from
    /// `&self` and lets long-lived background tasks cache it.
    /// Register one more back-channel writer for a session, idempotently.
    fn bind_back_channel(
        map: &Arc<dashmap::DashMap<
            crate::nfs::v4::protocol::SessionId,
            Vec<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
        >>,
        session: crate::nfs::v4::protocol::SessionId,
        bcw: &Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
    ) {
        bcw.mark_back_channel();
        let mut entry = map.entry(session).or_default();
        if !entry.iter().any(|w| Arc::ptr_eq(w, bcw)) {
            entry.push(Arc::clone(bcw));
        }
    }

    pub fn back_channels(
        &self,
    ) -> Arc<dashmap::DashMap<
        crate::nfs::v4::protocol::SessionId,
        Vec<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
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

    /// `dispatch_compound_with_back_channel` plus the caller's AUTH_SYS
    /// (uid, gid) so file-creating ops can stamp ownership. The plain
    /// entries pass None — identical to pre-cred behavior (files owned by
    /// the server process).
    pub async fn dispatch_compound_with_cred(
        &self,
        request: CompoundRequest,
        principal: Vec<u8>,
        unix_cred: Option<(u32, u32)>,
        back_channel: Option<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
    ) -> CompoundResponse {
        self.dispatch_compound_inner(request, principal, unix_cred, back_channel).await
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
        self.dispatch_compound_inner(request, principal, None, back_channel).await
    }

    async fn dispatch_compound_inner(
        &self,
        request: CompoundRequest,
        principal: Vec<u8>,
        unix_cred: Option<(u32, u32)>,
        back_channel: Option<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
    ) -> CompoundResponse {
        debug!("COMPOUND: tag={}, operations={}", request.tag, request.operations.len());

        // RFC 8881 §8.4.2.4 courtesy-release. Run a quick lease-
        // expiration sweep at the top of every COMPOUND so that any
        // dead client's locks / share-reservations / open-state are
        // gone before this compound's gates evaluate. Conflict checks
        // (LOCK, share-deny, EXCLUSIVE4 retry, etc.) become
        // self-healing instead of leaking a dead client's state
        // forever. Cost is one O(n_clients) pass on `leases.iter()`
        // per COMPOUND — negligible at typical client counts.
        //
        // The state managers live in two places: `StateManager`
        // (clients / sessions / stateids / delegations / leases) has
        // its own `cleanup_expired()` cascade; `LockManager` is owned
        // by the dispatcher, so we drive its lock-release pass from
        // here using the same expired-client list before the
        // StateManager cascade nukes the lease records.
        let expired = self.state_mgr.leases.get_expired_clients();
        for cid in &expired {
            self.lock_mgr.remove_client_locks(*cid);
        }
        if !expired.is_empty() {
            self.state_mgr.cleanup_expired();
        }

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
        // When a compound starts with a valid SEQUENCE but repeats one
        // later, the ops BEFORE the misplaced SEQUENCE must still run and
        // produce per-op results — clients read resarray[0] for the leading
        // SEQUENCE's result (pynfs SEQ2 crashes on an empty resarray). The
        // misplaced op itself gets the SEQUENCE_POS error in the main loop.
        let mut sequence_pos_at: Option<usize> = None;
        if request.minor_version >= NFS_V4_MINOR_VERSION_1 && !has_sole && !has_decode_error {
            let mut sequence_seen = false;
            let mut first_misplaced_seq: Option<usize> = None;
            let mut op_not_in_session = false;
            for (idx, op) in request.operations.iter().enumerate() {
                let is_seq = matches!(op, Operation::Sequence { .. });
                if is_seq {
                    if idx != 0 || sequence_seen {
                        first_misplaced_seq.get_or_insert(idx);
                    }
                    sequence_seen = true;
                } else if !sequence_seen {
                    // A non-SEQUENCE op with no preceding SEQUENCE in a
                    // v4.1 compound is OP_NOT_IN_SESSION.
                    op_not_in_session = true;
                }
            }
            if let Some(idx) = first_misplaced_seq {
                if op_not_in_session {
                    // No valid leading SEQUENCE either — nothing may run
                    // outside a session, so keep the results-less reply.
                    warn!("COMPOUND: SEQUENCE not first → SEQUENCE_POS");
                    return CompoundResponse {
                        status: Nfs4Status::SequencePos,
                        tag: request.tag,
                        results: Vec::new(),
                        raw_reply: None,
                        cache_slot: None,
                    };
                }
                warn!("COMPOUND: duplicated SEQUENCE at op {} → SEQUENCE_POS", idx);
                sequence_pos_at = Some(idx);
            } else if op_not_in_session && !request.operations.is_empty() {
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
        context.unix_cred = unix_cred;
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
        // Snapshot the original wire size for §18.46.4 REQ_TOO_BIG
        // checking after SEQUENCE binds the session below.
        let request_wire_size = request.wire_size;

        for (i, operation) in request.operations.into_iter().enumerate() {
            debug!("COMPOUND[{}]: Processing operation: {:?}", i, operation);

            // Misplaced SEQUENCE (validated above): emit its per-op error
            // WITHOUT dispatching — running it would corrupt the slot's
            // replay cache — and stop the compound here.
            if sequence_pos_at == Some(i) {
                final_status = Nfs4Status::SequencePos;
                results.push(OperationResult::Sequence(Nfs4Status::SequencePos, None));
                break;
            }

            // Log pNFS operations with high visibility
            match &operation {
                Operation::LayoutGet { .. } => {
                    debug!("🔴🔴🔴 ABOUT TO DISPATCH LAYOUTGET OPERATION 🔴🔴🔴");
                }
                Operation::GetDeviceInfo { .. } => {
                    debug!("🔴🔴🔴 ABOUT TO DISPATCH GETDEVICEINFO OPERATION 🔴🔴🔴");
                }
                _ => {}
            }

            // Dispatch operation
            let result = self.dispatch_operation(operation, &mut context).await;

            // RFC 8881 §18.36.4 + §18.46.4: per-session limits the client
            // negotiated at CREATE_SESSION. Both fire only after SEQUENCE
            // has bound a session.
            //
            //   * `ca_maxoperations` (TOO_MANY_OPS) — first op past the
            //     limit emits the sentinel.
            //   * `ca_maxrequestsize` (REQ_TOO_BIG) — total wire size of
            //     this COMPOUND's args > what the session said it could
            //     accept. `wire_size == 0` means the caller didn't plumb
            //     the length (older test fixtures); skip the check then.
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
                    if request_wire_size > 0
                        && request_wire_size > s.fore_chan_maxrequestsize as usize
                    {
                        warn!(
                            "COMPOUND: wire_size {} > ca_maxrequestsize {} → REQ_TOO_BIG",
                            request_wire_size, s.fore_chan_maxrequestsize,
                        );
                        // Push the just-dispatched result first so the
                        // SEQUENCE op's own result lands at results[0]
                        // (clients index by op position to read SEQUENCE's
                        // sr_status). Then push the REQ_TOO_BIG sentinel.
                        results.push(result);
                        results.push(OperationResult::Unsupported {
                            opcode: 0,
                            status: Nfs4Status::ReqTooBig,
                        });
                        final_status = Nfs4Status::ReqTooBig;
                        return CompoundResponse {
                            status: final_status,
                            tag: request.tag,
                            results,
                            raw_reply: None,
                            cache_slot: context.cache_slot,
                        };
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
                debug!("COMPOUND[{}]: Operation failed with status {:?}", i, status);
                final_status = status;
                results.push(result);
                break; // Stop on first error
            }

            results.push(result);
        }

        // RFC 8881 §18.46.4: enforce the session's ca_maxresponsesize.
        // We measure the actual encoded reply (cheap — one extra
        // encode pass and only on the rare oversize path) rather than
        // estimating per-op result sizes. If oversize, we replace
        // raw_reply with the encoded form of a stripped-down response
        // (status=REP_TOO_BIG plus a single REP_TOO_BIG sentinel
        // result). Linux clients honour this and re-issue with
        // smaller bs / fewer ops; pynfs's CSESS26 negotiates a
        // 400-byte cap to exercise the gate.
        // RFC 8881 §18.46.4: enforce ca_maxresponsesize. Encode-then-
        // measure is one extra encode pass on the rare oversize case;
        // estimating per-op result sizes up-front would be brittle for
        // a small win.
        if let Some(sid) = context.session_id {
            if let Some(s) = self.state_mgr.sessions.get_session(&sid) {
                let max = s.fore_chan_maxresponsesize as usize;
                if max > 0 {
                    let cache_slot = context.cache_slot;
                    let trial = CompoundResponse {
                        status: final_status,
                        tag: request.tag.clone(),
                        results: results.clone(),
                        raw_reply: None,
                        cache_slot,
                    };
                    let measured = trial.encode();
                    if measured.len() > max {
                        warn!(
                            "COMPOUND: encoded reply {} > ca_maxresponsesize {} → REP_TOO_BIG",
                            measured.len(), max,
                        );
                        // Keep the SEQUENCE result first so the client
                        // can still read sr_status (pynfs CSESS26 indexes
                        // by op position). Replace everything after with
                        // a single REP_TOO_BIG sentinel.
                        let mut stripped_results = Vec::new();
                        if !results.is_empty() {
                            stripped_results.push(results.into_iter().next().unwrap());
                        }
                        stripped_results.push(OperationResult::Unsupported {
                            opcode: 0,
                            status: Nfs4Status::RepTooBig,
                        });
                        let stripped = CompoundResponse {
                            status: Nfs4Status::RepTooBig,
                            tag: request.tag.clone(),
                            results: stripped_results,
                            raw_reply: None,
                            cache_slot,
                        };
                        let stripped_bytes = stripped.encode();
                        return CompoundResponse {
                            status: Nfs4Status::RepTooBig,
                            tag: request.tag,
                            results: Vec::new(),
                            raw_reply: Some(stripped_bytes),
                            cache_slot,
                        };
                    }
                    return CompoundResponse {
                        status: final_status,
                        tag: request.tag,
                        results: Vec::new(),
                        raw_reply: Some(measured),
                        cache_slot,
                    };
                }
            }
        }
        CompoundResponse {
            status: final_status,
            tag: request.tag,
            results,
            raw_reply: None,
            cache_slot: context.cache_slot,
        }
    }

    /// Dispatch a single operation to the appropriate handler
    async fn dispatch_operation(
        &self,
        operation: Operation,
        context: &mut CompoundContext,
    ) -> OperationResult {
        // An operation that does not exist in the negotiated minor version
        // is illegal, not merely unimplemented (RFC 8881 §2.6, §15.2).
        //
        // Without this the server routes NFSv4.2 opcodes purely by opcode
        // NUMBER: compound.rs decodes them unconditionally, the match below
        // dispatches them, and the only minor-version check in this file
        // rejects `> 2`. So the pNFS MDS mount's `minorversion=1` (set in
        // main.rs, with no StorageClass override) was protecting the 4.2
        // handlers only by CLIENT CONVENTION — one hand-mount against the
        // MDS Service port reached every one of them. This makes it a
        // server property.
        if context.minor_version < NFS_V4_MINOR_VERSION_2 {
            if let Some(opcode) = minor_version_2_opcode(&operation) {
                warn!(
                    "NFSv4.2 opcode {} arrived in a minorversion={} COMPOUND → OP_ILLEGAL",
                    opcode, context.minor_version
                );
                return OperationResult::Unsupported {
                    opcode,
                    status: Nfs4Status::OpIllegal,
                };
            }
        }

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
                    // A pNFS-enabled dispatcher IS the MDS, so advertise the
                    // MDS role (RFC 8881 §18.35.3) — without EXCHGID4_FLAG_
                    // USE_PNFS_MDS the client never asks for a layout and
                    // every read/write goes through the metadata server.
                    //
                    // This lives here rather than in the MDS's RPC layer
                    // because it was the ONLY behavioural difference between
                    // that layer and the standalone one; keeping it here is
                    // what lets both share a single serving path.
                    let flags = if self.pnfs_handler.is_some() {
                        crate::pnfs::exchange_id::set_pnfs_mds_flags(res.flags)
                    } else {
                        res.flags
                    };
                    OperationResult::ExchangeId(res.status, Some(ExchangeIdResult {
                        clientid: res.clientid,
                        sequenceid: res.sequenceid,
                        flags,
                        server_owner: res.server_owner,
                        server_scope: res.server_scope,
                    }))
                } else {
                    OperationResult::ExchangeId(res.status, None)
                }
            }

            Operation::CreateSession { clientid, sequence, flags, fore_chan_attrs, back_chan_attrs, cb_program, cb_sec } => {
                let op = CreateSessionOp {
                    clientid,
                    sequence,
                    flags,
                    fore_chan_attrs: fore_chan_attrs.clone(),
                    back_chan_attrs: back_chan_attrs.clone(),
                    cb_program,
                    cb_sec: cb_sec.clone(),
                };
                let res = self.session_handler.handle_create_session(op, context);
                if res.status == Nfs4Status::Ok {
                    // RFC 8881 §18.36.3: if the client set
                    // `CREATE_SESSION4_FLAG_CONN_BACK_CHAN` in
                    // csa_flags, the same TCP connection acts as the
                    // session's back-channel — equivalent to a
                    // BIND_CONN_TO_SESSION(BACK) on the current
                    // connection. Linux's NFSv4.1 client uses this
                    // path on every fresh mount and never sends a
                    // separate BIND_CONN_TO_SESSION, so this is the
                    // only place the binding is ever established.
                    //
                    // C9: register the writer iff the reply ECHOED the
                    // flag (`back_chan_bound`), not merely because the
                    // client asked. Registering without echoing was the
                    // whole defect: the server queued callbacks on a
                    // connection the client had never been told was a
                    // back channel, so every CB_LAYOUTRECALL came back
                    // BADSESSION at CB_SEQUENCE. Reading the decision
                    // off `res` keeps the promise and the plumbing from
                    // ever disagreeing again.
                    if res.back_chan_bound {
                        if let Some(bcw) = context.back_channel.as_ref() {
                            Self::bind_back_channel(&self.back_channels, res.sessionid, bcw);
                            info!(
                                "CREATE_SESSION: back channel ACCEPTED for session {:?} — \
                                 csr_flags echoes CONN_BACK_CHAN, callbacks will use this connection",
                                res.sessionid,
                            );
                        }
                    } else if flags & 0x0000_0002 != 0 {
                        // Asked for, not granted: the only way here is a
                        // dispatch with no writer (unit test / in-process).
                        // Worth a line, because a client in this state will
                        // never receive a layout recall.
                        warn!(
                            "CREATE_SESSION: client requested CONN_BACK_CHAN but no back-channel \
                             writer is available for session {:?} — recalls cannot be delivered",
                            res.sessionid,
                        );
                    }
                    // CREATE_SESSION binds its connection to the new
                    // session (RFC 8881 §18.36.3).
                    self.bind_conn_to_session(res.sessionid, context);
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
                    // A successful SEQUENCE implicitly binds the connection
                    // to the session (RFC 8881 §2.10.3.1, SP4_NONE).
                    self.bind_conn_to_session(res.sessionid, context);

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
                // RFC 8881 §18.37.3: DESTROY_SESSION must arrive on a
                // connection bound to the session (pynfs DSESS9001). Only
                // gate sessions that exist — unknown ids stay BADSESSION.
                if self.state_mgr.sessions.get_session(&sessionid).is_some()
                    && !self.conn_bound_to_session(&sessionid, context)
                {
                    warn!(
                        "DESTROY_SESSION: connection not bound to session {:?} → CONN_NOT_BOUND_TO_SESSION",
                        sessionid,
                    );
                    return OperationResult::DestroySession(
                        Nfs4Status::ConnNotBoundToSession,
                    );
                }
                let op = DestroySessionOp { sessionid };
                let res = self.session_handler.handle_destroy_session(op);
                if res.status == Nfs4Status::Ok {
                    self.session_bound_conns.remove(&sessionid);
                }
                OperationResult::DestroySession(res.status)
            }

            Operation::BindConnToSession { sessionid, dir, use_conn_in_rdma_mode } => {
                info!("BIND_CONN_TO_SESSION: sessionid={:?}, dir={}", sessionid, dir);
                if self.state_mgr.sessions.get_session(&sessionid).is_some() {
                    info!("BIND_CONN_TO_SESSION: Session found, binding connection");
                    self.bind_conn_to_session(sessionid, context);
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
                            Self::bind_back_channel(&self.back_channels, sessionid, bcw);
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
                // The decode arm repacked the fattr4 as
                // [bitmap_len][words...][attrlen][values...]; split it back
                // into bitmap words + raw values for the handler. (Reading
                // the blob head as an attr value is exactly the bug that
                // made every chmod over NFS set mode 0o002 — the bitmap
                // length word.)
                let unpack = || -> Option<crate::nfs::v4::operations::fileops::Fattr4> {
                    let b = attrs.as_ref();
                    let word_count = u32::from_be_bytes(b.get(0..4)?.try_into().ok()?) as usize;
                    // Cap: bitmap4 words beyond attr 95 are unused by any
                    // client; a huge count is a malformed request.
                    if word_count > 8 {
                        return None;
                    }
                    let mut attrmask = Vec::with_capacity(word_count);
                    for i in 0..word_count {
                        let off = 4 + i * 4;
                        attrmask.push(u32::from_be_bytes(b.get(off..off + 4)?.try_into().ok()?));
                    }
                    let len_off = 4 + word_count * 4;
                    let attr_len =
                        u32::from_be_bytes(b.get(len_off..len_off + 4)?.try_into().ok()?) as usize;
                    let vals = b.get(len_off + 4..len_off + 4 + attr_len)?;
                    Some(crate::nfs::v4::operations::fileops::Fattr4 {
                        attrmask,
                        attr_vals: vals.to_vec(),
                    })
                };
                match unpack() {
                    Some(fattr) => {
                        // A size-set on a striped file must reach the
                        // DS stripe files too — capture the requested
                        // size before the handler consumes the attrs.
                        let requested_size = crate::nfs::v4::operations::fileops::decode_settable_attrs(
                            &fattr.attrmask,
                            &fattr.attr_vals,
                        )
                        .ok()
                        .and_then(|d| d.size);
                        let op = SetAttrOp {
                            stateid,
                            obj_attributes: fattr,
                        };
                        let res = self.file_handler.handle_setattr(op, context).await;
                        // Gate on the APPLIED bitmap, not the status: a
                        // compound that set size then failed on times
                        // still truncated the stub (RFC 8881 §18.30.4
                        // reports it in attrsset) — the stripes must
                        // follow regardless.
                        const FATTR4_SIZE_BIT: u32 = 1 << 4; // fattr4 attr 4, word 0
                        let size_applied = res
                            .attrsset
                            .first()
                            .is_some_and(|w| w & FATTR4_SIZE_BIT != 0);
                        if size_applied {
                            if let (Some(pnfs), Some(size)) = (&self.pnfs_handler, requested_size) {
                                if let Some(key) = self.pnfs_current_fh_key(context) {
                                    let ino = self.ino_for_key(&key);
                                    pnfs.note_truncate(&key, size, ino).await;
                                }
                            }
                        }
                        OperationResult::SetAttr(res.status, res.attrsset)
                    }
                    None => OperationResult::SetAttr(Nfs4Status::BadXdr, vec![]),
                }
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
                                    attrmask: openhow.attrmask.clone(),
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
                                attrmask: openhow.attrmask.clone(),
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
                                attrmask: openhow.attrmask.clone(),
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

                // Grace-period gating, RFC 8881 §18.16.3 + §18.51:
                //
                //   * Reclaim OPENs (CLAIM_PREVIOUS=1, CLAIM_DELEGATE_PREV=3,
                //     CLAIM_DELEG_PREV_FH=6) are only valid during the
                //     post-restart grace window AND before the client has
                //     issued RECLAIM_COMPLETE. Either condition violated
                //     → NFS4ERR_NO_GRACE.
                //
                //   * Non-reclaim OPENs from a client that hasn't yet
                //     issued RECLAIM_COMPLETE while the SERVER is still
                //     in its grace window → NFS4ERR_GRACE. The client is
                //     told to keep reclaiming first; once it sends
                //     RECLAIM_COMPLETE(rca_one_fs=FALSE), normal opens go
                //     through. (After grace expires, no client can
                //     reclaim, so non-reclaim OPENs are unrestricted
                //     regardless of whether the bit was ever flipped.)
                //
                // Linux kernel clients reliably RECLAIM_COMPLETE on every
                // mount, so the GRACE gate doesn't fire for them in
                // steady state. pynfs's RECC suite + the §18.51.3
                // wording is what we're protecting.
                let is_reclaim_claim = matches!(claim.claim_type, 1 | 3 | 6);
                let client_id = context
                    .session_id
                    .and_then(|sid| self.state_mgr.sessions.get_session(&sid))
                    .map(|s| s.client_id);
                let in_grace = self.state_mgr.leases.in_grace_period();
                let already_complete = client_id
                    .map(|cid| self.state_mgr.clients.is_reclaim_complete(cid))
                    .unwrap_or(false);
                if is_reclaim_claim {
                    if !in_grace || already_complete {
                        warn!(
                            "OPEN claim_type={} rejected: in_grace={}, reclaim_complete={}",
                            claim.claim_type, in_grace, already_complete,
                        );
                        return OperationResult::Open(Nfs4Status::NoGrace, None);
                    }
                } else if in_grace && !already_complete {
                    warn!(
                        "OPEN claim_type={} rejected: server in grace, client hasn't done RECLAIM_COMPLETE",
                        claim.claim_type,
                    );
                    return OperationResult::Open(Nfs4Status::Grace, None);
                }

                // Convert compound::OpenClaim to ioops::OpenClaim
                let converted_claim = match claim.claim_type {
                    0 => crate::nfs::v4::operations::ioops::OpenClaim::Null(claim.file),
                    4 => crate::nfs::v4::operations::ioops::OpenClaim::Fh,
                    _ => crate::nfs::v4::operations::ioops::OpenClaim::Fh, // Default to Fh
                };

                // A size createattr (O_CREAT|O_TRUNC) that lands on an
                // existing striped file must reach the DS stripe files
                // too — capture the requested size for the post-success
                // hook below.
                let requested_size = match &converted_openhow {
                    crate::nfs::v4::operations::ioops::OpenHow::Create(a)
                    | crate::nfs::v4::operations::ioops::OpenHow::Exclusive4_1 { attrs: a, .. } => {
                        crate::nfs::v4::operations::fileops::decode_settable_attrs(
                            &a.attrmask,
                            &a.attr_vals,
                        )
                        .ok()
                        .and_then(|d| d.size)
                    }
                    _ => None,
                };
                let op = OpenOp {
                    seqid,
                    share_access,
                    share_deny,
                    owner,
                    openhow: converted_openhow,
                    claim: converted_claim,
                };
                let res = self.io_handler.handle_open(op, context).await;
                {
                    // OPEN set the current FH to the opened file; a
                    // fresh create has no pin and no-ops inside. Gate
                    // on the APPLIED attrset bit (an OPEN that failed
                    // after truncating couldn't have — the size is the
                    // only attr applied on the existing-file path —
                    // but the bitmap is the authoritative record).
                    const FATTR4_SIZE_BIT: u32 = 1 << 4; // fattr4 attr 4, word 0
                    let size_applied = res
                        .attrset
                        .first()
                        .is_some_and(|w| w & FATTR4_SIZE_BIT != 0);
                    if res.status == Nfs4Status::Ok && size_applied {
                        if let (Some(pnfs), Some(size)) = (&self.pnfs_handler, requested_size) {
                            if let Some(key) = self.pnfs_current_fh_key(context) {
                                let ino = self.ino_for_key(&key);
                                pnfs.note_truncate(&key, size, ino).await;
                            }
                        }
                    }
                }
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

            Operation::OpenDowngrade { stateid, seqid: _, share_access, share_deny } => {
                // Resolve the "current stateid" sentinel (RFC 8881 §16.2.3.1.2).
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::OpenDowngrade(Nfs4Status::BadStateId, None),
                };
                match self.state_mgr.stateids.downgrade_open(&stateid, share_access, share_deny) {
                    Ok(refreshed) => {
                        // §16.2.3.1.2: a successful state-changing op
                        // populates the current stateid.
                        context.current_stateid = Some(refreshed);
                        OperationResult::OpenDowngrade(Nfs4Status::Ok, Some(refreshed))
                    }
                    Err(status) => {
                        warn!(
                            "OPEN_DOWNGRADE failed: {:?} (access={:#x}, deny={:#x})",
                            status, share_access, share_deny
                        );
                        OperationResult::OpenDowngrade(status, None)
                    }
                }
            }

            Operation::Read { stateid, offset, count } => {
                // MDS-mode guard: a striped file's bytes live on the
                // DSes; the local file is a sparse size-only stub, and
                // serving it returns silent zeros. The kernel client
                // falls back to READ-through-MDS whenever a DS is
                // unreachable (observed live on runn 2026-07-06: an
                // empty per-DS Service fails fast with ECONNREFUSED and
                // the client immediately read the stub — wrong data, no
                // error). Bounded escalation: DELAY only while a pinned
                // DS is down and recently so; IO once the fleet is
                // healthy again (or the outage exceeds the ceiling) —
                // the fallback loop never re-drives the client's layout
                // path, so a fatal completion is the only way it ever
                // recovers (kernel-verified; see the runbook's
                // "DELAY livelock" section).
                let (disp, stub) = self.stub_io_disposition(context, "READ");
                match disp {
                    crate::pnfs::FallbackIoDisposition::Serve => {}
                    crate::pnfs::FallbackIoDisposition::Delay => {
                        return OperationResult::Read(Nfs4Status::Delay, None);
                    }
                    crate::pnfs::FallbackIoDisposition::FailFast => {
                        return OperationResult::Read(Nfs4Status::Io, None);
                    }
                    // F66: healthy fleet — serve the fallback read FROM
                    // THE STRIPES via the DsControl proxy. On transient
                    // proxy failure answer DELAY: the client retries,
                    // and a genuinely dead DS stops heartbeating within
                    // the interval, after which the ordinary bounded
                    // Delay→FailFast ladder owns the file.
                    crate::pnfs::FallbackIoDisposition::Proxy => {
                        if context.resolve_stateid(stateid).is_none() {
                            return OperationResult::Read(Nfs4Status::BadStateId, None);
                        }
                        let (pnfs, t) = match (&self.pnfs_handler, stub) {
                            (Some(p), Some(t)) => (p, t),
                            _ => return OperationResult::Read(Nfs4Status::Io, None),
                        };
                        let stub_size =
                            std::fs::metadata(&t.path).map(|m| m.len()).unwrap_or(0);
                        return match pnfs
                            .proxy_fallback_read(&t.file_key, offset, count, stub_size)
                            .await
                        {
                            Ok((data, eof)) => {
                                if let Some(m) = &self.f68a {
                                    m.proxy_read(data.len() as u64);
                                }
                                use crate::nfs::v4::compound::ReadResult;
                                OperationResult::Read(
                                    Nfs4Status::Ok,
                                    Some(ReadResult { eof, data: Bytes::from(data) }),
                                )
                            }
                            Err(e) => {
                                warn!("🔁 fallback READ proxy for '{}' failed (DELAY): {}", t.file_key, e);
                                OperationResult::Read(Nfs4Status::Delay, None)
                            }
                        };
                    }
                }
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::Read(Nfs4Status::BadStateId, None),
                };
                let op = ReadOp { stateid, offset, count };
                let res = self.io_handler.handle_read(op, context).await;
                if res.status == Nfs4Status::Ok {
                    // F68a: on an MDS, a locally-served READ is client
                    // data crossing the MDS — the silent lane the
                    // runbg F68c flip rode. Meter it.
                    if let Some(m) = &self.f68a {
                        m.served_read(res.data.len() as u64);
                    }
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
                // Same MDS-mode guard as READ — a fallback WRITE into
                // the sparse stub would silently diverge from the DS
                // stripes (worse than the read case: persistent, not
                // transient). Same bounded escalation.
                let (disp, stub) = self.stub_io_disposition(context, "WRITE");
                match disp {
                    crate::pnfs::FallbackIoDisposition::Serve => {}
                    crate::pnfs::FallbackIoDisposition::Delay => {
                        return OperationResult::Write(Nfs4Status::Delay, None);
                    }
                    crate::pnfs::FallbackIoDisposition::FailFast => {
                        return OperationResult::Write(Nfs4Status::Io, None);
                    }
                    // F66: apply the fallback write TO THE STRIPES. The
                    // DS fdatasyncs before answering, so FILE_SYNC is
                    // honest — and the stub is extended in this same
                    // dispatch, which is what LAYOUTCOMMIT would have
                    // done for a DS-path write (without it, stat serves
                    // the stale stub size).
                    crate::pnfs::FallbackIoDisposition::Proxy => {
                        if context.resolve_stateid(stateid).is_none() {
                            return OperationResult::Write(Nfs4Status::BadStateId, None);
                        }
                        let (pnfs, t) = match (&self.pnfs_handler, stub) {
                            (Some(p), Some(t)) => (p, t),
                            _ => return OperationResult::Write(Nfs4Status::Io, None),
                        };
                        let len = data.len() as u64;
                        return match pnfs
                            .proxy_fallback_write(&t.file_key, offset, data)
                            .await
                        {
                            Ok(()) => {
                                if let Some(m) = &self.f68a {
                                    m.proxy_write(len);
                                }
                                let end = offset.saturating_add(len);
                                let cur =
                                    std::fs::metadata(&t.path).map(|m| m.len()).unwrap_or(0);
                                if end > cur {
                                    if let Err(e) = std::fs::OpenOptions::new()
                                        .write(true)
                                        .open(&t.path)
                                        .and_then(|f| f.set_len(end))
                                    {
                                        // The stripes hold the bytes but the
                                        // size authority failed to advance —
                                        // surfacing an error beats lying
                                        // about durability of the SIZE.
                                        warn!(
                                            "🔁 proxied WRITE landed but stub set_len({}) on {:?} failed: {}",
                                            end, t.path, e
                                        );
                                        return OperationResult::Write(Nfs4Status::Io, None);
                                    }
                                }
                                use crate::nfs::v4::compound::WriteResult;
                                OperationResult::Write(
                                    Nfs4Status::Ok,
                                    Some(WriteResult {
                                        count: len as u32,
                                        committed: 2, // FILE_SYNC4 — data fdatasync'd on the DS, size advanced here
                                        verifier: self.io_handler.write_verifier().to_be_bytes(),
                                    }),
                                )
                            }
                            Err(e) => {
                                warn!("🔁 fallback WRITE proxy for '{}' failed (DELAY): {}", t.file_key, e);
                                OperationResult::Write(Nfs4Status::Delay, None)
                            }
                        };
                    }
                }
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
                    // F68a: locally-landed WRITE on an MDS = client
                    // data crossing the MDS (and a stub going dense).
                    if let Some(m) = &self.f68a {
                        m.served_write(res.count as u64);
                    }
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
            Operation::Copy {
                src_stateid, dst_stateid, src_offset, dst_offset, count,
                consecutive: _, synchronous, source_server_count,
            } => {
                // A non-empty ca_source_server is an INTER-server copy:
                // "pull these bytes from that other server". flint has no
                // COPY_NOTIFY, no inter-server auth, and no client for a
                // remote read. Performing a local copy and reporting OK
                // would answer a question nobody asked — the F15 class.
                if source_server_count > 0 {
                    warn!(
                        "⛔ COPY names {} source server(s) — inter-server copy is not implemented; NFS4ERR_NOTSUPP rather than a silent local copy",
                        source_server_count
                    );
                    return OperationResult::Copy(Nfs4Status::NotSupp, None);
                }
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
                        // Must be the SAME verifier COMMIT reports — the
                        // client compares them within one compound.
                        verifier: self.io_handler.write_verifier(),
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

            // ALLOCATE / DEALLOCATE / SEEK operate on the CURRENT
            // filehandle, and their handlers read `ctx.current_fh` to
            // resolve it — so a guard keyed on `pnfs_current_fh_key` reads
            // bit-identically what the handler will read. (COPY and CLONE
            // are guarded inside perfops instead: their source is SAVED_FH
            // and they never touch the context. See
            // PerfOperationHandler::is_striped.)
            //
            // For a striped file the MDS's local file is a sparse size-only
            // stub, so: ALLOCATE reserves space on the wrong node and
            // reports the file extended; DEALLOCATE punches a hole in a
            // stub that is already all holes, a structural no-op reported
            // as success while the DS bytes survive; SEEK walks the stub
            // and answers "the entire file is one hole", which is the
            // fake-sparse shape F15 was raised for.
            Operation::Allocate { stateid, offset, length } => {
                if let Some(res) = self.refuse_if_striped("ALLOCATE", context) {
                    return OperationResult::Allocate(res);
                }
                let op = AllocateOp { stateid, offset, length };
                let res = self.perf_handler.handle_allocate(op, context).await;
                OperationResult::Allocate(res.status)
            }

            Operation::Deallocate { stateid, offset, length } => {
                if let Some(res) = self.refuse_if_striped("DEALLOCATE", context) {
                    return OperationResult::Deallocate(res);
                }
                let op = DeallocateOp { stateid, offset, length };
                let res = self.perf_handler.handle_deallocate(op, context).await;
                OperationResult::Deallocate(res.status)
            }

            Operation::Seek { stateid, offset, what } => {
                if let Some(res) = self.refuse_if_striped("SEEK", context) {
                    return OperationResult::Seek(res, None);
                }
                // data_content4 (RFC 7862 §15.11.1) has exactly two arms:
                // NFS4_CONTENT_DATA = 0, NFS4_CONTENT_HOLE = 1. This used
                // to be `if what == 0 { Data } else { Hole }`, which
                // answers a question the client did not ask for every
                // other value — and RFC 7862 §11.1.1.1 defines a code for
                // precisely this case: "the server supports the given
                // operation, [but] it does not support the selected arm of
                // the discriminated union".
                let what = match what {
                    0 => SeekType::Data,
                    1 => SeekType::Hole,
                    other => {
                        warn!("SEEK: unsupported data_content4 arm {} → UNION_NOTSUPP", other);
                        return OperationResult::Seek(Nfs4Status::UnionNotsupp, None);
                    }
                };
                let op = SeekOp { stateid, offset, what };
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
                // Per-class refusal (libflint design §3): byte-range
                // locks are NOTSUPP on scsi-class volumes, refused HERE
                // where the volume identity is known (lockops has none)
                // — the honest contract, instead of granting locks the
                // server's own lease machinery silently drops. LOCKU is
                // deliberately not gated: releases must always work.
                if self.scsi_class_cfh(context) {
                    return OperationResult::Lock(Nfs4Status::NotSupp, None, None);
                }
                let stateid = match context.resolve_stateid(stateid) {
                    Some(s) => s,
                    None => return OperationResult::Lock(Nfs4Status::BadStateId, None, None),
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
                OperationResult::Lock(res.status, res.stateid, res.denied)
            }

            Operation::LockT { locktype, offset, length, owner } => {
                if self.scsi_class_cfh(context) {
                    return OperationResult::LockT(Nfs4Status::NotSupp, None);
                }
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
                OperationResult::LockT(res.status, res.denied)
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
                    // Unknown stateid → Ok, not BadStateId. FREE_STATEID is
                    // how the client DISPOSES of state after TEST_STATEID
                    // said "not found" — refusing the free leaves the client
                    // retesting the same dead stateid every recovery cycle,
                    // forever (F20: the non-converging TEST_STATEID churn
                    // behind the periodic RWX connect stalls). Freeing
                    // already-forgotten state is idempotent success.
                    None => OperationResult::FreeStateId(Nfs4Status::Ok),
                    // Revoked state exists precisely so the client can learn
                    // of the revocation and then free it — this must succeed.
                    Some(e) if e.revoked => {
                        self.state_mgr.stateids.close_open_state(&stateid.other);
                        OperationResult::FreeStateId(Nfs4Status::Ok)
                    }
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
                // Resolve the victim's pNFS key AND its inode BEFORE the
                // fs remove — the extent tables key on the inode, and it
                // is unrecoverable after the unlink.
                let removed_key = self.pnfs_file_key(context.current_fh.as_ref(), &name);
                let removed_ino = removed_key.as_deref().map(|k| self.ino_for_key(k)).unwrap_or(0);
                let op = RemoveOp { target: name };
                let res = self.file_handler.handle_remove(op, context).await;
                if res.status == Nfs4Status::Ok {
                    // Forget the pin + enqueue DS stripe cleanup, so a
                    // future same-name file gets a fresh placement and
                    // can never read this file's stripes.
                    if let (Some(pnfs), Some(key)) = (&self.pnfs_handler, removed_key) {
                        pnfs.note_remove(&key, removed_ino);
                    }
                }
                OperationResult::Remove(res.status, res.change_info)
            }

            Operation::Rename { oldname, newname } => {
                use crate::nfs::v4::operations::fileops::RenameOp;
                // RFC 8881 §18.26: SAVED FH = source dir, CURRENT FH =
                // target dir.
                let old_key = self.pnfs_file_key(context.saved_fh.as_ref(), &oldname);
                let new_key = self.pnfs_file_key(context.current_fh.as_ref(), &newname);
                if let (Some(pnfs), Some(old)) = (&self.pnfs_handler, old_key.as_deref()) {
                    if !pnfs.rename_preserves_data(old) {
                        // Legacy path-keyed striped file: its DS
                        // stripes are keyed by this path; renaming
                        // would silently serve zeros to fresh readers.
                        // Refuse loudly instead (files pinned by this
                        // MDS version are identity-keyed and rename
                        // fine — only pre-upgrade stripes hit this).
                        warn!(
                            "⛔ RENAME '{}' refused: legacy path-keyed striped file — copy to a new name instead",
                            old
                        );
                        return OperationResult::Rename(Nfs4Status::NotSupp, None, None);
                    }
                }
                let op = RenameOp { oldname, newname };
                let res = self.file_handler.handle_rename(op, context).await;
                if res.status == Nfs4Status::Ok {
                    if let (Some(pnfs), Some(old), Some(new)) =
                        (&self.pnfs_handler, old_key.as_deref(), new_key.as_deref())
                    {
                        pnfs.note_rename(old, new);
                    }
                }
                OperationResult::Rename(res.status, res.source_cinfo, res.target_cinfo)
            }

            Operation::Link(newname) => {
                use crate::nfs::v4::operations::fileops::LinkOp;
                // LINK target = SAVED FH (the existing file). A hard
                // link to a striped file would give it a second,
                // UNPINNED name — reads via the link would serve the
                // sparse stub as silent zeros. Refuse for pinned files.
                if let Some(pnfs) = &self.pnfs_handler {
                    if let Some(target_key) = self.pnfs_saved_fh_key(context) {
                        if !pnfs.link_allowed(&target_key) {
                            warn!("⛔ LINK to striped file '{}' refused (pin is path-keyed)", target_key);
                            return OperationResult::Link(Nfs4Status::NotSupp, None);
                        }
                    }
                }
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
                // RFC 8881 §18.51: marks the client as having finished
                // reclaiming pre-restart state. Two scopes:
                //   * `rca_one_fs == FALSE` (whole-client) is the
                //     scope that flips the client's "exited grace
                //     mode" bit. A second whole-client RECLAIM_COMPLETE
                //     returns NFS4ERR_COMPLETE_ALREADY (§18.51.4).
                //   * `rca_one_fs == TRUE` (per-filesystem) is a
                //     filesystem-scoped completion that does not
                //     affect the global "is reclaiming?" state — it
                //     can be repeated per-fs and never bumps to
                //     COMPLETE_ALREADY. We currently don't track
                //     per-fs reclaim state because we serve a single
                //     fsid; just always return OK for this case.
                //   * Session refers to an unknown client → that
                //     would be BADSESSION at the SEQUENCE arm,
                //     so a missing client here is "should never
                //     happen" but we map it to STALE_CLIENTID for
                //     defense-in-depth.
                info!("RECLAIM_COMPLETE: one_fs={}", one_fs);
                let client_id = match context.session_id
                    .and_then(|sid| self.state_mgr.sessions.get_session(&sid))
                    .map(|s| s.client_id)
                {
                    Some(cid) => cid,
                    None => {
                        warn!("RECLAIM_COMPLETE without preceding SEQUENCE");
                        return OperationResult::ReclaimComplete(Nfs4Status::OpNotInSession);
                    }
                };
                if one_fs {
                    // Per-fs scope: no state mutation, just OK.
                    debug!("RECLAIM_COMPLETE(one_fs=TRUE) on client {} — no-op (single fsid)", client_id);
                    return OperationResult::ReclaimComplete(Nfs4Status::Ok);
                }
                use crate::nfs::v4::state::client::ReclaimCompleteOutcome;
                match self.state_mgr.clients.mark_reclaim_complete(client_id) {
                    ReclaimCompleteOutcome::Set => {
                        info!("RECLAIM_COMPLETE: client {} now exited grace mode", client_id);
                        OperationResult::ReclaimComplete(Nfs4Status::Ok)
                    }
                    ReclaimCompleteOutcome::AlreadyComplete => {
                        warn!("RECLAIM_COMPLETE: client {} already complete", client_id);
                        OperationResult::ReclaimComplete(Nfs4Status::CompleteAlready)
                    }
                    ReclaimCompleteOutcome::NoSuchClient => {
                        warn!("RECLAIM_COMPLETE: client {} not found", client_id);
                        OperationResult::ReclaimComplete(Nfs4Status::StaleClientId)
                    }
                }
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
                match tokio::fs::metadata(&parent_path).await {
                    Ok(m) if !m.is_dir() => return OperationResult::SecInfo(Nfs4Status::NotDir),
                    Err(_) => return OperationResult::SecInfo(Nfs4Status::Stale),
                    _ => {}
                }
                let child = parent_path.join(&component);
                match tokio::fs::symlink_metadata(&child).await {
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
                debug!("🚨🚨🚨 LAYOUTGET OPERATION DISPATCHED IN DISPATCHER.RS 🚨🚨🚨");
                debug!("   offset={}, length={}, iomode={}, layout_type={}", offset, length, iomode, layout_type);
                self.handle_layoutget(signal_layout_avail, layout_type, iomode, offset, length, minlength, stateid, maxcount, context).await
            }
            
            Operation::GetDeviceInfo { device_id, layout_type, maxcount, notify_types } => {
                self.handle_getdeviceinfo(device_id, layout_type, maxcount, notify_types, context)
            }
            
            Operation::LayoutReturn { reclaim, layout_type, iomode, return_body } => {
                self.handle_layoutreturn(reclaim, layout_type, iomode, return_body, context).await
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
                .await
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

            // A component name failed utf8str_cs validation (RFC 8881 §14.4).
            Operation::InvalidName(opcode) => {
                warn!("invalid utf8str_cs name for opcode={} → INVAL", opcode);
                OperationResult::Unsupported { opcode, status: Nfs4Status::Inval }
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

    /// Export-relative pNFS placement key for `name` inside the
    /// directory `dir_fh` resolves to. None when there's no pnfs
    /// handler, no FH, or the FH doesn't resolve — callers treat None
    /// as "not pNFS-relevant".
    fn pnfs_file_key(&self, dir_fh: Option<&Nfs4FileHandle>, name: &str) -> Option<String> {
        self.pnfs_handler.as_ref()?;
        let dir = self.file_handler.fh_manager().resolve_handle(dir_fh?).ok()?;
        let full = dir.join(name);
        let export = self.file_handler.fh_manager().get_export_path().to_path_buf();
        let key = full
            .strip_prefix(&export)
            .unwrap_or(&full)
            .to_string_lossy()
            .into_owned();
        if key.is_empty() { None } else { Some(key) }
    }

    /// Export-relative key of the file the SAVED FH names (LINK's
    /// target).
    fn pnfs_saved_fh_key(&self, context: &CompoundContext) -> Option<String> {
        let fh = context.saved_fh.as_ref()?;
        let path = self.file_handler.fh_manager().resolve_handle(fh).ok()?;
        let export = self.file_handler.fh_manager().get_export_path().to_path_buf();
        let key = path
            .strip_prefix(&export)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if key.is_empty() { None } else { Some(key) }
    }

    /// Export-relative key of the file the CURRENT FH names (SETATTR's
    /// and OPEN's target). None when there's no pnfs handler, no FH, or
    /// the FH doesn't resolve — callers treat None as "not
    /// pNFS-relevant".
    fn pnfs_current_fh_key(&self, context: &CompoundContext) -> Option<String> {
        self.pnfs_handler.as_ref()?;
        let fh = context.current_fh.as_ref()?;
        let path = self.file_handler.fh_manager().resolve_handle(fh).ok()?;
        let export = self.file_handler.fh_manager().get_export_path().to_path_buf();
        let key = path
            .strip_prefix(&export)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if key.is_empty() { None } else { Some(key) }
    }

    /// The inode of `key`'s stub — the identity the extent tables key
    /// on. 0 = unresolvable (already removed): the hooks treat that as
    /// "leak to the sweep, loudly".
    fn ino_for_key(&self, key: &str) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let export = self.file_handler.fh_manager().get_export_path().to_path_buf();
        std::fs::metadata(export.join(key)).map(|m| m.ino()).unwrap_or(0)
    }

    /// Whether the CURRENT filehandle lives on a scsi-class
    /// (pnfs-block) volume. False when there is no pNFS handler, no
    /// CFH, or the key does not resolve — every "don't know" answers
    /// File, which keeps the historical path untouched.
    fn scsi_class_cfh(&self, context: &CompoundContext) -> bool {
        match (self.pnfs_handler.as_ref(), self.pnfs_current_fh_key(context)) {
            (Some(p), Some(key)) => {
                p.layout_class_for(&key) == crate::pnfs::mds::layout::LayoutClass::Scsi
            }
            _ => false,
        }
    }

    /// NFS4ERR_NOTSUPP when the CURRENT filehandle names a striped file,
    /// else None (proceed). For the space-management ops whose whole
    /// premise is that the local file holds the data.
    ///
    /// NOTSUPP rather than a hard error: it is what this server already
    /// answers for the operations it declines on a striped file (LINK,
    /// RENAME) and for READ_PLUS, whose fallback to plain READ was
    /// observed live during F15. What NOTSUPP costs on these three ops is
    /// NOT established — in particular whether a Linux client clears the
    /// capability mount-wide or re-asks per call — and ALLOCATE is the one
    /// op with prior wire evidence of arriving at all (PG16's
    /// posix_fallocate, captured per-op on runw).
    fn refuse_if_striped(&self, op: &str, context: &CompoundContext) -> Option<Nfs4Status> {
        let pnfs = self.pnfs_handler.as_ref()?;
        let key = self.pnfs_current_fh_key(context)?;
        if !pnfs.is_pnfs_managed(&key) {
            return None;
        }
        warn!(
            "⛔ {} refused for striped file '{}' — its bytes live on the DSes and the MDS file is a sparse size-only stub, so this would report success against data that is not here (NFS4ERR_NOTSUPP)",
            op, key
        );
        Some(Nfs4Status::NotSupp)
    }

    /// Disposition for READ/WRITE through the MDS when the current
    /// filehandle names a striped (placement-pinned) file. Such a
    /// file's data is NOT here — the local file is a sparse stub — so
    /// the op is either parked (NFS4ERR_DELAY, while a pinned DS is
    /// down within the bounded window) or failed (NFS4ERR_IO, once the
    /// fleet is healthy or the outage exceeds the ceiling — the only
    /// completion that springs the kernel client's MDS-fallback loop).
    /// Standalone and DS roles have no pnfs_handler and serve
    /// everything; files the MDS holds that were never layouted stay
    /// fully accessible.
    ///
    /// Returns the resolved [`StubTarget`] alongside the disposition so
    /// the F66 Proxy arm doesn't re-resolve: the stub is the size
    /// authority for hole resolution on proxied reads and the set_len
    /// target after extending proxied writes.
    fn stub_io_disposition(
        &self,
        context: &CompoundContext,
        op: &str,
    ) -> (crate::pnfs::FallbackIoDisposition, Option<StubTarget>) {
        use crate::pnfs::FallbackIoDisposition as D;
        let pnfs = match &self.pnfs_handler {
            Some(p) => p,
            None => return (D::Serve, None),
        };
        let fh = match &context.current_fh {
            Some(fh) => fh,
            None => return (D::Serve, None),
        };
        let path = match self.file_handler.fh_manager().resolve_handle(fh) {
            Ok(p) => p,
            Err(_) => return (D::Serve, None),
        };
        let export = self.file_handler.fh_manager().get_export_path().to_path_buf();
        let file_key = path
            .strip_prefix(&export)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if file_key.is_empty() {
            return (D::Serve, None);
        }
        let disp = match pnfs.fallback_io_disposition(&file_key) {
            D::Serve => D::Serve,
            D::Delay => {
                warn!(
                    "⛔ {} through MDS refused for striped file '{}' — data lives on the DSes, the local file is a sparse stub. NFS4ERR_DELAY (pinned DS down, within the bounded window)",
                    op, file_key
                );
                D::Delay
            }
            D::FailFast => {
                warn!(
                    "⛔ {} through MDS failed fast for striped file '{}' — pinned DSes down past the ceiling, or the fallback proxy is off/unconfigured; NFS4ERR_IO is the client's recovery signal (runbook: the DELAY livelock)",
                    op, file_key
                );
                D::FailFast
            }
            // F66: healthy fleet — apply the I/O to the stripes through
            // the DsControl proxy instead of refusing a legal fallback
            // (the straggler-EIO fsx failure). The stub path rides
            // along: it is the size authority for hole resolution and
            // the set_len target for extending writes.
            D::Proxy => {
                // Per-op line at debug only: the proxy lane runs at
                // full RPC rate (runbg: ~3000 ops/s), and the F68a
                // meter + reporter carry the signal at INFO/WARN.
                debug!("🔁 {} through MDS for striped '{}' → fallback proxy", op, file_key);
                D::Proxy
            }
        };
        (disp, Some(StubTarget { file_key, path }))
    }

    #[allow(clippy::too_many_arguments)]
    /// Map a wire `layout_type` onto the one layout type this server
    /// actually serves, for the operations that *emit* a layout-typed
    /// body: LAYOUTGET and GETDEVICEINFO.
    ///
    /// We advertise `[LAYOUT4_NFSV4_1_FILES]` and nothing else — both
    /// `FATTR4_FS_LAYOUT_TYPES` encoder arms in `operations/fileops.rs`
    /// call the shared `encode_fs_layout_types`, which emits the
    /// one-element array `[1]` — and both replies are encoded
    /// as files-layout structures (`nfsv4_1_file_layout4`,
    /// `nfsv4_1_file_layout_ds_addr4`).
    ///
    /// So anything other than type 1 must be refused *here*. Accepting
    /// it and then encoding a files-layout body is worse than refusing:
    /// the client is told NFS4_OK and handed a structure that is not
    /// the one it asked for. Type 4 (FFLv4, RFC 8435) was accepted this
    /// way — LAYOUTGET answered it with a body tagged type 1, and
    /// GETDEVICEINFO echoed type 4 back over a files-layout device
    /// address. Latent only because nothing advertises type 4; one flag
    /// away from a client acting on a mislabelled body.
    ///
    /// RFC 8881 §18.43.3 (LAYOUTGET) and §18.40.3 (GETDEVICEINFO):
    /// NFS4ERR_UNKNOWN_LAYOUTTYPE is the error for a layout type the
    /// server does not support. NFS4ERR_NOTSUPP, which this used to
    /// return for types 2 and 3, is the generic "op not supported" and
    /// says nothing about *why*.
    ///
    /// LAYOUTRETURN deliberately does not use this — see the note there.
    /// (Since the pnfs-block class: type 5, LAYOUT4_SCSI, is also in
    /// the served set — but only for scsi-class VOLUMES. This function
    /// answers "does this server speak the type at all"; the per-volume
    /// policing — a files volume refuses 5, a scsi volume refuses 1 —
    /// happens in the handlers, where the file's class is known.)
    fn layout_type_served(
        layout_type: u32,
    ) -> Result<crate::pnfs::mds::layout::LayoutType, Nfs4Status> {
        match layout_type {
            1 => Ok(crate::pnfs::mds::layout::LayoutType::NfsV4_1Files),
            5 => Ok(crate::pnfs::mds::layout::LayoutType::Scsi),
            _ => Err(Nfs4Status::UnknownLayoutType),
        }
    }

    async fn handle_layoutget(
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
        use crate::pnfs::mds::layout::{LayoutOwner, IoMode};
        use crate::nfs::xdr::XdrEncoder;
        
        // Check if pNFS handler is available
        let pnfs = match &self.pnfs_handler {
            Some(handler) => handler,
            None => {
                warn!("❌ LAYOUTGET requested but pNFS not configured");
                return OperationResult::LayoutGet(Nfs4Status::NotSupp, None);
            }
        };
        
        debug!("📥📥📥 LAYOUTGET RECEIVED 📥📥📥");
        debug!("📥 LAYOUTGET: offset={}, length={}, iomode={}, layout_type={}", offset, length, iomode, layout_type);
        
        // Get current filehandle
        let (filehandle, file_key, fs_path) = match context.current_fh {
            Some(ref fh) => {
                // Export-relative path — the stable identity that keys
                // the file's pinned stripe placement. Raw FH bytes
                // won't do: they embed the server instance id.
                let path = match self.file_handler.fh_manager().resolve_handle(fh) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("❌ LAYOUTGET: stale/invalid CFH: {}", e);
                        return OperationResult::LayoutGet(Nfs4Status::Stale, None);
                    }
                };
                let export = self.file_handler.fh_manager().get_export_path().to_path_buf();
                let mut file_key = path
                    .strip_prefix(&export)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                if file_key.is_empty() {
                    // LAYOUTGET on the export root itself (kernel
                    // probes it at mount time) — keep the key non-empty
                    // so the placement table stays unambiguous.
                    file_key = "/".to_string();
                }
                (fh.data.clone(), file_key, path)
            }
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

        // Per-volume class dispatch (design doc §5): the scsi grant
        // path. The allocator's grant rows are the authority on what
        // the client holds; the layout state machine entry minted here
        // is the RECALL HANDLE (stateid ↔ owner/file), so nothing is
        // ever granted that CB_LAYOUTRECALL cannot find.
        if pnfs.layout_class_for(&file_key) == crate::pnfs::mds::layout::LayoutClass::Scsi {
            if layout_type != 5 {
                warn!(
                    "❌ LAYOUTGET type {} on scsi-class volume (file '{}') — serves 5 only",
                    layout_type, file_key
                );
                return OperationResult::LayoutGet(Nfs4Status::UnknownLayoutType, None);
            }
            return self
                .handle_layoutget_scsi(
                    pnfs, &file_key, &fs_path, filehandle, iomode, offset, length,
                    _minlength, context,
                )
                .await;
        }

        // The converse policing: a files-class volume serves type 1
        // only. Refusing here keeps the status honest
        // (UNKNOWN_LAYOUTTYPE, not a mapped-away LAYOUTUNAVAILABLE).
        if layout_type == 5 {
            warn!(
                "❌ LAYOUTGET type 5 on files-class volume (file '{}') — serves 1 only",
                file_key
            );
            return OperationResult::LayoutGet(Nfs4Status::UnknownLayoutType, None);
        }

        // Convert arguments
        let args = LayoutGetArgs {
            signal_layout_avail: _signal_layout_avail,
            layout_type: match Self::layout_type_served(layout_type) {
                Ok(lt) => lt,
                Err(status) => {
                    warn!("❌ LAYOUTGET for layout type {} — we serve FILES (1) only",
                          layout_type);
                    return OperationResult::LayoutGet(status, None);
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
            file_key,
            owner,
        };
        
        // F68a: layout grants/refusals are THE discriminator for a
        // fallback flip — they show whether a client doing MDS I/O
        // stopped asking for layouts (client-side latch) or was
        // refused them (server-side). Low-rate (per open), so INFO.
        let lg_file = args.file_key.clone();
        let lg_iomode = args.iomode;

        // Call pNFS handler
        match pnfs.layoutget(args) {
            Ok(result) => {
                if let Some(m) = &self.f68a {
                    m.layoutget_granted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                info!(
                    "📄 LAYOUTGET granted: '{}' iomode {:?} ({} layout(s))",
                    lg_file, lg_iomode, result.layouts.len()
                );
                debug!("   Available data servers: {}", result.layouts.len());
                
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

                    if layout.segments.is_empty() {
                        warn!("❌ Layout has no segments!");
                        return OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None);
                    }

                    debug!("   📤 Encoding FILE layout (RFC 5661 §13.3) with {} segments",
                          layout.segments.len());

                    // Stripe unit + composite deviceid come from the
                    // file's pinned placement (carried on the Layout),
                    // NOT the live config/registry — they must match
                    // what GETDEVICEINFO will resolve for this group.
                    let layout_content = Self::encode_file_layout_striped(
                        &layout.segments,
                        &filehandle,
                        layout.stripe_unit,
                        layout.device_id_bin,
                        layout.file_id,
                    );

                    debug!("   📤 FILE layout content encoded: {} bytes", layout_content.len());
                    encoder.encode_opaque(&layout_content);
                }
                
                let final_response = encoder.finish();
                debug!("✅ LAYOUTGET successful: {} layouts returned", result.layouts.len());
                debug!("✅ Total LAYOUTGET response: {} bytes", final_response.len());
                debug!("✅ Response hex (first 128 bytes): {:02x?}", &final_response[..final_response.len().min(128)]);
                OperationResult::LayoutGet(Nfs4Status::Ok, Some(final_response))
            }
            Err(e) => {
                if let Some(m) = &self.f68a {
                    m.layoutget_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                warn!(
                    "❌ LAYOUTGET refused: '{}' iomode {:?} → {:?}",
                    lg_file, lg_iomode, e
                );
                let status = match e {
                    // Truncate-dirty gate: transient — the client
                    // retries (or falls back to MDS I/O, which parks on
                    // the same gate) until the DS stripe truncation is
                    // confirmed.
                    crate::pnfs::mds::operations::LayoutGetError::TryLater => {
                        Nfs4Status::LayoutTrylater
                    }
                    _ => Nfs4Status::LayoutUnavail,
                };
                OperationResult::LayoutGet(status, None)
            }
        }
    }
    
    /// The scsi layout body (RFC 8154 §2.4, `pnfs_scsi_layout4`): the
    /// extent list, each extent carrying the volume deviceid, the
    /// file/storage offsets, and its state — INVALID_DATA until the
    /// client's LAYOUTCOMMIT promotes it (reads of INVALID extents are
    /// zeros client-side, which is what makes an uncommitted grant
    /// unobservable).
    fn encode_scsi_layout(
        segments: &[ScsiSegment],
        device_id: &[u8; 16],
    ) -> bytes::Bytes {
        use crate::nfs::xdr::XdrEncoder;
        let mut e = XdrEncoder::new();
        e.encode_u32(segments.len() as u32);
        for s in segments {
            e.encode_fixed_opaque(device_id); // se_vol_id (deviceid4, fixed 16)
            e.encode_u64(s.file_offset); // se_file_offset
            e.encode_u64(s.length); // se_length
            e.encode_u64(s.storage_offset); // se_storage_offset
            e.encode_u32(s.state); // pnfs_scsi_extent_state4
        }
        e.finish()
    }

    /// Build a READ layout's segment list: committed extents as
    /// READ_DATA, every gap — leading, interior, trailing — as NONE_DATA
    /// (reads as zeros client-side). Kernel-verified constraints
    /// (`verify_extent`, v6.14/v7.0): a read layout REFUSES
    /// RW_DATA/INVALID_DATA outright, and extents must tile the layout
    /// range contiguously from its offset — which is why the holes must
    /// be filled rather than skipped.
    fn scsi_read_segments(
        extents: &[crate::state_backend::extent_alloc::GrantedExtent],
        offset: u64,
        length: u64,
    ) -> Vec<ScsiSegment> {
        const READ_DATA: u32 = 1;
        const NONE_DATA: u32 = 3;
        let end = offset.saturating_add(length);
        let mut segs = Vec::new();
        let mut cursor = offset;
        for x in extents {
            // Clip to the requested window; the allocator returns whole
            // extents, which may start before `offset` or run past `end`.
            let x_start = x.logical_offset.max(offset);
            let x_end = (x.logical_offset + x.length).min(end);
            if x_end <= x_start {
                continue;
            }
            if x_start > cursor {
                segs.push(ScsiSegment {
                    file_offset: cursor,
                    length: x_start - cursor,
                    storage_offset: 0,
                    state: NONE_DATA,
                });
            }
            segs.push(ScsiSegment {
                file_offset: x_start,
                length: x_end - x_start,
                storage_offset: x.physical_offset + (x_start - x.logical_offset),
                state: READ_DATA,
            });
            cursor = x_end;
        }
        if cursor < end {
            segs.push(ScsiSegment {
                file_offset: cursor,
                length: end - cursor,
                storage_offset: 0,
                state: NONE_DATA,
            });
        }
        segs
    }

    /// The scsi-class LAYOUTGET: allocate extents (fresh space only —
    /// reuse stays locked until the MDS initiator can write_zeroes,
    /// per GrantedExtent::needs_scrub), mint the recall handle, encode
    /// pnfs_scsi_layout4. READ iomode takes the non-allocating query
    /// and the READ_DATA/NONE_DATA presentation instead.
    #[allow(clippy::too_many_arguments)]
    async fn handle_layoutget_scsi(
        &self,
        pnfs: &std::sync::Arc<dyn crate::pnfs::PnfsOperations>,
        file_key: &str,
        fs_path: &std::path::Path,
        filehandle: Vec<u8>,
        iomode: u32,
        offset: u64,
        length: u64,
        minlength: u64,
        context: &CompoundContext,
    ) -> OperationResult {
        use crate::nfs::xdr::XdrEncoder;
        use crate::pnfs::mds::layout::{IoMode, LayoutOwner};

        let refused = |m: &Option<std::sync::Arc<crate::pnfs::mds::f68a_meter::DataPathMeter>>| {
            if let Some(m) = m {
                m.layoutget_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        };

        let im = match iomode {
            1 => IoMode::Read,
            2 => IoMode::ReadWrite,
            // ANY is a LAYOUTRETURN concept; a grant must pick.
            _ => return OperationResult::LayoutGet(Nfs4Status::BadIoMode, None),
        };
        let Some(backend) = pnfs.extent_backend() else {
            refused(&self.f68a);
            warn!("❌ LAYOUTGET (scsi) '{}': handler has no extent backend", file_key);
            return OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None);
        };
        let Some(volume) = file_key.split('/').find(|c| !c.is_empty()).map(str::to_string)
        else {
            return OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None);
        };
        let (client_id, session_id) = match context.session_id {
            Some(sid) => (
                self.state_mgr.sessions.get_session(&sid).map(|s| s.client_id).unwrap_or(0),
                sid.0,
            ),
            None => return OperationResult::LayoutGet(Nfs4Status::OpNotInSession, None),
        };
        if client_id == 0 {
            return OperationResult::LayoutGet(Nfs4Status::OpNotInSession, None);
        }
        // file_id = the stub's inode — the identity the fileid attr
        // reports and the extent tables key on.
        use std::os::unix::fs::MetadataExt;
        let file_id = match std::fs::metadata(fs_path) {
            Ok(m) => m.ino(),
            Err(e) => {
                warn!("❌ LAYOUTGET (scsi): stat {:?}: {}", fs_path, e);
                return OperationResult::LayoutGet(Nfs4Status::Stale, None);
            }
        };
        // Grant window: the desired length capped at 1 GiB per grant
        // (to-EOF requests would otherwise allocate the whole arena),
        // floored at minlength (RFC: the server must satisfy at least
        // that), clamped into the allocator's i64 domain. NoSpace past
        // the ceiling is the allocator's honest answer.
        let i64_room = (i64::MAX as u64).saturating_sub(offset);
        let want = length.min(1 << 30).max(minlength).min(i64_room);
        if want == 0 {
            return OperationResult::LayoutGet(Nfs4Status::Inval, None);
        }

        // Host admission BEFORE the grant transaction (design doc §5:
        // the grant lifecycle drives the export allow-list). Ordered
        // this way so a failed admission leaves no grant behind — the
        // client simply retries. A client whose NVMe identity we cannot
        // derive can never reach the device, so granting it extents
        // would be theater: LAYOUTUNAVAIL is the honest refusal.
        let Some(host_nqn) = self
            .state_mgr
            .clients
            .get_client(client_id)
            .and_then(|c| Self::hostnqn_from_co_ownerid(&c.owner))
        else {
            refused(&self.f68a);
            warn!(
                "❌ LAYOUTGET (scsi) '{}': cannot derive an NVMe host identity for \
                 client {} — expected the Linux uniform co_ownerid shape",
                file_key, client_id
            );
            return OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None);
        };
        if let Err(e) = pnfs.admit_block_host(&volume, client_id, &host_nqn).await {
            refused(&self.f68a);
            warn!(
                "❌ LAYOUTGET (scsi) '{}': host admission of {} did not converge: {} — \
                 TRYLATER",
                file_key, host_nqn, e
            );
            return OperationResult::LayoutGet(Nfs4Status::LayoutTrylater, None);
        }

        // The two iomodes take different allocator paths on purpose: a
        // WRITE grant allocates fresh space; a READ grant must never
        // allocate (a kernel's big-window LAYOUTGET on a small file
        // would mint arena for zeros) and must present committed bytes
        // only — the kernel refuses RW/INVALID states in read layouts.
        let grant_result = if im == IoMode::Read {
            backend.extent_grant_read(&volume, file_id, client_id, offset, want).await
        } else {
            backend.extent_grant(&volume, file_id, client_id, offset, want, true).await
        };
        match grant_result {
            Ok(Ok(extents)) => {
                if extents.iter().any(|x| x.needs_scrub) {
                    // fresh_only=true makes this unreachable; if it ever
                    // fires, refusing beats shipping a prior owner's
                    // bytes (BlindProvision, in the flesh).
                    tracing::error!(
                        "LAYOUTGET (scsi) '{}': fresh_only grant returned needs_scrub \
                         extents — refusing",
                        file_key
                    );
                    refused(&self.f68a);
                    return OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None);
                }
                let owner = LayoutOwner { client_id, session_id, fsid: 1 };
                let Some(stateid) =
                    pnfs.register_scsi_layout(owner, filehandle, file_key, im)
                else {
                    refused(&self.f68a);
                    return OperationResult::LayoutGet(Nfs4Status::LayoutUnavail, None);
                };
                if let Some(m) = &self.f68a {
                    m.layoutget_granted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let (lo_offset, lo_length, segments) = if im == IoMode::Read {
                    // The read layout covers exactly the requested window;
                    // holes read as zeros via NONE_DATA fillers.
                    (offset, want, Self::scsi_read_segments(&extents, offset, want))
                } else {
                    let span_start =
                        extents.iter().map(|x| x.logical_offset).min().unwrap_or(offset);
                    let span_end = extents
                        .iter()
                        .map(|x| x.logical_offset + x.length)
                        .max()
                        .unwrap_or(offset + want);
                    let segs = extents
                        .iter()
                        .map(|x| ScsiSegment {
                            file_offset: x.logical_offset,
                            length: x.length,
                            storage_offset: x.physical_offset,
                            // READ_WRITE_DATA = 0, INVALID_DATA = 2
                            state: if x.committed { 0 } else { 2 },
                        })
                        .collect();
                    (span_start, span_end - span_start, segs)
                };
                info!(
                    "📄 LAYOUTGET (scsi) granted: '{}' {:?} [{}, {}) {} segment(s), client {}",
                    file_key,
                    im,
                    lo_offset,
                    lo_offset + lo_length,
                    segments.len(),
                    client_id
                );
                let device_id = crate::nvmeof_export::scsi_device_id(&volume);
                let mut e = XdrEncoder::new();
                e.encode_bool(true); // return_on_close (matches the handle)
                e.encode_fixed_opaque(&stateid);
                e.encode_u32(1); // one layout4
                e.encode_u64(lo_offset);
                e.encode_u64(lo_length);
                e.encode_u32(iomode);
                e.encode_u32(5); // LAYOUT4_SCSI
                e.encode_opaque(&Self::encode_scsi_layout(&segments, &device_id));
                OperationResult::LayoutGet(Nfs4Status::Ok, Some(e.finish()))
            }
            Ok(Err(verdict)) => {
                refused(&self.f68a);
                use crate::state_backend::extent_alloc::ExtentAllocError as E;
                warn!("❌ LAYOUTGET (scsi) refused: '{}': {}", file_key, verdict);
                let status = match verdict {
                    // Held by another client: transient once the recall
                    // machinery drives conflicting holders; the client
                    // retries or falls back to MDS I/O meanwhile.
                    E::Conflict { .. } | E::NotQuiescent { .. } | E::FencedClient => {
                        Nfs4Status::LayoutTrylater
                    }
                    // Arena full: not transient — fall back to MDS I/O,
                    // which reports ENOSPC with a real errno.
                    E::NoSpace { .. } => Nfs4Status::LayoutUnavail,
                    E::InvalidRange(_) => Nfs4Status::Inval,
                    E::CommitRejected(_) | E::Corruption(_) | E::Sql(_) => {
                        Nfs4Status::ServerFault
                    }
                };
                OperationResult::LayoutGet(status, None)
            }
            Err(e) => {
                refused(&self.f68a);
                warn!("❌ LAYOUTGET (scsi) backend error: '{}': {}", file_key, e);
                OperationResult::LayoutGet(Nfs4Status::ServerFault, None)
            }
        }
    }

    /// Derive the client node's flint host NQN from its NFSv4.1+
    /// co_ownerid. The kernel's uniform client string (verified against
    /// v6.11 fs/nfs/nfs4proc.c, `nfs4_init_uniform_client_string`) is
    /// `"Linux NFSv4.<minor> <nodename>"`, or with `nfs.nfs4_unique_id`
    /// set `"Linux NFSv4.<minor> <uniquifier>/<nodename>"` — the
    /// nodename is always the LAST '/'-component. flint's fleet
    /// discipline closes the loop: node name == kernel hostname, and
    /// every flint initiator connects as `flint_host_nqn(node name)`
    /// (csi-node and the rig both), so the MDS can derive the NVMe
    /// identity with zero registration protocol. Anything else — v4.0's
    /// nonuniform shape, pynfs's custom owners, non-Linux clients —
    /// refuses (`None`): no admission is safer than a guessed one.
    fn hostnqn_from_co_ownerid(owner: &[u8]) -> Option<String> {
        let s = std::str::from_utf8(owner).ok()?;
        let rest = s
            .strip_prefix("Linux NFSv4.1 ")
            .or_else(|| s.strip_prefix("Linux NFSv4.2 "))?;
        let nodename = rest.rsplit('/').next()?.trim();
        if nodename.is_empty() {
            return None;
        }
        Some(crate::nvmeof_export::flint_host_nqn(nodename))
    }

    /// Frame a GETDEVICEINFO success body, honouring `maxcount`: the
    /// client's declared ceiling for the WHOLE res body. Ignoring it —
    /// the historical behaviour — was harmless while the files device
    /// address was tiny; scsi volume-topology bodies made TOOSMALL
    /// handling real (design doc §5). TOOSMALL replies carry
    /// gdir_mincount so the client can retry sized right.
    fn frame_getdeviceinfo_reply(
        layout_type: u32,
        dev_addr_encoded: &[u8],
        maxcount: u32,
    ) -> OperationResult {
        use crate::nfs::xdr::XdrEncoder;
        let padded = (dev_addr_encoded.len() + 3) & !3;
        let total = 4 + 4 + padded + 4; // type + opaque<len,body> + empty notify bitmap
        if maxcount > 0 && total as u64 > maxcount as u64 {
            let mut e = XdrEncoder::new();
            e.encode_u32(total as u32); // gdir_mincount
            return OperationResult::GetDeviceInfo(Nfs4Status::TooSmall, Some(e.finish()));
        }
        let mut e = XdrEncoder::new();
        e.encode_u32(layout_type);
        e.encode_opaque(dev_addr_encoded);
        e.encode_u32(0); // empty notification bitmap
        OperationResult::GetDeviceInfo(Nfs4Status::Ok, Some(e.finish()))
    }

    /// The scsi-class device address (RFC 8154 §2.2.2, NVMe designators
    /// per RFC 9561 §2.1): one BASE volume whose designator is the
    /// volume's NGUID in EUI64 form — the same 16 bytes the namespace
    /// carries on `nvmf_subsystem_add_ns`, so the kernel's device
    /// matching succeeds by construction. `pr_key` is the caller's
    /// reservation key (per-client, RFC 8154: GETDEVICEINFO is the key
    /// distribution channel).
    fn encode_scsi_device_addr(nguid: &[u8; 16], pr_key: u64) -> bytes::Bytes {
        use crate::nfs::xdr::XdrEncoder;
        let mut e = XdrEncoder::new();
        e.encode_u32(1); // sda_volumes<>: one entry
        e.encode_u32(4); // PNFS_SCSI_VOLUME_BASE
        e.encode_u32(1); // sd_code_set = PS_CODE_SET_BINARY
        e.encode_u32(2); // sd_designator_type = PS_DESIGNATOR_EUI64
        e.encode_opaque(nguid); // sd_designator<>: 16 octets = NGUID form
        e.encode_u64(pr_key); // sbv_pr_key
        e.finish()
    }

    fn handle_getdeviceinfo(
        &self,
        device_id: Vec<u8>,
        layout_type: u32,
        _maxcount: u32,
        _notify_types: Vec<u32>,
        context: &CompoundContext,
    ) -> OperationResult {
        use crate::pnfs::mds::operations::GetDeviceInfoArgs;
        use crate::pnfs::mds::device::DeviceId;
        
        // Check if pNFS handler is available
        let pnfs = match &self.pnfs_handler {
            Some(handler) => handler,
            None => {
                warn!("❌ GETDEVICEINFO requested but pNFS not configured");
                return OperationResult::GetDeviceInfo(Nfs4Status::NotSupp, None);
            }
        };
        
        debug!("🔥🔥🔥 GETDEVICEINFO RECEIVED! 🔥🔥🔥");
        debug!("📥 GETDEVICEINFO: device_id len={}, layout_type={}", device_id.len(), layout_type);
        debug!("📥 Device ID bytes: {:02x?}", device_id);
        
        // Convert device_id to [u8; 16]
        let mut dev_id: DeviceId = [0; 16];
        if device_id.len() >= 16 {
            dev_id.copy_from_slice(&device_id[0..16]);
        } else {
            warn!("❌ Invalid device_id length: {}", device_id.len());
            return OperationResult::GetDeviceInfo(Nfs4Status::NoEnt, None);
        }

        // scsi-class branch: the deviceid IS the volume's NGUID, so it
        // resolves by geometry scan — nothing remembered, restart-proof.
        // The pr_key handed out is the caller's client id: stable,
        // unique per client (server-minted, non-zero), and exactly what
        // the phase-2 reservation machinery will register and preempt.
        if layout_type == 5 {
            let Some(volume) = pnfs.scsi_volume_for_deviceid(&dev_id) else {
                warn!("❌ GETDEVICEINFO (scsi): unknown deviceid {:02x?}", dev_id);
                return OperationResult::GetDeviceInfo(Nfs4Status::NoEnt, None);
            };
            let pr_key = match context.session_id {
                Some(sid) => self
                    .state_mgr
                    .sessions
                    .get_session(&sid)
                    .map(|s| s.client_id)
                    .unwrap_or(0),
                None => 0,
            };
            if pr_key == 0 {
                warn!("❌ GETDEVICEINFO (scsi) without a session — no pr_key to hand out");
                return OperationResult::GetDeviceInfo(Nfs4Status::OpNotInSession, None);
            }
            let body = Self::encode_scsi_device_addr(&dev_id, pr_key);
            info!("📡 GETDEVICEINFO (scsi): volume '{}' → BASE/NGUID device", volume);
            return Self::frame_getdeviceinfo_reply(layout_type, &body, _maxcount);
        }

        let args = GetDeviceInfoArgs {
            device_id: dev_id,
            layout_type: match Self::layout_type_served(layout_type) {
                Ok(lt) => lt,
                Err(status) => {
                    warn!("❌ GETDEVICEINFO for layout type {} — we serve FILES (1) only",
                          layout_type);
                    return OperationResult::GetDeviceInfo(status, None);
                }
            },
            maxcount: _maxcount,
            notify_types: _notify_types,
        };
        
        match pnfs.getdeviceinfo(args) {
            Ok(result) => {
                let dev_addr_encoded = Self::encode_device_addr(&result.device_addr);
                debug!("✅ GETDEVICEINFO successful");
                Self::frame_getdeviceinfo_reply(layout_type, &dev_addr_encoded, _maxcount)
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

    async fn handle_layoutreturn(
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

        // NOT `layout_type_served`, deliberately. That guard exists
        // because LAYOUTGET and GETDEVICEINFO *emit* a layout-typed
        // body and must not mislabel it. LAYOUTRETURN emits nothing —
        // the body travels client→server, and we decode it as whatever
        // the client says it is, so there is nothing to mislabel.
        //
        // Staying lenient on type 4 is what lets a client hand back a
        // layout this server itself granted before `cdbbe21`, when
        // FFLv4 was briefly advertised. Refusing it would strand that
        // layout as unreturnable state on a server that has no other
        // way to learn the client is done with it. Return paths should
        // accept more than request paths grant.
        let lt = match layout_type {
            1 => LayoutType::NfsV4_1Files,
            4 => LayoutType::FlexFiles,  // RFC 8435 — accepted on return only
            5 => {
                let Some(pnfs) = &self.pnfs_handler else {
                    return OperationResult::LayoutReturn(Nfs4Status::Ok);
                };
                match return_body {
                    LayoutReturn4Body::File { offset, length, stateid, .. } => {
                        let mut sid = [0u8; 16];
                        sid[0..4].copy_from_slice(&stateid.seqid.to_be_bytes());
                        sid[4..16].copy_from_slice(&stateid.other);
                        let Some((owner_client, file_ident)) = pnfs.take_scsi_layout(&sid)
                        else {
                            // Already returned or server-side revoked —
                            // the benign shape, same as the files path
                            // after a revoke.
                            debug!("LAYOUTRETURN (scsi): unknown stateid — treating as done");
                            return OperationResult::LayoutReturn(Nfs4Status::Ok);
                        };
                        // Drop the allocator's grant rows: this is the
                        // client's quiescence promise, and what lets a
                        // reclaim of these extents complete cleanly
                        // instead of quarantining.
                        let Some(backend) = pnfs.extent_backend() else {
                            return OperationResult::LayoutReturn(Nfs4Status::Ok);
                        };
                        let volume = file_ident
                            .split('/')
                            .find(|c| !c.is_empty())
                            .unwrap_or("")
                            .to_string();
                        let export =
                            self.file_handler.fh_manager().get_export_path().to_path_buf();
                        use std::os::unix::fs::MetadataExt;
                        let file_id = std::fs::metadata(export.join(&file_ident))
                            .map(|m| m.ino())
                            .unwrap_or(0);
                        if volume.is_empty() || file_id == 0 {
                            // File already removed: the grant rows will be
                            // swept by the reclaim/lease path. Loud, not
                            // fatal — the return itself succeeds.
                            warn!(
                                "LAYOUTRETURN (scsi): '{}' unresolvable — grant rows \
                                 left for the reclaim sweep",
                                file_ident
                            );
                            return OperationResult::LayoutReturn(Nfs4Status::Ok);
                        }
                        match backend
                            .extent_layout_return(&volume, file_id, owner_client, offset, length.min((i64::MAX as u64).saturating_sub(offset)))
                            .await
                        {
                            Ok(Ok(n)) => {
                                info!(
                                    "📄 LAYOUTRETURN (scsi): '{}' dropped {} grant row(s), \
                                     client {}",
                                    file_ident, n, owner_client
                                );
                            }
                            Ok(Err(e)) => {
                                tracing::error!(
                                    "LAYOUTRETURN (scsi) '{}': allocator refused: {} — \
                                     grant rows leak until the reclaim sweep",
                                    file_ident, e
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "LAYOUTRETURN (scsi) '{}': backend error: {} — \
                                     grant rows leak until the reclaim sweep",
                                    file_ident, e
                                );
                            }
                        }
                        return OperationResult::LayoutReturn(Nfs4Status::Ok);
                    }
                    LayoutReturn4Body::Fsid | LayoutReturn4Body::All => {
                        // Bulk scsi returns (unmount): per-stateid rows are
                        // dropped as each File return arrives; a client
                        // that skips straight to ALL leaves grant rows for
                        // the lease/reclaim sweep. Accept — refusing would
                        // error a benign cleanup — and say so.
                        warn!(
                            "LAYOUTRETURN (scsi) {:?}: bulk return accepted; any \
                             remaining grant rows await the lease/reclaim sweep (owed)",
                            if matches!(return_body, LayoutReturn4Body::All) { "ALL" } else { "FSID" }
                        );
                        return OperationResult::LayoutReturn(Nfs4Status::Ok);
                    }
                }
            }
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
                if let Some(m) = &self.f68a {
                    m.layouts_returned.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                info!("📥 LAYOUTRETURN ok (client_id={:#x})", client_id);
                OperationResult::LayoutReturn(Nfs4Status::Ok)
            }
            Err(e) => {
                // SERVERFAULT was wrong for every one of these and actively
                // harmful for BadStateId: RFC 8881 wants BAD_STATEID / a
                // no-matching-layout answer, which clients treat as benign,
                // and Linux compounds LAYOUTRETURN into CLOSE — a failed op
                // aborts the compound and the CLOSE behind it never runs
                // (audit R3).
                use crate::pnfs::mds::operations::LayoutReturnError;
                let status = match e {
                    LayoutReturnError::BadStateId => Nfs4Status::BadStateId,
                    LayoutReturnError::UnknownLayoutType => Nfs4Status::UnknownLayoutType,
                    LayoutReturnError::Inval => Nfs4Status::Inval,
                };
                warn!("LAYOUTRETURN failed: {:?} → {:?}", e, status);
                OperationResult::LayoutReturn(status)
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
    async fn handle_layoutcommit(
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

        // AUDIT C4: never extend the stub past a cut that is still landing
        // on the DSes. The recall a truncate fires is what prompts the
        // client to LAYOUTCOMMIT in the first place, so this is the common
        // ordering, not a rare one. The truncate is the newer operation and
        // wins; the commit's tail is bytes that are being deleted anyway.
        let commit_ceiling = self
            .pnfs_current_fh_key(context)
            .and_then(|k| self.pnfs_handler.as_ref().and_then(|p| p.truncate_gate_ceiling(&k)));

        // pnfs-block (scsi class): the allocator half runs FIRST, and a
        // refusal aborts before any size change — the transactional
        // coupling FlintExtents' UngatedSize mutation exists for (the
        // half-stub world is exactly "the size half lands while the
        // range half refuses"). Two stores are involved (sqlite, then
        // the stub file), so the coupling is ORDERING: size never
        // advances on a refused commit, and a crash between the two
        // leaves promotion-without-size — the safe direction (committed
        // extents beyond size are simply not yet visible).
        if let Some(key) = self.pnfs_current_fh_key(context) {
            let scsi = self
                .pnfs_handler
                .as_ref()
                .map(|p| p.layout_class_for(&key) == crate::pnfs::mds::layout::LayoutClass::Scsi)
                .unwrap_or(false);
            if scsi {
                if _layout_type != 5 {
                    warn!("❌ LAYOUTCOMMIT type {} on scsi-class volume — serves 5 only", _layout_type);
                    return OperationResult::LayoutCommit(Nfs4Status::UnknownLayoutType, None);
                }
                let backend = match self.pnfs_handler.as_ref().and_then(|p| p.extent_backend()) {
                    Some(b) => b,
                    None => {
                        warn!("❌ LAYOUTCOMMIT (scsi): no extent backend on this handler");
                        return OperationResult::LayoutCommit(Nfs4Status::ServerFault, None);
                    }
                };
                let client_id = match context.session_id {
                    Some(sid) => self
                        .state_mgr
                        .sessions
                        .get_session(&sid)
                        .map(|s| s.client_id)
                        .unwrap_or(0),
                    None => 0,
                };
                if client_id == 0 {
                    return OperationResult::LayoutCommit(Nfs4Status::OpNotInSession, None);
                }
                let Some(volume) =
                    key.split('/').find(|c| !c.is_empty()).map(str::to_string)
                else {
                    return OperationResult::LayoutCommit(Nfs4Status::BadLayout, None);
                };
                // file_id = the stub's inode, the same identity the
                // fileid attribute reports — stable across rename,
                // fresh per re-create, which is what keys the extent
                // tables per file.
                use std::os::unix::fs::MetadataExt;
                let file_id = match std::fs::metadata(&path) {
                    Ok(m) => m.ino(),
                    Err(e) => {
                        warn!("❌ LAYOUTCOMMIT (scsi): stat {:?}: {}", path, e);
                        return OperationResult::LayoutCommit(Nfs4Status::Stale, None);
                    }
                };
                // Clamp the range into the allocator's i64 domain;
                // u64::MAX means to-EOF and every real commit list fits.
                let length = _length.min((i64::MAX as u64).saturating_sub(_offset));
                match backend
                    .extent_commit(&volume, file_id, client_id, _offset, length)
                    .await
                {
                    Ok(Ok(promoted)) => {
                        info!(
                            "📥 LAYOUTCOMMIT (scsi): '{}' [{}, +{}) promoted {} extent(s)",
                            key, _offset, length, promoted
                        );
                        // Fall through to the shared size path below.
                    }
                    Ok(Err(verdict)) => {
                        // The (client, gen)-validation refused: a stale,
                        // fenced, or forged commit. BADLAYOUT, and no
                        // size change happens — that is the point.
                        warn!("❌ LAYOUTCOMMIT (scsi) refused: '{}': {}", key, verdict);
                        return OperationResult::LayoutCommit(Nfs4Status::BadLayout, None);
                    }
                    Err(e) => {
                        warn!("❌ LAYOUTCOMMIT (scsi) backend error: '{}': {}", key, e);
                        return OperationResult::LayoutCommit(Nfs4Status::ServerFault, None);
                    }
                }
            }
        }

        // The open/set_len/set_times sequence hits the export's backing
        // device — run it on the blocking pool, not an async worker.
        let blocking_path = path.clone();
        let applied = tokio::task::spawn_blocking(move || -> Result<Option<u64>, String> {
            let mut new_size: Option<u64> = None;
            if let Some(lwo) = last_write_offset {
                // last_write_offset is the offset of the *last byte written*
                // (RFC 8881 §18.42.1), so EOF is one past that.
                let candidate = lwo.saturating_add(1);
                // Clamp rather than refuse: a client whose commit straddles a
                // truncate is not in error, and NFS4ERR_DELAY would only make
                // it retry a claim that can never become true.
                let candidate = match commit_ceiling {
                    Some(ceiling) if candidate > ceiling => {
                        debug!(
                            "📥 LAYOUTCOMMIT: clamping {} → {} (truncate to {} still landing)",
                            candidate, ceiling, ceiling,
                        );
                        ceiling
                    }
                    _ => candidate,
                };
                match std::fs::OpenOptions::new().write(true).open(&blocking_path) {
                    Ok(file) => {
                        let cur_size = file.metadata().map(|m| m.len()).unwrap_or(0);
                        if candidate > cur_size {
                            if let Err(e) = file.set_len(candidate) {
                                return Err(format!("set_len({}): {}", candidate, e));
                            }
                            debug!("📥 LAYOUTCOMMIT: extended {:?} {} → {}", blocking_path, cur_size, candidate);
                            new_size = Some(candidate);
                        }
                    }
                    Err(e) => return Err(format!("open: {}", e)),
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
                if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&blocking_path) {
                    let _ = file.set_times(ft);
                }
            }
            Ok(new_size)
        })
        .await;

        let new_size_reported: Option<u64>;
        match applied {
            Ok(Ok(new_size)) => new_size_reported = new_size,
            Ok(Err(e)) => {
                warn!("LAYOUTCOMMIT: {:?}: {}", path, e);
                return OperationResult::LayoutCommit(Nfs4Status::Io, None);
            }
            Err(e) => {
                warn!("LAYOUTCOMMIT: spawn_blocking: {}", e);
                return OperationResult::LayoutCommit(Nfs4Status::ServerFault, None);
            }
        }

        OperationResult::LayoutCommit(Nfs4Status::Ok, new_size_reported)
    }
    
    /// Encode FILE layout with multiple segments for striping across DSes
    /// Per RFC 5661 Section 13.3 - NFSv4.1 File Layout Type
    fn encode_file_layout_striped(
        segments: &[crate::pnfs::mds::layout::LayoutSegment],
        filehandle: &[u8],
        stripe_unit: u64,
        device_id_bytes: [u8; 16],
        file_id: u64,
    ) -> Bytes {
        use crate::nfs::xdr::XdrEncoder;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        if segments.is_empty() {
            warn!("⚠️ encode_file_layout_striped called with no segments!");
            return Bytes::new();
        }

        let mut encoder = XdrEncoder::new();

        // Rotate the stripe pattern per file: without this every file's
        // first stripe lands on DS[0], so any file smaller than one stripe
        // unit (8 MiB — i.e. most files in an ML dataset) lives entirely on
        // the first DS while the rest idle. nfl_first_stripe_index is the
        // protocol-native rotation knob (RFC 8881 §13.4.4): the client maps
        // stripe unit u to device (u + first_stripe_index) % N.
        //
        // The rotation MUST be derived from something immutable for the
        // file's lifetime — every LAYOUTGET ever issued for the file has
        // to agree, or readers reassemble the stripes in a different
        // order than the writer laid them down. For identity-keyed pins
        // that is the file_id (rename-stable; the FILEHANDLE is not —
        // a fresh reader post-rename holds a different FH, which is
        // exactly how the placement drill caught this). Legacy pins
        // (file_id 0) keep the historical FH-derived rotation — their
        // FHs are path-stable because their renames are refused.
        let first_stripe_index = if file_id != 0 {
            // THE shared formula (F66): the fallback proxy targets
            // stripes with the same rotation this encode advertises.
            // The two diverging is a proxied write on the wrong stripe
            // file — silent zeros on the client's next read.
            crate::pnfs::mds::layout::FilePlacement::wire_first_stripe_index(
                file_id,
                segments.len(),
            )
        } else {
            let mut h = DefaultHasher::new();
            filehandle.hash(&mut h);
            (h.finish() % segments.len() as u64) as u32
        };

        debug!("   🔧 Encoding STRIPED FILE layout (RFC 5661 Section 13.3):");
        debug!("      Number of DSes in stripe: {}", segments.len());
        debug!("      device_id binary (16 bytes): {:02x?}", device_id_bytes);
        debug!("      stripe_unit: {} bytes ({} MB)", stripe_unit, stripe_unit / (1024*1024));
        debug!("      first_stripe_index: {} (per-file rotation)", first_stripe_index);
        debug!("      pattern_offset: 0");
        
        // Encode deviceid (16 bytes fixed, no length prefix)
        encoder.encode_fixed_opaque(&device_id_bytes);
        
        // nfl_util: stripe unit size (u32 per RFC 5661)
        encoder.encode_u32(stripe_unit as u32);
        
        // nfl_first_stripe_index: per-file rotation (see above).
        encoder.encode_u32(first_stripe_index);
        
        // nfl_pattern_offset: offset where stripe pattern starts (always 0)
        encoder.encode_u64(0);

        if file_id != 0 {
            // nfl_fh_list: one v2 (file-ID based) filehandle per DS
            // slot. Per RFC 8881 §13.4.2 / Linux filelayout, the FH for
            // stripe unit u is nfl_fh_list[j] where j is the same index
            // that selects the DS — so slot j's FH carries
            // stripe_index=j and the DS stores its stripes at
            // {file_id:016x}.stripe{j}, independent of the file's PATH.
            // That identity-keying is what makes RENAME a pure metadata
            // op and prevents a recreated same-name file from ever
            // reading its predecessor's stripes.
            encoder.encode_u32(segments.len() as u32);
            for j in 0..segments.len() {
                let fh = crate::nfs::v4::filehandle_pnfs::generate_pnfs_filehandle_from_id(
                    0, // instance check disabled DS-side (PNFS_INSTANCE_ID unset)
                    file_id,
                    j as u32,
                );
                encoder.encode_opaque(&fh.data);
            }
            let result = encoder.finish();
            debug!(
                "      📦 Encoded STRIPED FILE layout: {} bytes total, {} v2 fh(s) (file_id {:016x})",
                result.len(),
                segments.len(),
                file_id
            );
            return result;
        }

        // Legacy (file_id == 0) pins: empty nfl_fh_list per RFC 8881
        // §13.4.2 — signals the kernel to use the MDS filehandle (from
        // LAYOUTGET's current_fh) for I/O to all DSes. The DSes accept
        // MDS filehandles via parse_path_lenient (path-rebased storage).
        encoder.encode_u32(0);

        let result = encoder.finish();
        debug!("      📦 Encoded STRIPED FILE layout: {} bytes total, empty nfl_fh_list (legacy pin)", result.len());
        debug!("      📦 First 128 bytes: {:02x?}", &result[..result.len().min(128)]);

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
        
        debug!("   🔧 Encoding FILE layout (RFC 5661 Section 13.2):");
        debug!("      device_id string: '{}'", segment.device_id);
        debug!("      device_id binary (16 bytes): {:02x?}", device_id_bytes);
        debug!("      stripe_unit: {} bytes ({} MB)", stripe_unit, stripe_unit / (1024*1024));
        debug!("      first_stripe_index: {}", segment.stripe_index);
        debug!("      pattern_offset: {}", segment.pattern_offset);
        debug!("      filehandle length: {} bytes", filehandle.len());
        
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
        debug!("      📦 Encoded FILE layout: {} bytes total", result.len());
        debug!("      📦 First 64 bytes: {:02x?}", &result[..result.len().min(64)]);
        
        result
    }
    
    /// Encode striped device address per RFC 5661 Section 13.3
    /// 
    /// For N DSes in stripe pattern:
    /// - stripe_indices = [0, 1, 2, ..., N-1]  // Round-robin across all DSes
    /// - multipath_ds_list = [ [addr0], [addr1], ..., [addrN] ]  // All DS addresses
    /// Encode device address per RFC 8881 §13.2.1.
    ///
    /// nfsv4_1_file_layout_ds_addr4 {
    ///     uint32_t        stripe_indices<>;       // Indices into multipath_ds_list
    ///     multipath_list4 multipath_ds_list<>;    // Array of DS address sets
    /// }
    ///
    /// multipath_list4 {
    ///     netaddr4 ml_naddr<>;    // ALL addresses for ONE DS
    /// }
    ///
    /// One encoder for every shape: a single DS is a 1-entry ds_list, a
    /// stripe group is N entries, and each entry's inner list carries
    /// that DS's multipath addresses — the kernel opens a trunked
    /// transport per extra address (`rpc_clnt_add_xprt`), which is the
    /// server-side lever for single-client throughput.
    fn encode_device_addr(addr: &crate::pnfs::mds::operations::DeviceAddr4) -> Bytes {
        use crate::nfs::xdr::XdrEncoder;
        use crate::pnfs::protocol::endpoint_to_uaddr;

        let mut encoder = XdrEncoder::new();
        let n = addr.ds_list.len();

        // PART 1: stripe_indices<> — round-robin over the DS list.
        encoder.encode_u32(n as u32);
        for i in 0..n {
            encoder.encode_u32(i as u32);
        }

        // PART 2: multipath_ds_list<> — one multipath_list4 per DS.
        encoder.encode_u32(n as u32);
        for (i, ds_addrs) in addr.ds_list.iter().enumerate() {
            encoder.encode_u32(ds_addrs.len() as u32);
            for ep in ds_addrs {
                // netaddr4: netid + universal address
                // ("10.42.214.18:2049" → "10.42.214.18.8.1").
                encoder.encode_string(&addr.netid);
                let uaddr = endpoint_to_uaddr(ep).unwrap_or_else(|_| ep.clone());
                encoder.encode_string(&uaddr);
            }
            debug!("   DS[{}]: {:?}", i, ds_addrs);
        }

        let result = encoder.finish();
        debug!(
            "📦 Device address encoded: {} DS(es), {} bytes",
            n,
            result.len()
        );
        result
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

/// The opcode of `op` when that operation exists ONLY in NFSv4.2
/// (RFC 7862), else None.
///
/// Deliberately a total match over the 4.2 variants rather than a range
/// test on a numeric opcode: adding a new 4.2 operation to the `Operation`
/// enum without listing it here is a compile-time-visible omission in one
/// place, not a silently ungated handler.
fn minor_version_2_opcode(op: &Operation) -> Option<u32> {
    Some(match op {
        Operation::Allocate { .. } => opcode::ALLOCATE,
        Operation::Copy { .. } => opcode::COPY,
        Operation::Deallocate { .. } => opcode::DEALLOCATE,
        Operation::IoAdvise { .. } => opcode::IO_ADVISE,
        Operation::ReadPlus { .. } => opcode::READ_PLUS,
        Operation::Seek { .. } => opcode::SEEK,
        Operation::Clone { .. } => opcode::CLONE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_dispatcher() -> (CompoundDispatcher, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr);
        (dispatcher, temp_dir)
    }

    /// The nfsv4_1_file_layout_ds_addr4 encoding must keep the two
    /// dimensions apart: outer = stripe map (one entry per DS), inner =
    /// multipath_list4 (all addresses of ONE DS). A DS with extra
    /// addresses must NOT grow the stripe map.
    #[test]
    fn multipath_device_addr_encoding_shape() {
        use crate::nfs::xdr::XdrDecoder;

        let addr = crate::pnfs::mds::operations::DeviceAddr4 {
            netid: "tcp".to_string(),
            ds_list: vec![
                vec!["10.0.0.1:2049".to_string(), "10.0.1.1:2049".to_string()],
                vec!["10.0.0.2:2049".to_string()],
            ],
        };
        let body = CompoundDispatcher::encode_device_addr(&addr);
        let mut d = XdrDecoder::new(body);

        // stripe_indices: exactly one index per DS, in order.
        assert_eq!(d.decode_u32().unwrap(), 2);
        assert_eq!(d.decode_u32().unwrap(), 0);
        assert_eq!(d.decode_u32().unwrap(), 1);

        // ds_list: DS[0] carries BOTH its addresses, DS[1] one.
        assert_eq!(d.decode_u32().unwrap(), 2);

        assert_eq!(d.decode_u32().unwrap(), 2); // DS[0] multipath count
        assert_eq!(d.decode_string().unwrap(), "tcp");
        assert_eq!(d.decode_string().unwrap(), "10.0.0.1.8.1");
        assert_eq!(d.decode_string().unwrap(), "tcp");
        assert_eq!(d.decode_string().unwrap(), "10.0.1.1.8.1");

        assert_eq!(d.decode_u32().unwrap(), 1); // DS[1] multipath count
        assert_eq!(d.decode_string().unwrap(), "tcp");
        assert_eq!(d.decode_string().unwrap(), "10.0.0.2.8.1");
    }

    /// A PnfsOperations that answers only the two questions the I/O
    /// guards ask, from a fixed set of pinned keys.
    ///
    /// It overrides `fallback_io_disposition` DIRECTLY rather than
    /// inheriting the trait default. The default derives its answer from
    /// `is_pnfs_managed` and can only ever return Serve or Delay — so a
    /// fake that overrode `is_pnfs_managed` alone would leave the
    /// `FailFast` arm of the READ/WRITE guard structurally unreachable,
    /// and any test claiming to cover it would be testing nothing.
    struct FakePnfs {
        pinned: std::collections::HashSet<String>,
        disposition: crate::pnfs::FallbackIoDisposition,
    }

    impl FakePnfs {
        fn new(
            pinned: &[&str],
            disposition: crate::pnfs::FallbackIoDisposition,
        ) -> Arc<dyn crate::pnfs::PnfsOperations> {
            Arc::new(Self {
                pinned: pinned.iter().map(|s| s.to_string()).collect(),
                disposition,
            })
        }
    }

    #[tonic::async_trait]
    impl crate::pnfs::PnfsOperations for FakePnfs {
        fn layoutget(
            &self,
            _args: crate::pnfs::mds::operations::LayoutGetArgs,
        ) -> Result<
            crate::pnfs::mds::operations::LayoutGetResult,
            crate::pnfs::mds::operations::LayoutGetError,
        > {
            Err(crate::pnfs::mds::operations::LayoutGetError::LayoutUnavailable)
        }

        fn getdeviceinfo(
            &self,
            _args: crate::pnfs::mds::operations::GetDeviceInfoArgs,
        ) -> Result<
            crate::pnfs::mds::operations::GetDeviceInfoResult,
            crate::pnfs::mds::operations::GetDeviceInfoError,
        > {
            Err(crate::pnfs::mds::operations::GetDeviceInfoError::NoEnt)
        }

        fn layoutreturn(
            &self,
            _args: crate::pnfs::mds::operations::LayoutReturnArgs,
        ) -> Result<(), crate::pnfs::mds::operations::LayoutReturnError> {
            Ok(())
        }

        fn is_pnfs_managed(&self, file_key: &str) -> bool {
            self.pinned.contains(file_key)
        }

        fn fallback_io_disposition(
            &self,
            file_key: &str,
        ) -> crate::pnfs::FallbackIoDisposition {
            if self.pinned.contains(file_key) {
                self.disposition
            } else {
                crate::pnfs::FallbackIoDisposition::Serve
            }
        }
    }

    fn create_test_dispatcher_pnfs(
        pinned: &[&str],
        disposition: crate::pnfs::FallbackIoDisposition,
    ) -> (CompoundDispatcher, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        // Canonicalized deliberately. FileHandleManager::new canonicalizes
        // the export path, while resolve_handle returns a path that is
        // explicitly NOT canonicalized (it must not follow symlinks), and
        // both pNFS key helpers strip one from the other. On macOS the temp
        // dir is /var/... with /private/var/... as its real path, so an
        // uncanonicalized export makes every strip_prefix miss and every
        // guard below silently degrade to "not striped" — the tests would
        // pass while asserting nothing. Production export paths have no
        // symlink component, so canonicalizing here reproduces production
        // rather than papering over it. (That the guards depend on this at
        // all is a real fragility; noted, not fixed here.)
        let export_path = temp_dir.path().canonicalize().unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = CompoundDispatcher::new_with_pnfs(
            fh_mgr,
            state_mgr,
            lock_mgr,
            Some(FakePnfs::new(pinned, disposition)),
        );
        (dispatcher, temp_dir)
    }

    /// T0 — the harness meta-assertion every guard test below depends on.
    ///
    /// If `create_test_dispatcher_pnfs` ever loses its pNFS handler, every
    /// guard short-circuits to "no pnfs_handler ⇒ allow" before resolving
    /// anything, and each of those tests goes green while asserting
    /// nothing. Without this test that failure is invisible.
    #[test]
    fn the_pnfs_test_dispatcher_actually_has_a_pnfs_handler() {
        let (d, _t) = create_test_dispatcher_pnfs(
            &["f"],
            crate::pnfs::FallbackIoDisposition::Delay,
        );
        assert!(
            d.pnfs_handler.is_some(),
            "the pNFS test dispatcher must carry a handler or every guard test below is vacuous"
        );
        let (plain, _t2) = create_test_dispatcher();
        assert!(plain.pnfs_handler.is_none());
    }

    /// Set `current_fh` to a real file in the export and return an Open
    /// stateid bound to it.
    fn pin_current_fh(
        d: &CompoundDispatcher,
        ctx: &mut CompoundContext,
        temp: &TempDir,
        name: &str,
        contents: &[u8],
    ) -> StateId {
        let path = temp.path().canonicalize().unwrap().join(name);
        std::fs::write(&path, contents).unwrap();
        let fh = d.file_handler.fh_manager().path_to_filehandle(&path).unwrap();
        ctx.current_fh = Some(fh.clone());
        d.state_mgr.stateids.allocate(
            crate::nfs::v4::state::StateType::Open,
            1,
            Some(fh.data.clone()),
        )
    }

    /// T1 — retro-coverage for the READ/WRITE stub guard.
    ///
    /// That guard has shipped since runn (2026-07-06) with NOTHING in the
    /// tree that would go red if it were deleted. Both dispositions are
    /// asserted by exact status, and the `FailFast` arm is reachable only
    /// because FakePnfs overrides `fallback_io_disposition` directly
    /// rather than inheriting the trait default (which cannot produce it).
    #[tokio::test]
    async fn read_and_write_through_the_mds_honour_both_stub_dispositions() {
        for (disposition, expected) in [
            (crate::pnfs::FallbackIoDisposition::Delay, Nfs4Status::Delay),
            (crate::pnfs::FallbackIoDisposition::FailFast, Nfs4Status::Io),
        ] {
            let (d, temp) = create_test_dispatcher_pnfs(&["striped.dat"], disposition);
            let mut ctx = CompoundContext::new(2);
            let sid = pin_current_fh(&d, &mut ctx, &temp, "striped.dat", b"stub");

            let read = d
                .dispatch_operation(
                    Operation::Read { stateid: sid.clone(), offset: 0, count: 4 },
                    &mut ctx,
                )
                .await;
            assert_eq!(read.status(), expected, "READ under {:?}", disposition);

            let write = d
                .dispatch_operation(
                    Operation::Write {
                        stateid: sid,
                        offset: 0,
                        stable: 2,
                        data: bytes::Bytes::from_static(b"zzzz"),
                    },
                    &mut ctx,
                )
                .await;
            assert_eq!(write.status(), expected, "WRITE under {:?}", disposition);
        }
    }

    /// The same guard must not touch a file that was never layouted. An
    /// MDS deliberately serves those; a role-level guard would break them
    /// and still pass the test above.
    #[tokio::test]
    async fn read_of_a_never_layouted_file_on_an_mds_is_served() {
        let (d, temp) = create_test_dispatcher_pnfs(
            &["some-other-file.dat"],
            crate::pnfs::FallbackIoDisposition::FailFast,
        );
        let mut ctx = CompoundContext::new(2);
        let sid = pin_current_fh(&d, &mut ctx, &temp, "ordinary.dat", b"real bytes");

        let read = d
            .dispatch_operation(
                Operation::Read { stateid: sid, offset: 0, count: 10 },
                &mut ctx,
            )
            .await;
        assert_eq!(read.status(), Nfs4Status::Ok);
    }

    /// The space-management guard's predicate, asserted directly so it is
    /// covered on every platform. The wired-up A/B test below can only run
    /// on Linux (see its comment), and a predicate this small should not
    /// be provable on one target only.
    #[test]
    fn the_space_op_guard_fires_only_for_pinned_files_and_only_on_an_mds() {
        let (d, temp) = create_test_dispatcher_pnfs(
            &["striped.dat"],
            crate::pnfs::FallbackIoDisposition::FailFast,
        );
        let mut ctx = CompoundContext::new(2);
        pin_current_fh(&d, &mut ctx, &temp, "striped.dat", b"stub");
        assert_eq!(d.refuse_if_striped("SEEK", &ctx), Some(Nfs4Status::NotSupp));

        pin_current_fh(&d, &mut ctx, &temp, "ordinary.dat", b"real");
        assert_eq!(d.refuse_if_striped("SEEK", &ctx), None);

        // Standalone / DS role: no pnfs handler, nothing is striped.
        let (plain, temp2) = create_test_dispatcher();
        let mut ctx2 = CompoundContext::new(2);
        pin_current_fh(&plain, &mut ctx2, &temp2, "striped.dat", b"stub");
        assert_eq!(plain.refuse_if_striped("SEEK", &ctx2), None);
    }

    /// ALLOCATE / DEALLOCATE / SEEK on a striped file are refused, and on
    /// an unpinned file still work.
    ///
    /// Linux-only, and the B arm is why. The real bodies of all three are
    /// `#[cfg(target_os = "linux")]`; every other target returns NOTSUPP
    /// unconditionally, so on darwin the pinned arm would pass against
    /// completely unguarded code. Arm B — the unpinned control must NOT
    /// answer NOTSUPP — is what distinguishes a working guard from a build
    /// where the operation simply does not exist.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn space_ops_refuse_striped_files_and_still_serve_unpinned_ones() {
        use crate::nfs::v4::compound::Operation as Op;

        for pinned in [true, false] {
            let (d, temp) = create_test_dispatcher_pnfs(
                &["striped.dat"],
                crate::pnfs::FallbackIoDisposition::FailFast,
            );
            let name = if pinned { "striped.dat" } else { "ordinary.dat" };
            let mut ctx = CompoundContext::new(2);
            let sid = pin_current_fh(&d, &mut ctx, &temp, name, &vec![0u8; 8192]);

            let ops = [
                Op::Allocate { stateid: sid.clone(), offset: 0, length: 4096 },
                Op::Deallocate { stateid: sid.clone(), offset: 0, length: 4096 },
                Op::Seek { stateid: sid.clone(), offset: 0, what: 0 },
            ];
            for op in ops {
                let label = format!("{:?} pinned={}", std::mem::discriminant(&op), pinned);
                let res = d.dispatch_operation(op, &mut ctx).await;
                if pinned {
                    assert_eq!(res.status(), Nfs4Status::NotSupp, "{label}");
                } else {
                    assert_ne!(
                        res.status(),
                        Nfs4Status::NotSupp,
                        "{label}: the guard fired on a file that was never layouted"
                    );
                }
            }
        }
    }

    /// The opcode/minor-version gate. An NFSv4.2 operation in a 4.1
    /// COMPOUND is OP_ILLEGAL, and the same operation in a 4.2 COMPOUND
    /// reaches its handler.
    ///
    /// Arm B is mandatory: without it, a gate that rejected these opcodes
    /// unconditionally — i.e. removed 4.2 support altogether — passes.
    #[tokio::test]
    async fn nfsv4_2_opcodes_are_illegal_in_a_4_1_compound() {
        use crate::nfs::v4::compound::Operation as Op;

        let dummy = crate::nfs::v4::protocol::StateId::new(1, [0u8; 12]);
        let mk = || {
            vec![
                (opcode::ALLOCATE, Op::Allocate { stateid: dummy.clone(), offset: 0, length: 1 }),
                (opcode::DEALLOCATE, Op::Deallocate { stateid: dummy.clone(), offset: 0, length: 1 }),
                (opcode::SEEK, Op::Seek { stateid: dummy.clone(), offset: 0, what: 0 }),
                (opcode::READ_PLUS, Op::ReadPlus { stateid: dummy.clone(), offset: 0, count: 1 }),
                (opcode::COPY, Op::Copy {
                    src_stateid: dummy.clone(), dst_stateid: dummy.clone(),
                    src_offset: 0, dst_offset: 0, count: 1,
                    consecutive: false, synchronous: true,
                    source_server_count: 0,
                }),
                (opcode::CLONE, Op::Clone {
                    src_stateid: dummy.clone(), dst_stateid: dummy.clone(),
                    src_offset: 0, dst_offset: 0, count: 1,
                }),
            ]
        };

        // Arm A: minorversion 1 — illegal, and the reply names the opcode.
        for (code, op) in mk() {
            let (d, _t) = create_test_dispatcher();
            let mut ctx = CompoundContext::new(1);
            let res = d.dispatch_operation(op, &mut ctx).await;
            assert_eq!(res.status(), Nfs4Status::OpIllegal, "opcode {code} at 4.1");
            match res {
                OperationResult::Unsupported { opcode, .. } => assert_eq!(opcode, code),
                other => panic!("opcode {code}: expected Unsupported, got {other:?}"),
            }
        }

        // Arm B: minorversion 2 — the gate must let them through to their
        // handlers. A dummy stateid means they fail on BADSTATEID, which is
        // exactly the point: they got past the gate.
        for (code, op) in mk() {
            let (d, _t) = create_test_dispatcher();
            let mut ctx = CompoundContext::new(2);
            let res = d.dispatch_operation(op, &mut ctx).await;
            assert_ne!(
                res.status(),
                Nfs4Status::OpIllegal,
                "opcode {code} must be legal in a 4.2 COMPOUND"
            );
        }
    }

    /// An inter-server COPY is refused, not silently performed locally.
    ///
    /// The stateids are dummies, so a server that ignored
    /// `ca_source_server` would answer BADSTATEID here. Asserting the
    /// exact NOTSUPP is what distinguishes "refused the inter-server
    /// request" from "failed for an unrelated reason", and it also proves
    /// the check runs BEFORE stateid resolution.
    #[tokio::test]
    async fn an_inter_server_copy_is_refused_rather_than_done_locally() {
        let dummy = crate::nfs::v4::protocol::StateId::new(1, [0u8; 12]);
        let mk = |source_server_count| Operation::Copy {
            src_stateid: dummy.clone(),
            dst_stateid: dummy.clone(),
            src_offset: 0,
            dst_offset: 0,
            count: 1,
            consecutive: false,
            synchronous: true,
            source_server_count,
        };

        let (d, _t) = create_test_dispatcher();
        let mut ctx = CompoundContext::new(2);
        assert_eq!(
            d.dispatch_operation(mk(1), &mut ctx).await.status(),
            Nfs4Status::NotSupp
        );

        // Control: the ordinary intra-server case must still reach the
        // handler. Without this arm, refusing every COPY passes.
        assert_eq!(
            d.dispatch_operation(mk(0), &mut ctx).await.status(),
            Nfs4Status::BadStateId
        );
    }

    /// A striped file must not report zero allocation.
    ///
    /// The MDS stub is `set_len`-only, so `blocks()` is 0 while the size
    /// is real — the metadata signature of a fully sparse file. Measured
    /// on lima 2026-08-01 with the raw value: `tar --sparse` of a 24 MiB
    /// striped file produced a 10,240-byte archive and restored a file
    /// with ZERO non-zero bytes, exit status 0. `du` said 0.
    ///
    /// The unpinned arm is mandatory. Without it, "always report size"
    /// passes — and that would break genuinely sparse files, which are
    /// the reason the attribute exists.
    #[tokio::test]
    async fn a_striped_file_does_not_report_zero_allocation() {
        use crate::nfs::v4::operations::fileops::GetAttrOp;
        const FATTR4_SPACE_USED: u32 = 45;
        const SIZE: u64 = 3 * 1024 * 1024;

        for pinned in [true, false] {
            let (d, temp) = create_test_dispatcher_pnfs(
                &["stub.dat"],
                crate::pnfs::FallbackIoDisposition::FailFast,
            );
            let name = if pinned { "stub.dat" } else { "ordinary.dat" };
            let path = temp.path().canonicalize().unwrap().join(name);

            // A size-only stub: real length, zero blocks — exactly what
            // the MDS creates for a placement-pinned file.
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(SIZE).unwrap();
            drop(f);

            let mut ctx = CompoundContext::new(1);
            ctx.current_fh =
                Some(d.file_handler.fh_manager().path_to_filehandle(&path).unwrap());

            let res = d
                .file_handler
                .handle_getattr(
                    GetAttrOp {
                        // attr 45 lives in bitmap WORD 1 (attrs 32..63),
                        // bit 45-32 = 13.
                        attr_request: vec![0, 1u32 << (FATTR4_SPACE_USED - 32), 0],
                    },
                    &ctx,
                )
                .await;
            assert_eq!(res.status, Nfs4Status::Ok, "GETATTR failed for {name}");

            // fattr4 body: bitmap words are in the reply; the value we
            // asked for is the only one present, so it is the trailing u64.
            let attrs = res.obj_attributes.expect("attributes present");
            let v = &attrs.attr_vals;
            assert!(v.len() >= 8, "attr body too short for a u64: {} bytes", v.len());
            let space_used =
                u64::from_be_bytes(v[v.len() - 8..].try_into().expect("8 bytes"));

            if pinned {
                assert_eq!(
                    space_used, SIZE,
                    "a striped file must report its size as allocated, not 0 — \
                     0 makes tar --sparse back up nothing"
                );
            } else {
                assert_eq!(
                    space_used, 0,
                    "a genuinely sparse, never-layouted file must keep reporting \
                     its real (zero) allocation"
                );
            }
        }
    }

    /// COPY's `wr_writeverf` must equal what COMMIT reports.
    ///
    /// Measured on lima (Ubuntu 24.04, kernel 6.8.0-136) on 2026-08-01:
    /// Linux issues COPY and COMMIT in ONE compound and compares the two
    /// verifiers. flint returned a hardcoded zero for COPY's, commented
    /// "sync copy: unused". The client read every successful copy as a
    /// server reboot and reissued the identical COPY — a single
    /// `copy_file_range()` of 1 MiB produced 264,601 COPY RPCs, each of
    /// which the server really performed, and the syscall never returned.
    ///
    /// No single-operation assertion can see this: both replies are
    /// individually well-formed and both say NFS4_OK. Only the RELATION
    /// between them is wrong, so the test has to compare the two.
    #[tokio::test]
    async fn copy_and_commit_report_the_same_write_verifier() {
        let (d, temp) = create_test_dispatcher();
        let mut ctx = CompoundContext::new(2);

        let src = temp.path().join("src.bin");
        let dst = temp.path().join("dst.bin");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let src_fh = d.file_handler.fh_manager().path_to_filehandle(&src).unwrap();
        let dst_fh = d.file_handler.fh_manager().path_to_filehandle(&dst).unwrap();
        let mk_sid = |fh: &crate::nfs::v4::protocol::Nfs4FileHandle| {
            d.state_mgr.stateids.allocate(
                crate::nfs::v4::state::StateType::Open, 1, Some(fh.data.clone()))
        };

        ctx.current_fh = Some(dst_fh.clone());
        let copy = d
            .dispatch_operation(
                Operation::Copy {
                    src_stateid: mk_sid(&src_fh),
                    dst_stateid: mk_sid(&dst_fh),
                    src_offset: 0,
                    dst_offset: 0,
                    count: 4096,
                    consecutive: false,
                    synchronous: true,
                    source_server_count: 0,
                },
                &mut ctx,
            )
            .await;

        let copy_verf = match copy {
            OperationResult::Copy(Nfs4Status::Ok, Some(r)) => r.verifier,
            other => panic!("COPY did not succeed: {other:?}"),
        };

        let commit = d
            .dispatch_operation(Operation::Commit { offset: 0, count: 4096 }, &mut ctx)
            .await;
        let commit_verf = match commit {
            OperationResult::Commit(Nfs4Status::Ok, Some(v)) => u64::from_be_bytes(v),
            other => panic!("COMMIT did not succeed: {other:?}"),
        };

        assert_eq!(
            copy_verf, commit_verf,
            "COPY's wr_writeverf must equal COMMIT's verifier; a mismatch makes a \
             Linux client retry the copy forever"
        );
        assert_ne!(copy_verf, 0, "a zero verifier is what caused the livelock");

        // AND on the wire. The struct assertions above are not enough: the
        // original bug was in the ENCODER, which wrote a hardcoded zero and
        // ignored whatever the handler produced. A test that stops at
        // CopyResult passes against the broken encoder — verified by
        // mutation, which is how this half came to be written.
        let encoded = crate::nfs::v4::compound::CompoundResponse {
            status: Nfs4Status::Ok,
            tag: String::new(),
            results: vec![OperationResult::Copy(
                Nfs4Status::Ok,
                Some(crate::nfs::v4::compound::CopyResult {
                    count: 4096,
                    consecutive: true,
                    synchronous: true,
                    verifier: copy_verf,
                }),
            )],
            raw_reply: None,
            cache_slot: None,
        }
        .encode();

        // status(4) tag_len(4) count(4) | opcode(4) status(4)
        // wr_callback_id count(4) wr_count(8) wr_committed(4) wr_writeverf(8)
        let verf_off = 4 + 4 + 4 + 4 + 4 + 4 + 8 + 4;
        let on_wire = u64::from_be_bytes(
            encoded[verf_off..verf_off + 8].try_into().expect("8 verifier bytes"),
        );
        assert_eq!(
            on_wire, copy_verf,
            "the ENCODED wr_writeverf must carry the real verifier, not a constant"
        );
    }

    /// data_content4 has exactly two arms. Anything else is
    /// NFS4ERR_UNION_NOTSUPP (RFC 7862 §11.1.1.1), not silently treated
    /// as HOLE.
    #[tokio::test]
    async fn seek_rejects_an_unknown_data_content_arm() {
        let dummy = crate::nfs::v4::protocol::StateId::new(1, [0u8; 12]);
        let (d, _t) = create_test_dispatcher();
        let mut ctx = CompoundContext::new(2);

        for what in [2u32, 3, u32::MAX] {
            let res = d
                .dispatch_operation(
                    Operation::Seek { stateid: dummy.clone(), offset: 0, what },
                    &mut ctx,
                )
                .await;
            assert_eq!(res.status(), Nfs4Status::UnionNotsupp, "sa_what={what}");
        }

        // Both defined arms must get PAST the union check. They fail on the
        // dummy stateid, which is the point — they were not rejected here.
        for what in [0u32, 1] {
            let res = d
                .dispatch_operation(
                    Operation::Seek { stateid: dummy.clone(), offset: 0, what },
                    &mut ctx,
                )
                .await;
            assert_ne!(res.status(), Nfs4Status::UnionNotsupp, "sa_what={what}");
        }
    }

    /// Operations that exist in 4.1 are untouched by the gate.
    #[tokio::test]
    async fn the_minor_version_gate_does_not_catch_pre_4_2_operations() {
        let (d, _t) = create_test_dispatcher();
        let mut ctx = CompoundContext::new(1);
        let res = d.dispatch_operation(Operation::PutRootFh, &mut ctx).await;
        assert_eq!(res.status(), Nfs4Status::Ok);
        assert!(minor_version_2_opcode(&Operation::PutRootFh).is_none());
        assert!(minor_version_2_opcode(&Operation::GetFh).is_none());
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
            wire_size: 0,
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
            wire_size: 0,
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

    /// EXCHANGE_ID must advertise the pNFS MDS role when — and only when —
    /// the dispatcher has a pNFS handler.
    ///
    /// This used to be a post-dispatch patch inside the MDS's own copy of
    /// the RPC layer. That copy is gone (it silently missed the SEQUENCE
    /// reply cache and the F55 drain gate for months), and this flag was the
    /// only behaviour it carried. Getting it wrong is quiet and expensive:
    /// without EXCHGID4_FLAG_USE_PNFS_MDS the client never asks for a
    /// layout, so every read and write goes through the metadata server and
    /// pNFS degrades to a plain — and much slower — NFS mount, with nothing
    /// failing to say so.
    #[tokio::test]
    async fn exchange_id_advertises_the_mds_role_only_when_pnfs_is_enabled() {
        use crate::pnfs::exchange_id::is_pnfs_mds_mode;

        async fn flags_from(dispatcher: &CompoundDispatcher) -> u32 {
            let request = CompoundRequest {
                tag: "eid".to_string(),
                tag_valid: true,
                minor_version: 2,
                operations: vec![Operation::ExchangeId {
                    clientowner: ClientId {
                        verifier: 99,
                        id: b"pnfs-flag-probe".to_vec(),
                    },
                    flags: 0,
                    state_protect: 0,
                    impl_id: vec![],
                }],
                wire_size: 0,
            };
            match &dispatcher.dispatch_compound(request, Vec::new()).await.results[0] {
                OperationResult::ExchangeId(Nfs4Status::Ok, Some(res)) => res.flags,
                other => panic!("expected a successful ExchangeId, got {:?}", other),
            }
        }

        let (pnfs_dispatcher, _t1) = create_test_dispatcher_pnfs(
            &[],
            crate::pnfs::FallbackIoDisposition::Serve,
        );
        assert!(
            is_pnfs_mds_mode(flags_from(&pnfs_dispatcher).await),
            "a pNFS dispatcher must set USE_PNFS_MDS, or clients never request a layout"
        );

        let (plain_dispatcher, _t2) = create_test_dispatcher();
        assert!(
            !is_pnfs_mds_mode(flags_from(&plain_dispatcher).await),
            "the standalone server must NOT claim to be a pNFS MDS"
        );
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
            wire_size: 0,
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
            wire_size: 0,
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
            wire_size: 0,
        };

        dispatcher.dispatch_compound(request, Vec::new()).await;

        let stats = dispatcher.get_stats();
        assert_eq!(stats.active_clients, 1);
    }

    /// nfl_first_stripe_index sits after the 16-byte deviceid + 4-byte
    /// nfl_util in the encoded nfsv4_1_file_layout4.
    fn encoded_first_stripe_index(fh: &[u8], file_id: u64, n_ds: usize) -> u32 {
        use crate::pnfs::mds::layout::{IoMode, LayoutSegment};
        let ids: Vec<String> = (0..n_ds).map(|i| format!("ds-{}", i + 1)).collect();
        let segments: Vec<LayoutSegment> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| LayoutSegment {
                offset: 0,
                length: u64::MAX,
                iomode: IoMode::ReadWrite,
                device_id: id.clone(),
                stripe_index: i as u32,
                pattern_offset: 0,
            })
            .collect();
        let device_id = crate::pnfs::mds::layout::composite_device_id(&ids);
        let enc = CompoundDispatcher::encode_file_layout_striped(
            &segments, fh, 8 << 20, device_id, file_id,
        );
        u32::from_be_bytes(enc[20..24].try_into().unwrap())
    }

    /// The identity-keyed rotation — the live path for every file with
    /// an allocated `file_id`.
    ///
    /// This is what the old version of this test *claimed* to cover and
    /// did not: it passed `file_id = 0` for every case, which routes to
    /// the legacy filehandle-hash arm, so the `file_id % N` branch was
    /// never executed once.
    #[test]
    fn striped_layout_rotates_first_stripe_index_per_file_id() {
        // Exactly the documented mapping, not merely "some rotation":
        // a wrong-but-deterministic formula would satisfy a spread check.
        for file_id in 1..=32u64 {
            assert_eq!(
                encoded_first_stripe_index(b"any-fh", file_id, 4),
                (file_id % 4) as u32,
                "file_id {} must rotate to file_id % N", file_id,
            );
        }

        // Always inside the stripe count, for stripe widths that are not
        // powers of two.
        for n in [1usize, 2, 3, 5, 7] {
            for file_id in [1u64, 2, 9, 1_000_003, u64::MAX] {
                assert!(encoded_first_stripe_index(b"fh", file_id, n) < n as u32);
            }
        }

        // Small files must not all start on DS[0]: across a sample of
        // file_ids every DS is used as a starting point.
        let mut seen = std::collections::HashSet::new();
        for file_id in 1..=64u64 {
            seen.insert(encoded_first_stripe_index(b"fh", file_id, 4));
        }
        assert_eq!(seen.len(), 4, "rotation does not reach every DS");
    }

    /// The invariant the placement drill actually caught, and the reason
    /// the rotation is keyed on `file_id` rather than the filehandle:
    /// a reader that arrives after a RENAME holds a *different* FH for
    /// the same file. If rotation followed the FH, that reader would
    /// reassemble the stripes in a different order than the writer laid
    /// them down.
    ///
    /// Uses many filehandles, not two: with N=4 a single pair has a 1-in-4
    /// chance of hashing to the same index anyway, so a two-FH version of
    /// this test passes 25% of the time on a server that is broken. It was
    /// written that way first, and the mutation run caught it.
    #[test]
    fn striped_layout_rotation_survives_rename() {
        const FILE_ID: u64 = 0xDEAD_BEEF;
        let expected = (FILE_ID % 4) as u32;
        for fh in [
            b"fh-before-rename".as_ref(),
            b"a-completely-different-fh",
            b"/exports/data/train-00001.tfrecord",
            b"\x02\x00\x00\x00\x00\x00\x00\x00\x01",
            b"x",
            b"",
        ] {
            assert_eq!(
                encoded_first_stripe_index(fh, FILE_ID, 4), expected,
                "rotation must depend on file_id alone, not the filehandle",
            );
        }
    }

    /// Legacy pins (`file_id == 0`) keep the historical FH-derived
    /// rotation. Their filehandles are path-stable because their renames
    /// are refused, so keying on the FH is safe for them — but it must
    /// stay deterministic and spread.
    #[test]
    fn striped_layout_legacy_pins_rotate_on_filehandle() {
        assert_eq!(encoded_first_stripe_index(b"file-A", 0, 2),
                   encoded_first_stripe_index(b"file-A", 0, 2));

        // Distinct from the identity-keyed arm: same FH, file_id 0 vs a
        // file_id chosen so the two arms must disagree. If the branch
        // were ever collapsed, this is what would catch it.
        //
        // The `+ 4` is load-bearing: it keeps the probe file_id non-zero
        // when `(legacy + 1) % 4 == 0`, which would otherwise land back
        // on the legacy sentinel and compare the arm against itself.
        let legacy = encoded_first_stripe_index(b"probe", 0, 4);
        let probe_file_id = (legacy as u64 + 1) % 4 + 4;
        assert_ne!(probe_file_id, 0);
        let identity = encoded_first_stripe_index(b"probe", probe_file_id, 4);
        assert_ne!(legacy, identity);

        let mut seen = std::collections::HashSet::new();
        for i in 0..16u8 {
            seen.insert(encoded_first_stripe_index(&[b'f', i], 0, 2));
        }
        assert_eq!(seen.len(), 2, "legacy rotation never picks the second DS");
    }

    /// We advertise `[LAYOUT4_NFSV4_1_FILES]` only, so the two
    /// operations that emit a layout-typed body must refuse everything
    /// else rather than answer with a mislabelled one.
    #[test]
    fn only_the_served_layout_types_are_accepted() {
        assert_eq!(CompoundDispatcher::layout_type_served(1),
                   Ok(crate::pnfs::mds::layout::LayoutType::NfsV4_1Files));
        // Since the pnfs-block class: LAYOUT4_SCSI is in the served set
        // (per-volume policing — a files volume still refuses 5 — lives
        // in the handlers, where the file's class is known).
        assert_eq!(CompoundDispatcher::layout_type_served(5),
                   Ok(crate::pnfs::mds::layout::LayoutType::Scsi));

        // Type 4 is the one that mattered: it used to be ACCEPTED here
        // and then answered NFS4_OK with a body tagged type 1.
        assert_eq!(CompoundDispatcher::layout_type_served(4),
                   Err(Nfs4Status::UnknownLayoutType));

        // RFC 8881 §15.1: the error is UNKNOWN_LAYOUTTYPE (10062), not
        // the generic NOTSUPP this used to return for 2 and 3.
        for t in [0u32, 2, 3, 99, u32::MAX] {
            assert_eq!(CompoundDispatcher::layout_type_served(t),
                       Err(Nfs4Status::UnknownLayoutType), "layout type {}", t);
        }
        assert_eq!(Nfs4Status::UnknownLayoutType as u32, 10062);
    }

    /// Golden bytes for the scsi device address (RFC 8154 §2.2.2): one
    /// BASE volume, BINARY code set, EUI64-form designator carrying the
    /// 16-byte NGUID, then the caller's pr_key. The kernel's blocklayout
    /// driver parses this strictly; the framing is pinned here the way
    /// tests/layoutget_encoding_test.rs pins the files layout.
    #[test]
    fn scsi_device_addr_encoding_shape() {
        let nguid: [u8; 16] = *b"0123456789abcdef";
        let body = CompoundDispatcher::encode_scsi_device_addr(&nguid, 0xDEAD_BEEF_CAFE_F00D);
        let mut want = bytes::BytesMut::new();
        want.extend_from_slice(&1u32.to_be_bytes());  // sda_volumes<> len
        want.extend_from_slice(&4u32.to_be_bytes());  // PNFS_SCSI_VOLUME_BASE
        want.extend_from_slice(&1u32.to_be_bytes());  // PS_CODE_SET_BINARY
        want.extend_from_slice(&2u32.to_be_bytes());  // PS_DESIGNATOR_EUI64
        want.extend_from_slice(&16u32.to_be_bytes()); // sd_designator len
        want.extend_from_slice(&nguid);               // 16 bytes, no pad needed
        want.extend_from_slice(&0xDEAD_BEEF_CAFE_F00Du64.to_be_bytes());
        assert_eq!(&body[..], &want[..]);
    }

    /// maxcount is honoured, and TOOSMALL replies carry gdir_mincount.
    #[test]
    fn getdeviceinfo_maxcount_toosmall_carries_mincount() {
        let body = [0u8; 40];
        // Total = 4 (type) + 4 (opaque len) + 40 + 4 (notify) = 52.
        match CompoundDispatcher::frame_getdeviceinfo_reply(5, &body, 51) {
            OperationResult::GetDeviceInfo(Nfs4Status::TooSmall, Some(b)) => {
                assert_eq!(&b[..], &52u32.to_be_bytes());
            }
            other => panic!("expected TooSmall with mincount, got {:?}", other),
        }
        match CompoundDispatcher::frame_getdeviceinfo_reply(5, &body, 52) {
            OperationResult::GetDeviceInfo(Nfs4Status::Ok, Some(b)) => {
                assert_eq!(b.len(), 52);
                assert_eq!(&b[0..4], &5u32.to_be_bytes(), "echoes the layout type");
            }
            other => panic!("expected Ok, got {:?}", other),
        }
        // maxcount 0 = the client declared no ceiling.
        assert!(matches!(
            CompoundDispatcher::frame_getdeviceinfo_reply(5, &body, 0),
            OperationResult::GetDeviceInfo(Nfs4Status::Ok, Some(_))
        ));
    }

    /// The co_ownerid → host NQN derivation, against the exact strings
    /// v6.11's `nfs4_init_uniform_client_string` emits (verified in the
    /// kernel source, not guessed): `"Linux NFSv4.<minor> <nodename>"`,
    /// or `"Linux NFSv4.<minor> <uniquifier>/<nodename>"` with
    /// nfs.nfs4_unique_id set. Everything else refuses — no admission is
    /// safer than a guessed one.
    #[test]
    fn hostnqn_derivation_handles_the_kernel_owner_shapes() {
        let derive = |s: &str| CompoundDispatcher::hostnqn_from_co_ownerid(s.as_bytes());
        assert_eq!(
            derive("Linux NFSv4.1 worker-3"),
            Some("nqn.2024-11.com.flint:node:worker-3".to_string()),
            "plain uniform shape"
        );
        assert_eq!(
            derive("Linux NFSv4.2 my-uniq-id/worker-3"),
            Some("nqn.2024-11.com.flint:node:worker-3".to_string()),
            "uniquifier shape: nodename is the LAST /-component"
        );
        // v4.0's nonuniform shape ends in the peer ADDRESS, not the
        // nodename — deriving from it would admit a host named after an
        // IP. v4.0 does no pNFS; refuse.
        assert_eq!(derive("Linux NFSv4.0 worker-3/10.0.0.7"), None);
        assert_eq!(derive("pynfs-owner-12345"), None, "non-Linux owners refuse");
        assert_eq!(derive("Linux NFSv4.1 "), None, "empty nodename refuses");
        assert_eq!(
            CompoundDispatcher::hostnqn_from_co_ownerid(&[0xff, 0xfe]),
            None,
            "non-UTF8 owners refuse"
        );
    }

    /// READ-layout segment tiling, against the kernel's `verify_extent`
    /// law: no RW/INVALID states, and segments must tile the layout
    /// window contiguously — leading, interior, and trailing holes as
    /// NONE_DATA, committed extents clipped into the window as
    /// READ_DATA with the storage offset shifted by the clip.
    #[test]
    fn read_segments_tile_the_window_with_none_data_holes() {
        use crate::state_backend::extent_alloc::GrantedExtent;
        let ext = |lo: u64, len: u64, phys: u64| GrantedExtent {
            logical_offset: lo,
            length: len,
            physical_offset: phys,
            generation: 1,
            committed: true,
            needs_scrub: false,
        };
        // Window [4096, 4096+24576); committed extents [0,8192) (clipped)
        // and [16384, 20480) — leading part-overlap, interior hole,
        // trailing hole.
        let segs = CompoundDispatcher::scsi_read_segments(
            &[ext(0, 8192, 100_000), ext(16384, 4096, 200_000)],
            4096,
            24576,
        );
        let flat: Vec<(u64, u64, u64, u32)> = segs
            .iter()
            .map(|s| (s.file_offset, s.length, s.storage_offset, s.state))
            .collect();
        assert_eq!(
            flat,
            vec![
                (4096, 4096, 104_096, 1), // clipped READ_DATA, phys shifted
                (8192, 8192, 0, 3),       // interior hole
                (16384, 4096, 200_000, 1),
                (20480, 8192, 0, 3),      // trailing hole
            ]
        );
        // Contiguity — the exact thing verify_extent polices.
        let mut cursor = 4096;
        for (off, len, _, state) in &flat {
            assert_eq!(*off, cursor, "gapless tiling");
            assert_ne!(*state, 0, "no RW_DATA in a read layout");
            assert_ne!(*state, 2, "no INVALID_DATA in a read layout");
            cursor = off + len;
        }
        assert_eq!(cursor, 4096 + 24576, "window fully covered");

        // Empty file: the whole window is one NONE_DATA hole.
        let segs = CompoundDispatcher::scsi_read_segments(&[], 0, 4096);
        assert_eq!(segs.len(), 1);
        assert_eq!((segs[0].file_offset, segs[0].length, segs[0].state), (0, 4096, 3));
    }
}

