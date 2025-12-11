// COMPOUND Operation Framework
//
// NFSv4 uses COMPOUND operations where multiple operations are batched together.
// This is THE fundamental difference from NFSv3 - there are only 2 procedures:
// NULL and COMPOUND.
//
// COMPOUND structure:
// - Tag (for client tracking)
// - MinorVersion (0 = v4.0, 1 = v4.1, 2 = v4.2)
// - Array of operations
//
// Each operation in the array is executed sequentially, and the COMPOUND stops
// on first error (unless operation allows continuation).
//
// File handle context is maintained across operations:
// - Current filehandle (CFH)
// - Saved filehandle (SFH)
//
// Operations like PUTFH set CFH, SAVEFH copies CFH to SFH, RESTOREFH restores.

use super::protocol::*;
use super::xdr::{Nfs4XdrDecoder, Nfs4XdrEncoder};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;
use tracing::{info, warn, debug};

/// COMPOUND request
#[derive(Debug)]
pub struct CompoundRequest {
    pub tag: String,
    pub minor_version: u32,
    pub operations: Vec<Operation>,
}

/// COMPOUND response
#[derive(Debug)]
pub struct CompoundResponse {
    pub status: Nfs4Status,
    pub tag: String,
    pub results: Vec<OperationResult>,
}

/// Individual operation in a COMPOUND
#[derive(Debug)]
pub enum Operation {
    // File handle operations
    PutRootFh,
    PutFh(Nfs4FileHandle),
    GetFh,
    SaveFh,
    RestoreFh,
    PutPubFh,

    // Lookup and directory operations
    Lookup(String),              // component name
    LookupP,                     // lookup parent
    ReadDir {
        cookie: u64,
        cookieverf: [u8; 8],
        dircount: u32,
        maxcount: u32,
        attr_request: Vec<u32>,  // bitmap
    },

    // File I/O operations
    Open {
        seqid: u32,
        share_access: u32,
        share_deny: u32,
        owner: Vec<u8>,
        openhow: OpenHow,
        claim: OpenClaim,
    },
    Close {
        seqid: u32,
        stateid: StateId,
    },
    Read {
        stateid: StateId,
        offset: u64,
        count: u32,
    },
    Write {
        stateid: StateId,
        offset: u64,
        stable: u32,
        data: Bytes,
    },
    Commit {
        offset: u64,
        count: u32,
    },

    // Attribute operations
    GetAttr(Vec<u32>),           // bitmap of requested attributes
    SetAttr {
        stateid: StateId,
        attrs: Bytes,            // encoded attributes
    },
    Access(u32),                 // access bits

    // Modify operations
    Create {
        objtype: Nfs4FileType,
        objname: String,
    },
    Remove(String),              // component name
    Rename {
        oldname: String,
        newname: String,
    },
    Link(String),                // newname
    ReadLink,

    // Session operations (NFSv4.1)
    ExchangeId {
        clientowner: ClientId,
        flags: u32,
        state_protect: u32,      // SP4_NONE = 0
        impl_id: Vec<u8>,
    },
    CreateSession {
        clientid: u64,
        sequence: u32,
        flags: u32,
        fore_chan_attrs: ChannelAttrs,
        back_chan_attrs: ChannelAttrs,
    },
    DestroySession(SessionId),
    DestroyClientId(u64),        // clientid
    Sequence {
        sessionid: SessionId,
        sequenceid: u32,
        slotid: u32,
        highest_slotid: u32,
        cachethis: bool,
    },
    ReclaimComplete(bool),       // one_fs
    SecInfoNoName(u32),          // style

    // Lock operations (Phase 3)
    Lock {
        locktype: u32,
        reclaim: bool,
        offset: u64,
        length: u64,
        stateid: StateId,
        owner: Vec<u8>,
    },
    LockT {
        locktype: u32,
        offset: u64,
        length: u64,
        owner: Vec<u8>,
    },
    LockU {
        locktype: u32,
        seqid: u32,
        stateid: StateId,
        offset: u64,
        length: u64,
    },

    // NFSv4.2 Performance operations (Phase 2)
    Allocate {
        stateid: StateId,
        offset: u64,
        length: u64,
    },
    Deallocate {
        stateid: StateId,
        offset: u64,
        length: u64,
    },
    Seek {
        stateid: StateId,
        offset: u64,
        what: u32,  // DATA=0, HOLE=1
    },
    Copy {
        src_stateid: StateId,
        dst_stateid: StateId,
        src_offset: u64,
        dst_offset: u64,
        count: u64,
        consecutive: bool,
        synchronous: bool,
    },
    Clone {
        src_stateid: StateId,
        dst_stateid: StateId,
        src_offset: u64,
        dst_offset: u64,
        count: u64,
    },
    ReadPlus {
        stateid: StateId,
        offset: u64,
        count: u32,
    },
    IoAdvise {
        stateid: StateId,
        offset: u64,
        count: u64,
        hints: u32,
    },

    // Placeholder for unsupported operations
    Unsupported(u32),            // operation code
}

// Additional result types needed by OperationResult

/// Result for EXCHANGE_ID operation
#[derive(Debug, Clone)]
pub struct ExchangeIdResult {
    pub clientid: u64,
    pub sequenceid: u32,
    pub flags: u32,
    pub server_owner: String,
    pub server_scope: Vec<u8>,
}

/// Result for CREATE_SESSION operation
#[derive(Debug, Clone)]
pub struct CreateSessionResult {
    pub sessionid: SessionId,
    pub sequenceid: u32,
    pub flags: u32,
    pub fore_chan_attrs: ChannelAttrs,
    pub back_chan_attrs: ChannelAttrs,
}

/// Result for SEQUENCE operation
#[derive(Debug, Clone)]
pub struct SequenceResult {
    pub sessionid: SessionId,
    pub sequenceid: u32,
    pub slotid: u32,
    pub highest_slotid: u32,
    pub target_highest_slotid: u32,
    pub status_flags: u32,
}

/// Result for COPY operation
#[derive(Debug, Clone)]
pub struct CopyResult {
    pub count: u64,
    pub consecutive: bool,
    pub synchronous: bool,
}

