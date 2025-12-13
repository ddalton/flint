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
use crate::nfs::v4::compound::{CompoundRequest, CompoundResponse, CompoundContext, Operation, OperationResult, ExchangeIdResult, CreateSessionResult, SequenceResult, ChannelAttrs};
use crate::nfs::v4::state::{StateManager, StateType};
use crate::nfs::v4::filehandle::FileHandleManager;
use crate::nfs::v4::operations::*;
use crate::nfs::v4::operations::ioops::UNSTABLE4;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// COMPOUND dispatcher - processes COMPOUND requests
pub struct CompoundDispatcher {
    /// File handle manager
    fh_mgr: Arc<FileHandleManager>,

    /// State manager (clients, sessions, stateids, leases)
    state_mgr: Arc<StateManager>,

    /// Lock manager
    lock_mgr: Arc<LockManager>,

    /// Operation handlers
    session_handler: SessionOperationHandler,
    file_handler: FileOperationHandler,
    io_handler: IoOperationHandler,
    perf_handler: PerfOperationHandler,
    lock_handler: LockOperationHandler,
}

impl CompoundDispatcher {
    /// Create a new COMPOUND dispatcher
    pub fn new(
        fh_mgr: Arc<FileHandleManager>,
        state_mgr: Arc<StateManager>,
        lock_mgr: Arc<LockManager>,
    ) -> Self {
        // Create operation handlers
        let session_handler = SessionOperationHandler::new(state_mgr.clone());
        let file_handler = FileOperationHandler::new(fh_mgr.clone());
        let io_handler = IoOperationHandler::new(state_mgr.clone(), fh_mgr.clone());
        let perf_handler = PerfOperationHandler::new(state_mgr.clone(), fh_mgr.clone());
        let lock_handler = LockOperationHandler::new(state_mgr.clone(), lock_mgr.clone());

        Self {
            fh_mgr,
            state_mgr,
            lock_mgr,
            session_handler,
            file_handler,
            io_handler,
            perf_handler,
            lock_handler,
        }
    }

