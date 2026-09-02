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
use crate::nfs::v4::operations::Fattr4;
use super::xdr::{Nfs4XdrDecoder, Nfs4XdrEncoder};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;
use tracing::{warn, debug};

/// One entry of `csa_sec_parms<>` (RFC 8881 §18.36.1): a credential the
/// client is willing to accept on callback CALLs.
///
/// Only the two flavours flint can actually emit are represented.
/// RPCSEC_GSS is parsed far enough to be SKIPPED correctly — the body is
/// `gss_cb_handles4`, and mis-framing it would desync every later entry,
/// so it is consumed rather than ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackSecParms {
    /// AUTH_NONE — no credential body.
    None,
    /// AUTH_SYS. The server must echo back exactly these values, so the
    /// whole `authsys_parms` is kept, not just the flavour.
    Sys {
        stamp: u32,
        machinename: String,
        uid: u32,
        gid: u32,
        gids: Vec<u32>,
    },
    /// RPCSEC_GSS — recognised and skipped; flint cannot emit it yet.
    Gss,
}

/// The credential the server will actually EMIT on this session's back
/// channel, chosen from what the client said it would accept.
///
/// AUTH_SYS is preferred when offered — it is what Linux advertises and
/// what it will honour; AUTH_NONE only when explicitly offered or when
/// nothing was. RPCSEC_GSS is recognised but not emittable, so it is
/// skipped rather than selected (a client granted state on a GSS-only
/// cb_sec could never be recalled — the grant gate refuses it).
///
/// Shared by CREATE_SESSION and BACKCHANNEL_CTL on purpose: they set
/// the same field, and two copies of this policy would be two chances
/// for a session to end up with a credential one of them would have
/// refused.
pub fn pick_cb_cred(offered: &[CallbackSecParms]) -> Option<CallbackSecParms> {
    offered
        .iter()
        .find(|p| matches!(p, CallbackSecParms::Sys { .. }))
        .or_else(|| offered.iter().find(|p| matches!(p, CallbackSecParms::None)))
        .cloned()
}

/// Decode `csa_sec_parms<>`.
///
/// Returns Err on a malformed body so the caller can degrade to
/// AUTH_NONE rather than desync — see the call site for why that is the
/// right failure mode here.
fn decode_callback_sec_parms(d: &mut XdrDecoder) -> Result<Vec<CallbackSecParms>, String> {
    const AUTH_NONE: u32 = 0;
    const AUTH_SYS: u32 = 1;
    const RPCSEC_GSS: u32 = 6;

    let count = d.decode_u32()? as usize;
    // A client offering hundreds of callback credentials is malformed;
    // bound it so a bad length cannot make us allocate wildly.
    if count > 16 {
        return Err(format!("csa_sec_parms<> length implausible: {}", count));
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        match d.decode_u32()? {
            AUTH_NONE => out.push(CallbackSecParms::None),
            AUTH_SYS => {
                let stamp = d.decode_u32()?;
                let machinename = d.decode_string()?;
                let uid = d.decode_u32()?;
                let gid = d.decode_u32()?;
                let n = d.decode_u32()? as usize;
                if n > 16 {
                    return Err(format!("authsys_parms gids<16> too long: {}", n));
                }
                let mut gids = Vec::with_capacity(n);
                for _ in 0..n {
                    gids.push(d.decode_u32()?);
                }
                out.push(CallbackSecParms::Sys { stamp, machinename, uid, gid, gids });
            }
            RPCSEC_GSS => {
                // gss_cb_handles4: gcbp_service (u32) + two opaque<> handles.
                let _service = d.decode_u32()?;
                let _handle_from_server = d.decode_opaque()?;
                let _handle_from_client = d.decode_opaque()?;
                out.push(CallbackSecParms::Gss);
            }
            other => return Err(format!("unknown callback sec flavour {}", other)),
        }
    }
    Ok(out)
}