/// Result for SEEK operation
#[derive(Debug, Clone)]
pub struct SeekResult {
    pub eof: bool,
    pub offset: u64,
}

/// Result for READ_PLUS operation
#[derive(Debug, Clone)]
pub struct ReadPlusResult {
    pub eof: bool,
    pub segments: Vec<ReadPlusSegment>,
}

/// Channel attributes for sessions
#[derive(Debug, Clone)]
pub struct ChannelAttrs {
    pub header_pad_size: u32,
    pub max_request_size: u32,
    pub max_response_size: u32,
    pub max_response_size_cached: u32,
    pub max_operations: u32,
    pub max_requests: u32,
}

/// Change info for operations that modify namespace
#[derive(Debug, Clone, Default)]
pub struct ChangeInfo {
    pub atomic: bool,
    pub before: u64,
    pub after: u64,
}

/// READ_PLUS segment types
#[derive(Debug, Clone)]
pub enum ReadPlusSegment {
    Data { offset: u64, data: Bytes },
    Hole { offset: u64, length: u64 },
}

/// Operation result
#[derive(Debug)]
pub enum OperationResult {
    // File handle operations
    PutRootFh(Nfs4Status),
    PutFh(Nfs4Status),
    GetFh(Nfs4Status, Option<Nfs4FileHandle>),
    SaveFh(Nfs4Status),
    RestoreFh(Nfs4Status),

    // Lookup operations
    Lookup(Nfs4Status),
    ReadDir(Nfs4Status, Option<ReadDirResult>),

    // File I/O
    Open(Nfs4Status, Option<OpenResult>),
    Close(Nfs4Status, Option<StateId>),
    Read(Nfs4Status, Option<ReadResult>),
    Write(Nfs4Status, Option<WriteResult>),
    Commit(Nfs4Status, Option<[u8; 8]>),  // verifier

    // Attributes
    GetAttr(Nfs4Status, Option<Bytes>),   // encoded attributes
    SetAttr(Nfs4Status),
    Access(Nfs4Status, Option<u32>),      // supported access

    // Modify
    Create(Nfs4Status),
    Remove(Nfs4Status),
    Rename(Nfs4Status, Option<ChangeInfo>, Option<ChangeInfo>), // source_cinfo, target_cinfo
    Link(Nfs4Status, Option<ChangeInfo>),
    ReadLink(Nfs4Status, Option<String>), // link target
    PutPubFh(Nfs4Status),

    // Sessions
    ExchangeId(Nfs4Status, Option<ExchangeIdResult>),
    CreateSession(Nfs4Status, Option<CreateSessionResult>),
    DestroySession(Nfs4Status),
    DestroyClientId(Nfs4Status),
    Sequence(Nfs4Status, Option<SequenceResult>),
    ReclaimComplete(Nfs4Status),
    SecInfoNoName(Nfs4Status),

    // NFSv4.2 Performance
    Allocate(Nfs4Status),
    Deallocate(Nfs4Status),
    Seek(Nfs4Status, Option<SeekResult>),
    Copy(Nfs4Status, Option<CopyResult>),
    Clone(Nfs4Status),
    ReadPlus(Nfs4Status, Option<ReadPlusResult>),

    // Generic result for unsupported operations
    Unsupported(Nfs4Status),

    // Locking operations
    Lock(Nfs4Status, Option<StateId>),
    LockT(Nfs4Status),
    LockU(Nfs4Status, Option<StateId>),
}

impl OperationResult {
    /// Extract the status code from any operation result
    pub fn status(&self) -> Nfs4Status {
        match self {
            OperationResult::PutRootFh(s) => *s,
            OperationResult::PutFh(s) => *s,
            OperationResult::GetFh(s, _) => *s,
            OperationResult::SaveFh(s) => *s,
            OperationResult::RestoreFh(s) => *s,
            OperationResult::Lookup(s) => *s,
            OperationResult::ReadDir(s, _) => *s,
            OperationResult::Open(s, _) => *s,
            OperationResult::Close(s, _) => *s,
            OperationResult::Read(s, _) => *s,
            OperationResult::Write(s, _) => *s,
            OperationResult::Commit(s, _) => *s,
            OperationResult::GetAttr(s, _) => *s,
            OperationResult::SetAttr(s) => *s,
            OperationResult::Access(s, _) => *s,
            OperationResult::Create(s) => *s,
            OperationResult::Remove(s) => *s,
            OperationResult::Rename(s, _, _) => *s,
            OperationResult::Link(s, _) => *s,
            OperationResult::ReadLink(s, _) => *s,
            OperationResult::PutPubFh(s) => *s,
            OperationResult::ExchangeId(s, _) => *s,
            OperationResult::CreateSession(s, _) => *s,
            OperationResult::DestroySession(s) => *s,
            OperationResult::DestroyClientId(s) => *s,
            OperationResult::Sequence(s, _) => *s,
            OperationResult::ReclaimComplete(s) => *s,
            OperationResult::SecInfoNoName(s) => *s,
            OperationResult::Allocate(s) => *s,
            OperationResult::Deallocate(s) => *s,
            OperationResult::Seek(s, _) => *s,
            OperationResult::Copy(s, _) => *s,
            OperationResult::Clone(s) => *s,
            OperationResult::ReadPlus(s, _) => *s,
            OperationResult::Lock(s, _) => *s,
            OperationResult::LockT(s) => *s,
            OperationResult::LockU(s, _) => *s,
            OperationResult::Unsupported(s) => *s,
        }
    }
}

// Helper structs for complex operation results

#[derive(Debug, Clone)]
pub struct OpenHow {
    pub createmode: u32,
    pub attrs: Option<Bytes>,
}

#[derive(Debug, Clone)]
pub struct OpenClaim {
    pub claim_type: u32,
    pub file: String,
}

#[derive(Debug, Clone)]
pub struct OpenResult {
    pub stateid: StateId,
    pub change_info: ChangeInfo,
    pub result_flags: u32,
    pub attrset: Vec<u32>,
    pub delegation: Option<Delegation>,
}

#[derive(Debug, Clone)]
pub struct Delegation {
    pub delegation_type: u32,
    pub stateid: StateId,
}