    /// Process a COMPOUND request
    pub async fn dispatch_compound(&self, request: CompoundRequest) -> CompoundResponse {
        info!("COMPOUND: tag={}, operations={}", request.tag, request.operations.len());

        // Create context
        let mut context = CompoundContext::new(request.minor_version);

        // Process operations sequentially
        let mut results = Vec::new();
        let mut final_status = Nfs4Status::Ok;

        for (i, operation) in request.operations.into_iter().enumerate() {
            debug!("COMPOUND[{}]: Processing operation: {:?}", i, operation);

            // Dispatch operation
            let result = self.dispatch_operation(operation, &mut context).await;

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
                let res = self.session_handler.handle_exchange_id(op);
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

            Operation::CreateSession { clientid, sequence, flags, fore_chan_attrs, back_chan_attrs } => {
                let op = CreateSessionOp {
                    clientid,
                    sequence,
                    flags,
                    fore_chan_attrs: fore_chan_attrs.clone(),
                    back_chan_attrs: back_chan_attrs.clone(),
                    cb_program: 0,
                };
                let res = self.session_handler.handle_create_session(op);
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
                let res = self.session_handler.handle_sequence(op);
                if res.status == Nfs4Status::Ok {
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

            Operation::DestroyClientId(clientid) => {
                // DESTROY_CLIENTID is used to destroy unused client IDs
                // For now, we'll just return OK status
                // TODO: Implement actual client cleanup in ClientManager
                info!("DESTROY_CLIENTID: clientid={}", clientid);
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

            // File handle operations
            Operation::PutRootFh => {
                let res = self.file_handler.handle_putrootfh(PutRootFhOp, context);
                OperationResult::PutRootFh(res.status)
            }

            Operation::PutFh(filehandle) => {
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
                let res = self.file_handler.handle_savefh(SaveFhOp, context);
                OperationResult::SaveFh(res.status)
            }

            Operation::RestoreFh => {
                let res = self.file_handler.handle_restorefh(RestoreFhOp, context);
                OperationResult::RestoreFh(res.status)
            }

            Operation::Lookup(component) => {
                let op = LookupOp { component };
                let res = self.file_handler.handle_lookup(op, context).await;
                OperationResult::Lookup(res.status)
            }

            Operation::LookupP => {
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
                    use crate::nfs::v4::compound::{ReadDirResult, DirEntry};
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
                let op = CloseOp {
                    seqid,
                    stateid,
                };
                let res = self.io_handler.handle_close(op, context);
                OperationResult::Close(res.status, res.stateid)
            }

            Operation::Read { stateid, offset, count } => {
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
            Operation::Copy { src_stateid, dst_stateid, src_offset, dst_offset, count, consecutive, synchronous } => {
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
                OperationResult::LockU(res.status, res.stateid)
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
                OperationResult::Create(res.status)
            }

            Operation::Remove(name) => {
                use crate::nfs::v4::operations::fileops::RemoveOp;
                let op = RemoveOp { target: name };
                let res = self.file_handler.handle_remove(op, context).await;
                OperationResult::Remove(res.status)
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
            Operation::SecInfoNoName(style) => {
                // SECINFO_NO_NAME (opcode 52) - return supported security flavors
                info!("SECINFO_NO_NAME: style={}", style);
                OperationResult::SecInfoNoName(Nfs4Status::Ok)
            }

            // Unsupported operations
            Operation::Unsupported(opcode) => {
                warn!("Unsupported operation: opcode={}", opcode);
                OperationResult::Unsupported(Nfs4Status::NotSupp)
            }

            // Catch-all for any unhandled operations
            _ => {
                warn!("Unhandled operation in dispatcher - returning NotSupp");
                OperationResult::Unsupported(Nfs4Status::NotSupp)
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
        let state_mgr = Arc::new(StateManager::new());
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr);
        (dispatcher, temp_dir)
    }

    #[tokio::test]
    async fn test_simple_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "test".to_string(),
            minor_version: 2,
            operations: vec![
                Operation::PutRootFh,
                Operation::GetFh,
            ],
        };

        let response = dispatcher.dispatch_compound(request).await;
        assert_eq!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 2);
    }

    #[tokio::test]
    async fn test_session_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "session".to_string(),
            minor_version: 2,
            operations: vec![
                Operation::ExchangeId {
                    client_owner: b"test-client".to_vec(),
                    verifier: 12345,
                    flags: 0,
                },
            ],
        };

        let response = dispatcher.dispatch_compound(request).await;
        assert_eq!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 1);

        match &response.results[0] {
            OperationResult::ExchangeId { status, clientid, .. } => {
                assert_eq!(*status, Nfs4Status::Ok);
                assert_eq!(*clientid, 1);
            }
            _ => panic!("Expected ExchangeId result"),
        }
    }

    #[tokio::test]
    async fn test_file_ops_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "fileops".to_string(),
            minor_version: 2,
            operations: vec![
                Operation::PutRootFh,
                Operation::SaveFh,
                Operation::RestoreFh,
                Operation::GetFh,
            ],
        };

        let response = dispatcher.dispatch_compound(request).await;
        assert_eq!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 4);
    }

    #[tokio::test]
    async fn test_error_stops_compound() {
        let (dispatcher, _temp) = create_test_dispatcher();

        let request = CompoundRequest {
            tag: "error".to_string(),
            minor_version: 2,
            operations: vec![
                Operation::GetFh,  // This will fail (no current FH)
                Operation::PutRootFh,  // This won't execute
            ],
        };

        let response = dispatcher.dispatch_compound(request).await;
        assert_ne!(response.status, Nfs4Status::Ok);
        assert_eq!(response.results.len(), 1); // Only first operation
    }

    #[tokio::test]
    async fn test_get_stats() {
        let (dispatcher, _temp) = create_test_dispatcher();

        // Create a client via EXCHANGE_ID
        let request = CompoundRequest {
            tag: "stats".to_string(),
            minor_version: 2,
            operations: vec![
                Operation::ExchangeId {
                    client_owner: b"test".to_vec(),
                    verifier: 1,
                    flags: 0,
                },
            ],
        };

        dispatcher.dispatch_compound(request).await;

        let stats = dispatcher.get_stats();
        assert_eq!(stats.active_clients, 1);
    }
}
