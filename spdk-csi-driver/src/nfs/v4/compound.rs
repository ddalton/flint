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
    Sequence {
        sessionid: SessionId,
        sequenceid: u32,
        slotid: u32,
        highest_slotid: u32,
        cachethis: bool,
    },
    ReclaimComplete(bool),       // one_fs

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
#[derive(Debug, Clone, Default)]
pub struct ChannelAttrs {
    pub headerpadsize: u32,
    pub maxrequestsize: u32,
    pub maxresponsesize: u32,
    pub maxresponsesize_cached: u32,
    pub maxoperations: u32,
    pub maxrequests: u32,
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
    Rename(Nfs4Status),

    // Sessions
    ExchangeId(Nfs4Status, Option<ExchangeIdResult>),
    CreateSession(Nfs4Status, Option<CreateSessionResult>),
    DestroySession(Nfs4Status),
    Sequence(Nfs4Status, Option<SequenceResult>),
    ReclaimComplete(Nfs4Status),

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
            OperationResult::Rename(s) => *s,
            OperationResult::ExchangeId(s, _) => *s,
            OperationResult::CreateSession(s, _) => *s,
            OperationResult::DestroySession(s) => *s,
            OperationResult::Sequence(s, _) => *s,
            OperationResult::ReclaimComplete(s) => *s,
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
        // Decode tag
        let tag = decoder.decode_string()?;

        // Decode minor version
        let minor_version = decoder.decode_u32()?;

        // Decode operation count
        let op_count = decoder.decode_u32()? as usize;
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
            opcode::PUTROOTFH => Ok(Operation::PutRootFh),
            opcode::PUTFH => {
                let fh = decoder.decode_filehandle()?;
                Ok(Operation::PutFh(fh))
            }
            opcode::GETFH => Ok(Operation::GetFh),
            opcode::SAVEFH => Ok(Operation::SaveFh),
            opcode::RESTOREFH => Ok(Operation::RestoreFh),

            opcode::GETATTR => {
                let bitmap = decoder.decode_bitmap()?;
                Ok(Operation::GetAttr(bitmap))
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
        encoder.encode_u32(self.results.len() as u32);

        // Encode each result
        for result in self.results {
            Self::encode_result(&mut encoder, result);
        }

        encoder.finish()
    }

    /// Encode a single operation result
    fn encode_result(encoder: &mut XdrEncoder, result: OperationResult) {
        match result {
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
                        encoder.encode_filehandle(&fh);
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
            OperationResult::GetAttr(status, attrs) => {
                encoder.encode_u32(opcode::GETATTR);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(attrs) = attrs {
                        // Encode attributes opaque data
                        encoder.encode_opaque(&attrs);
                    }
                }
            }
            OperationResult::Sequence(status, seq_res) => {
                encoder.encode_u32(opcode::SEQUENCE);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(res) = seq_res {
                        encoder.encode_sessionid(&res.sessionid);
                        encoder.encode_u32(res.sequenceid);
                        encoder.encode_u32(res.slotid);
                        encoder.encode_u32(res.highest_slotid);
                        encoder.encode_u32(res.target_highest_slotid);
                        encoder.encode_u32(res.status_flags);
                    }
                }
            }
            OperationResult::Unsupported(status) => {
                // For unsupported operations, just encode status
                encoder.encode_status(status);
            }
            _ => {
                // TODO: Implement encoding for other operation results
                warn!("Result encoding not yet implemented for this operation type");
            }
        }
    }
}