#[derive(Debug, Clone)]
pub struct ReadDirResult {
    pub entries: Vec<DirEntry>,
    pub eof: bool,
    pub cookieverf: u64,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub cookie: u64,
    pub name: String,
    pub attrs: Bytes,
}

#[derive(Debug, Clone)]
pub struct ReadResult {
    pub eof: bool,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct WriteResult {
    pub count: u32,
    pub committed: u32,
    pub verifier: [u8; 8],
}

/// COMPOUND execution context
/// Maintains current and saved file handles across operations
pub struct CompoundContext {
    pub current_fh: Option<Nfs4FileHandle>,
    pub saved_fh: Option<Nfs4FileHandle>,
    pub minor_version: u32,
}

impl CompoundContext {
    pub fn new(minor_version: u32) -> Self {
        Self {
            current_fh: None,
            saved_fh: None,
            minor_version,
        }
    }

    /// Check if current filehandle is set
    pub fn has_current_fh(&self) -> bool {
        self.current_fh.is_some()
    }

    /// Get current filehandle (returns error if not set)
    pub fn get_current_fh(&self) -> Result<&Nfs4FileHandle, Nfs4Status> {
        self.current_fh.as_ref().ok_or(Nfs4Status::NoFileHandle)
    }

    /// Set current filehandle
    pub fn set_current_fh(&mut self, fh: Nfs4FileHandle) {
        self.current_fh = Some(fh);
    }

    /// Clear current filehandle
    pub fn clear_current_fh(&mut self) {
        self.current_fh = None;
    }

    /// Save current filehandle
    pub fn save_fh(&mut self) -> Result<(), Nfs4Status> {
        if let Some(fh) = &self.current_fh {
            self.saved_fh = Some(fh.clone());
            Ok(())
        } else {
            Err(Nfs4Status::NoFileHandle)
        }
    }

    /// Restore saved filehandle
    pub fn restore_fh(&mut self) -> Result<(), Nfs4Status> {
        if let Some(fh) = &self.saved_fh {
            self.current_fh = Some(fh.clone());
            Ok(())
        } else {
            Err(Nfs4Status::RestoReFh)
        }
    }
}

impl CompoundRequest {
    /// Decode a COMPOUND request from XDR
    pub fn decode(mut decoder: XdrDecoder) -> Result<Self, String> {
        eprintln!("DEBUG CompoundRequest::decode: Starting with {} bytes", decoder.remaining());

        // Decode tag
        let tag = decoder.decode_string()?;
        eprintln!("DEBUG CompoundRequest::decode: After tag decode (tag='{}'): {} bytes remaining", tag, decoder.remaining());

        // Decode minor version
        let minor_version = decoder.decode_u32()?;
        eprintln!("DEBUG CompoundRequest::decode: After minor_version decode (={}): {} bytes remaining", minor_version, decoder.remaining());

        // Decode operation count
        let op_count = decoder.decode_u32()? as usize;
        eprintln!("DEBUG CompoundRequest::decode: After op_count decode (={}): {} bytes remaining", op_count, decoder.remaining());
        debug!("COMPOUND: tag='{}', minor_version={}, op_count={}", tag, minor_version, op_count);

        // Decode operations
        let mut operations = Vec::with_capacity(op_count);
        for i in 0..op_count {
            let opcode = decoder.decode_u32()?;
            debug!("  Operation {}: opcode={}", i, opcode);

            let op = Self::decode_operation(&mut decoder, opcode)?;
            operations.push(op);
        }

        Ok(Self {
            tag,
            minor_version,
            operations,
        })
    }