/// utf8str_cs validity beyond raw UTF-8: RFC 8881 §14.4 excludes Unicode
/// noncharacters (pynfs COMP3 sends U+FFFE as a compound tag and expects
/// NFS4ERR_INVAL; RNM8/9 do the same with component names).
pub fn utf8str_cs_ok(s: &str) -> bool {
    !s.chars().any(|c| {
        let cp = c as u32;
        (0xFDD0..=0xFDEF).contains(&cp) || (cp & 0xFFFE) == 0xFFFE
    })
}

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
    /// Original on-wire byte length of this COMPOUND's args (post-RPC-
    /// header). Set at decode time by the entry point that has the raw
    /// bytes; the dispatcher compares this against the session's
    /// negotiated `ca_maxrequestsize` after SEQUENCE binds the session
    /// and emits `NFS4ERR_REQ_TOO_BIG` if exceeded (RFC 8881 §18.46.4).
    /// `0` means "size unknown / skip check" — kept that way so older
    /// callers that don't set it don't accidentally trigger the gate.
    pub wire_size: usize,
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
    /// A reply already encoded upstream, carried as WIRE SEGMENTS.
    ///
    /// Segments rather than one `Bytes` because the dispatcher's
    /// response-size check encodes the whole reply already, and if that
    /// encoding flattens, a READ payload is copied there and every
    /// saving downstream is fictional — which is exactly what happened
    /// the first time this was optimised. Keeping it segmented from the
    /// point of first encode is the only shape in which the payload is
    /// never copied at all.
    pub raw_reply: Option<Vec<crate::nfs::segment::Segment>>,
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
    OpenDowngrade {
        stateid: StateId,
        seqid: u32,
        share_access: u32,
        share_deny: u32,
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

    // Delegation operations. Decoded even though the server has never
    // granted a delegation: an undecodable known opcode used to fall
    // through to `Unsupported`, which STOPS the compound decode (the
    // cursor sits mid-argument), so a client probing DELEGPURGE or
    // returning a delegation truncated every op behind it.
    DelegPurge {
        clientid: u64,
    },
    /// BACKCHANNEL_CTL (RFC 8881 §18.33): re-point the CURRENT session's
    /// back channel. No sessionid argument — it applies to the session
    /// the compound is running on.
    BackchannelCtl {
        cb_program: u32,
        cb_sec: Vec<CallbackSecParms>,
    },
    DelegReturn {
        stateid: StateId,
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
        /// createattrs (RFC 8881 §18.4): the attributes the client asks
        /// the new object to be created with. These used to be decoded
        /// and DISCARDED ("consumed for wire alignment"), so every
        /// `mkdir(mode)` landed with default permissions — measured on a
        /// real kernel client: 0700 / 0750 / 0711 / 0777 all became
        /// 0755. `handle_create` has always been able to apply them.
        createattrs: Fattr4,
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
        /// `csa_sec_parms<>` — the credentials the client will ACCEPT on
        /// callback CALLs (RFC 8881 §18.36.3). Empty means it offered
        /// none, which we read as AUTH_NONE.
        cb_sec: Vec<CallbackSecParms>,
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
        /// locker4 discriminant. FALSE = exist_lock_owner4: `stateid` is a
        /// LOCK stateid and `owner` is empty (the wire doesn't repeat the
        /// owner bytes) — the handler must resolve the owner from the lock
        /// table, and the request may be an atomic upgrade/downgrade of
        /// that owner's own lock, never a conflict against it.
        new_lock_owner: bool,
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
    /// COPY (RFC 7862 §15.2). `source_server_count` is the length of the
    /// decoded `ca_source_server<netloc4>` array: 0 for an ordinary
    /// intra-server copy, non-zero for an inter-server copy, which this
    /// server does not implement and must refuse rather than silently
    /// perform locally.
    Copy {
        src_stateid: StateId,
        dst_stateid: StateId,
        src_offset: u64,
        dst_offset: u64,
        count: u64,
        consecutive: bool,
        synchronous: bool,
        source_server_count: u32,
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
    /// A component4 name in the op's args is not valid utf8str_cs (bad
    /// UTF-8 bytes or Unicode noncharacters). The bytes were consumed, so
    /// later ops still parse; the dispatcher replies NFS4ERR_INVAL
    /// (RFC 8881 §14.4 — pynfs RNM8/RNM9).
    InvalidName(u32),            // operation code
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
    /// `wr_writeverf` — this server lifetime's write verifier, the SAME
    /// value COMMIT reports. See `IoOperationHandler::write_verifier`:
    /// a mismatch livelocks a Linux client.
    pub verifier: u64,
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

impl ChannelAttrs {
    /// Decode one `channel_attrs4` (RFC 8881 §18.36) — the ONE wire
    /// decode both lanes use. The struct ends with `ca_rdma_ird<1>`, an
    /// optional one-element u32 array; skipping it (as two earlier
    /// hand-rolled decoders did, one in this file and one in the DS)
    /// silently mis-frames every subsequent field on the wire.
    pub(crate) fn decode(d: &mut XdrDecoder) -> Result<ChannelAttrs, String> {
        let header_pad_size = d.decode_u32()?;
        let max_request_size = d.decode_u32()?;
        let max_response_size = d.decode_u32()?;
        let max_response_size_cached = d.decode_u32()?;
        let max_operations = d.decode_u32()?;
        let max_requests = d.decode_u32()?;

        // ca_rdma_ird<1>: 0 or 1 u32. Anything longer is invalid per
        // the XDR <1> bound; surface as a decode error so the caller
        // returns BADXDR instead of silently desyncing.
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
///
/// NOT `Clone`: a READ payload may be a pipe (see [`ReadResult`]), and a
/// pipe has one reader. Nothing needed to clone a whole result anyway —
/// the one caller that did, the `ca_maxresponsesize` gate, only ever
/// wanted the SEQUENCE result and now takes just that.
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
    OpenDowngrade(Nfs4Status, Option<StateId>),
    Read(Nfs4Status, Option<ReadResult>),
    Write(Nfs4Status, Option<WriteResult>),
    Commit(Nfs4Status, Option<[u8; 8]>),  // verifier

    // Delegations (both results are status-only)
    DelegPurge(Nfs4Status),
    DelegReturn(Nfs4Status),
    BackchannelCtl(Nfs4Status),

    // Attributes
    GetAttr(Nfs4Status, Option<Bytes>),   // encoded attributes
    SetAttr(Nfs4Status, Vec<u32> /* attrsset bitmap words */),
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
    Lock(Nfs4Status, Option<StateId>, Option<crate::nfs::v4::operations::lockops::LockDenied>),
    LockT(Nfs4Status, Option<crate::nfs::v4::operations::lockops::LockDenied>),
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
            OperationResult::OpenDowngrade(s, _) => *s,
            OperationResult::Read(s, _) => *s,
            OperationResult::Write(s, _) => *s,
            OperationResult::Commit(s, _) => *s,
            OperationResult::DelegPurge(s) => *s,
            OperationResult::DelegReturn(s) => *s,
            OperationResult::BackchannelCtl(s) => *s,
            OperationResult::GetAttr(s, _) => *s,
            OperationResult::SetAttr(s, _) => *s,
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
            OperationResult::Lock(s, _, _) => *s,
            OperationResult::LockT(s, _) => *s,
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
    /// The createattrs fattr4 bitmap words — `attrs` is only decodable
    /// against these. Empty for NOCREATE/EXCLUSIVE4.
    pub attrmask: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct OpenClaim {
    pub claim_type: u32,
    pub file: String,
    /// CLAIM_PREVIOUS (1) only: the open_delegation_type4 the client says
    /// it held before the restart. Threaded (not discarded) so the reclaim
    /// arm answers the delegation half deliberately — today always NONE.
    pub delegate_type: Option<u32>,
    /// CLAIM_DELEGATE_CUR (2) / CLAIM_DELEG_CUR_FH (5) only: the delegation
    /// stateid the client is converting a locally-cached open under. Used
    /// to be decoded and DROPPED, so a conversion open executed with the
    /// stateid never validated.
    pub delegate_stateid: Option<StateId>,
}

#[derive(Debug, Clone)]
pub struct OpenResult {
    pub stateid: StateId,
    pub change_info: ChangeInfo,
    pub result_flags: u32,
    pub attrset: Vec<u32>,
    pub delegation: Option<Delegation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delegation {
    /// OPEN_DELEGATE_READ (1) — the only type flint grants.
    Read { stateid: StateId },
    /// OPEN_DELEGATE_NONE_EXT (3) — "no delegation, and here is why"
    /// (RFC 8881 §18.16.2). Emitted ONLY when the client set a
    /// share_access WANT bit: NONE_EXT is also how a server signals
    /// that it understands those bits at all, so sending it to a
    /// client that never asked answers a question it did not pose.
    NoneExt { why: WhyNoDelegation },
}

/// `why_no_delegation4` (RFC 8881 §18.16.2), narrowed to the reasons
/// flint can actually give. The full enum has nine arms; carrying the
/// five we never emit would be five untested encode paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhyNoDelegation {
    /// WND4_NOT_WANTED — the client set WANT_NO_DELEG. The one case
    /// the RFC makes mandatory for a want-bit-aware server.
    NotWanted,
    /// WND4_CONTENTION — someone else's use of the file stopped it:
    /// a write open, a live conflicting record, the post-recall
    /// cooldown.
    Contention,
    /// WND4_RESOURCE — the server could not, rather than would not:
    /// quota, circuit breaker, kill switch, grace, no back-channel.
    Resource,
    /// WND4_CANCELLED — the client set WANT_CANCEL.
    Cancelled,
}

impl WhyNoDelegation {
    pub fn code(self) -> u32 {
        match self {
            WhyNoDelegation::NotWanted => 0,
            WhyNoDelegation::Contention => 1,
            WhyNoDelegation::Resource => 2,
            WhyNoDelegation::Cancelled => 7,
        }
    }

    /// The union arm. `open_none_delegation4` switches on ond_why and
    /// only CONTENTION and RESOURCE carry a trailing bool; every other
    /// case is `void`, and encoding a bool there would desynchronise
    /// the whole rest of the compound for the client.
    pub fn trailing_bool(self) -> Option<bool> {
        match self {
            // We never push a delegation at the client later...
            WhyNoDelegation::Contention => Some(false),
            // ...nor signal that one has become available.
            WhyNoDelegation::Resource => Some(false),
            _ => None,
        }
    }
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

/// A READ payload.
///
/// `data` is a [`Segment`](crate::nfs::segment::Segment), not `Bytes`,
/// because the payload does not have to be memory the server holds — it
/// can be a pipe staged straight from the file. That is also why this
/// type is NOT `Clone`: a pipe has one reader, and duplicating a reply
/// payload was never something the server needed to do.
#[derive(Debug)]
pub struct ReadResult {
    pub eof: bool,
    pub data: crate::nfs::segment::Segment,
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
    /// May this COMPOUND's READ payload be spliced from the file to the
    /// socket without passing through userspace?
    ///
    /// **Defaults to FALSE, and that default is the safety property.**
    /// Only the plain-TCP RPC path sets it, so every in-process caller —
    /// the File API (`pnfs::mds::fileapi::hubfs`) most of all, which
    /// consumes READ results as bytes and never touches a socket — is
    /// correct by construction rather than by remembering a guard.
    ///
    /// Still not sufficient on its own: the slot reply cache must store
    /// the exact octets a replay returns, so the READ path also requires
    /// `cache_slot.is_none()`. SEQUENCE is op 0, so that is already
    /// decided by the time a READ runs.
    pub can_splice: bool,
    /// RPC-level principal for this COMPOUND. Computed by the RPC layer
    /// from the call's auth credential (`Auth::principal()`). Used by
    /// EXCHANGE_ID's RFC 8881 §18.35.5 client-record state machine to
    /// distinguish "same client owner, different principal" — which
    /// changes the outcome from "renew existing client" to "evict and
    /// replace" (or NFS4ERR_PERM, depending on flags).
    pub principal: Vec<u8>,
    /// The caller's AUTH_SYS (uid, gid) for this COMPOUND, None under
    /// AUTH_NONE/GSS. File-creating ops (OPEN, CREATE) stamp it onto the
    /// backing object so ownership round-trips for permission-sensitive
    /// workloads; GETATTR already reports the backing uid/gid.
    pub unix_cred: Option<(u32, u32)>,
    /// Supplementary groups from the AUTH_SYS credential. Separate from
    /// `unix_cred` so the two chown-stamp call sites keep their simple
    /// tuple; permission checking builds a full `authz::Cred` from both.
    pub unix_gids: Vec<u32>,
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
    /// The caller's AUTH_SYS identity as a permission-checking
    /// credential, or `None` when there is no uid to evaluate
    /// (AUTH_NONE, RPCSEC_GSS, or the hub's own in-process file API).
    pub fn cred(&self) -> Option<crate::nfs::v4::authz::Cred> {
        self.unix_cred.map(|(uid, gid)| crate::nfs::v4::authz::Cred {
            uid,
            gid,
            gids: self.unix_gids.clone(),
        })
    }

    pub fn new(minor_version: u32) -> Self {
        Self {
            current_fh: None,
            saved_fh: None,
            saved_stateid: None,
            minor_version,
            session_id: None,
            replay_reply: None,
            can_splice: false,
            cache_slot: None,
            principal: Vec::new(),
            unix_cred: None,
            unix_gids: Vec::new(),
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
        let tag_valid = std::str::from_utf8(&tag_bytes)
            .map(utf8str_cs_ok)
            .unwrap_or(false);
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
            return Ok(Self { tag, tag_valid, minor_version, operations: Vec::new(), wire_size: 0 });
        }

        // Decode operation count. Bounded against the bytes actually
        // left: each op is at least a 4-byte opcode, so a larger count
        // cannot be honoured no matter what it claims. Unbounded, this
        // fed Vec::with_capacity straight from an unauthenticated frame.
        let op_count = decoder.decode_u32()?;
        let op_count = crate::nfs::xdr::checked_array_len(
            op_count, decoder.remaining(), 4, "COMPOUND argarray",
        )?;
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
            // An op we can't decode leaves the cursor mid-argument, so the
            // remaining ops would parse as garbage (a v4.2 COPY's args used
            // to become the "next op" and the whole reply came out
            // mis-shaped). Its error result legitimately ends the compound
            // (RFC 8881 §15.2.4: results run up to and including the first
            // failing op) — stop decoding here.
            let stop = matches!(op, Operation::Unsupported(_) | Operation::BadXdr(_));
            operations.push(op);
            if stop {
                break;
            }
        }

        Ok(Self {
            tag,
            tag_valid,
            minor_version,
            operations,
            // The decoder doesn't see the original wire bytes' length;
            // the caller (handle_compound in server_v4 / pnfs/mds/server)
            // knows it and sets it post-decode.
            wire_size: 0,
        })
    }

    /// Consume a `netloc4<>` array and return its length.
    ///
    /// RFC 7862 §3.1:
    /// ```text
    /// enum netloc_type4 { NL4_NAME = 1, NL4_URL = 2, NL4_NETADDR = 3 };
    /// union netloc4 switch (netloc_type4 nl_type) {
    ///   case NL4_NAME:    utf8str_cis nl_name;
    ///   case NL4_URL:     utf8str_cis nl_url;
    ///   case NL4_NETADDR: netaddr4    nl_addr;   /* two strings */
    /// };
    /// ```
    ///
    /// The point is to leave the cursor EXACTLY at the end of the array so
    /// the next operation decodes from the right offset. Returning the
    /// count without consuming the arms would move the lie rather than fix
    /// it, so an unrecognised `nl_type` is a decode error (→ BADXDR) — the
    /// discriminant determines the arm's width, and a server that cannot
    /// determine the width cannot honestly claim to have consumed it.
    fn decode_netloc_array(decoder: &mut XdrDecoder) -> Result<u32, String> {
        let count = decoder.decode_u32()?;
        // Length-prefixed arrays are attacker-controlled; each element is
        // at least 8 bytes on the wire, so anything past that is malformed
        // rather than merely large.
        if count > 1024 {
            return Err(format!("implausible ca_source_server count {count}"));
        }
        for i in 0..count {
            match decoder.decode_u32()? {
                1 | 2 => {
                    decoder.decode_string()?;
                }
                3 => {
                    decoder.decode_string()?; // na_r_netid
                    decoder.decode_string()?; // na_r_addr
                }
                other => {
                    return Err(format!(
                        "unknown netloc_type4 {other} at ca_source_server[{i}]"
                    ))
                }
            }
        }
        Ok(count)
    }

    /// Decode a single operation
    fn decode_operation(decoder: &mut XdrDecoder, opcode: u32) -> Result<Operation, String> {
        // Decode a component4: opaque bytes that must be valid utf8str_cs.
        // The bytes are consumed either way so subsequent ops stay
        // parseable; an invalid name surfaces as Ok(Err(())) and the arm
        // returns Operation::InvalidName (→ NFS4ERR_INVAL).
        fn decode_component(decoder: &mut XdrDecoder) -> Result<Result<String, ()>, String> {
            let bytes = decoder.decode_opaque()?;
            match std::str::from_utf8(&bytes) {
                Ok(s) if utf8str_cs_ok(s) => Ok(Ok(s.to_owned())),
                _ => Ok(Err(())),
            }
        }

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
                match decode_component(decoder)? {
                    Ok(component) => Ok(Operation::Lookup(component)),
                    Err(()) => Ok(Operation::InvalidName(opcode)),
                }
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
                
                let bitmap_len = crate::nfs::xdr::checked_array_len(
                    bitmap_len, decoder.remaining(), 4, "bitmap4",
                )? as u32;
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
                let bitmap_len = crate::nfs::xdr::checked_array_len(
                    bitmap_len, decoder.remaining(), 4, "bitmap4",
                )? as u32;
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

            // Delegation operations (RFC 8881 §18.5 / §18.6). Both replies
            // are status-only; the args must decode so the ops behind them
            // in the compound survive.
            opcode::DELEGPURGE => {
                let clientid = decoder.decode_u64()?;
                Ok(Operation::DelegPurge { clientid })
            }
            opcode::DELEGRETURN => {
                let stateid = decoder.decode_stateid()?;
                Ok(Operation::DelegReturn { stateid })
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

                            // Decode bitmap4 (array of u32). The words are
                            // LOAD-BEARING: attr_vals is only decodable
                            // against them (this decoder used to discard
                            // them, which silently ignored every OPEN
                            // createattr — mode on create, size for
                            // O_CREAT|O_TRUNC).
                            let bitmap_len = decoder.decode_u32()?;
                            tracing::trace!("DEBUG OPEN: bitmap_len={}, {} bytes after", bitmap_len, decoder.remaining());
                            let mut attrmask = Vec::with_capacity(bitmap_len.min(8) as usize);
                            for _ in 0..bitmap_len {
                                attrmask.push(decoder.decode_u32()?);
                            }

                            // Decode attrlist4 (opaque bytes)
                            let attrs = decoder.decode_opaque()?;
                            tracing::trace!("DEBUG OPEN: decoded fattr4: {} bitmap words, {} bytes attrs, {} bytes remaining",
                                     bitmap_len, attrs.len(), decoder.remaining());
                            OpenHow { createmode, attrs: Some(attrs), attrmask }
                        }
                        2 => {
                            // EXCLUSIVE4 - decode verifier only
                            let verf = decoder.decode_fixed_opaque(8)?;
                            OpenHow { createmode, attrs: Some(verf), attrmask: Vec::new() }
                        }
                        3 => {
                            // EXCLUSIVE4_1 (NFSv4.1) - createverf4 (8 bytes)
                            // followed by createattrs (bitmap + fattr4).
                            // Pack `[verifier8 || attrs]` into the
                            // OpenHow.attrs Bytes so the dispatcher's
                            // first-8-bytes-are-verifier convention
                            // (see ioops.rs OpenHow::Exclusive4_1
                            // decode in dispatcher.rs) actually finds
                            // the verifier. Without this, two
                            // EXCLUSIVE4_1 retries with different
                            // verifiers look identical to us and
                            // pynfs OPEN6's mismatch-→-EXIST contract
                            // breaks.
                            let verf = decoder.decode_fixed_opaque(8)?;

                            // Decode bitmap4 (kept — see the 0|1 arm)
                            let bitmap_len = decoder.decode_u32()?;
                            let mut attrmask = Vec::with_capacity(bitmap_len.min(8) as usize);
                            for _ in 0..bitmap_len {
                                attrmask.push(decoder.decode_u32()?);
                            }

                            // Decode attrlist4
                            let attrs = decoder.decode_opaque()?;
                            let mut combined = Vec::with_capacity(8 + attrs.len());
                            combined.extend_from_slice(&verf);
                            combined.extend_from_slice(&attrs);
                            OpenHow { createmode, attrs: Some(combined.into()), attrmask }
                        }
                        _ => OpenHow { createmode: 0, attrs: None, attrmask: Vec::new() },
                    }
                } else {
                    // OPEN4_NOCREATE - no createhow4
                    OpenHow { createmode: 0, attrs: None, attrmask: Vec::new() }
                };
                
                // Claim (discriminated union) - RFC 5661 Section 18.16
                let claim_type = decoder.decode_u32()?;
                tracing::trace!("DEBUG OPEN: claim_type={}, {} bytes remaining", claim_type, decoder.remaining());
                let mut delegate_type = None;
                let mut delegate_stateid = None;
                let file = match claim_type {
                    0 => {
                        // CLAIM_NULL - filename
                        decoder.decode_string()?
                    }
                    1 => {
                        // CLAIM_PREVIOUS - delegate_type (u32)
                        // Used for reclaim after server reboot
                        delegate_type = Some(decoder.decode_u32()?);
                        String::new()
                    }
                    2 => {
                        // CLAIM_DELEGATE_CUR - delegate_stateid + filename
                        delegate_stateid = Some(decoder.decode_stateid()?);
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
                        delegate_stateid = Some(decoder.decode_stateid()?);
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
                let claim = OpenClaim { claim_type, file, delegate_type, delegate_stateid };
                
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
            opcode::OPEN_DOWNGRADE => {
                // OPEN_DOWNGRADE4args (RFC 8881 §18.18.1): open_stateid,
                // seqid, share_access, share_deny. The kernel sends this
                // on partial close of dup'd fds with mixed open modes —
                // fsstress storms it; NotSupp used to kick the client
                // into state recovery around every such close.
                let stateid = decoder.decode_stateid()?;
                let seqid = decoder.decode_u32()?;
                let share_access = decoder.decode_u32()?;
                let share_deny = decoder.decode_u32()?;
                Ok(Operation::OpenDowngrade { stateid, seqid, share_access, share_deny })
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

                let objname = match decode_component(decoder)? {
                    Ok(n) => n,
                    Err(()) => return Ok(Operation::InvalidName(opcode)),
                };
                tracing::trace!("DEBUG CREATE: objname='{}'", objname);

                // createattrs (fattr4 = bitmap4 + attrlist4 opaque). These
                // are CARRIED, not discarded: the handler applies them, and
                // dropping them here is what made every mkdir(mode) land
                // with default permissions.
                let bitmap_len = decoder.decode_u32()?;
                let mut attrmask = Vec::with_capacity(bitmap_len.min(8) as usize);
                for _ in 0..bitmap_len {
                    attrmask.push(decoder.decode_u32()?);
                }
                let attrs = decoder.decode_opaque()?;
                let createattrs = Fattr4 { attrmask, attr_vals: attrs.to_vec() };

                Ok(Operation::Create { objtype, objname, linkdata, createattrs })
            }
            opcode::REMOVE => {
                match decode_component(decoder)? {
                    Ok(component) => Ok(Operation::Remove(component)),
                    Err(()) => Ok(Operation::InvalidName(opcode)),
                }
            }
            opcode::RENAME => {
                // Consume BOTH names before validity checks so an invalid
                // oldname doesn't strand the cursor before newname.
                let oldname = decode_component(decoder)?;
                let newname = decode_component(decoder)?;
                match (oldname, newname) {
                    (Ok(oldname), Ok(newname)) => Ok(Operation::Rename { oldname, newname }),
                    _ => Ok(Operation::InvalidName(opcode)),
                }
            }
            opcode::LINK => {
                match decode_component(decoder)? {
                    Ok(newname) => Ok(Operation::Link(newname)),
                    Err(()) => Ok(Operation::InvalidName(opcode)),
                }
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
                // eia_state_protect is a union (RFC 8881 §18.35.1); the arm
                // bodies must be consumed or everything after them decodes
                // garbage (an SP4_SSV request used to surface as BADXDR).
                let state_protect = decoder.decode_u32()?;
                match state_protect {
                    0 => { /* SP4_NONE — void */ }
                    1 | 2 => {
                        // state_protect_ops4: two bitmap4s.
                        let _must_enforce = decoder.decode_bitmap()?;
                        let _must_allow = decoder.decode_bitmap()?;
                        if state_protect == 2 {
                            // ssv_sp_parms4 tail: hash algs, encr algs
                            // (arrays of sec_oid4), window, num_gss_handles.
                            for _ in 0..2 {
                                let alg_count = decoder.decode_u32()? as usize;
                                if alg_count > 16 {
                                    return Err(format!("ssp alg array too long: {}", alg_count));
                                }
                                for _ in 0..alg_count {
                                    let _oid = decoder.decode_opaque()?;
                                }
                            }
                            let _window = decoder.decode_u32()?;
                            let _num_gss_handles = decoder.decode_u32()?;
                        }
                    }
                    other => return Err(format!("bad spa_how: {}", other)),
                }

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
                let clientid = decoder.decode_u64()?;
                let sequence = decoder.decode_u32()?;
                let flags = decoder.decode_u32()?;

                let fore_chan_attrs = ChannelAttrs::decode(decoder)?;
                let back_chan_attrs = ChannelAttrs::decode(decoder)?;

                // csa_cb_program: the program number the client expects
                // callback CALLs to be addressed to. We persist it on the
                // Session so the CB-side RPC framing in callback.rs can
                // emit a well-formed CALL header.
                let cb_program = decoder.decode_u32()?;

                // csa_sec_parms<> is a discriminated union on auth_flavor4 —
                // variable, flavor-specific body sizes, NOT a uniform length
                // prefix per element. AUTH_NONE has 0 bytes, AUTH_SYS carries
                // authsys_parms, RPCSEC_GSS carries gss_cb_handles4.
                //
                // C8: this used to be left unconsumed, on the stated
                // assumption that emitting AUTH_NONE creds "matches Linux
                // client behaviour for v4.1 mounts". The 2026-07-31 runas
                // drill falsified it: a Linux 6.1 client DENIED the callback
                // RPC outright (reply_status=1) in 419 µs. RFC 8881 §18.36.3
                // is explicit — the server MUST use one of the credentials
                // the client offered here. So decode them and remember what
                // was on offer.
                //
                // A decode failure is NOT fatal: falling back to an empty
                // list degrades to AUTH_NONE, which is exactly the old
                // behaviour, and refusing the whole CREATE_SESSION over a
                // callback detail would break mounts that never use one.
                let cb_sec = decode_callback_sec_parms(decoder).unwrap_or_default();

                Ok(Operation::CreateSession {
                    clientid,
                    sequence,
                    flags,
                    fore_chan_attrs,
                    back_chan_attrs,
                    cb_program,
                    cb_sec,
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
            opcode::BACKCHANNEL_CTL => {
                // BACKCHANNEL_CTL4args (RFC 8881 §18.33.1): the callback
                // program and a fresh `bca_sec_parms<>`. Decoded for the
                // same reason DELEGPURGE is: an undecodable KNOWN opcode
                // fell through to `Unsupported`, which stops the compound
                // decode with the cursor mid-argument — so a client that
                // changed its callback credential truncated every op
                // behind it in the same compound, and the change itself
                // was silently lost.
                let cb_program = decoder.decode_u32()?;
                let cb_sec = decode_callback_sec_parms(decoder)?;
                Ok(Operation::BackchannelCtl { cb_program, cb_sec })
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
                let count = decoder.decode_u32()?;
                // stateid4 = seqid(4) + other[12] = 16 bytes each.
                let count = crate::nfs::xdr::checked_array_len(
                    count, decoder.remaining(), 16, "TEST_STATEID stateids",
                )?;
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
                    new_lock_owner,
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
                // COPY4args ends with ca_source_server<netloc4> (RFC 7862
                // §15.2.1). This decoder used to stop at `synchronous`,
                // which had two consequences, both silent:
                //
                //   * the array's length word was read as the NEXT opcode.
                //     For the common empty case that word is 0, which is
                //     reserved, so the op became Unsupported and the
                //     `stop` break below truncated the compound. The loop's
                //     own comment records exactly this symptom being
                //     treated at the wrong layer once already.
                //   * a NON-empty array — an inter-server copy request —
                //     was ignored, and a LOCAL copy was performed and
                //     reported OK. That is the F15 class: answering a
                //     question that was not asked, successfully.
                let source_server_count = Self::decode_netloc_array(decoder)?;
                Ok(Operation::Copy {
                    src_stateid,
                    dst_stateid,
                    src_offset,
                    dst_offset,
                    count,
                    consecutive,
                    synchronous,
                    source_server_count,
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
/// LOCK4denied (RFC 8881 §18.10.2): offset, length, locktype,
/// lock_owner4 { clientid, owner }. A `None` here with a Denied status
/// is a server-side inconsistency — encode a zeroed body rather than
/// truncate the XDR stream (the client's decode offset must stay
/// aligned for the rest of the compound).
fn encode_lock_denied(
    encoder: &mut XdrEncoder,
    denied: Option<&crate::nfs::v4::operations::lockops::LockDenied>,
) {
    match denied {
        Some(d) => {
            encoder.encode_u64(d.offset);
            encoder.encode_u64(d.length);
            encoder.encode_u32(d.locktype as u32);
            encoder.encode_u64(d.client_id);
            encoder.encode_opaque(&d.owner);
        }
        None => {
            warn!("LOCK denied without conflict details — encoding zeroed LOCK4denied");
            encoder.encode_u64(0);
            encoder.encode_u64(0);
            encoder.encode_u32(1); // READ_LT
            encoder.encode_u64(0);
            encoder.encode_opaque(&[]);
        }
    }
}

/// The flavors SECINFO advertises, in PREFERENCE ORDER.
///
/// ⚠ THE ORDER IS THE SECURITY POLICY. A client picks the first entry it
/// supports, so whatever is listed first is what the mount will actually
/// use. AUTH_NONE was listed FIRST, and the consequence was measured
/// 2026-08-24: a stock `mount -t nfs -o vers=4.1` against flint
/// negotiated **`sec=null`**, confirmed in `/proc/mounts`. Every
/// operation then arrives with NO credential — no uid, no gid, nothing —
/// so the server cannot evaluate permissions even in principle, and
/// `ctx.unix_cred` is `None` for the whole mount.
///
/// That is the floor under the whole authorization gap: it is not merely
/// that ACCESS did not check, it is that there was nothing to check
/// against. It also silently defeats the chown-the-caller stamp, since
/// there is no caller identity to stamp.
///
/// AUTH_SYS therefore goes first, matching knfsd (whose default export
/// is `sec=sys`). AUTH_NONE is kept, LAST, so a client that genuinely
/// has nothing else still works — but no client that can do better will
/// choose it.
fn encode_secinfo_flavors(encoder: &mut XdrEncoder) {
    // GSS is advertised ONLY when a keytab actually loaded.
    //
    // This used to advertise RPCSEC_GSS unconditionally, which invited
    // every client into a mechanism a keytab-less server then refused —
    // and nothing outside `deployments/pnfs-*.yaml` sets KRB5_KTNAME, so
    // that was the shipped default. Advertising what you cannot honour is
    // the same defect as claiming protection you do not apply, pointed
    // the other way.
    encode_secinfo_flavors_with(
        encoder,
        crate::nfs::rpcsec_gss::gss_is_available(),
        crate::nfs::sec_policy::active(),
    )
}

/// The body of [`encode_secinfo_flavors`], with its two inputs passed in
/// rather than read from process-wide state, so the filtering can be
/// tested across every floor without a test mutating the environment.
fn encode_secinfo_flavors_with(
    encoder: &mut XdrEncoder,
    gss: bool,
    policy: crate::nfs::sec_policy::SecPolicy,
) {
    use crate::nfs::sec_policy::SecLevel;

    // Kerberos V5 OID (1.2.840.113554.1.2.2)
    const KRB5_OID: [u8; 9] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];

    // rpc_gss_svc_none / _integrity / _privacy — all three implemented now
    // (RFC 2203 §5.3.2, framing in `nfs::gss_framing`). Advertising only
    // svc_none, as this did, means a client that wants krb5p is never
    // offered it. Strongest first, since a client picks from the top.
    const GSS_RUNGS: [(SecLevel, u32); 3] = [
        (SecLevel::Krb5p, 3),
        (SecLevel::Krb5i, 2),
        (SecLevel::Krb5, 1),
    ];

    // Nothing below the export's floor is offered. The server refuses
    // such a call with AUTH_TOOWEAK anyway (`nfs::sec_policy`), and
    // listing a flavor it will refuse is the same defect as the
    // keytab-less GSS advertisement above.
    let sys = policy.advertises(SecLevel::Sys);
    let none = policy.advertises(SecLevel::None);
    let rungs: Vec<(SecLevel, u32)> = if gss {
        GSS_RUNGS
            .iter()
            .copied()
            .filter(|(level, _)| policy.advertises(*level))
            .collect()
    } else {
        Vec::new()
    };

    encoder.encode_u32(sys as u32 + none as u32 + rungs.len() as u32);
    if sys {
        encoder.encode_u32(1); // AUTH_SYS — first: carries a uid to check
    }
    for (_, svc) in &rungs {
        encoder.encode_u32(6); // RPCSEC_GSS
        encoder.encode_opaque(&KRB5_OID);
        encoder.encode_u32(0); // QOP
        encoder.encode_u32(*svc);
    }
    if none {
        encoder.encode_u32(0); // AUTH_NONE — LAST, see the note above
    }
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
            // Flattening here is correct and deliberate: `encode` is the
            // contiguous-bytes surface (GSS sealing, the replay cache).
            // The segmented surface is `encode_segments`.
            if raw.len() == 1 {
                return raw.into_iter().next().expect("len checked").into_mem();
            }
            let total: usize = crate::nfs::segment::total_len(&raw);
            let mut flat = bytes::BytesMut::with_capacity(total);
            for b in &raw {
                flat.extend_from_slice(b.as_mem());
            }
            return flat.freeze();
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


    /// Encode the COMPOUND body as an ordered list of wire segments,
    /// keeping each READ payload as the `Bytes` the I/O layer produced
    /// instead of copying it into a growing buffer.
    ///
    /// WHY THIS EXISTS. [`Self::encode`] flattens everything into one
    /// `XdrEncoder`, whose `BytesMut` is born at 8 KiB and doubles. A
    /// 1 MiB READ reply therefore costs a full `put_slice` of the
    /// payload PLUS the realloc-copies on the way past 1 MiB, and then
    /// `frame_reply` copies the whole thing again into the RPC reply
    /// buffer. The data server hit exactly this and fixed it in
    /// `133e6db0` (`pnfs/ds/server.rs::assemble_reply`), which measured
    /// ~11 MiB of memory traffic per 1 MiB served. The shared v4 stack
    /// never got that fix, so `flint-nfs-server`, `flint-pnfs-mds` in
    /// standalone mode and the flint-lite hub all still paid it.
    ///
    /// MEASURED, 2026-08-28, lima 2 vCPU, server cache warm and client
    /// O_DIRECT so neither disk nor client cache is in the path: flint
    /// 1119 MiB/s at ~1920 cpu-ms/GiB against knfsd's 4107 MiB/s at
    /// ~560 — 3.4x the CPU per byte, with both servers CPU-saturated
    /// and RPC counts matched (518 vs 516).
    ///
    /// The wire bytes are IDENTICAL to `encode`; only the copy count
    /// changes. `segments_match_the_flattened_encoding` asserts that
    /// against the real encoder rather than a reimplementation of it.
    pub fn encode_segments(self) -> Vec<crate::nfs::segment::Segment> {
        // A replay is served verbatim from the slot cache; there is
        // nothing to segment and nothing to gain.
        if let Some(raw) = self.raw_reply {
            return raw;
        }

        let mut segs: Vec<crate::nfs::segment::Segment> = Vec::with_capacity(3);
        let mut cur = XdrEncoder::new();
        cur.encode_status(self.status);
        cur.encode_string(&self.tag);
        cur.encode_u32(self.results.len() as u32);

        for result in self.results.into_iter() {
            match result {
                // The ONLY segmented case. Every other result is a
                // handful of words, where cutting a segment would cost
                // more than the copy it saves.
                OperationResult::Read(Nfs4Status::Ok, Some(res)) if !res.data.is_empty() => {
                    cur.encode_u32(opcode::READ);
                    cur.encode_status(Nfs4Status::Ok);
                    cur.encode_bool(res.eof);
                    // The head of the opaque: its length. The bytes and
                    // the XDR pad follow as their own segments, which is
                    // what `encode_opaque` would have written inline.
                    cur.encode_u32(res.data.len() as u32);
                    let pad = (4 - res.data.len() % 4) % 4;
                    segs.push(cur.finish().into());
                    cur = XdrEncoder::new();
                    segs.push(res.data);
                    if pad > 0 {
                        cur.append_raw(&[0u8; 3][..pad]);
                    }
                }
                other => Self::encode_result(&mut cur, other),
            }
        }

        let tail = cur.finish();
        if !tail.is_empty() {
            segs.push(tail.into());
        }
        segs
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
            OperationResult::SetAttr(status, attrsset) => {
                // RFC 5661 §18.30.2: SETATTR4res = { status, bitmap4 attrsset }.
                // The bitmap is required regardless of status (on error it
                // reports the attrs applied before the failure). Without it,
                // the client's XDR decoder hits EOF parsing the next op
                // result and the whole COMPOUND is unreadable.
                encoder.encode_u32(opcode::SETATTR);
                encoder.encode_status(status);
                encoder.encode_bitmap(&attrsset);
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
                        // open_delegation4 (RFC 8881 §18.16.2). The
                        // chain lesson: this encoder, the server's
                        // advertisement, and the client's heuristics
                        // are three parties — never make this arm
                        // live without DELEGRETURN decode shipped.
                        match &res.delegation {
                            Some(Delegation::Read { stateid }) => {
                                encoder.encode_u32(1); // OPEN_DELEGATE_READ
                                encoder.encode_stateid(stateid);
                                encoder.encode_bool(false); // recall
                                // One permissive nfsace4: ALLOW /
                                // no flags / read mask / EVERYONE@.
                                // Linux ignores it; a zero-length who
                                // is riskier to foreign decoders.
                                encoder.encode_u32(0); // ACCESS_ALLOWED_ACE_TYPE
                                encoder.encode_u32(0); // aceflag
                                encoder.encode_u32(0x0012_0089); // read set
                                encoder.encode_string("EVERYONE@");
                            }
                            Some(Delegation::NoneExt { why }) => {
                                encoder.encode_u32(3); // OPEN_DELEGATE_NONE_EXT
                                encoder.encode_u32(why.code());
                                if let Some(b) = why.trailing_bool() {
                                    encoder.encode_bool(b);
                                }
                            }
                            None => encoder.encode_u32(0), // OPEN_DELEGATE_NONE
                        }
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
            OperationResult::OpenDowngrade(status, stateid) => {
                encoder.encode_u32(opcode::OPEN_DOWNGRADE);
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
                        // The FLAT encode. Reached only when the reply must
                        // be contiguous — the slot reply cache, or GSS — and
                        // both of those force `can_splice` off, so the
                        // payload here is memory by construction.
                        encoder.encode_opaque(res.data.as_mem());
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
                        debug!("🔍 MDS EXCHANGE_ID response encoding:");
                        debug!("   clientid={} (0x{:016x})", res.clientid, res.clientid);
                        debug!("   sequenceid={}", res.sequenceid);
                        debug!("   flags=0x{:08x}", res.flags);
                        debug!("   server_owner={:?}", res.server_owner);
                        debug!("   server_scope={:?}", String::from_utf8_lossy(&res.server_scope));
                        
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
                        debug!("✅ MDS EXCHANGE_ID encoded: {} bytes", after_len - before_len);
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
            OperationResult::Lock(status, stateid, denied) => {
                encoder.encode_u32(opcode::LOCK);
                encoder.encode_status(status);
                if status == Nfs4Status::Ok {
                    if let Some(sid) = stateid {
                        encoder.encode_stateid(&sid);
                    }
                } else if status == Nfs4Status::Denied {
                    // RFC 8881 §18.10.2: LOCK4res carries a LOCK4denied
                    // body on NFS4ERR_DENIED — bare status is malformed
                    // XDR and derails the client's reply decode.
                    encode_lock_denied(encoder, denied.as_ref());
                }
            }
            OperationResult::LockT(status, denied) => {
                encoder.encode_u32(opcode::LOCKT);
                encoder.encode_status(status);
                if status == Nfs4Status::Denied {
                    // RFC 8881 §18.11.2: same LOCK4denied body as LOCK.
                    encode_lock_denied(encoder, denied.as_ref());
                }
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
                        // COPY4resok = write_response4 + consecutive +
                        // synchronous (RFC 7862 §15.2.3). The reply used to
                        // skip write_response4's callback-id array,
                        // committed and verifier fields — clients hit EOF
                        // decoding it (pynfs COPY5).
                        encoder.encode_u32(0); // wr_callback_id<1>: empty (copy is synchronous)
                        encoder.encode_u64(res.count);
                        encoder.encode_u32(2); // wr_committed = FILE_SYNC4
                        // wr_writeverf. This was a hardcoded zero commented
                        // "sync copy: unused" — and that assumption is what
                        // hung a real client. Linux 6.8 sends COPY and
                        // COMMIT in ONE compound and compares the two
                        // verifiers; zeros never match COMMIT's, so it read
                        // every successful copy as a server reboot and
                        // reissued the identical COPY forever. Measured on
                        // lima 2026-08-01: one copy_file_range() of 1 MiB
                        // produced 264,601 COPY RPCs, each of which the
                        // server actually performed, and the syscall never
                        // returned.
                        encoder.encode_fixed_opaque(&res.verifier.to_be_bytes());
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
                // GETDEVICEINFO4res carries a body on TOOSMALL too:
                // gdir_mincount (RFC 8881 §18.40), so the client can
                // retry with a big enough buffer instead of guessing.
                if status == Nfs4Status::Ok || status == Nfs4Status::TooSmall {
                    if let Some(device_data) = data {
                        encoder.append_raw(&device_data);
                    }
                }
            }
            OperationResult::DelegPurge(status) => {
                // DELEGPURGE4res (RFC 8881 §18.5.2): status only.
                encoder.encode_u32(opcode::DELEGPURGE);
                encoder.encode_status(status);
            }
            OperationResult::BackchannelCtl(status) => {
                // BACKCHANNEL_CTL4res (RFC 8881 §18.33.2): status only.
                encoder.encode_u32(opcode::BACKCHANNEL_CTL);
                encoder.encode_status(status);
            }
            OperationResult::DelegReturn(status) => {
                // DELEGRETURN4res (RFC 8881 §18.6.2): status only.
                encoder.encode_u32(opcode::DELEGRETURN);
                encoder.encode_status(status);
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
mod deleg_wire_tests {
    //! The delegation ops' WIRE honesty, pinned. Before these arms
    //! existed, DELEGPURGE/DELEGRETURN hit the decoder's default arm,
    //! which STOPS the compound decode with the cursor mid-argument —
    //! every op behind them was silently dropped from the reply
    //! (design doc: docs/plans/nfs-delegations-design.md, negative
    //! leg 7). The claim arms decoded their delegation payloads and
    //! DISCARDED them; claim threading is what the grant/recall
    //! machine will validate against.

    use super::*;
    use crate::nfs::v4::protocol::StateId;

    fn req(build_ops: impl FnOnce(&mut XdrEncoder), op_count: u32) -> CompoundRequest {
        let mut e = XdrEncoder::new();
        e.encode_opaque(b"t");        // tag
        e.encode_u32(1);              // minorversion
        e.encode_u32(op_count);       // op count
        build_ops(&mut e);
        CompoundRequest::decode(XdrDecoder::new(e.finish())).expect("compound must decode")
    }

    fn sid() -> StateId {
        StateId { seqid: 7, other: [0xab; 12] }
    }

    #[test]
    fn delegpurge_decodes_and_the_op_behind_it_survives() {
        let r = req(
            |e| {
                e.encode_u32(opcode::DELEGPURGE);
                e.encode_u64(0xdead_beef);   // clientid4
                e.encode_u32(opcode::GETFH); // no args
            },
            2,
        );
        assert_eq!(r.operations.len(), 2, "DELEGPURGE truncated the compound");
        assert!(
            matches!(r.operations[0], Operation::DelegPurge { clientid: 0xdead_beef }),
            "got {:?}",
            r.operations[0]
        );
        assert!(matches!(r.operations[1], Operation::GetFh));
    }

    #[test]
    fn backchannel_ctl_decodes_and_the_op_behind_it_survives() {
        // Until this decoded, BACKCHANNEL_CTL fell to the `Unsupported`
        // arm with the cursor sitting mid-argument, so it truncated
        // every op behind it AND lost the credential change itself.
        // pynfs DELEG7 saw only the second half of that: a CB_RECALL
        // still carrying the CREATE_SESSION uid/gid.
        let r = req(
            |e| {
                e.encode_u32(opcode::BACKCHANNEL_CTL);
                e.encode_u32(0x4000_0001);      // bca_cb_program
                e.encode_u32(1);                // bca_sec_parms<> count
                e.encode_u32(1);                // AUTH_SYS
                e.encode_u32(13);               // stamp
                e.encode_string("fake name");   // machinename
                e.encode_u32(29);               // uid
                e.encode_u32(31);               // gid
                e.encode_u32(0);                // gids<>
                e.encode_u32(opcode::GETFH);    // no args
            },
            2,
        );
        assert_eq!(r.operations.len(), 2, "BACKCHANNEL_CTL truncated the compound");
        match &r.operations[0] {
            Operation::BackchannelCtl { cb_program, cb_sec } => {
                assert_eq!(*cb_program, 0x4000_0001);
                assert_eq!(cb_sec.len(), 1);
                assert!(
                    matches!(&cb_sec[0], CallbackSecParms::Sys { uid: 29, gid: 31, .. }),
                    "got {:?}",
                    cb_sec[0],
                );
            }
            other => panic!("got {other:?}"),
        }
        assert!(matches!(r.operations[1], Operation::GetFh));
    }

    /// One policy, two callers. CREATE_SESSION and BACKCHANNEL_CTL both
    /// set `cb_cred`, and a second copy of the preference order would be
    /// a second chance to store a credential the other would have
    /// refused — notably a GSS-only offer, which is recognised but not
    /// emittable, so a client granted state on its strength could never
    /// be recalled.
    #[test]
    fn the_callback_credential_policy_prefers_sys_then_none_and_never_gss() {
        let sys = CallbackSecParms::Sys {
            stamp: 1,
            machinename: "m".to_string(),
            uid: 5,
            gid: 6,
            gids: vec![],
        };
        assert!(matches!(
            pick_cb_cred(&[CallbackSecParms::None, sys.clone()]),
            Some(CallbackSecParms::Sys { uid: 5, .. }),
        ), "AUTH_SYS wins even when offered second");
        assert!(matches!(
            pick_cb_cred(&[CallbackSecParms::None]),
            Some(CallbackSecParms::None),
        ));
        assert_eq!(
            pick_cb_cred(&[CallbackSecParms::Gss]),
            None,
            "a GSS-only offer is unemittable, so nothing is selected",
        );
        assert_eq!(pick_cb_cred(&[]), None);
    }

    #[test]
    fn delegreturn_decodes_and_the_op_behind_it_survives() {
        let r = req(
            |e| {
                e.encode_u32(opcode::DELEGRETURN);
                e.encode_u32(7);                 // stateid.seqid
                e.encode_fixed_opaque(&[0xab; 12]); // stateid.other
                e.encode_u32(opcode::GETFH);
            },
            2,
        );
        assert_eq!(r.operations.len(), 2, "DELEGRETURN truncated the compound");
        match &r.operations[0] {
            Operation::DelegReturn { stateid } => assert_eq!(*stateid, sid()),
            other => panic!("got {:?}", other),
        }
        assert!(matches!(r.operations[1], Operation::GetFh));
    }

    /// One OPEN with the given claim payload; returns the decoded claim.
    fn open_with_claim(claim: impl FnOnce(&mut XdrEncoder)) -> OpenClaim {
        let r = req(
            |e| {
                e.encode_u32(opcode::OPEN);
                e.encode_u32(0);            // seqid
                e.encode_u32(1);            // share_access READ
                e.encode_u32(0);            // share_deny NONE
                e.encode_u64(0);            // open_owner4.clientid
                e.encode_opaque(b"owner");  // open_owner4.owner
                e.encode_u32(0);            // OPEN4_NOCREATE
                claim(e);
            },
            1,
        );
        match r.operations.into_iter().next().expect("one op") {
            Operation::Open { claim, .. } => claim,
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn claim_previous_threads_its_delegate_type() {
        let c = open_with_claim(|e| {
            e.encode_u32(1); // CLAIM_PREVIOUS
            e.encode_u32(1); // OPEN_DELEGATE_READ
        });
        assert_eq!(c.claim_type, 1);
        assert_eq!(c.delegate_type, Some(1), "delegate_type discarded again");
        assert_eq!(c.delegate_stateid, None);
    }

    #[test]
    fn claim_deleg_cur_fh_threads_its_stateid() {
        let c = open_with_claim(|e| {
            e.encode_u32(5); // CLAIM_DELEG_CUR_FH
            e.encode_u32(7);
            e.encode_fixed_opaque(&[0xab; 12]);
        });
        assert_eq!(c.claim_type, 5);
        assert_eq!(c.delegate_stateid, Some(sid()), "delegation stateid discarded again");
    }

    #[test]
    fn claim_delegate_cur_threads_stateid_and_name() {
        let c = open_with_claim(|e| {
            e.encode_u32(2); // CLAIM_DELEGATE_CUR
            e.encode_u32(7);
            e.encode_fixed_opaque(&[0xab; 12]);
            e.encode_string("cached.bin");
        });
        assert_eq!(c.claim_type, 2);
        assert_eq!(c.delegate_stateid, Some(sid()));
        assert_eq!(c.file, "cached.bin");
    }

    /// Both replies are status-only and must echo their own opcode —
    /// a reply the client can correlate, instead of the truncated
    /// compound the default arm produced.
    #[test]
    fn deleg_results_encode_opcode_and_status_only() {
        let mut resp = CompoundResponse::new();
        resp.status = Nfs4Status::NotSupp;
        resp.tag = String::new();
        resp.results.push(OperationResult::DelegPurge(Nfs4Status::NotSupp));
        let b = resp.encode();
        // status, tag(len 0), count=1, opcode, op status
        let words: Vec<u32> = b.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect();
        assert_eq!(words[2], 1, "result count");
        assert_eq!(words[3], opcode::DELEGPURGE);
        assert_eq!(words[4], Nfs4Status::NotSupp as u32);

        let mut resp = CompoundResponse::new();
        resp.status = Nfs4Status::BadStateId;
        resp.tag = String::new();
        resp.results.push(OperationResult::DelegReturn(Nfs4Status::BadStateId));
        let b = resp.encode();
        let words: Vec<u32> = b.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect();
        assert_eq!(words[3], opcode::DELEGRETURN);
        assert_eq!(words[4], Nfs4Status::BadStateId as u32);
    }

    /// OPEN_DELEGATE_NONE_EXT and its union, which is where an encoder
    /// can quietly corrupt everything downstream of it.
    ///
    /// `open_none_delegation4` switches on ond_why and only
    /// WND4_CONTENTION and WND4_RESOURCE carry a trailing bool — every
    /// other arm is `void`. Encoding a bool on a void arm does not
    /// produce a visible error: it shifts every following word of the
    /// compound by four bytes, so the client mis-decodes the NEXT
    /// operation's result and blames that. So the assertion that
    /// matters here is the reply LENGTH, not the reason code.
    #[test]
    fn the_none_ext_arm_encodes_its_union_and_nothing_more() {
        let mk = |deleg: Option<Delegation>| {
            let mut resp = CompoundResponse::new();
            resp.status = Nfs4Status::Ok;
            resp.tag = String::new();
            resp.results.push(OperationResult::Open(
                Nfs4Status::Ok,
                Some(OpenResult {
                    stateid: StateId { seqid: 1, other: [1u8; 12] },
                    change_info: ChangeInfo { atomic: true, before: 0, after: 0 },
                    result_flags: 0,
                    attrset: vec![],
                    delegation: deleg,
                }),
            ));
            let b = resp.encode();
            b.chunks(4)
                .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
                .collect::<Vec<u32>>()
        };
        let deleg_at = 5 + 4 + 5 + 1 + 1;

        // WND4_NOT_WANTED — the DELEG4 case, and a VOID union arm.
        let w = mk(Some(Delegation::NoneExt {
            why: WhyNoDelegation::NotWanted,
        }));
        assert_eq!(w[deleg_at], 3, "OPEN_DELEGATE_NONE_EXT");
        assert_eq!(w[deleg_at + 1], 0, "WND4_NOT_WANTED");
        assert_eq!(
            w.len(),
            deleg_at + 2,
            "a void union arm must add NOTHING after ond_why",
        );

        // WND4_CANCELLED — also void, and a different code, so this
        // is not the previous case passing by coincidence.
        let w = mk(Some(Delegation::NoneExt {
            why: WhyNoDelegation::Cancelled,
        }));
        assert_eq!(w[deleg_at + 1], 7, "WND4_CANCELLED");
        assert_eq!(w.len(), deleg_at + 2);

        // WND4_CONTENTION — one bool, ond_server_will_push_deleg.
        let w = mk(Some(Delegation::NoneExt {
            why: WhyNoDelegation::Contention,
        }));
        assert_eq!(w[deleg_at + 1], 1, "WND4_CONTENTION");
        assert_eq!(w.len(), deleg_at + 3, "CONTENTION carries one bool");
        assert_eq!(w[deleg_at + 2], 0, "we never push a delegation later");

        // WND4_RESOURCE — one bool, ond_server_will_signal_avail.
        let w = mk(Some(Delegation::NoneExt {
            why: WhyNoDelegation::Resource,
        }));
        assert_eq!(w[deleg_at + 1], 2, "WND4_RESOURCE");
        assert_eq!(w.len(), deleg_at + 3, "RESOURCE carries one bool");
        assert_eq!(w[deleg_at + 2], 0, "we never signal availability");

        // And the plain NONE arm is still shorter than either — the
        // three shapes are distinguishable on the wire by length alone.
        assert_eq!(mk(None).len(), deleg_at + 1);
    }

    /// A granted delegation actually reaches the wire: the OPEN result
    /// encodes OPEN_DELEGATE_READ + stateid + recall=false + one
    /// EVERYONE@ ALLOW ace — and without a grant the arm stays
    /// OPEN_DELEGATE_NONE (the encoder hardcoded 0 for years; this is
    /// the third party of the encoder/advertisement/heuristics chain).
    #[test]
    fn open_result_encodes_the_read_delegation_arm() {
        let deleg_sid = StateId {
            seqid: 1,
            other: [7u8; 12],
        };
        let mk = |deleg: Option<Delegation>| {
            let mut resp = CompoundResponse::new();
            resp.status = Nfs4Status::Ok;
            resp.tag = String::new();
            resp.results.push(OperationResult::Open(
                Nfs4Status::Ok,
                Some(OpenResult {
                    stateid: StateId {
                        seqid: 1,
                        other: [1u8; 12],
                    },
                    change_info: ChangeInfo {
                        atomic: true,
                        before: 0,
                        after: 0,
                    },
                    result_flags: 0,
                    attrset: vec![],
                    delegation: deleg,
                }),
            ));
            resp.encode()
        };

        // No grant: the delegation word (after stateid(4) + cinfo(5) +
        // rflags(1) + empty bitmap(1) = 11 words past the op status)
        // is OPEN_DELEGATE_NONE and the reply ends there.
        let b = mk(None);
        let words: Vec<u32> = b
            .chunks(4)
            .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(words[3], opcode::OPEN);
        assert_eq!(words[4], Nfs4Status::Ok as u32);
        let deleg_at = 5 + 4 + 5 + 1 + 1;
        assert_eq!(words[deleg_at], 0, "OPEN_DELEGATE_NONE");
        assert_eq!(words.len(), deleg_at + 1, "reply ends after the NONE arm");

        // Granted: READ arm with the delegation stateid verbatim, then
        // recall=false, then the single permissive ace.
        let b = mk(Some(Delegation::Read {
            stateid: deleg_sid,
        }));
        let words: Vec<u32> = b
            .chunks(4)
            .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(words[deleg_at], 1, "OPEN_DELEGATE_READ");
        assert_eq!(words[deleg_at + 1], 1, "delegation stateid.seqid");
        assert_eq!(
            words[deleg_at + 2],
            u32::from_be_bytes([7, 7, 7, 7]),
            "delegation stateid.other travels verbatim"
        );
        assert_eq!(words[deleg_at + 5], 0, "recall=false at grant");
        assert_eq!(words[deleg_at + 6], 0, "ace type ALLOW");
        assert_eq!(words[deleg_at + 7], 0, "ace flag");
        assert_eq!(words[deleg_at + 8], 0x0012_0089, "ace access mask");
        assert_eq!(words[deleg_at + 9], 9, "who length");
        assert_eq!(&b[(deleg_at + 10) * 4..(deleg_at + 10) * 4 + 9], b"EVERYONE@");
    }
}

#[cfg(test)]
mod segment_tests {
    use super::*;

    /// Build the same response twice — `CompoundResponse` is not Clone,
    /// and a test that encoded one object twice would be asserting
    /// nothing about the second path.
    fn resp(payload: &[u8], eof: bool) -> CompoundResponse {
        let mut r = CompoundResponse::new();
        r.tag = "tag".to_string();
        r.results.push(OperationResult::PutRootFh(Nfs4Status::Ok));
        r.results.push(OperationResult::Read(
            Nfs4Status::Ok,
            Some(ReadResult { eof, data: Bytes::copy_from_slice(payload).into() }),
        ));
        r.results.push(OperationResult::PutRootFh(Nfs4Status::Ok));
        r
    }

    fn joined(segs: Vec<crate::nfs::segment::Segment>) -> Vec<u8> {
        let mut v = Vec::new();
        for s in segs {
            v.extend_from_slice(s.as_mem());
        }
        v
    }

    /// THE SAFETY PROPERTY. `encode_segments` changes the copy count,
    /// never the wire. Asserted against the REAL `encode`, not against a
    /// reimplementation of it — a hand-written reference would drift
    /// from the encoder and start agreeing with the wrong bytes.
    ///
    /// Payload lengths cover every XDR pad residue, because the pad is
    /// the one thing the segmented path emits from a different place
    /// than `encode_opaque` does: it lands in the segment AFTER the
    /// payload rather than inline behind it.
    #[test]
    fn segments_match_the_flattened_encoding() {
        for len in [0usize, 1, 2, 3, 4, 5, 7, 8, 4096, 4097] {
            for eof in [false, true] {
                let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                let flat = resp(&payload, eof).encode();
                let segs = resp(&payload, eof).encode_segments();
                assert_eq!(
                    joined(segs),
                    flat.to_vec(),
                    "len={} eof={} — segmented and flattened wire bytes diverged",
                    len,
                    eof
                );
            }
        }
    }

    /// THE HEADROOM IS LOAD-BEARING, AND THIS IS WHERE IT IS CHECKED.
    ///
    /// The fore-channel cap we advertise is `MAX_IO_PAYLOAD +
    /// CHANNEL_HEADROOM`, and a client that takes us at our word asks
    /// for `MAX_IO_PAYLOAD` bytes in a single READ. What comes back is
    /// that payload PLUS a COMPOUND envelope, and the dispatcher
    /// refuses any reply longer than the negotiated `ca_maxresponsesize`
    /// with REP_TOO_BIG. If the headroom does not cover the envelope,
    /// the server rejects exactly the request its own advertisement
    /// invited — on every full-size read, not as an edge case.
    ///
    /// The envelope is MEASURED, not written down. A constant would
    /// keep agreeing with today's op list while a future compound grew
    /// past it.
    #[test]
    fn a_full_size_read_reply_fits_under_the_cap_we_advertise() {
        use crate::nfs::v4::operations::session::{MAX_IO_PAYLOAD, SERVER_MAX_RESPONSE};

        // The real shape of a read compound: SEQUENCE, PUTFH, READ.
        let payload = vec![0u8; MAX_IO_PAYLOAD as usize];
        let mut r = CompoundResponse::new();
        r.tag = String::new();
        r.results.push(OperationResult::Sequence(
            Nfs4Status::Ok,
            Some(SequenceResult {
                sessionid: SessionId([0u8; 16]),
                sequenceid: 1,
                slotid: 0,
                highest_slotid: 127,
                target_highest_slotid: 127,
                status_flags: 0,
            }),
        ));
        r.results.push(OperationResult::PutFh(Nfs4Status::Ok));
        r.results.push(OperationResult::Read(
            Nfs4Status::Ok,
            Some(ReadResult { eof: false, data: Bytes::from(payload).into() }),
        ));

        let encoded: usize = r.encode_segments().iter().map(|s| s.len()).sum();
        assert!(
            encoded <= SERVER_MAX_RESPONSE as usize,
            "a MAX_IO_PAYLOAD READ encodes to {encoded} bytes, above the advertised \
             ca_maxresponsesize {SERVER_MAX_RESPONSE} — the dispatcher would answer \
             REP_TOO_BIG to the very read the negotiated rsize invites",
        );
        // Anti-vacuity: the margin has to be the headroom absorbing a
        // real envelope, not an envelope that encoded to nothing.
        assert!(
            encoded > MAX_IO_PAYLOAD as usize,
            "the COMPOUND envelope encoded to zero bytes — this assertion would \
             hold with no headroom at all and is not testing anything",
        );
    }

    /// The payload must travel as its OWN segment, not be copied into a
    /// buffer. Without this the test above would still pass for an
    /// implementation that segmented nothing and simply called `encode`.
    #[test]
    fn a_read_payload_is_carried_as_its_own_segment() {
        let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let segs = resp(&payload, false).encode_segments();
        assert!(
            segs.len() >= 3,
            "expected head/payload/tail segments, got {}",
            segs.len()
        );
        assert!(
            segs.iter().any(|s| s.len() == 4096 && s.as_mem().as_ref() == payload.as_slice()),
            "the payload is not present as an uncopied standalone segment"
        );
    }

    /// A zero-length READ cuts no segment: there are no bytes and no
    /// pad, so segmenting it would add a wire-invisible empty segment
    /// and an allocation for nothing.
    #[test]
    fn an_empty_read_payload_cuts_no_segment() {
        let flat = resp(&[], true).encode();
        let segs = resp(&[], true).encode_segments();
        assert_eq!(segs.len(), 1, "an empty payload must not be segmented");
        assert_eq!(joined(segs), flat.to_vec());
    }

    /// A replay is served verbatim from the slot cache and must not be
    /// re-encoded by either path.
    #[test]
    fn a_raw_replay_reply_passes_through_untouched() {
        let raw = Bytes::from_static(b"already-encoded-reply");
        let mut r = CompoundResponse::new();
        r.raw_reply = Some(vec![raw.clone().into()]);
        assert_eq!(joined(r.encode_segments()), raw.to_vec());
    }
}

#[cfg(test)]
mod tests {
    // ── C8: csa_sec_parms<> ──────────────────────────────────────────
    //
    // There were NO CREATE_SESSION decode tests before this. That is how
    // "we leave them unconsumed" survived: nothing looked at the field,
    // and the one CB-encoding test that existed asserted AUTH_NONE
    // unconditionally — encoding the bug as a requirement.
    //
    // The AUTH_SYS bytes below are the real ones a Linux 6.1 client sent
    // on runas, 2026-07-31, captured with tcpdump at CREATE_SESSION.

    fn sec_parms_bytes(entries: &[&[u8]]) -> bytes::Bytes {
        let mut v = Vec::new();
        v.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for e in entries {
            v.extend_from_slice(e);
        }
        bytes::Bytes::from(v)
    }

    /// The exact AUTH_SYS entry observed on the wire.
    fn linux_auth_sys_entry() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&1u32.to_be_bytes());          // AUTH_SYS
        v.extend_from_slice(&0xda039c49u32.to_be_bytes()); // stamp
        let name = b"ip-172-31-15-129.us-west-1.compute.internal"; // 43 bytes
        v.extend_from_slice(&(name.len() as u32).to_be_bytes());
        v.extend_from_slice(name);
        v.push(0); // XDR pad to a 4-byte boundary (43 -> 44)
        v.extend_from_slice(&0u32.to_be_bytes());          // uid
        v.extend_from_slice(&0u32.to_be_bytes());          // gid
        v.extend_from_slice(&0u32.to_be_bytes());          // gids<> empty
        v
    }

    // ── COPY4args.ca_source_server<netloc4> ──────────────────────────
    //
    // Same failure shape as C8 above and worth stating plainly: a
    // variable-length trailing field that was never consumed, so
    // everything after it decoded from the wrong offset. Here the array's
    // length word was read as the NEXT opcode, and for the ordinary empty
    // case that word is 0 — a reserved opcode — so the compound was
    // silently truncated to one operation plus an OP_ILLEGAL.

    /// Build a COMPOUND: tag "", minorversion 2, [COPY(args), GETATTR].
    fn compound_with_copy(netloc: &[u8]) -> bytes::Bytes {
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_be_bytes()); // tag<> empty
        v.extend_from_slice(&2u32.to_be_bytes()); // minorversion
        v.extend_from_slice(&2u32.to_be_bytes()); // 2 operations

        v.extend_from_slice(&opcode::COPY.to_be_bytes());
        for _ in 0..2 {
            v.extend_from_slice(&1u32.to_be_bytes()); // stateid seqid
            v.extend_from_slice(&[0u8; 12]); // stateid other
        }
        v.extend_from_slice(&0u64.to_be_bytes()); // ca_src_offset
        v.extend_from_slice(&0u64.to_be_bytes()); // ca_dst_offset
        v.extend_from_slice(&16u64.to_be_bytes()); // ca_count
        v.extend_from_slice(&0u32.to_be_bytes()); // ca_consecutive = FALSE
        v.extend_from_slice(&1u32.to_be_bytes()); // ca_synchronous = TRUE
        v.extend_from_slice(netloc); // ca_source_server<>

        v.extend_from_slice(&opcode::GETATTR.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // attr bitmap<> empty

        bytes::Bytes::from(v)
    }

    #[test]
    fn an_empty_source_server_list_leaves_the_next_operation_decodable() {
        let empty = 0u32.to_be_bytes().to_vec();
        let req = CompoundRequest::decode(XdrDecoder::new(compound_with_copy(&empty))).expect("decodes");
        assert_eq!(
            req.operations.len(),
            2,
            "the GETATTR after COPY must survive: {:?}",
            req.operations
        );
        assert!(matches!(req.operations[1], Operation::GetAttr(_)));
        match &req.operations[0] {
            Operation::Copy { source_server_count, count, .. } => {
                assert_eq!(*source_server_count, 0);
                assert_eq!(*count, 16);
            }
            other => panic!("expected COPY, got {other:?}"),
        }
    }

    /// A non-empty list is an INTER-server copy. Each arm has a different
    /// width, so the decoder has to walk them to leave the cursor right —
    /// returning the count without consuming the bodies would move the
    /// desync rather than remove it.
    #[test]
    fn a_populated_source_server_list_is_consumed_arm_by_arm() {
        // One NL4_NETADDR entry: two strings, the second needing XDR pad.
        let mut netloc = Vec::new();
        netloc.extend_from_slice(&1u32.to_be_bytes()); // one entry
        netloc.extend_from_slice(&3u32.to_be_bytes()); // NL4_NETADDR
        netloc.extend_from_slice(&3u32.to_be_bytes()); // na_r_netid len
        netloc.extend_from_slice(b"tcp");
        netloc.push(0); // pad 3 -> 4
        netloc.extend_from_slice(&9u32.to_be_bytes()); // na_r_addr len
        netloc.extend_from_slice(b"10.0.0.1.");
        netloc.extend_from_slice(&[0, 0, 0]); // pad 9 -> 12

        let req = CompoundRequest::decode(XdrDecoder::new(compound_with_copy(&netloc))).expect("decodes");
        assert_eq!(req.operations.len(), 2, "GETATTR must still be reachable");
        assert!(matches!(req.operations[1], Operation::GetAttr(_)));
        match &req.operations[0] {
            Operation::Copy { source_server_count, .. } => assert_eq!(*source_server_count, 1),
            other => panic!("expected COPY, got {other:?}"),
        }
    }

    /// An NL4_NAME entry (single string) — the other arm width.
    #[test]
    fn a_name_arm_source_server_entry_is_consumed() {
        let mut netloc = Vec::new();
        netloc.extend_from_slice(&1u32.to_be_bytes());
        netloc.extend_from_slice(&1u32.to_be_bytes()); // NL4_NAME
        netloc.extend_from_slice(&4u32.to_be_bytes());
        netloc.extend_from_slice(b"srv1");

        let req = CompoundRequest::decode(XdrDecoder::new(compound_with_copy(&netloc))).expect("decodes");
        assert_eq!(req.operations.len(), 2);
        assert!(matches!(req.operations[1], Operation::GetAttr(_)));
    }

    /// An unknown discriminant has an unknown width, so the decoder cannot
    /// honestly claim to have consumed it: BADXDR, not a guess.
    #[test]
    fn an_unknown_netloc_arm_is_a_decode_error_not_a_silent_skip() {
        let mut netloc = Vec::new();
        netloc.extend_from_slice(&1u32.to_be_bytes());
        netloc.extend_from_slice(&99u32.to_be_bytes()); // not a netloc_type4

        let req = CompoundRequest::decode(XdrDecoder::new(compound_with_copy(&netloc))).expect("decodes");
        assert!(
            matches!(req.operations[0], Operation::BadXdr(opcode::COPY)),
            "got {:?}",
            req.operations[0]
        );
    }

    #[test]
    fn decodes_the_auth_sys_offer_a_real_linux_client_sends() {
        let buf = sec_parms_bytes(&[&linux_auth_sys_entry()]);
        let mut d = XdrDecoder::new(buf);
        let parsed = decode_callback_sec_parms(&mut d).expect("decodes");
        assert_eq!(
            parsed,
            vec![CallbackSecParms::Sys {
                stamp: 0xda039c49,
                machinename: "ip-172-31-15-129.us-west-1.compute.internal".into(),
                uid: 0,
                gid: 0,
                gids: vec![],
            }],
            "a Linux 6.1 client offers exactly one credential and it is AUTH_SYS — \
             the pre-C8 hardcoded AUTH_NONE could only ever be DENIED",
        );
    }

    #[test]
    fn decodes_auth_none_and_an_empty_offer() {
        let none_entry = 0u32.to_be_bytes().to_vec();
        let mut d = XdrDecoder::new(sec_parms_bytes(&[&none_entry]));
        assert_eq!(
            decode_callback_sec_parms(&mut d).unwrap(),
            vec![CallbackSecParms::None]
        );

        let mut d = XdrDecoder::new(sec_parms_bytes(&[]));
        assert!(decode_callback_sec_parms(&mut d).unwrap().is_empty());
    }

    /// RPCSEC_GSS must be CONSUMED, not ignored: its body is variable
    /// length, so mis-framing it desyncs every entry after it.
    #[test]
    fn gss_entry_is_consumed_so_later_entries_still_parse() {
        let mut gss = Vec::new();
        gss.extend_from_slice(&6u32.to_be_bytes()); // RPCSEC_GSS
        gss.extend_from_slice(&1u32.to_be_bytes()); // gcbp_service
        gss.extend_from_slice(&4u32.to_be_bytes()); // handle len
        gss.extend_from_slice(b"abcd");
        gss.extend_from_slice(&2u32.to_be_bytes()); // handle len
        gss.extend_from_slice(b"xy");
        gss.extend_from_slice(&[0, 0]);             // pad to 4

        let buf = sec_parms_bytes(&[&gss, &linux_auth_sys_entry()]);
        let mut d = XdrDecoder::new(buf);
        let parsed = decode_callback_sec_parms(&mut d).expect("decodes past the GSS entry");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], CallbackSecParms::Gss);
        assert!(
            matches!(&parsed[1], CallbackSecParms::Sys { machinename, .. }
                     if machinename.starts_with("ip-172-31")),
            "the entry AFTER the GSS one must still frame correctly",
        );
    }

    #[test]
    fn refuses_an_implausible_offer_rather_than_allocating() {
        let mut v = Vec::new();
        v.extend_from_slice(&9999u32.to_be_bytes());
        let mut d = XdrDecoder::new(bytes::Bytes::from(v));
        assert!(decode_callback_sec_parms(&mut d).is_err());
    }

    use super::*;
    use crate::nfs::xdr::XdrDecoder;
    use bytes::{BytesMut, BufMut};
    use crate::nfs::v4::operations::Fattr4;

    /// Every wire-fed array count must be bounded BEFORE it reaches
    /// `Vec::with_capacity`.
    ///
    /// Counts were read straight off the wire and used as a capacity
    /// with no bound: a ~30-byte unauthenticated COMPOUND claiming
    /// `op_count = 0xFFFFFFFF` asked for a multi-hundred-GiB allocation
    /// from any host that can reach port 2049. Decode runs before the
    /// RPC credential is inspected, and AUTH_SYS authenticates nothing,
    /// so there is no gate in front of it.
    ///
    /// Each case here is a distinct call site. They assert the frame is
    /// REFUSED rather than merely "does not crash" — a test that only
    /// checked for a panic would pass on the unbounded code, since the
    /// allocation is lazy on Linux and the loop errors out on the next
    /// byte anyway.
    #[test]
    fn wire_array_counts_are_bounded_before_allocating() {
        // A count that cannot possibly be described by the bytes left.
        const LIE: u32 = 0xFFFF_FFFF;

        // -- site 1: COMPOUND argarray (the reachable one, pre-auth) ----
        let mut b = BytesMut::new();
        b.put_u32(0);      // tag<> empty
        b.put_u32(1);      // minorversion 1
        b.put_u32(LIE);    // op_count — and then nothing at all
        let err = CompoundRequest::decode(XdrDecoder::new(b.freeze()))
            .expect_err("an impossible op_count must be refused");
        assert!(
            err.contains("argarray") || err.contains("exceeds"),
            "op_count must be refused by the bound, got: {err}"
        );

        // -- site 2: SETATTR bitmap4 ------------------------------------
        let mut b = BytesMut::new();
        b.put_u32(0); b.put_u32(0); b.put_u32(0); // stateid seqid + other(12)
        b.put_u32(0); b.put_u32(0);
        b.put_u32(LIE); // bitmap_len
        let mut d = XdrDecoder::new(b.freeze());
        assert!(
            CompoundRequest::decode_operation(&mut d, opcode::SETATTR).is_err(),
            "SETATTR bitmap4 length must be refused"
        );

        // -- site 3: TEST_STATEID stateid array -------------------------
        let mut b = BytesMut::new();
        b.put_u32(LIE);
        let mut d = XdrDecoder::new(b.freeze());
        assert!(
            CompoundRequest::decode_operation(&mut d, opcode::TEST_STATEID).is_err(),
            "TEST_STATEID array length must be refused"
        );

        // -- the bound itself, both directions --------------------------
        use crate::nfs::xdr::checked_array_len;
        // Refuses the impossible...
        assert!(checked_array_len(LIE, 12, 4, "t").is_err());
        assert!(checked_array_len(3, 12, 4, "t").is_ok()); // 3 x 4B fits in 12B
        assert!(checked_array_len(4, 12, 4, "t").is_err()); // 4 x 4B does not
        // ...and must NOT refuse anything that could actually decode.
        // This is the anti-vacuity half: a bound of "always reject" or a
        // small magic constant would pass every assert above and break
        // legitimate traffic. 1024 ops in 4 KiB is legal.
        assert_eq!(checked_array_len(1024, 4096, 4, "t").unwrap(), 1024);
        assert_eq!(checked_array_len(0, 0, 4, "t").unwrap(), 0);
    }

    /// CREATE must carry the client's `createattrs` off the wire.
    ///
    /// The decoder used to parse the bitmap and the attrlist and then
    /// DISCARD both ("consumed for wire alignment"), while the dispatcher
    /// handed `handle_create` a hardcoded empty Fattr4. The handler has
    /// always applied whatever it was given, so a unit test at that level
    /// passed while every real `mkdir(mode)` on the wire landed with
    /// default permissions — measured against a Linux kernel client:
    /// 0700 / 0750 / 0711 / 0777 all came back 0755, i.e. a directory the
    /// caller asked to be private was world-readable.
    #[test]
    fn create_carries_createattrs_off_the_wire() {
        // CREATE args: type(NF4DIR=2), objname<>, createattrs(bitmap+attrs)
        let mut buf = BytesMut::new();
        buf.put_u32(2); // NF4DIR — no type-specific tail for a directory
        let name = b"workspace";
        buf.put_u32(name.len() as u32);
        buf.put_slice(name);
        buf.put_slice(&[0u8; 3]); // pad 9 -> 12
        // fattr4: bitmap4 of one word with FATTR4_MODE (33) => word1 bit1.
        buf.put_u32(2); // two bitmap words
        buf.put_u32(0);
        buf.put_u32(1 << 1);
        // attrlist4: mode 0o700 as a 4-byte opaque
        buf.put_u32(4);
        buf.put_u32(0o700);

        let mut dec = XdrDecoder::new(buf.freeze());
        let op = CompoundRequest::decode_operation(&mut dec, opcode::CREATE)
            .expect("CREATE must decode");

        match op {
            Operation::Create { objtype, objname, createattrs, .. } => {
                assert_eq!(objname, "workspace");
                assert!(matches!(objtype, Nfs4FileType::Directory));
                // The whole point: the attrs must SURVIVE the decode.
                assert_eq!(
                    createattrs.attrmask,
                    vec![0u32, 1 << 1],
                    "createattrs bitmap was dropped by the decoder"
                );
                assert_eq!(
                    createattrs.attr_vals,
                    0o700u32.to_be_bytes().to_vec(),
                    "createattrs values were dropped by the decoder"
                );
                // And they must decode back to the mode the client asked
                // for — an attrmask carried without its values would pass
                // the two asserts above but still lose the mode.
                let want = crate::nfs::v4::operations::fileops::decode_settable_attrs(
                    &createattrs.attrmask,
                    &createattrs.attr_vals,
                )
                .expect("createattrs must be decodable");
                assert_eq!(want.mode, Some(0o700), "mode did not survive end-to-end");
            }
            other => panic!("expected Operation::Create, got {other:?}"),
        }
    }

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
    fn secinfo_advertises_only_what_the_server_can_honour() {
        // ⚠ THE ORDER IS THE ASSERTION. A client takes the FIRST flavor
        // it supports, so whichever is listed first is what every mount
        // actually uses. This test previously pinned AUTH_NONE first and
        // passed happily while a stock `mount -t nfs -o vers=4.1`
        // negotiated `sec=null` — every operation arriving with no uid,
        // no gid and no identity of any kind. Measured in /proc/mounts,
        // 2026-08-24.
        //
        // It also pinned `service = 1` (rpc_gss_svc_none) as the ONLY GSS
        // entry, which is what a server with no per-message protection
        // must say. Both krb5i and krb5p are implemented now, so all
        // three services are offered, strongest first.
        //
        // And GSS is offered only when a keytab actually loaded: it used
        // to be advertised unconditionally, inviting every client into a
        // mechanism a keytab-less server then refused.
        use crate::nfs::rpcsec_gss::set_gss_available_for_test;

        // --- with GSS available -------------------------------------
        set_gss_available_for_test(true);
        let mut encoder = XdrEncoder::new();
        encode_secinfo_flavors(&mut encoder);
        let mut d = XdrDecoder::new(encoder.finish());
        assert_eq!(d.decode_u32().unwrap(), 5, "AUTH_SYS + 3 GSS services + AUTH_NONE");
        assert_eq!(
            d.decode_u32().unwrap(),
            1,
            "AUTH_SYS MUST be advertised first — it is the first flavor a client will \
             take, and it is the only non-GSS one that carries a uid to check"
        );
        // rpc_gss_svc_privacy, _integrity, _none — strongest first.
        for want in [3u32, 2, 1] {
            assert_eq!(d.decode_u32().unwrap(), 6, "RPCSEC_GSS");
            assert_eq!(
                d.decode_opaque().unwrap().as_ref(),
                &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02],
                "Kerberos V5 OID 1.2.840.113554.1.2.2",
            );
            assert_eq!(d.decode_u32().unwrap(), 0, "QOP");
            assert_eq!(d.decode_u32().unwrap(), want, "service");
        }
        assert_eq!(
            d.decode_u32().unwrap(),
            0,
            "AUTH_NONE must be LAST — advertising it first is how this server spent its \
             whole life negotiating sec=null"
        );
        assert_eq!(d.remaining(), 0, "no trailing bytes");

        // --- without a keytab ---------------------------------------
        set_gss_available_for_test(false);
        let mut encoder = XdrEncoder::new();
        encode_secinfo_flavors(&mut encoder);
        let mut d = XdrDecoder::new(encoder.finish());
        assert_eq!(d.decode_u32().unwrap(), 2, "AUTH_SYS + AUTH_NONE only");
        assert_eq!(d.decode_u32().unwrap(), 1, "AUTH_SYS still first");
        assert_eq!(d.decode_u32().unwrap(), 0, "AUTH_NONE still last");
        assert_eq!(
            d.remaining(),
            0,
            "no GSS entry may be advertised when no keytab loaded"
        );

        set_gss_available_for_test(false);
    }

    /// Decode a SECINFO flavor list back into the levels it offers.
    #[cfg(test)]
    fn advertised_levels(bytes: Bytes) -> Vec<crate::nfs::sec_policy::SecLevel> {
        use crate::nfs::sec_policy::SecLevel;
        let mut d = XdrDecoder::new(bytes);
        let count = d.decode_u32().unwrap();
        let mut out = Vec::new();
        for _ in 0..count {
            match d.decode_u32().unwrap() {
                0 => out.push(SecLevel::None),
                1 => out.push(SecLevel::Sys),
                6 => {
                    d.decode_opaque().unwrap();
                    assert_eq!(d.decode_u32().unwrap(), 0, "QOP");
                    out.push(match d.decode_u32().unwrap() {
                        1 => SecLevel::Krb5,
                        2 => SecLevel::Krb5i,
                        3 => SecLevel::Krb5p,
                        other => panic!("unknown GSS service {other}"),
                    });
                }
                other => panic!("unknown flavor {other}"),
            }
        }
        assert_eq!(d.remaining(), 0, "no trailing bytes");
        out
    }

    #[test]
    fn secinfo_offers_exactly_what_the_floor_will_accept() {
        // The invariant, not the two code paths, is the real thing:
        // whatever SECINFO lists, the accept path must serve, and
        // whatever it omits, the accept path must refuse. A server that
        // advertises sec=sys under a krb5p floor sends every client
        // down a mount that will only ever come back AUTH_TOOWEAK.
        use crate::nfs::sec_policy::{SecLevel, SecPolicy};

        for &floor in &SecLevel::ALL {
            let policy = SecPolicy::new(floor);
            let mut encoder = XdrEncoder::new();
            encode_secinfo_flavors_with(&mut encoder, true, policy);
            let offered = advertised_levels(encoder.finish());

            for &level in &SecLevel::ALL {
                assert_eq!(
                    offered.contains(&level),
                    policy.permits(level),
                    "floor {}: advertising {} but permits() says {}",
                    floor.name(),
                    level.name(),
                    policy.permits(level)
                );
            }
        }
    }

    #[test]
    fn a_krb5p_floor_advertises_only_krb5p() {
        use crate::nfs::sec_policy::{SecLevel, SecPolicy};
        let mut encoder = XdrEncoder::new();
        encode_secinfo_flavors_with(&mut encoder, true, SecPolicy::new(SecLevel::Krb5p));
        assert_eq!(advertised_levels(encoder.finish()), vec![SecLevel::Krb5p]);
    }

    #[test]
    fn a_kerberos_floor_without_a_keytab_advertises_nothing_at_all() {
        // An export configured to require Kerberos on a server that
        // loaded no keys can serve no one. The empty list is the honest
        // answer — the alternative is offering sec=sys as a fallback,
        // which would quietly serve the traffic the floor exists to
        // stop. `NfsServer::new` logs this configuration loudly.
        use crate::nfs::sec_policy::{SecLevel, SecPolicy};
        for floor in [SecLevel::Krb5, SecLevel::Krb5i, SecLevel::Krb5p] {
            let mut encoder = XdrEncoder::new();
            encode_secinfo_flavors_with(&mut encoder, false, SecPolicy::new(floor));
            assert!(
                advertised_levels(encoder.finish()).is_empty(),
                "floor {} with no keytab offered something",
                floor.name()
            );
        }
    }

    #[test]
    fn the_default_floor_leaves_the_advertisement_exactly_as_it_shipped() {
        // Guards the non-breaking claim: an export that never set the
        // knob must advertise the same five entries, in the same order,
        // as before the floor existed.
        use crate::nfs::sec_policy::{SecLevel, SecPolicy};
        let mut encoder = XdrEncoder::new();
        encode_secinfo_flavors_with(&mut encoder, true, SecPolicy::default());
        assert_eq!(
            advertised_levels(encoder.finish()),
            vec![
                SecLevel::Sys,
                SecLevel::Krb5p,
                SecLevel::Krb5i,
                SecLevel::Krb5,
                SecLevel::None
            ]
        );
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
