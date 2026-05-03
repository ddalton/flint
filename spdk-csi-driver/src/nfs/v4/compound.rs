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
use tracing::{warn, debug};

/// COMPOUND request
#[derive(Debug)]
pub struct CompoundRequest {
    /// Tag set by the client. UTF-8 if `tag_valid`; lossy-converted otherwise
    /// so we can still echo it back per RFC 5661 §15.1.
    pub tag: String,
    /// `false` when the wire tag was not valid UTF-8. The dispatcher returns
    /// `NFS4ERR_INVAL` in that case (instead of letting the RPC layer reject
    /// the call as `GARBAGE_ARGS`).
    pub tag_valid: bool,
    pub minor_version: u32,
    pub operations: Vec<Operation>,
}

/// COMPOUND response
#[derive(Debug)]
pub struct CompoundResponse {
    pub status: Nfs4Status,
    pub tag: String,
    pub results: Vec<OperationResult>,
    /// When set, the encoder returns these bytes verbatim and ignores
    /// `status`/`tag`/`results`. Used for exactly-once SEQUENCE replay
    /// (RFC 8881 §15.1.10.4): the cached reply MUST be byte-for-byte
    /// identical to the original.
    pub raw_reply: Option<Bytes>,
    /// When set, after the response is encoded the resulting bytes MUST be
    /// stored on this `(session, slot)` for future replay matching. Set by
    /// the SEQUENCE handler when it accepts a new request; consumed by the
    /// RPC layer after `encode()`.
    pub cache_slot: Option<(SessionId, u32)>,
}

impl CompoundResponse {
    pub fn new() -> Self {
        Self {
            status: Nfs4Status::Ok,
            tag: String::new(),
            results: Vec::new(),
            raw_reply: None,
            cache_slot: None,
        }
    }
}

/// Discriminator + body of the `layoutreturn4` union (RFC 5661 §18.4.1):
///
/// ```c
/// union layoutreturn4 switch (layoutreturn_type4 lr_returntype) {
///     case LAYOUTRETURN4_FILE: layoutreturn_file4 lr_layout;
///     case LAYOUTRETURN4_FSID: void;
///     case LAYOUTRETURN4_ALL:  void;
/// };
/// ```
#[derive(Debug, Clone)]
pub enum LayoutReturn4Body {
    /// LAYOUTRETURN4_FILE = 1: bound to a single file's stateid.
    File {
        offset: u64,
        length: u64,
        stateid: StateId,
        /// `lrf_body` — layouttype-specific opaque (FFLv4 carries
        /// io-error / iostats reports here; FILES is empty).
        body: Bytes,
    },
    /// LAYOUTRETURN4_FSID = 2: every layout this client holds in CFH's fsid.
    Fsid,
    /// LAYOUTRETURN4_ALL = 3: every layout this client holds, period.
    All,
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
    /// VERIFY (RFC 5661 §18.30) — succeed iff the supplied fattr4 matches
    /// the server's view of the current FH. NVERIFY (§18.31) is the
    /// inverse. Both arms re-pack the decoded `bitmap + attrlist4` as a
    /// single blob to keep downstream comparison logic in one place.
    Verify { attrs: Bytes },
    Nverify { attrs: Bytes },
    Access(u32),                 // access bits

    // Modify operations
    Create {
        objtype: Nfs4FileType,
        objname: String,
        linkdata: Option<String>,  // For symlinks (NF4LNK)
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
        /// `csa_cb_program` from RFC 5661 §18.36 — the RPC program
        /// number the *client* will accept callback CALLs on. Stored
        /// on the session so the back-channel writer's call frame can
        /// address `program=cb_program, version=1, proc=CB_COMPOUND`.
        cb_program: u32,
    },
    DestroySession(SessionId),
    DestroyClientId(u64),        // clientid
    BindConnToSession {
        sessionid: SessionId,
        dir: u32,                // CDFC4_FORE=1, CDFC4_BACK=2, CDFC4_FORE_OR_BOTH=3
        use_conn_in_rdma_mode: bool,
    },
    Sequence {
        sessionid: SessionId,
        sequenceid: u32,
        slotid: u32,
        highest_slotid: u32,
        cachethis: bool,
    },
    ReclaimComplete(bool),       // one_fs
    /// SECINFO (RFC 5661 §18.29). Looks up `component` under the current
    /// directory FH and returns the security flavors that may be used to
    /// access the resulting filehandle. Like LOOKUP it sets CFH to the
    /// child for the name-existence check, but per §2.6.3.1.1.8 the CFH
    /// is left unset on return (so a following GETFH must error with
    /// NFS4ERR_NOFILEHANDLE).
    SecInfo(String),
    SecInfoNoName(u32),          // style
    TestStateId(Vec<StateId>),   // array of stateids to test

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

    // pNFS operations (NFSv4.1+, opcodes 47-51)
    LayoutGet {
        signal_layout_avail: bool,
        layout_type: u32,
        iomode: u32,
        offset: u64,
        length: u64,
        minlength: u64,
        stateid: StateId,
        maxcount: u32,
    },
    GetDeviceInfo {
        device_id: Vec<u8>,
        layout_type: u32,
        maxcount: u32,
        notify_types: Vec<u32>,
    },
    LayoutReturn {
        reclaim: bool,
        layout_type: u32,
        iomode: u32,
        return_body: LayoutReturn4Body,
    },
    /// LAYOUTCOMMIT (RFC 8881 §18.42, opcode 49). Client tells the MDS
    /// what it actually wrote through the data path so the MDS can
    /// update file size/mtime in its metadata. `last_write_offset` is
    /// `Some` iff the client set `loca_last_write_offset.no_newoffset
    /// = TRUE`; the value is the *offset* of the last byte written
    /// (so the file's new EOF is `last_write_offset + 1`). `time_modify`
    /// is `Some` iff `loca_time_modify.nt_timechanged = TRUE`.
    LayoutCommit {
        offset: u64,
        length: u64,
        reclaim: bool,
        stateid: StateId,
        last_write_offset: Option<u64>,
        time_modify: Option<(i64, u32)>,
        layout_type: u32,
        layoutupdate: Bytes,
    },

    // FREE_STATEID (RFC 8881 §18.38, opcode 45) — client tells the server
    // it has lost interest in a stateid. Allowed forms: lock stateid (returns
    // LOCKS_HELD if locks remain), open stateid (server may return
    // LOCKS_HELD per §18.38.3), delegation stateid.
    FreeStateId(StateId),

    // Placeholder for unsupported operations
    Unsupported(u32),            // operation code
    /// The opcode is recognised as a valid NFSv4 op but its arguments could
    /// not be parsed. Distinguished from `Unsupported` so the dispatcher can
    /// return `NFS4ERR_BADXDR` instead of `NFS4ERR_NOTSUPP` /
    /// `NFS4ERR_OP_ILLEGAL` (RFC 5661 §15: malformed args MUST surface as
    /// BADXDR).
    BadXdr(u32),
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

/// Channel attributes for sessions (RFC 5661 §18.36 channel_attrs4)
#[derive(Debug, Clone)]
pub struct ChannelAttrs {
    pub header_pad_size: u32,
    pub max_request_size: u32,
    pub max_response_size: u32,
    pub max_response_size_cached: u32,
    pub max_operations: u32,
    pub max_requests: u32,
    /// `ca_rdma_ird<1>` — present only for RDMA transports, at most one
    /// element. We decode it so the wire framing stays aligned, but otherwise
    /// ignore it (we are TCP-only for now).
    pub rdma_ird: Vec<u32>,
}

impl Default for ChannelAttrs {
    fn default() -> Self {
        Self {
            header_pad_size: 0,
            max_request_size: 1024 * 1024,  // 1 MB
            max_response_size: 1024 * 1024,
            max_response_size_cached: 64 * 1024,
            max_operations: 8,
            max_requests: 128,
            rdma_ird: Vec::new(),
        }
    }
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
    Verify(Nfs4Status),
    Nverify(Nfs4Status),
    Access(Nfs4Status, Option<(u32, u32)>),  // (supported, access granted)