    /// Decode a single operation
    fn decode_operation(decoder: &mut XdrDecoder, opcode: u32) -> Result<Operation, String> {
        match opcode {
            // File handle operations
            opcode::PUTROOTFH => Ok(Operation::PutRootFh),
            opcode::PUTPUBFH => Ok(Operation::PutPubFh),
            opcode::PUTFH => {
                let fh = decoder.decode_filehandle()?;
                Ok(Operation::PutFh(fh))
            }
            opcode::GETFH => Ok(Operation::GetFh),
            opcode::SAVEFH => Ok(Operation::SaveFh),
            opcode::RESTOREFH => Ok(Operation::RestoreFh),

            // Lookup and directory operations
            opcode::LOOKUP => {
                let component = decoder.decode_string()?;
                Ok(Operation::Lookup(component))
            }
            opcode::LOOKUPP => Ok(Operation::LookupP),
            opcode::READDIR => {
                let cookie = decoder.decode_u64()?;
                let verf_bytes = decoder.decode_fixed_opaque(8)?;
                let mut cookieverf = [0u8; 8];
                cookieverf.copy_from_slice(&verf_bytes[..8]);
                let dircount = decoder.decode_u32()?;
                let maxcount = decoder.decode_u32()?;
                let attr_request = decoder.decode_bitmap()?;
                Ok(Operation::ReadDir {
                    cookie,
                    cookieverf,
                    dircount,
                    maxcount,
                    attr_request,
                })
            }

            // Attribute operations
            opcode::GETATTR => {
                let bitmap = decoder.decode_bitmap()?;
                Ok(Operation::GetAttr(bitmap))
            }
            opcode::SETATTR => {
                let stateid = decoder.decode_stateid()?;
                let attrs = decoder.decode_opaque()?;
                Ok(Operation::SetAttr { stateid, attrs })
            }
            opcode::ACCESS => {
                let access = decoder.decode_u32()?;
                Ok(Operation::Access(access))
            }

            // File I/O operations
            opcode::OPEN => {
                let seqid = decoder.decode_u32()?;
                let share_access = decoder.decode_u32()?;
                let share_deny = decoder.decode_u32()?;
                
                // Owner (state_owner)
                let owner = decoder.decode_opaque()?.to_vec();
                
                // Openhow (discriminated union)
                let createmode = decoder.decode_u32()?;
                let openhow = match createmode {
                    0 => OpenHow { createmode, attrs: None },  // UNCHECKED4
                    1 => {
                        // GUARDED4 - decode createattrs
                        let attrs = decoder.decode_opaque()?;
                        OpenHow { createmode, attrs: Some(attrs) }
                    }
                    2 => {
                        // EXCLUSIVE4 - decode verifier (stored in attrs)
                        let verf = decoder.decode_fixed_opaque(8)?;
                        OpenHow { createmode, attrs: Some(verf) }
                    }
                    _ => OpenHow { createmode: 0, attrs: None },
                };
                
                // Claim (discriminated union)
                let claim_type = decoder.decode_u32()?;
                let file = match claim_type {
                    0 => decoder.decode_string()?,  // CLAIM_NULL - filename
                    _ => String::new(),  // Other claim types
                };
                let claim = OpenClaim { claim_type, file };
                
                Ok(Operation::Open {
                    seqid,
                    share_access,
                    share_deny,
                    owner,
                    openhow,
                    claim,
                })
            }
            opcode::CLOSE => {
                let seqid = decoder.decode_u32()?;
                let stateid = decoder.decode_stateid()?;
                Ok(Operation::Close { seqid, stateid })
            }
            opcode::READ => {
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let count = decoder.decode_u32()?;
                Ok(Operation::Read { stateid, offset, count })
            }
            opcode::WRITE => {
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let stable = decoder.decode_u32()?;
                let data = decoder.decode_opaque()?;
                Ok(Operation::Write { stateid, offset, stable, data })
            }
            opcode::COMMIT => {
                let offset = decoder.decode_u64()?;
                let count = decoder.decode_u32()?;
                Ok(Operation::Commit { offset, count })
            }

            // Modify operations
            opcode::CREATE => {
                let objtype_raw = decoder.decode_u32()?;
                let objtype = match objtype_raw {
                    1 => Nfs4FileType::Regular,
                    2 => Nfs4FileType::Directory,
                    3 => Nfs4FileType::BlockDevice,
                    4 => Nfs4FileType::CharDevice,
                    5 => Nfs4FileType::Symlink,
                    6 => Nfs4FileType::Socket,
                    7 => Nfs4FileType::Fifo,
                    8 => Nfs4FileType::AttrDir,
                    9 => Nfs4FileType::NamedAttr,
                    _ => Nfs4FileType::Regular,
                };
                let objname = decoder.decode_string()?;
                Ok(Operation::Create { objtype, objname })
            }
            opcode::REMOVE => {
                let component = decoder.decode_string()?;
                Ok(Operation::Remove(component))
            }
            opcode::RENAME => {
                let oldname = decoder.decode_string()?;
                let newname = decoder.decode_string()?;
                Ok(Operation::Rename { oldname, newname })
            }
            opcode::LINK => {
                let newname = decoder.decode_string()?;
                Ok(Operation::Link(newname))
            }
            opcode::READLINK => Ok(Operation::ReadLink),

            // Session operations (NFSv4.1)
            opcode::EXCHANGE_ID => {
                // ClientOwner structure: verifier FIRST, then id
                // Verifier (8 bytes)
                let verifier_bytes = decoder.decode_verifier()?;
                let verifier = u64::from_be_bytes(verifier_bytes);

                // Client ID (opaque string)
                let client_id_bytes = decoder.decode_opaque()?;

                let clientowner = ClientId {
                    verifier,
                    id: client_id_bytes.to_vec(),
                };
                
                let flags = decoder.decode_u32()?;
                let state_protect = decoder.decode_u32()?;
                
                // Implementation ID (optional) - for now skip
                let has_impl_id = decoder.decode_bool()?;
                let impl_id = if has_impl_id {
                    decoder.decode_opaque()?.to_vec()
                } else {
                    Vec::new()
                };
                
                Ok(Operation::ExchangeId {
                    clientowner,
                    flags,
                    state_protect,
                    impl_id,
                })
            }
            opcode::CREATE_SESSION => {
                let clientid = decoder.decode_u64()?;
                let sequence = decoder.decode_u32()?;
                let flags = decoder.decode_u32()?;
                
                // Fore channel attributes
                let fore_chan_attrs = ChannelAttrs {
                    header_pad_size: decoder.decode_u32()?,
                    max_request_size: decoder.decode_u32()?,
                    max_response_size: decoder.decode_u32()?,
                    max_response_size_cached: decoder.decode_u32()?,
                    max_operations: decoder.decode_u32()?,
                    max_requests: decoder.decode_u32()?,
                };

                // Back channel attributes
                let back_chan_attrs = ChannelAttrs {
                    header_pad_size: decoder.decode_u32()?,
                    max_request_size: decoder.decode_u32()?,
                    max_response_size: decoder.decode_u32()?,
                    max_response_size_cached: decoder.decode_u32()?,
                    max_operations: decoder.decode_u32()?,
                    max_requests: decoder.decode_u32()?,
                };
                
                Ok(Operation::CreateSession {
                    clientid,
                    sequence,
                    flags,
                    fore_chan_attrs,
                    back_chan_attrs,
                })
            }
            opcode::DESTROY_SESSION => {
                let sessionid = decoder.decode_sessionid()?;
                Ok(Operation::DestroySession(sessionid))
            }
            opcode::DESTROY_CLIENTID => {
                let clientid = decoder.decode_u64()?;
                Ok(Operation::DestroyClientId(clientid))
            }
            opcode::SEQUENCE => {
                let sessionid = decoder.decode_sessionid()?;
                let sequenceid = decoder.decode_u32()?;
                let slotid = decoder.decode_u32()?;
                let highest_slotid = decoder.decode_u32()?;
                let cachethis = decoder.decode_bool()?;
                Ok(Operation::Sequence {
                    sessionid,
                    sequenceid,
                    slotid,
                    highest_slotid,
                    cachethis,
                })
            }
            opcode::RECLAIM_COMPLETE => {
                let one_fs = decoder.decode_bool()?;
                Ok(Operation::ReclaimComplete(one_fs))
            }

            // Lock operations
            opcode::LOCK => {
                let locktype = decoder.decode_u32()?;
                let reclaim = decoder.decode_bool()?;
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                let stateid = decoder.decode_stateid()?;
                let owner = decoder.decode_opaque()?.to_vec();
                Ok(Operation::Lock {
                    locktype,
                    reclaim,
                    offset,
                    length,
                    stateid,
                    owner,
                })
            }
            opcode::LOCKT => {
                let locktype = decoder.decode_u32()?;
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                let owner = decoder.decode_opaque()?.to_vec();
                Ok(Operation::LockT {
                    locktype,
                    offset,
                    length,
                    owner,
                })
            }
            opcode::LOCKU => {
                let locktype = decoder.decode_u32()?;
                let seqid = decoder.decode_u32()?;
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                Ok(Operation::LockU {
                    locktype,
                    seqid,
                    stateid,
                    offset,
                    length,
                })
            }

            // NFSv4.2 Performance operations
            opcode::ALLOCATE => {
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                Ok(Operation::Allocate { stateid, offset, length })
            }
            opcode::DEALLOCATE => {
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                Ok(Operation::Deallocate { stateid, offset, length })
            }
            opcode::SEEK => {
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let what = decoder.decode_u32()?;
                Ok(Operation::Seek { stateid, offset, what })
            }
            opcode::COPY => {
                let src_stateid = decoder.decode_stateid()?;
                let dst_stateid = decoder.decode_stateid()?;
                let src_offset = decoder.decode_u64()?;
                let dst_offset = decoder.decode_u64()?;
                let count = decoder.decode_u64()?;
                let consecutive = decoder.decode_bool()?;
                let synchronous = decoder.decode_bool()?;
                Ok(Operation::Copy {
                    src_stateid,
                    dst_stateid,
                    src_offset,
                    dst_offset,
                    count,
                    consecutive,
                    synchronous,
                })
            }
            opcode::CLONE => {
                let src_stateid = decoder.decode_stateid()?;
                let dst_stateid = decoder.decode_stateid()?;
                let src_offset = decoder.decode_u64()?;
                let dst_offset = decoder.decode_u64()?;
                let count = decoder.decode_u64()?;
                Ok(Operation::Clone {
                    src_stateid,
                    dst_stateid,
                    src_offset,
                    dst_offset,
                    count,
                })
            }
            opcode::READ_PLUS => {
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let count = decoder.decode_u32()?;
                Ok(Operation::ReadPlus { stateid, offset, count })
            }
            opcode::IO_ADVISE => {
                let stateid = decoder.decode_stateid()?;
                let offset = decoder.decode_u64()?;
                let count = decoder.decode_u64()?;
                let hints = decoder.decode_u32()?;
                Ok(Operation::IoAdvise { stateid, offset, count, hints })
            }
            
            // Security operations
            opcode::SECINFO_NO_NAME => {
                // SECINFO_NO_NAME takes a style argument (RFC 5661 Section 18.45)
                let style = decoder.decode_u32()?;
                Ok(Operation::SecInfoNoName(style))
            }

            // For now, return unsupported for operations we haven't implemented yet
            _ => {
                warn!("Unsupported operation: {}", opcode);
                Ok(Operation::Unsupported(opcode))
            }
        }
    }
}

impl CompoundResponse {
    /// Encode a COMPOUND response to XDR
    pub fn encode(self) -> Bytes {
        let mut encoder = XdrEncoder::new();

        // Encode overall status
        encoder.encode_status(self.status);

        // Encode tag
        encoder.encode_string(&self.tag);

        // Encode result count
        let result_count = self.results.len();
        encoder.encode_u32(result_count as u32);
        debug!("🔍 Encoding COMPOUND response with {} results", result_count);

        // Encode each result
        for (i, result) in self.results.into_iter().enumerate() {
            debug!("   Encoding result #{}: {:?}", i, std::mem::discriminant(&result));
            Self::encode_result(&mut encoder, result);
        }

        let bytes = encoder.finish();
        eprintln!("DEBUG CompoundResponse: Sending {} bytes", bytes.len());
        eprintln!("DEBUG CompoundResponse: First 80 bytes: {:02x?}", &bytes[..bytes.len().min(80)]);
        debug!("✅ COMPOUND response encoded: {} results, {} bytes total", result_count, bytes.len());
        bytes
    }

    /// Encode a single operation result
    fn encode_result(encoder: &mut XdrEncoder, result: OperationResult) {
        match result {
            // File handle operations
            OperationResult::PutRootFh(status) => {
                encoder.encode_u32(opcode::PUTROOTFH);
                encoder.encode_status(status);
            }
            OperationResult::PutFh(status) => {
                encoder.encode_u32(opcode::PUTFH);
                encoder.encode_status(status);
            }
            OperationResult::GetFh(status, fh) => {
                encoder.encode_u32(opcode::GETFH);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(fh) = fh {
                        debug!("🔍 Encoding GETFH response: filehandle {} bytes", fh.data.len());
                        encoder.encode_filehandle(&fh);
                        debug!("✅ GETFH filehandle encoded");
                    } else {
                        warn!("⚠️  GETFH encoding: no filehandle to encode!");
                    }
                }
            }
            OperationResult::SaveFh(status) => {
                encoder.encode_u32(opcode::SAVEFH);
                encoder.encode_status(status);
            }
            OperationResult::RestoreFh(status) => {
                encoder.encode_u32(opcode::RESTOREFH);
                encoder.encode_status(status);
            }

            // Lookup and directory operations
            OperationResult::Lookup(status) => {
                encoder.encode_u32(opcode::LOOKUP);
                encoder.encode_status(status);
            }
            OperationResult::ReadDir(status, result) => {
                encoder.encode_u32(opcode::READDIR);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        // Encode cookieverf (u64)
                        encoder.encode_u64(res.cookieverf);
                        
                        // RFC 5661: dirlist4 is a linked list of entry4
                        // Each entry has: cookie, name, attrs, nextentry pointer
                        
                        if res.entries.is_empty() {
                            // Empty directory: value_follows = FALSE, then EOF
                            encoder.encode_bool(false);
                            encoder.encode_bool(res.eof);
                        } else {
                            // Encode directory entries as linked list
                            for (i, entry) in res.entries.iter().enumerate() {
                                // value_follows (or next_entry for subsequent entries)
                                encoder.encode_bool(true);
                                
                                // entry4 fields
                                encoder.encode_u64(entry.cookie);
                                encoder.encode_string(&entry.name);
                                
                                // Attrs are already pre-encoded as Bytes (fattr4 structure)
                                encoder.append_raw(&entry.attrs);
                            }
                            
                            // End of list: nextentry = FALSE
                            encoder.encode_bool(false);
                            
                            // EOF flag
                            encoder.encode_bool(res.eof);
                        }
                    }
                }
            }