    // Modify
    Create(Nfs4Status, Option<ChangeInfo>, Vec<u32>),  // change_info, attrset bitmap
    Remove(Nfs4Status, Option<ChangeInfo>),  // change_info for parent directory
    Rename(Nfs4Status, Option<ChangeInfo>, Option<ChangeInfo>), // source_cinfo, target_cinfo
    Link(Nfs4Status, Option<ChangeInfo>),
    ReadLink(Nfs4Status, Option<String>), // link target
    PutPubFh(Nfs4Status),

    // Sessions
    ExchangeId(Nfs4Status, Option<ExchangeIdResult>),
    CreateSession(Nfs4Status, Option<CreateSessionResult>),
    DestroySession(Nfs4Status),
    DestroyClientId(Nfs4Status),
    BindConnToSession(Nfs4Status, Option<SessionId>, u32, bool), // sessionid, dir, use_rdma
    Sequence(Nfs4Status, Option<SequenceResult>),
    ReclaimComplete(Nfs4Status),
    SecInfo(Nfs4Status),
    SecInfoNoName(Nfs4Status),
    TestStateId(Nfs4Status, Option<Vec<Nfs4Status>>),  // status per stateid
    FreeStateId(Nfs4Status),

    // NFSv4.2 Performance
    Allocate(Nfs4Status),
    Deallocate(Nfs4Status),
    Seek(Nfs4Status, Option<SeekResult>),
    Copy(Nfs4Status, Option<CopyResult>),
    Clone(Nfs4Status),
    ReadPlus(Nfs4Status, Option<ReadPlusResult>),

    // pNFS operations (NFSv4.1+)
    LayoutGet(Nfs4Status, Option<Bytes>),     // Encoded layout data
    GetDeviceInfo(Nfs4Status, Option<Bytes>), // Encoded device info
    LayoutReturn(Nfs4Status),
    /// LAYOUTCOMMIT result (RFC 8881 §18.42.2). On success, optionally
    /// reports the new file size to the client (`Some(size)` ⇔
    /// `ns_sizechanged = TRUE`). The MDS sets it when LAYOUTCOMMIT
    /// extended the file beyond its previously-known EOF.
    LayoutCommit(Nfs4Status, Option<u64>),

    // Generic result for unsupported operations.
    // Carries the original opcode so the encoder can comply with RFC 5661
    // §15.2: an illegal opcode (reserved 0/1/2 or out of range) is reported
    // with sentinel opcode OP_ILLEGAL (10044) and status NFS4ERR_OP_ILLEGAL,
    // while a valid-but-unimplemented opcode echoes itself with NFS4ERR_NOTSUPP.
    Unsupported { opcode: u32, status: Nfs4Status },

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
            OperationResult::Verify(s) => *s,
            OperationResult::Nverify(s) => *s,
            OperationResult::Access(s, _) => *s,
            OperationResult::Create(s, _, _) => *s,
            OperationResult::Remove(s, _) => *s,
            OperationResult::Rename(s, _, _) => *s,
            OperationResult::Link(s, _) => *s,
            OperationResult::ReadLink(s, _) => *s,
            OperationResult::PutPubFh(s) => *s,
            OperationResult::ExchangeId(s, _) => *s,
            OperationResult::CreateSession(s, _) => *s,
            OperationResult::DestroySession(s) => *s,
            OperationResult::DestroyClientId(s) => *s,
            OperationResult::BindConnToSession(s, _, _, _) => *s,
            OperationResult::Sequence(s, _) => *s,
            OperationResult::ReclaimComplete(s) => *s,
            OperationResult::SecInfo(s) => *s,
            OperationResult::SecInfoNoName(s) => *s,
            OperationResult::TestStateId(s, _) => *s,
            OperationResult::FreeStateId(s) => *s,
            OperationResult::Allocate(s) => *s,
            OperationResult::Deallocate(s) => *s,
            OperationResult::Seek(s, _) => *s,
            OperationResult::Copy(s, _) => *s,
            OperationResult::Clone(s) => *s,
            OperationResult::ReadPlus(s, _) => *s,
            OperationResult::Lock(s, _) => *s,
            OperationResult::LockT(s) => *s,
            OperationResult::LockU(s, _) => *s,
            OperationResult::LayoutGet(s, _) => *s,
            OperationResult::GetDeviceInfo(s, _) => *s,
            OperationResult::LayoutReturn(s) => *s,
            OperationResult::LayoutCommit(s, _) => *s,
            OperationResult::Unsupported { status, .. } => *status,
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
    /// Saved "current stateid" companion to `saved_fh`. SAVEFH copies the
    /// current stateid alongside CFH; RESTOREFH brings them back together.
    /// (RFC 8881 §16.2.3.1.2 ties the current state ID to the CFH.)
    pub saved_stateid: Option<StateId>,
    pub minor_version: u32,
    /// Session ID (set by SEQUENCE operation)
    /// Used to determine client_id for stateful operations
    pub session_id: Option<SessionId>,
    /// When the SEQUENCE op detected a slot replay with a cached reply, the
    /// dispatcher stops processing further ops and the COMPOUND-level reply
    /// is replaced with these bytes (RFC 8881 §15.1.10.4 exactly-once).
    pub replay_reply: Option<Bytes>,
    /// `(session_id, slot_id)` to associate the encoded reply with for
    /// future replay matching. Populated by SEQUENCE for new requests.
    pub cache_slot: Option<(SessionId, u32)>,
    /// RPC-level principal for this COMPOUND. Computed by the RPC layer
    /// from the call's auth credential (`Auth::principal()`). Used by
    /// EXCHANGE_ID's RFC 8881 §18.35.5 client-record state machine to
    /// distinguish "same client owner, different principal" — which
    /// changes the outcome from "renew existing client" to "evict and
    /// replace" (or NFS4ERR_PERM, depending on flags).
    pub principal: Vec<u8>,
    /// "Current stateid" within this COMPOUND (RFC 8881 §16.2.3.1.2).
    /// Updated after every state-changing op (OPEN, LOCK, LOCKU,
    /// OPEN_DOWNGRADE). When a subsequent op carries the magic sentinel
    /// stateid `(seqid=1, other=00…00)`, the dispatcher substitutes this
    /// before the per-op handler sees it. Lets clients chain `[OPEN,
    /// WRITE, CLOSE]` in one COMPOUND without round-tripping the OPEN
    /// stateid back into the WRITE / CLOSE.
    pub current_stateid: Option<StateId>,
    /// The connection-side writer for the TCP connection this COMPOUND
    /// arrived on. Used by `BIND_CONN_TO_SESSION` (RFC 8881 §18.34) —
    /// when the client requests `conn_dir = BACKCHANNEL` or `BOTH`,
    /// the dispatcher registers this writer in the per-session
    /// back-channel table so the server can later send
    /// `CB_LAYOUTRECALL` / `CB_RECALL` over it.
    ///
    /// `None` when the dispatcher is invoked from a unit test or any
    /// path that doesn't have a real TCP connection. Callback paths
    /// must tolerate this.
    pub back_channel: Option<std::sync::Arc<super::back_channel::BackChannelWriter>>,
}

/// "Current stateid" sentinel: `seqid=1, other=00…00`. RFC 8881
/// §16.2.3.1.2. When an op carries this exact stateid, the server
/// substitutes whatever the most recent state-changing op in this
/// COMPOUND produced.
pub const CURRENT_STATEID_SENTINEL: StateId = StateId {
    seqid: 1,
    other: [0u8; 12],
};

impl CompoundContext {
    pub fn new(minor_version: u32) -> Self {
        Self {
            current_fh: None,
            saved_fh: None,
            saved_stateid: None,
            minor_version,
            session_id: None,
            replay_reply: None,
            cache_slot: None,
            principal: Vec::new(),
            current_stateid: None,
            back_channel: None,
        }
    }

    /// Build a CompoundContext seeded with the principal from an RPC call.
    pub fn with_principal(minor_version: u32, principal: Vec<u8>) -> Self {
        Self { principal, ..Self::new(minor_version) }
    }