            // Attribute operations
            OperationResult::GetAttr(status, attrs) => {
                encoder.encode_u32(opcode::GETATTR);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(attrs) = attrs {
                        // attrs already contains the properly encoded fattr4 structure  
                        // (bitmap + attr_vals), so write it directly without opaque wrapper
                        debug!("📝 GETATTR encoding: appending {} bytes directly (no opaque wrapper)", attrs.len());
                        debug!("   First 32 bytes: {:02x?}", &attrs[..attrs.len().min(32)]);
                        encoder.append_raw(&attrs);
                        debug!("✅ GETATTR fattr4 appended");
                    }
                }
            }
            OperationResult::SetAttr(status) => {
                encoder.encode_u32(opcode::SETATTR);
                encoder.encode_status(status);
            }
            OperationResult::Access(status, supported) => {
                encoder.encode_u32(opcode::ACCESS);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(access) = supported {
                        encoder.encode_u32(access);
                    }
                }
            }

            // File I/O operations
            OperationResult::Open(status, result) => {
                encoder.encode_u32(opcode::OPEN);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        encoder.encode_stateid(&res.stateid);
                        // Change info
                        encoder.encode_bool(res.change_info.atomic);
                        encoder.encode_u64(res.change_info.before);
                        encoder.encode_u64(res.change_info.after);
                        // Result flags
                        encoder.encode_u32(res.result_flags);
                        // Attrset bitmap
                        encoder.encode_bitmap(&res.attrset);
                        // Delegation (simplified - None for now)
                        encoder.encode_u32(0); // OPEN_DELEGATE_NONE
                    }
                }
            }
            OperationResult::Close(status, stateid) => {
                encoder.encode_u32(opcode::CLOSE);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(sid) = stateid {
                        encoder.encode_stateid(&sid);
                    }
                }
            }
            OperationResult::Read(status, result) => {
                encoder.encode_u32(opcode::READ);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        encoder.encode_bool(res.eof);
                        encoder.encode_opaque(&res.data);
                    }
                }
            }
            OperationResult::Write(status, result) => {
                encoder.encode_u32(opcode::WRITE);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        encoder.encode_u32(res.count);
                        encoder.encode_u32(res.committed);
                        encoder.encode_fixed_opaque(&res.verifier);
                    }
                }
            }
            OperationResult::Commit(status, verifier) => {
                encoder.encode_u32(opcode::COMMIT);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(verf) = verifier {
                        encoder.encode_fixed_opaque(&verf);
                    }
                }
            }

            // Modify operations
            OperationResult::Create(status) => {
                encoder.encode_u32(opcode::CREATE);
                encoder.encode_status(status);
            }
            OperationResult::Remove(status) => {
                encoder.encode_u32(opcode::REMOVE);
                encoder.encode_status(status);
            }
            OperationResult::Rename(status, source_cinfo, target_cinfo) => {
                encoder.encode_u32(opcode::RENAME);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    // Source directory change info
                    if let Some(cinfo) = source_cinfo {
                        encoder.encode_bool(cinfo.atomic);
                        encoder.encode_u64(cinfo.before);
                        encoder.encode_u64(cinfo.after);
                    }
                    // Target directory change info
                    if let Some(cinfo) = target_cinfo {
                        encoder.encode_bool(cinfo.atomic);
                        encoder.encode_u64(cinfo.before);
                        encoder.encode_u64(cinfo.after);
                    }
                }
            }
            OperationResult::Link(status, change_info) => {
                encoder.encode_u32(opcode::LINK);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(cinfo) = change_info {
                        encoder.encode_bool(cinfo.atomic);
                        encoder.encode_u64(cinfo.before);
                        encoder.encode_u64(cinfo.after);
                    }
                }
            }
            OperationResult::ReadLink(status, link) => {
                encoder.encode_u32(opcode::READLINK);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(target) = link {
                        encoder.encode_string(&target);
                    }
                }
            }
            OperationResult::PutPubFh(status) => {
                encoder.encode_u32(opcode::PUTPUBFH);
                encoder.encode_status(status);
            }

            // Session operations (NFSv4.1)
            OperationResult::ExchangeId(status, result) => {
                encoder.encode_u32(opcode::EXCHANGE_ID);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        debug!("🔍 Encoding EXCHANGE_ID response: clientid={}, sequenceid={}, flags={}", 
                               res.clientid, res.sequenceid, res.flags);
                        debug!("   server_owner={}, server_scope={:?}", 
                               res.server_owner, res.server_scope);
                        encoder.encode_u64(res.clientid);
                        encoder.encode_u32(res.sequenceid);
                        encoder.encode_u32(res.flags);
                        encoder.encode_u32(0); // state_protect: SP4_NONE
                        // server_owner4: struct with so_minor_id (u64) and so_major_id (opaque)
                        // Per RFC 8881 Section 18.35
                        encoder.encode_u64(0); // so_minor_id (using 0 for simplicity)
                        encoder.encode_string(&res.server_owner); // so_major_id
                        encoder.encode_opaque(&Bytes::from(res.server_scope));
                        // Implementation ID (empty array - length 0)
                        encoder.encode_u32(0);
                        debug!("✅ EXCHANGE_ID encoded: total response will include clientid={}", res.clientid);
                    }
                }
            }
            OperationResult::CreateSession(status, result) => {
                encoder.encode_u32(opcode::CREATE_SESSION);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        debug!("🔍 Encoding CREATE_SESSION response:");
                        debug!("   sessionid={:?}", res.sessionid);
                        debug!("   sequenceid={}, flags={}", res.sequenceid, res.flags);
                        
                        encoder.encode_sessionid(&res.sessionid);
                        encoder.encode_u32(res.sequenceid);
                        encoder.encode_u32(res.flags);
                        
                        // Fore channel attributes
                        let fore = &res.fore_chan_attrs;
                        debug!("   Fore channel: pad={}, max_req={}, max_resp={}, max_resp_cached={}, max_ops={}, max_reqs={}",
                               fore.header_pad_size, fore.max_request_size, fore.max_response_size,
                               fore.max_response_size_cached, fore.max_operations, fore.max_requests);
                        encoder.encode_u32(fore.header_pad_size);
                        encoder.encode_u32(fore.max_request_size);
                        encoder.encode_u32(fore.max_response_size);
                        encoder.encode_u32(fore.max_response_size_cached);
                        encoder.encode_u32(fore.max_operations);
                        encoder.encode_u32(fore.max_requests);
                        encoder.encode_u32(0); // ca_rdma_ird<> array length (empty for non-RDMA)

                        // Back channel attributes
                        let back = &res.back_chan_attrs;
                        debug!("   Back channel: pad={}, max_req={}, max_resp={}, max_resp_cached={}, max_ops={}, max_reqs={}",
                               back.header_pad_size, back.max_request_size, back.max_response_size,
                               back.max_response_size_cached, back.max_operations, back.max_requests);
                        encoder.encode_u32(back.header_pad_size);
                        encoder.encode_u32(back.max_request_size);
                        encoder.encode_u32(back.max_response_size);
                        encoder.encode_u32(back.max_response_size_cached);
                        encoder.encode_u32(back.max_operations);
                        encoder.encode_u32(back.max_requests);
                        encoder.encode_u32(0); // ca_rdma_ird<> array length (empty for non-RDMA)
                        
                        debug!("✅ CREATE_SESSION encoded successfully");
                    }
                }
            }
            OperationResult::DestroySession(status) => {
                encoder.encode_u32(opcode::DESTROY_SESSION);
                encoder.encode_status(status);
            }
            OperationResult::DestroyClientId(status) => {
                encoder.encode_u32(opcode::DESTROY_CLIENTID);
                encoder.encode_status(status);
            }
            OperationResult::Sequence(status, seq_res) => {
                encoder.encode_u32(opcode::SEQUENCE);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = seq_res {
                        debug!("🔍 Encoding SEQUENCE response:");
                        debug!("   sessionid={:?}", res.sessionid);
                        debug!("   sequenceid={}, slotid={}", res.sequenceid, res.slotid);
                        debug!("   highest_slotid={}, target_highest_slotid={}", 
                               res.highest_slotid, res.target_highest_slotid);
                        debug!("   status_flags=0x{:08x}", res.status_flags);
                        
                        encoder.encode_sessionid(&res.sessionid);
                        encoder.encode_u32(res.sequenceid);
                        encoder.encode_u32(res.slotid);
                        encoder.encode_u32(res.highest_slotid);
                        encoder.encode_u32(res.target_highest_slotid);
                        encoder.encode_u32(res.status_flags);
                        
                        debug!("✅ SEQUENCE encoded");
                    }
                }
            }
            OperationResult::ReclaimComplete(status) => {
                encoder.encode_u32(opcode::RECLAIM_COMPLETE);
                encoder.encode_status(status);
            }
            OperationResult::SecInfoNoName(status) => {
                encoder.encode_u32(opcode::SECINFO_NO_NAME);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    // Return array of supported security flavors
                    // Per RFC 5661, return both AUTH_NONE and AUTH_SYS like Ganesha does
                    // We accept both (parse but don't enforce credentials)
                    encoder.encode_u32(2); // Array length: 2 flavors
                    encoder.encode_u32(0); // AUTH_NONE
                    encoder.encode_u32(1); // AUTH_SYS (Unix auth)
                }
            }

            // Lock operations
            OperationResult::Lock(status, stateid) => {
                encoder.encode_u32(opcode::LOCK);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(sid) = stateid {
                        encoder.encode_stateid(&sid);
                    }
                }
            }
            OperationResult::LockT(status) => {
                encoder.encode_u32(opcode::LOCKT);
                encoder.encode_status(status);
            }
            OperationResult::LockU(status, stateid) => {
                encoder.encode_u32(opcode::LOCKU);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(sid) = stateid {
                        encoder.encode_stateid(&sid);
                    }
                }
            }

            // NFSv4.2 Performance operations
            OperationResult::Allocate(status) => {
                encoder.encode_u32(opcode::ALLOCATE);
                encoder.encode_status(status);
            }
            OperationResult::Deallocate(status) => {
                encoder.encode_u32(opcode::DEALLOCATE);
                encoder.encode_status(status);
            }
            OperationResult::Seek(status, result) => {
                encoder.encode_u32(opcode::SEEK);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        encoder.encode_bool(res.eof);
                        encoder.encode_u64(res.offset);
                    }
                }
            }
            OperationResult::Copy(status, result) => {
                encoder.encode_u32(opcode::COPY);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        encoder.encode_u64(res.count);
                        encoder.encode_bool(res.consecutive);
                        encoder.encode_bool(res.synchronous);
                    }
                }
            }
            OperationResult::Clone(status) => {
                encoder.encode_u32(opcode::CLONE);
                encoder.encode_status(status);
            }
            OperationResult::ReadPlus(status, result) => {
                encoder.encode_u32(opcode::READ_PLUS);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = result {
                        encoder.encode_bool(res.eof);
                        
                        // Encode segments
                        encoder.encode_u32(res.segments.len() as u32);
                        for segment in res.segments {
                            match segment {
                                ReadPlusSegment::Data { offset, data } => {
                                    encoder.encode_u32(0); // DATA
                                    encoder.encode_u64(offset);
                                    encoder.encode_opaque(&data);
                                }
                                ReadPlusSegment::Hole { offset, length } => {
                                    encoder.encode_u32(1); // HOLE
                                    encoder.encode_u64(offset);
                                    encoder.encode_u64(length);
                                }
                            }
                        }
                    }
                }
            }

            // Unsupported operations
            OperationResult::Unsupported(status) => {
                encoder.encode_status(status);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::xdr::XdrDecoder;

    #[test]
    fn test_getattr_response_encoding() {
        // This test verifies that GETATTR response is encoded correctly per RFC 5661
        // The bug was that we were wrapping the fattr4 structure in encode_opaque(),
        // which added an extra length prefix that the Linux NFS client couldn't parse.
        
        // Create a mock fattr4 structure with bitmap and attribute values
        // Simulating a response for attributes: TYPE (1), SIZE (3)
        let mut attr_vals = BytesMut::new();
        attr_vals.put_u32(2); // NF4DIR (directory type)
        attr_vals.put_u64(4096); // size = 4096 bytes
        
        let fattr = Fattr4 {
            attrmask: vec![0x0000000A], // bits 1 and 3 set (TYPE=1, SIZE=3)
            attr_vals: attr_vals.to_vec(),
        };
        
        // Encode using dispatcher logic (what goes into attrs bytes)
        let mut dispatcher_buf = BytesMut::new();
        dispatcher_buf.put_u32(fattr.attrmask.len() as u32); // bitmap array length
        for &word in &fattr.attrmask {
            dispatcher_buf.put_u32(word);
        }
        dispatcher_buf.put_u32(fattr.attr_vals.len() as u32); // attr_vals length
        dispatcher_buf.put_slice(&fattr.attr_vals);
        
        let attrs_bytes = dispatcher_buf.to_vec();
        
        // Now encode the full GETATTR response
        let result = OperationResult::GetAttr(Nfs4Status::Ok, Some(attrs_bytes.clone()));
        
        let mut encoder = XdrEncoder::new();
        let mut response = CompoundResponse::new();
        response.encode_single_result(&result, &mut encoder);
        
        let encoded = encoder.finish();
        
        // Decode and verify the structure
        let mut decoder = XdrDecoder::new(encoded);
        
        // Should be: opcode (u32) + status (u32) + fattr4
        let opcode = decoder.decode_u32().expect("decode opcode");
        assert_eq!(opcode, opcode::GETATTR);
        
        let status = decoder.decode_u32().expect("decode status");
        assert_eq!(status, Nfs4Status::Ok.to_u32());
        
        // Now should come the fattr4 structure DIRECTLY (not wrapped in opaque)
        // fattr4 = bitmap array + attr_vals
        let bitmap_len = decoder.decode_u32().expect("decode bitmap len");
        assert_eq!(bitmap_len, 1, "Should have 1 bitmap word");
        
        let bitmap_word0 = decoder.decode_u32().expect("decode bitmap word 0");
        assert_eq!(bitmap_word0, 0x0000000A, "Bitmap should have bits 1,3 set");
        
        let attr_vals_len = decoder.decode_u32().expect("decode attr_vals len");
        assert_eq!(attr_vals_len, 12, "attr_vals should be 12 bytes (u32 + u64)");
        
        let type_val = decoder.decode_u32().expect("decode type");
        assert_eq!(type_val, 2, "Type should be NF4DIR");
        
        let size_val = decoder.decode_u64().expect("decode size");
        assert_eq!(size_val, 4096, "Size should be 4096");
        
        // Should have consumed all data
        assert_eq!(decoder.remaining(), 0, "Should have no remaining bytes");
    }
    
    #[test]
    fn test_getattr_no_double_wrapping() {
        // Verify that we DON'T wrap fattr4 in encode_opaque (which would add extra length)
        // The old buggy code did: encode_opaque(&attrs) which added a u32 length prefix
        
        let attrs_bytes = vec![
            0x00, 0x00, 0x00, 0x01, // bitmap array length = 1
            0x00, 0x00, 0x00, 0x02, // bitmap word 0 = 0x02 (bit 1 = TYPE)
            0x00, 0x00, 0x00, 0x04, // attr_vals length = 4 bytes
            0x00, 0x00, 0x00, 0x01, // TYPE = NF4REG (regular file)
        ];
        
        let result = OperationResult::GetAttr(Nfs4Status::Ok, Some(attrs_bytes.clone()));
        
        let mut encoder = XdrEncoder::new();
        let mut response = CompoundResponse::new();
        response.encode_single_result(&result, &mut encoder);
        
        let encoded = encoder.finish();
        
        // Encoded should be: opcode (4) + status (4) + attrs_bytes (16) = 24 bytes total
        assert_eq!(encoded.len(), 24, 
            "Expected 24 bytes: 4 (opcode) + 4 (status) + 16 (fattr4). Got {} bytes", 
            encoded.len());
        
        // If we had wrongly used encode_opaque, it would be:
        // 4 (opcode) + 4 (status) + 4 (opaque length) + 16 (data) = 28 bytes
        // So the test would fail if the bug was present
        
        // Verify the bytes directly
        let bytes: Vec<u8> = encoded.to_vec();
        assert_eq!(&bytes[0..4], &[0x00, 0x00, 0x00, opcode::GETATTR as u8], "opcode");
        assert_eq!(&bytes[4..8], &[0x00, 0x00, 0x00, 0x00], "status OK");
        assert_eq!(&bytes[8..], &attrs_bytes[..], "fattr4 data should follow directly");
    }
    
    #[test]
    fn test_secinfo_no_name_dual_flavors() {
        // Verify SECINFO_NO_NAME returns both AUTH_NONE and AUTH_SYS
        // This matches Ganesha behavior and gives clients flexibility
        
        let result = OperationResult::SecInfoNoName(Nfs4Status::Ok);
        
        let mut encoder = XdrEncoder::new();
        let mut response = CompoundResponse::new();
        response.encode_single_result(&result, &mut encoder);
        
        let encoded = encoder.finish();
        let mut decoder = XdrDecoder::new(encoded);
        
        let opcode = decoder.decode_u32().expect("decode opcode");
        assert_eq!(opcode, opcode::SECINFO_NO_NAME);
        
        let status = decoder.decode_u32().expect("decode status");
        assert_eq!(status, Nfs4Status::Ok.to_u32());
        
        // Should have array of 2 security flavors
        let flavor_count = decoder.decode_u32().expect("decode flavor count");
        assert_eq!(flavor_count, 2, "Should return 2 security flavors");
        
        let flavor1 = decoder.decode_u32().expect("decode flavor 1");
        assert_eq!(flavor1, 0, "First flavor should be AUTH_NONE (0)");
        
        let flavor2 = decoder.decode_u32().expect("decode flavor 2");
        assert_eq!(flavor2, 1, "Second flavor should be AUTH_SYS (1)");
        
        assert_eq!(decoder.remaining(), 0, "Should have consumed all data");
    }
}