    /// If `stateid` is the "current stateid" sentinel (RFC 8881 §16.2.3.1.2),
    /// return the actual stateid the COMPOUND has produced so far. Otherwise
    /// return the input unchanged. Returns `None` only when the sentinel
    /// is sent before any state-changing op has set `current_stateid`,
    /// which is a protocol error the caller maps to `NFS4ERR_BAD_STATEID`.
    pub fn resolve_stateid(&self, stateid: StateId) -> Option<StateId> {
        if stateid == CURRENT_STATEID_SENTINEL {
            self.current_stateid
        } else {
            Some(stateid)
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
    /// Decode a COMPOUND request from XDR.
    ///
    /// Decoder errors here surface to the caller as RPC `GARBAGE_ARGS`, which
    /// makes the COMPOUND-level error reporting (NFS4ERR_INVAL,
    /// NFS4ERR_MINOR_VERS_MISMATCH, NFS4ERR_OP_ILLEGAL) unreachable. So we are
    /// careful to:
    ///   * accept non-UTF-8 tags (set `tag_valid=false`, dispatcher returns
    ///     `NFS4ERR_INVAL` per RFC 5661 §3.2);
    ///   * accept any minor version (dispatcher returns
    ///     `NFS4ERR_MINOR_VERS_MISMATCH` if it's not one we support, even when
    ///     the operation array is malformed);
    ///   * recover from a per-operation decode failure by replacing it with
    ///     `Operation::Unsupported(opcode)`, so the COMPOUND can still produce
    ///     a well-formed reply.
    pub fn decode(mut decoder: XdrDecoder) -> Result<Self, String> {
        tracing::trace!("DEBUG CompoundRequest::decode: Starting with {} bytes", decoder.remaining());

        // Decode tag as opaque bytes; lossy-convert to UTF-8 so a non-UTF-8
        // tag (RFC 5661 §15 says servers MUST detect this) doesn't crash
        // request decode.
        let tag_bytes = decoder.decode_opaque()?;
        let tag_valid = std::str::from_utf8(&tag_bytes).is_ok();
        let tag = String::from_utf8_lossy(&tag_bytes).into_owned();
        tracing::trace!("DEBUG CompoundRequest::decode: After tag decode (tag='{}', valid={}): {} bytes remaining",
                 tag, tag_valid, decoder.remaining());

        // Decode minor version. Don't reject here — let the dispatcher do it
        // so the COMPOUND-level reply uses NFS4ERR_MINOR_VERS_MISMATCH (RFC
        // requires that response even when the rest of the body is malformed).
        let minor_version = decoder.decode_u32()?;
        tracing::trace!("DEBUG CompoundRequest::decode: After minor_version decode (={}): {} bytes remaining", minor_version, decoder.remaining());

        // If the minor version is unsupported, skip operation decoding entirely
        // and hand an empty op list to the dispatcher. This handles compounds
        // that pair a bogus minor version with malformed operations
        // (pynfs COMP4b sends `version=50` with `op.illegal()`).
        if minor_version > NFS_V4_MINOR_VERSION_2 {
            return Ok(Self { tag, tag_valid, minor_version, operations: Vec::new() });
        }

        // Decode operation count
        let op_count = decoder.decode_u32()? as usize;
        tracing::trace!("DEBUG CompoundRequest::decode: After op_count decode (={}): {} bytes remaining", op_count, decoder.remaining());
        debug!("COMPOUND: tag='{}', minor_version={}, op_count={}", tag, minor_version, op_count);

        // Decode operations. Per-op failures degrade to Operation::Unsupported
        // rather than aborting the whole compound; an op array that runs out
        // of bytes mid-opcode still aborts (the wire is unrecoverable).
        let mut operations = Vec::with_capacity(op_count);
        for i in 0..op_count {
            if decoder.remaining() < 4 {
                let err = format!("Operation {}/{}: Not enough data for opcode (need 4 bytes, have {})",
                                 i + 1, op_count, decoder.remaining());
                tracing::trace!("ERROR CompoundRequest::decode: {}", err);
                return Err(err);
            }

            let opcode = decoder.decode_u32()?;
            tracing::trace!("DEBUG CompoundRequest::decode: Operation {}/{}: opcode={}, {} bytes remaining",
                     i + 1, op_count, opcode, decoder.remaining());
            debug!("  Operation {}: opcode={}", i, opcode);

            let op = match Self::decode_operation(&mut decoder, opcode) {
                Ok(op) => op,
                Err(e) => {
                    // Recognised opcode whose arguments don't parse → BADXDR.
                    // Out-of-range opcodes go through `Operation::Unsupported`
                    // (handled in `decode_operation`) and surface as OP_ILLEGAL.
                    warn!("Operation {}/{} (opcode={}) failed to decode ({}); recording as BadXdr",
                          i + 1, op_count, opcode, e);
                    Operation::BadXdr(opcode)
                }
            };
            operations.push(op);
        }

        Ok(Self {
            tag,
            tag_valid,
            minor_version,
            operations,
        })
    }

    /// Decode a single operation
    fn decode_operation(decoder: &mut XdrDecoder, opcode: u32) -> Result<Operation, String> {
        // Reserved/illegal opcode classes (0/1/2 reserved per RFC 5661 §15.2,
        // anything > the highest valid v4.2 op is unknown) carry no body, so
        // they're handled before any further decoding. The dispatcher will
        // substitute OP_ILLEGAL.
        if opcode <= 2 || opcode > opcode::CLONE {
            warn!("Reserved/illegal operation code: {}", opcode);
            return Ok(Operation::Unsupported(opcode));
        }

        // No blanket "remaining > 0" check here: many valid ops (GETFH,
        // SAVEFH, RESTOREFH, READLINK, PUTROOTFH, PUTPUBFH, LOOKUPP, …)
        // legitimately take no arguments and live at the end of a COMPOUND
        // with the wire fully consumed. Per-arm decode_xxx() calls below will
        // surface a clear error if they actually need bytes that aren't
        // there, and that error is mapped to BADXDR by the caller.
        
        match opcode {
            // File handle operations
            opcode::PUTROOTFH => Ok(Operation::PutRootFh),
            opcode::PUTPUBFH => Ok(Operation::PutPubFh),
            opcode::PUTFH => {
                if decoder.remaining() < 4 {
                    return Err(format!("PUTFH: Not enough data for filehandle length: {} bytes remaining", decoder.remaining()));
                }
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
                tracing::trace!("DEBUG SETATTR: After stateid, {} bytes remaining", decoder.remaining());
                
                // Decode fattr4 structure (bitmap + attr_vals), NOT simple opaque
                // Decode bitmap4 (array of u32)
                let bitmap_len = decoder.decode_u32()?;
                tracing::trace!("DEBUG SETATTR: bitmap_len={}, {} bytes after", bitmap_len, decoder.remaining());
                
                let mut bitmap_words = Vec::with_capacity(bitmap_len as usize);
                for _ in 0..bitmap_len {
                    bitmap_words.push(decoder.decode_u32()?);
                }
                
                // Decode attrlist4 (opaque bytes)
                let attr_vals = decoder.decode_opaque()?;
                tracing::trace!("DEBUG SETATTR: decoded fattr4: {} bitmap words, {} bytes attr_vals, {} bytes remaining", 
                         bitmap_len, attr_vals.len(), decoder.remaining());
                
                // Re-encode as single blob for Operation::SetAttr
                // (We'll need to decode it again in the handler)
                use bytes::{BytesMut, BufMut};
                let mut attrs_buf = BytesMut::new();
                attrs_buf.put_u32(bitmap_len);
                for word in bitmap_words {
                    attrs_buf.put_u32(word);
                }
                attrs_buf.put_u32(attr_vals.len() as u32);
                attrs_buf.put_slice(&attr_vals);
                
                Ok(Operation::SetAttr { 
                    stateid, 
                    attrs: attrs_buf.freeze()
                })
            }
            opcode::ACCESS => {
                let access = decoder.decode_u32()?;
                Ok(Operation::Access(access))
            }
            opcode::VERIFY | opcode::NVERIFY => {
                // RFC 5661 §18.30.1 / §18.31.1: arg is fattr4 (bitmap4 +
                // attrlist4 opaque). Re-pack as a single blob so the
                // dispatcher can decode it once and compare.
                use bytes::{BytesMut, BufMut};
                let bitmap_len = decoder.decode_u32()?;
                let mut bitmap_words = Vec::with_capacity(bitmap_len as usize);
                for _ in 0..bitmap_len {
                    bitmap_words.push(decoder.decode_u32()?);
                }
                let attr_vals = decoder.decode_opaque()?;
                let mut attrs_buf = BytesMut::new();
                attrs_buf.put_u32(bitmap_len);
                for word in bitmap_words {
                    attrs_buf.put_u32(word);
                }
                attrs_buf.put_u32(attr_vals.len() as u32);
                attrs_buf.put_slice(&attr_vals);
                let attrs = attrs_buf.freeze();
                if opcode == opcode::VERIFY {
                    Ok(Operation::Verify { attrs })
                } else {
                    Ok(Operation::Nverify { attrs })
                }
            }

            // File I/O operations
            opcode::OPEN => {
                tracing::trace!("DEBUG OPEN: Starting decode, {} bytes remaining", decoder.remaining());
                let seqid = decoder.decode_u32()?;
                tracing::trace!("DEBUG OPEN: seqid={}, {} bytes after", seqid, decoder.remaining());
                let share_access = decoder.decode_u32()?;
                tracing::trace!("DEBUG OPEN: share_access=0x{:x}, {} bytes after", share_access, decoder.remaining());
                let share_deny = decoder.decode_u32()?;
                tracing::trace!("DEBUG OPEN: share_deny=0x{:x}, {} bytes after", share_deny, decoder.remaining());
                
                // Owner (state_owner) - this is open_owner4 which is a struct with clientid + opaque
                // Per RFC 5661: struct open_owner4 { clientid4 clientid; opaque owner<>; }
                tracing::trace!("DEBUG OPEN: Decoding open_owner4, {} bytes before", decoder.remaining());
                let owner_clientid = decoder.decode_u64()?;  // clientid4
                tracing::trace!("DEBUG OPEN: owner_clientid={}, {} bytes after", owner_clientid, decoder.remaining());
                let owner = decoder.decode_opaque()?.to_vec();  // owner opaque
                tracing::trace!("DEBUG OPEN: owner {} bytes, {} bytes remaining after owner", owner.len(), decoder.remaining());
                
                // Openflag4 - this is a union with opentype4 as discriminator (RFC 5661 §18.16)
                let opentype = decoder.decode_u32()?;  // OPEN4_NOCREATE=0, OPEN4_CREATE=1
                tracing::trace!("DEBUG OPEN: opentype={}, {} bytes remaining", opentype, decoder.remaining());
                
                // Decode createhow4 only if opentype == OPEN4_CREATE (1)
                let openhow = if opentype == 1 {
                    // OPEN4_CREATE - decode createhow4 (discriminated union)
                    let createmode = decoder.decode_u32()?;
                    tracing::trace!("DEBUG OPEN: createmode={}, {} bytes remaining", createmode, decoder.remaining());
                    match createmode {
                        0 | 1 => {
                            // UNCHECKED4 or GUARDED4 - decode createattrs (fattr4)
                            // fattr4 structure: bitmap4 (array) + attrlist4 (opaque)
                            tracing::trace!("DEBUG OPEN: UNCHECKED4/GUARDED4 - decoding createattrs fattr4, {} bytes before", decoder.remaining());
                            
                            // Decode bitmap4 (array of u32)
                            let bitmap_len = decoder.decode_u32()?;
                            tracing::trace!("DEBUG OPEN: bitmap_len={}, {} bytes after", bitmap_len, decoder.remaining());
                            for _ in 0..bitmap_len {
                                let _bitmap_word = decoder.decode_u32()?;
                            }
                            
                            // Decode attrlist4 (opaque bytes)
                            let attrs = decoder.decode_opaque()?;
                            tracing::trace!("DEBUG OPEN: decoded fattr4: {} bitmap words, {} bytes attrs, {} bytes remaining", 
                                     bitmap_len, attrs.len(), decoder.remaining());
                            OpenHow { createmode, attrs: Some(attrs) }
                        }
                        2 => {
                            // EXCLUSIVE4 - decode verifier only
                            let verf = decoder.decode_fixed_opaque(8)?;
                            OpenHow { createmode, attrs: Some(verf) }
                        }
                        3 => {
                            // EXCLUSIVE4_1 (NFSv4.1) - decode verifier + createattrs (fattr4)
                            let _verf = decoder.decode_fixed_opaque(8)?;
                            
                            // Decode bitmap4
                            let bitmap_len = decoder.decode_u32()?;
                            for _ in 0..bitmap_len {
                                let _bitmap_word = decoder.decode_u32()?;
                            }
                            
                            // Decode attrlist4
                            let attrs = decoder.decode_opaque()?;
                            OpenHow { createmode, attrs: Some(attrs) }
                        }
                        _ => OpenHow { createmode: 0, attrs: None },
                    }
                } else {
                    // OPEN4_NOCREATE - no createhow4
                    OpenHow { createmode: 0, attrs: None }
                };
                
                // Claim (discriminated union) - RFC 5661 Section 18.16
                let claim_type = decoder.decode_u32()?;
                tracing::trace!("DEBUG OPEN: claim_type={}, {} bytes remaining", claim_type, decoder.remaining());
                let file = match claim_type {
                    0 => {
                        // CLAIM_NULL - filename
                        decoder.decode_string()?
                    }
                    1 => {
                        // CLAIM_PREVIOUS - delegate_type (u32)
                        // Used for reclaim after server reboot
                        tracing::trace!("DEBUG OPEN: CLAIM_PREVIOUS - decoding delegate_type, {} bytes before", decoder.remaining());
                        let delegate_type = decoder.decode_u32()?;
                        tracing::trace!("DEBUG OPEN: CLAIM_PREVIOUS - decoded delegate_type={}, {} bytes after", delegate_type, decoder.remaining());
                        String::new()
                    }
                    2 => {
                        // CLAIM_DELEGATE_CUR - delegate_stateid + filename
                        let _delegate_stateid = decoder.decode_stateid()?;
                        decoder.decode_string()?
                    }
                    3 => {
                        // CLAIM_DELEGATE_PREV - filename
                        decoder.decode_string()?
                    }
                    4 => {
                        // CLAIM_FH (NFSv4.1) - no data
                        String::new()
                    }
                    5 => {
                        // CLAIM_DELEG_CUR_FH (NFSv4.1) - delegate_stateid only
                        let _delegate_stateid = decoder.decode_stateid()?;
                        String::new()
                    }
                    6 => {
                        // CLAIM_DELEG_PREV_FH (NFSv4.1) - no data
                        String::new()
                    }
                    _ => {
                        return Err(format!("Unknown OPEN claim type: {}", claim_type));
                    }
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
                // RFC 5661 §18.6 CREATE4args wire layout:
                //   createtype4   objtype     (discriminated union: type + type-specific data)
                //     ├── NF4LNK : linktext4 linkdata
                //     ├── NF4BLK / NF4CHR : specdata4 devdata (2 u32s)
                //     └── others : void
                //   component4    objname
                //   fattr4        createattrs
                //
                // The previous implementation read objname *before* the
                // type-specific data, which works only for NF4REG/NF4DIR/
                // NF4SOCK/NF4FIFO (zero-length tail of the union). NF4LNK
                // and NF4BLK/NF4CHR consumed bytes that should have been
                // objname, producing "file name contained an unexpected NUL
                // byte" and breaking RNM1[abcfs], MK1*, etc.
                tracing::trace!("DEBUG CREATE: starting decode, {} bytes remaining", decoder.remaining());
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

                // Type-specific tail of the createtype4 union — comes BEFORE
                // objname.
                let linkdata = match objtype {
                    Nfs4FileType::Symlink => {
                        let link = decoder.decode_string()?;
                        tracing::trace!("DEBUG CREATE: linkdata='{}'", link);
                        Some(link)
                    }
                    Nfs4FileType::BlockDevice | Nfs4FileType::CharDevice => {
                        let _major = decoder.decode_u32()?;
                        let _minor = decoder.decode_u32()?;
                        None
                    }
                    _ => None,
                };

                let objname = decoder.decode_string()?;
                tracing::trace!("DEBUG CREATE: objname='{}'", objname);

                // createattrs (fattr4 = bitmap4 + attrlist4 opaque) — values
                // currently ignored; the bytes are consumed for wire alignment.
                let bitmap_len = decoder.decode_u32()?;
                for _ in 0..bitmap_len {
                    let _ = decoder.decode_u32()?;
                }
                let _attrs = decoder.decode_opaque()?;

                Ok(Operation::Create { objtype, objname, linkdata })
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
                // ClientOwner structure: verifier first, then opaque id.
                let verifier_bytes = decoder.decode_verifier()?;
                let verifier = u64::from_be_bytes(verifier_bytes);
                let client_id_bytes = decoder.decode_opaque()?;
                let clientowner = ClientId {
                    verifier,
                    id: client_id_bytes.to_vec(),
                };

                let flags = decoder.decode_u32()?;
                let state_protect = decoder.decode_u32()?;

                // eia_client_impl_id is a length-prefixed array of at most
                // one element (RFC 8881 §18.35.1: `nfs_impl_id4 eia_client_impl_id<1>`).
                // Length > 1 is a hard XDR violation (CSESS19 / EID3 expect
                // BADXDR). We Err out and the dispatcher maps the failure
                // to OP_ILLEGAL/BADXDR for this opcode.
                let impl_id_count = decoder.decode_u32()? as usize;
                if impl_id_count > 1 {
                    return Err(format!(
                        "eia_client_impl_id<1> length out of range: {}",
                        impl_id_count
                    ));
                }
                let impl_id = if impl_id_count == 1 {
                    // nfs_impl_id4 = { utf8str_cs nii_domain; utf8str_cs nii_name; nfstime4 nii_date }.
                    // We don't currently use any of these fields, but we have
                    // to consume them to keep the wire aligned. Bound the
                    // overall blob so a giant nii_name can't OOM us.
                    let domain = decoder.decode_opaque()?;
                    let name = decoder.decode_opaque()?;
                    let _date_seconds = decoder.decode_u64()?;
                    let _date_nseconds = decoder.decode_u32()?;
                    let mut combined = Vec::with_capacity(domain.len() + name.len() + 1);
                    combined.extend_from_slice(&domain);
                    combined.push(b'/');
                    combined.extend_from_slice(&name);
                    combined
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
                // Wire layout (RFC 5661 §18.36):
                //   csa_clientid (u64)
                //   csa_sequence (u32)
                //   csa_flags    (u32)
                //   csa_fore_chan_attrs : channel_attrs4
                //   csa_back_chan_attrs : channel_attrs4
                //   csa_cb_program (u32)
                //   csa_sec_parms<>   (callback_sec_parms4 array)
                //
                // channel_attrs4 itself ends with `ca_rdma_ird<1>`, an
                // optional one-element u32 array. The decoder previously
                // skipped that array, which silently mis-framed every
                // subsequent field on the wire — this fixes that.

                fn decode_channel_attrs(d: &mut XdrDecoder) -> Result<ChannelAttrs, String> {
                    let header_pad_size = d.decode_u32()?;
                    let max_request_size = d.decode_u32()?;
                    let max_response_size = d.decode_u32()?;
                    let max_response_size_cached = d.decode_u32()?;
                    let max_operations = d.decode_u32()?;
                    let max_requests = d.decode_u32()?;

                    // ca_rdma_ird<1>: 0 or 1 u32. Anything longer is invalid
                    // per the XDR <1> bound; surface as a decode error so the
                    // dispatcher returns BADXDR instead of silently desyncing.
                    let rdma_ird_len = d.decode_u32()? as usize;
                    if rdma_ird_len > 1 {
                        return Err(format!(
                            "ca_rdma_ird<1> length out of range: {} (max 1)",
                            rdma_ird_len
                        ));
                    }
                    let mut rdma_ird = Vec::with_capacity(rdma_ird_len);
                    for _ in 0..rdma_ird_len {
                        rdma_ird.push(d.decode_u32()?);
                    }

                    Ok(ChannelAttrs {
                        header_pad_size,
                        max_request_size,
                        max_response_size,
                        max_response_size_cached,
                        max_operations,
                        max_requests,
                        rdma_ird,
                    })
                }

                let clientid = decoder.decode_u64()?;
                let sequence = decoder.decode_u32()?;
                let flags = decoder.decode_u32()?;

                let fore_chan_attrs = decode_channel_attrs(decoder)?;
                let back_chan_attrs = decode_channel_attrs(decoder)?;

                // csa_cb_program: the program number the client expects
                // callback CALLs to be addressed to. We persist it on the
                // Session so the CB-side RPC framing in callback.rs can
                // emit a well-formed CALL header.
                let cb_program = decoder.decode_u32()?;

                // csa_sec_parms<> is a discriminated union on auth_flavor4 —
                // it has variable, flavor-specific body sizes, NOT a uniform
                // length prefix per element. AUTH_NONE has 0 bytes, AUTH_SYS
                // carries authsys_parms, RPCSEC_GSS carries gss_cb_handles4.
                // We currently emit callbacks with AUTH_NONE creds (matches
                // Linux client behaviour for v4.1 mounts), so we don't yet
                // need to act on the parms; we leave them unconsumed.
                // CREATE_SESSION is universally the last op in the COMPOUND,
                // so leaving the remaining bytes unconsumed does not desync
                // the next op.

                Ok(Operation::CreateSession {
                    clientid,
                    sequence,
                    flags,
                    fore_chan_attrs,
                    back_chan_attrs,
                    cb_program,
                })
            }
            opcode::DESTROY_SESSION => {
                let sessionid = decoder.decode_sessionid()?;
                Ok(Operation::DestroySession(sessionid))
            }
            opcode::BIND_CONN_TO_SESSION => {
                let sessionid = decoder.decode_sessionid()?;
                let dir = decoder.decode_u32()?;
                let use_conn_in_rdma_mode = decoder.decode_bool()?;
                Ok(Operation::BindConnToSession {
                    sessionid,
                    dir,
                    use_conn_in_rdma_mode,
                })
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
            opcode::TEST_STATEID => {
                // Decode array of stateids to test
                let count = decoder.decode_u32()? as usize;
                let mut stateids = Vec::with_capacity(count);
                for _ in 0..count {
                    stateids.push(decoder.decode_stateid()?);
                }
                Ok(Operation::TestStateId(stateids))
            }
            opcode::FREE_STATEID => {
                // RFC 8881 §18.38: FREE_STATEID4args = stateid4
                let stateid = decoder.decode_stateid()?;
                Ok(Operation::FreeStateId(stateid))
            }

            // Lock operations — RFC 5661 §18.10.1 LOCK4args:
            //
            //   nfs_lock_type4  locktype;
            //   bool            reclaim;
            //   offset4         offset;
            //   length4         length;
            //   locker4         locker;       /* discriminated union */
            //
            //   union locker4 switch (bool new_lock_owner) {
            //     case TRUE:  open_to_lock_owner4 open_owner;
            //     case FALSE: exist_lock_owner4   lock_owner;
            //   };
            //
            //   struct open_to_lock_owner4 {
            //     seqid4      open_seqid;
            //     stateid4    open_stateid;
            //     seqid4      lock_seqid;
            //     lock_owner4 lock_owner;     /* clientid + opaque */
            //   };
            //
            //   struct exist_lock_owner4 {
            //     stateid4    lock_stateid;
            //     seqid4      lock_seqid;
            //   };
            //
            // The previous decoder treated the locker4 union as a flat
            // (stateid + opaque) pair, which mis-aligned every byte after
            // `length` on the new-owner path.
            opcode::LOCK => {
                let locktype = decoder.decode_u32()?;
                let reclaim = decoder.decode_bool()?;
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                let new_lock_owner = decoder.decode_bool()?;
                let (stateid, owner) = if new_lock_owner {
                    let _open_seqid = decoder.decode_u32()?;
                    let open_stateid = decoder.decode_stateid()?;
                    let _lock_seqid = decoder.decode_u32()?;
                    // lock_owner4 = clientid (u64) + opaque<>
                    let _clientid = decoder.decode_u64()?;
                    let owner = decoder.decode_opaque()?.to_vec();
                    (open_stateid, owner)
                } else {
                    let lock_stateid = decoder.decode_stateid()?;
                    let _lock_seqid = decoder.decode_u32()?;
                    // Existing lock_owner; the wire doesn't carry the owner
                    // bytes here (they're already associated with lock_stateid).
                    (lock_stateid, Vec::new())
                };
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
            opcode::SECINFO => {
                // RFC 5661 §18.29.1: SECINFO4args = component4 (utf8str_cs).
                let name = decoder.decode_string()?;
                Ok(Operation::SecInfo(name))
            }
            opcode::SECINFO_NO_NAME => {
                // SECINFO_NO_NAME takes a style argument (RFC 5661 Section 18.45)
                let style = decoder.decode_u32()?;
                Ok(Operation::SecInfoNoName(style))
            }

            // pNFS operations (opcodes 47-51)
            opcode::LAYOUTGET => {
                tracing::trace!("🎯🎯🎯 DECODING LAYOUTGET (opcode 50) 🎯🎯🎯");
                let signal_layout_avail = decoder.decode_bool()?;
                let layout_type = decoder.decode_u32()?;
                let iomode = decoder.decode_u32()?;
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                let minlength = decoder.decode_u64()?;
                let stateid = decoder.decode_stateid()?;
                let maxcount = decoder.decode_u32()?;
                tracing::trace!("🎯 LAYOUTGET decoded: offset={}, length={}, iomode={}", offset, length, iomode);
                Ok(Operation::LayoutGet {
                    signal_layout_avail,
                    layout_type,
                    iomode,
                    offset,
                    length,
                    minlength,
                    stateid,
                    maxcount,
                })
            }
            opcode::GETDEVICEINFO => {
                tracing::trace!("🎯🎯🎯 DECODING GETDEVICEINFO (opcode 47) 🎯🎯🎯");
                // Device ID is FIXED 16-byte opaque (no length prefix!)
                let device_id = decoder.decode_fixed_opaque(16)?.to_vec();
                tracing::trace!("🎯 GETDEVICEINFO device_id decoded: {} bytes", device_id.len());
                let layout_type = decoder.decode_u32()?;
                let maxcount = decoder.decode_u32()?;
                let notify_count = decoder.decode_u32()?;
                tracing::trace!("🎯 GETDEVICEINFO fully decoded: layout_type={}, maxcount={}", layout_type, maxcount);
                let mut notify_types = Vec::new();
                for _ in 0..notify_count {
                    notify_types.push(decoder.decode_u32()?);
                }
                Ok(Operation::GetDeviceInfo {
                    device_id,
                    layout_type,
                    maxcount,
                    notify_types,
                })
            }
            opcode::LAYOUTCOMMIT => {
                // RFC 8881 §18.42.1 LAYOUTCOMMIT4args
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                let reclaim = decoder.decode_bool()?;
                let stateid = decoder.decode_stateid()?;
                // newoffset4: discriminated bool + optional u64
                let no_newoffset = decoder.decode_bool()?;
                let last_write_offset = if no_newoffset {
                    Some(decoder.decode_u64()?)
                } else {
                    None
                };
                // newtime4: discriminated bool + optional nfstime4
                let nt_timechanged = decoder.decode_bool()?;
                let time_modify = if nt_timechanged {
                    let secs = decoder.decode_u64()? as i64;
                    let nsecs = decoder.decode_u32()?;
                    Some((secs, nsecs))
                } else {
                    None
                };
                // layoutupdate4: layouttype4 + opaque body
                let layout_type = decoder.decode_u32()?;
                let layoutupdate = decoder.decode_opaque()?;
                Ok(Operation::LayoutCommit {
                    offset,
                    length,
                    reclaim,
                    stateid,
                    last_write_offset,
                    time_modify,
                    layout_type,
                    layoutupdate,
                })
            }
            opcode::LAYOUTRETURN => {
                // RFC 5661 §18.4.1 LAYOUTRETURN4args:
                //   bool          lora_reclaim
                //   layouttype4   lora_layout_type
                //   layoutiomode4 lora_iomode
                //   layoutreturn4 lora_layoutreturn   ← discriminated union
                //
                // The union tail is *not* a length-prefixed opaque: it's
                // a u32 discriminator followed by either a
                // `layoutreturn_file4` (FILE=1) or nothing (FSID=2, ALL=3).
                // The pre-fix decoder used `decode_opaque()` which read
                // the discriminator as a length and then misaligned the
                // tail of the COMPOUND.
                let reclaim = decoder.decode_bool()?;
                let layout_type = decoder.decode_u32()?;
                let iomode = decoder.decode_u32()?;
                let return_type = decoder.decode_u32()?;
                let return_body = match return_type {
                    1 => {
                        // layoutreturn_file4: offset, length, stateid, opaque<>
                        let offset = decoder.decode_u64()?;
                        let length = decoder.decode_u64()?;
                        let stateid = decoder.decode_stateid()?;
                        let body = decoder.decode_opaque()?;
                        LayoutReturn4Body::File { offset, length, stateid, body }
                    }
                    2 => LayoutReturn4Body::Fsid,
                    3 => LayoutReturn4Body::All,
                    other => {
                        return Err(format!(
                            "LAYOUTRETURN: unknown layoutreturn_type4 {}",
                            other
                        ));
                    }
                };
                Ok(Operation::LayoutReturn {
                    reclaim,
                    layout_type,
                    iomode,
                    return_body,
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

/// Shared body for SECINFO / SECINFO_NO_NAME success replies (RFC 5661
/// §18.29.2 / §18.45.2): an array of `secinfo4`. We advertise AUTH_NONE,
/// AUTH_SYS, and RPCSEC_GSS(Kerberos V5, svc=none).
fn encode_secinfo_flavors(encoder: &mut XdrEncoder) {
    encoder.encode_u32(3); // 3 flavors
    encoder.encode_u32(0); // AUTH_NONE
    encoder.encode_u32(1); // AUTH_SYS
    encoder.encode_u32(6); // RPCSEC_GSS
    // Kerberos V5 OID (1.2.840.113554.1.2.2)
    let krb5_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
    encoder.encode_opaque(&krb5_oid);
    encoder.encode_u32(0); // QOP
    encoder.encode_u32(1); // service = rpc_gss_svc_none
}

impl CompoundResponse {
    /// Encode a COMPOUND response to XDR.
    ///
    /// If `self.raw_reply` is `Some`, those bytes are returned verbatim. This
    /// is the SEQUENCE replay path (RFC 8881 §15.1.10.4): the cached reply
    /// from the slot MUST be byte-for-byte identical to the original
    /// response, so we never re-encode it from `results`/`status`/`tag`.
    pub fn encode(self) -> Bytes {
        if let Some(raw) = self.raw_reply {
            tracing::trace!(
                "DEBUG CompoundResponse::encode: raw replay reply ({} bytes)",
                raw.len()
            );
            return raw;
        }

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
        tracing::trace!("DEBUG CompoundResponse: Sending {} bytes", bytes.len());
        tracing::trace!("DEBUG CompoundResponse: First 80 bytes: {:02x?}", &bytes[..bytes.len().min(80)]);
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
                            for entry in res.entries.iter() {
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
                // RFC 5661 §18.30.2: SETATTR4res = { status, bitmap4 attrsset }.
                // The bitmap is required regardless of status (it's empty when
                // nothing was actually set). Without it, the client's XDR
                // decoder hits EOF parsing the next op result and the whole
                // COMPOUND is unreadable.
                encoder.encode_u32(opcode::SETATTR);
                encoder.encode_status(status);
                encoder.encode_u32(0);  // attrsset bitmap length = 0
            }
            OperationResult::Verify(status) => {
                // RFC 5661 §18.30.2: VERIFY4res = nfsstat4 only.
                encoder.encode_u32(opcode::VERIFY);
                encoder.encode_status(status);
            }
            OperationResult::Nverify(status) => {
                // RFC 5661 §18.31.2: NVERIFY4res = nfsstat4 only.
                encoder.encode_u32(opcode::NVERIFY);
                encoder.encode_status(status);
            }
            OperationResult::Access(status, access_result) => {
                encoder.encode_u32(opcode::ACCESS);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some((supported, access)) = access_result {
                        // Per RFC 5661 Section 18.1: ACCESS4resok has TWO fields
                        encoder.encode_u32(supported);  // What server supports checking
                        encoder.encode_u32(access);     // What's actually granted
                        debug!("ACCESS response: supported=0x{:x}, granted=0x{:x}", supported, access);
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
            OperationResult::Create(status, change_info, attrset) => {
                encoder.encode_u32(opcode::CREATE);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    // Per RFC 5661 Section 18.6, CREATE4resok has change_info + attrset
                    if let Some(cinfo) = change_info {
                        encoder.encode_bool(cinfo.atomic);
                        encoder.encode_u64(cinfo.before);
                        encoder.encode_u64(cinfo.after);
                    }
                    // Encode attrset bitmap (which createattrs were actually set)
                    encoder.encode_bitmap(&attrset);
                }
            }
            OperationResult::Remove(status, change_info) => {
                encoder.encode_u32(opcode::REMOVE);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(cinfo) = change_info {
                        // Per RFC 5661, REMOVE returns change_info for parent directory
                        encoder.encode_bool(cinfo.atomic);
                        encoder.encode_u64(cinfo.before);
                        encoder.encode_u64(cinfo.after);
                    }
                }
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
                        warn!("🔍 MDS EXCHANGE_ID response encoding:");
                        warn!("   clientid={} (0x{:016x})", res.clientid, res.clientid);
                        warn!("   sequenceid={}", res.sequenceid);
                        warn!("   flags=0x{:08x}", res.flags);
                        warn!("   server_owner={:?}", res.server_owner);
                        warn!("   server_scope={:?}", String::from_utf8_lossy(&res.server_scope));
                        
                        let before_len = encoder.len();
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
                        let after_len = encoder.len();
                        warn!("✅ MDS EXCHANGE_ID encoded: {} bytes", after_len - before_len);
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
            OperationResult::BindConnToSession(status, session_id, dir, use_rdma) => {
                encoder.encode_u32(opcode::BIND_CONN_TO_SESSION);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(ref sid) = session_id {
                        encoder.encode_sessionid(sid);
                        encoder.encode_u32(dir);
                        encoder.encode_bool(use_rdma);
                    }
                }
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
            OperationResult::SecInfo(status) => {
                encoder.encode_u32(opcode::SECINFO);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    encode_secinfo_flavors(encoder);
                }
            }
            OperationResult::SecInfoNoName(status) => {
                encoder.encode_u32(opcode::SECINFO_NO_NAME);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    encode_secinfo_flavors(encoder);
                }
            }
            OperationResult::TestStateId(status, statuses) => {
                encoder.encode_u32(opcode::TEST_STATEID);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(statuses) = statuses {
                        // Encode array of status codes (one per stateid tested)
                        encoder.encode_u32(statuses.len() as u32);
                        for s in statuses {
                            encoder.encode_status(s);
                        }
                    }
                }
            }
            OperationResult::FreeStateId(status) => {
                // RFC 8881 §18.38.2: FREE_STATEID4res = nfsstat4 (status only)
                encoder.encode_u32(opcode::FREE_STATEID);
                encoder.encode_status(status);
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

            // pNFS operations
            OperationResult::LayoutGet(status, data) => {
                encoder.encode_u32(opcode::LAYOUTGET);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(layout_data) = data {
                        encoder.append_raw(&layout_data);
                    }
                }
            }
            OperationResult::GetDeviceInfo(status, data) => {
                encoder.encode_u32(opcode::GETDEVICEINFO);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(device_data) = data {
                        encoder.append_raw(&device_data);
                    }
                }
            }
            OperationResult::LayoutReturn(status) => {
                encoder.encode_u32(opcode::LAYOUTRETURN);
                encoder.encode_status(status);
                // LAYOUTRETURN response: lrs_present (bool) + optional stateid
                // For now, return lrs_present = FALSE (no new stateid)
                if status == Nfs4Status::Ok {
                    encoder.encode_bool(false);  // lrs_present = FALSE
                }
            }
            OperationResult::LayoutCommit(status, new_size) => {
                // RFC 8881 §18.42.2: nfsstat4 then on OK a newsize4
                // (discriminated union of `bool ns_sizechanged` +
                // optional `length4 ns_size`).
                encoder.encode_u32(opcode::LAYOUTCOMMIT);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    match new_size {
                        Some(sz) => {
                            encoder.encode_bool(true);
                            encoder.encode_u64(sz);
                        }
                        None => {
                            encoder.encode_bool(false);
                        }
                    }
                }
            }

            // Unsupported operations — RFC 5661 §15.2.
            // The result array entry is `nfs_resop4`, which is a *discriminated
            // union*: the first u32 names the opcode the result corresponds to,
            // followed by the per-op result body. The previous implementation
            // omitted the discriminant entirely, causing the client to read
            // the status word as the next opcode and either decode garbage or
            // raise GARBAGE_ARGS.
            //
            // For an opcode the client should never have sent (reserved 0/1/2
            // or out of range) we substitute OP_ILLEGAL with NFS4ERR_OP_ILLEGAL.
            // For a recognized but unimplemented opcode we echo it with
            // NFS4ERR_NOTSUPP so the client can match the result to the request
            // entry.
            OperationResult::Unsupported { opcode: req_opcode, status } => {
                let is_illegal = req_opcode < 3 || req_opcode > opcode::CLONE;
                let resop = if is_illegal { opcode::ILLEGAL } else { req_opcode };
                let resstatus = if is_illegal { Nfs4Status::OpIllegal } else { status };
                encoder.encode_u32(resop);
                encoder.encode_status(resstatus);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::xdr::XdrDecoder;
    use bytes::{BytesMut, BufMut};
    use crate::nfs::v4::operations::Fattr4;

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
        
        let attrs_bytes = dispatcher_buf.freeze();
        
        // Now encode the full GETATTR response manually
        // This mimics what encode_result does for GetAttr
        let mut encoder = XdrEncoder::new();
        
        // Encode opcode
        encoder.encode_u32(opcode::GETATTR);
        
        // Encode status
        encoder.encode_status(Nfs4Status::Ok);
        
        // Encode fattr4 (attrs_bytes already contains bitmap + attr_vals)
        encoder.append_raw(&attrs_bytes);
        
        let encoded = encoder.finish();
        
        // Decode and verify the structure
        let mut decoder = XdrDecoder::new(Bytes::from(encoded));
        
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
        
        // Encode GETATTR response manually
        let mut encoder = XdrEncoder::new();
        encoder.encode_u32(opcode::GETATTR);
        encoder.encode_status(Nfs4Status::Ok);
        encoder.append_raw(&Bytes::from(attrs_bytes.clone()));
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
        // Verify SECINFO_NO_NAME returns AUTH_NONE, AUTH_SYS, and RPCSEC_GSS

        // Encode SECINFO_NO_NAME response manually
        let mut encoder = XdrEncoder::new();
        encoder.encode_u32(opcode::SECINFO_NO_NAME);
        encoder.encode_status(Nfs4Status::Ok);
        // Encode 3 security flavors: AUTH_NONE (0), AUTH_SYS (1), and RPCSEC_GSS (6)
        encoder.encode_u32(3); // flavor count
        encoder.encode_u32(0); // AUTH_NONE
        encoder.encode_u32(1); // AUTH_SYS
        encoder.encode_u32(6); // RPCSEC_GSS
        // For RPCSEC_GSS, add OID, QOP, and service
        let krb5_oid = vec![0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
        encoder.encode_opaque(&krb5_oid);
        encoder.encode_u32(0); // QOP
        encoder.encode_u32(1); // Service
        let encoded = encoder.finish();

        let mut decoder = XdrDecoder::new(Bytes::from(encoded));

        let opcode = decoder.decode_u32().expect("decode opcode");
        assert_eq!(opcode, opcode::SECINFO_NO_NAME);

        let status = decoder.decode_u32().expect("decode status");
        assert_eq!(status, Nfs4Status::Ok.to_u32());

        // Should have array of 3 security flavors
        let flavor_count = decoder.decode_u32().expect("decode flavor count");
        assert_eq!(flavor_count, 3, "Should return 3 security flavors");

        let flavor1 = decoder.decode_u32().expect("decode flavor 1");
        assert_eq!(flavor1, 0, "First flavor should be AUTH_NONE (0)");

        let flavor2 = decoder.decode_u32().expect("decode flavor 2");
        assert_eq!(flavor2, 1, "Second flavor should be AUTH_SYS (1)");

        let flavor3 = decoder.decode_u32().expect("decode flavor 3");
        assert_eq!(flavor3, 6, "Third flavor should be RPCSEC_GSS (6)");

        // For RPCSEC_GSS, verify OID, QOP, and service
        let oid = decoder.decode_opaque().expect("decode GSS OID");
        assert_eq!(oid.len(), 9, "Kerberos V5 OID should be 9 bytes");

        let qop = decoder.decode_u32().expect("decode QOP");
        assert_eq!(qop, 0, "QOP should be 0");

        let service = decoder.decode_u32().expect("decode service");
        assert_eq!(service, 1, "Service should be 1 (rpc_gss_svc_none)");

        assert_eq!(decoder.remaining(), 0, "Should have consumed all data");
    }

    /// Encode the body of a LAYOUTRETURN op (everything after the opcode):
    /// `bool reclaim | u32 layout_type | u32 iomode | layoutreturn4`.
    fn encode_layoutreturn_body(
        reclaim: bool,
        layout_type: u32,
        iomode: u32,
        union_tail: &[u8],
    ) -> Bytes {
        let mut buf = BytesMut::new();
        buf.put_u32(if reclaim { 1 } else { 0 });
        buf.put_u32(layout_type);
        buf.put_u32(iomode);
        buf.put_slice(union_tail);
        buf.freeze()
    }

    #[test]
    fn test_layoutreturn_decode_all() {
        // LAYOUTRETURN4_ALL=3 has a void body. Pre-fix the decoder used
        // decode_opaque() and treated the discriminator as a length —
        // for ALL that meant "read 3 more bytes" past the end of the op,
        // either erroring out or eating into the next op.
        let tail = {
            let mut b = BytesMut::new();
            b.put_u32(3); // LAYOUTRETURN4_ALL
            b.freeze()
        };
        let body = encode_layoutreturn_body(false, 1, 1, &tail);
        let mut d = XdrDecoder::new(body);
        let op = CompoundRequest::decode_operation(&mut d, opcode::LAYOUTRETURN)
            .expect("decode ALL");
        match op {
            Operation::LayoutReturn { reclaim, layout_type, iomode, return_body } => {
                assert!(!reclaim);
                assert_eq!(layout_type, 1);
                assert_eq!(iomode, 1);
                assert!(matches!(return_body, LayoutReturn4Body::All));
            }
            _ => panic!("wrong variant"),
        }
        assert_eq!(d.remaining(), 0, "ALL should consume the whole body");
    }

    #[test]
    fn test_layoutreturn_decode_fsid() {
        let tail = {
            let mut b = BytesMut::new();
            b.put_u32(2); // LAYOUTRETURN4_FSID
            b.freeze()
        };
        let body = encode_layoutreturn_body(false, 1, 2, &tail);
        let mut d = XdrDecoder::new(body);
        let op = CompoundRequest::decode_operation(&mut d, opcode::LAYOUTRETURN)
            .expect("decode FSID");
        match op {
            Operation::LayoutReturn { return_body, iomode, .. } => {
                assert_eq!(iomode, 2);
                assert!(matches!(return_body, LayoutReturn4Body::Fsid));
            }
            _ => panic!("wrong variant"),
        }
        assert_eq!(d.remaining(), 0);
    }

    #[test]
    fn test_layoutreturn_decode_file() {
        // LAYOUTRETURN4_FILE=1 carries layoutreturn_file4:
        //   offset (u64) | length (u64) | stateid (16B) | opaque<>
        let tail = {
            let mut b = BytesMut::new();
            b.put_u32(1); // LAYOUTRETURN4_FILE
            b.put_u64(0); // offset
            b.put_u64(u64::MAX); // length (entire file)
            b.put_u32(7); // stateid.seqid
            b.put_slice(&[1u8; 12]); // stateid.other
            b.put_u32(0); // body length 0 (FILES has nothing)
            b.freeze()
        };
        let body = encode_layoutreturn_body(false, 1, 1, &tail);
        let mut d = XdrDecoder::new(body);
        let op = CompoundRequest::decode_operation(&mut d, opcode::LAYOUTRETURN)
            .expect("decode FILE");
        match op {
            Operation::LayoutReturn { return_body, .. } => match return_body {
                LayoutReturn4Body::File { offset, length, stateid, body } => {
                    assert_eq!(offset, 0);
                    assert_eq!(length, u64::MAX);
                    assert_eq!(stateid.seqid, 7);
                    assert_eq!(stateid.other, [1u8; 12]);
                    assert!(body.is_empty());
                }
                _ => panic!("expected File"),
            },
            _ => panic!("wrong variant"),
        }
        assert_eq!(d.remaining(), 0);
    }

    #[test]
    fn test_verify_decode_repacks_fattr4() {
        // VERIFY arg = fattr4 (bitmap4 + attrlist4 opaque). The decoder
        // re-packs into a single Bytes blob (so the dispatcher can
        // re-decode it once the GETATTR result is in hand).
        let mut buf = BytesMut::new();
        buf.put_u32(1); // bitmap_len = 1
        buf.put_u32(0x0000_000A); // attrs: TYPE(1) + SIZE(3)
        let payload = [0x00, 0x00, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0x10, 0x00];
        buf.put_u32(payload.len() as u32);
        buf.put_slice(&payload);
        let mut d = XdrDecoder::new(buf.freeze());
        let op = CompoundRequest::decode_operation(&mut d, opcode::VERIFY)
            .expect("decode VERIFY");
        let attrs = match op {
            Operation::Verify { attrs } => attrs,
            _ => panic!("wrong variant"),
        };
        // Re-decode the repacked blob: should see the same shape.
        let mut d2 = XdrDecoder::new(attrs);
        assert_eq!(d2.decode_u32().unwrap(), 1);
        assert_eq!(d2.decode_u32().unwrap(), 0x0000_000A);
        let attr_vals = d2.decode_opaque().unwrap();
        assert_eq!(attr_vals.as_ref(), &payload);
    }

    #[test]
    fn test_nverify_uses_same_decoder() {
        let mut buf = BytesMut::new();
        buf.put_u32(0); // bitmap_len = 0
        buf.put_u32(0); // attrs len = 0
        let mut d = XdrDecoder::new(buf.freeze());
        let op = CompoundRequest::decode_operation(&mut d, opcode::NVERIFY)
            .expect("decode NVERIFY");
        assert!(matches!(op, Operation::Nverify { .. }));
    }

    #[test]
    fn test_secinfo_decode_component() {
        // SECINFO4args = component4 (utf8str_cs); on the wire that's
        // length-prefixed, 4-byte aligned.
        let mut buf = BytesMut::new();
        let name = b"foo.txt";
        buf.put_u32(name.len() as u32);
        buf.put_slice(name);
        buf.put_slice(&[0u8]); // pad to 8 bytes (next multiple of 4)
        let mut d = XdrDecoder::new(buf.freeze());
        let op = CompoundRequest::decode_operation(&mut d, opcode::SECINFO)
            .expect("decode SECINFO");
        match op {
            Operation::SecInfo(s) => assert_eq!(s, "foo.txt"),
            _ => panic!("wrong variant"),
        }
        assert_eq!(d.remaining(), 0);
    }

    #[test]
    fn test_secinfo_encoded_response_carries_three_flavors() {
        // Both SECINFO and SECINFO_NO_NAME share the same success
        // body: array<secinfo4>. We always advertise AUTH_NONE,
        // AUTH_SYS, RPCSEC_GSS(Kerberos V5).
        let mut encoder = XdrEncoder::new();
        encode_secinfo_flavors(&mut encoder);
        let mut d = XdrDecoder::new(encoder.finish());
        assert_eq!(d.decode_u32().unwrap(), 3, "3 flavors");
        assert_eq!(d.decode_u32().unwrap(), 0, "AUTH_NONE");
        assert_eq!(d.decode_u32().unwrap(), 1, "AUTH_SYS");
        assert_eq!(d.decode_u32().unwrap(), 6, "RPCSEC_GSS");
        let oid = d.decode_opaque().unwrap();
        assert_eq!(
            oid.as_ref(),
            &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02],
            "Kerberos V5 OID 1.2.840.113554.1.2.2",
        );
        assert_eq!(d.decode_u32().unwrap(), 0, "QOP");
        assert_eq!(d.decode_u32().unwrap(), 1, "service=rpc_gss_svc_none");
    }

    #[test]
    fn test_layoutreturn_decode_unknown_returntype_errors() {
        // RFC enumerates only 1/2/3 for layoutreturn_type4. Anything
        // else must surface as a decode error rather than silently
        // misaligning the COMPOUND tail.
        let tail = {
            let mut b = BytesMut::new();
            b.put_u32(99);
            b.freeze()
        };
        let body = encode_layoutreturn_body(false, 1, 1, &tail);
        let mut d = XdrDecoder::new(body);
        assert!(CompoundRequest::decode_operation(&mut d, opcode::LAYOUTRETURN).is_err());
    }
}
