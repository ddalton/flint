//! NFSv4.1 callback channel — CB_COMPOUND XDR + RPC framing.
//!
//! The callback channel is the mirror image of the forward channel:
//! the *server* sends RPC CALLs to the client over a back-channel TCP
//! connection (the same TCP socket the client used to send the
//! original COMPOUND, in the typical Linux v4.1 case — see
//! [`super::back_channel`] for how the writer is shared between
//! directions).
//!
//! Wire shape of one callback exchange:
//!
//! ```text
//!     server → client    record-marker | xid | CALL | rpcvers=2
//!                        | program=cb_program | version=1
//!                        | proc=CB_COMPOUND | cred(NULL) | verf(NULL)
//!                        | CB_COMPOUND4args (this module)
//!
//!     client → server    record-marker | xid | REPLY | accept_status
//!                        | verifier(NULL) | accept_status=SUCCESS
//!                        | CB_COMPOUND4res (this module)
//! ```
//!
//! `cb_program` is what the client advertised at CREATE_SESSION (RFC
//! 5661 §18.36 csa_cb_program) — we plumbed it onto `Session` so the
//! callback fan-out can pick it up by session id.
//!
//! Phase A.2 deliberately stops at "build a CALL frame and parse a
//! REPLY frame." Actually pushing it onto a [`BackChannelWriter`] and
//! awaiting the reply lives in Phase A.3, where xid pairing, timeout
//! and retry policy belong.

use crate::nfs::rpc::{
    AcceptStatus, AuthFlavor, MessageType, ReplyStatus,
};
use crate::nfs::v4::protocol::{
    cb_opcode, cb_procedure, layoutrecall_type,
    CB_VERSION, Nfs4Status, SessionId, StateId,
};
use crate::nfs::v4::xdr::{Nfs4XdrDecoder, Nfs4XdrEncoder};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;

/// One callback operation in a CB_COMPOUND.
///
/// We model only the variants we actually emit (`CB_SEQUENCE`,
/// `CB_LAYOUTRECALL`). Adding `CB_RECALL` for delegations later will
/// be a new variant rather than a separate type, so the
/// `encode_compound`/`decode_reply` plumbing stays single-source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CbOp {
    /// `CB_SEQUENCE` (RFC 8881 §20.9). MUST be the first op in every
    /// non-empty CB_COMPOUND, the same way `SEQUENCE` is on the
    /// forward channel.
    Sequence {
        sessionid: SessionId,
        sequenceid: u32,
        slotid: u32,
        highest_slotid: u32,
        cachethis: bool,
        // referring_call_lists<> — left empty by this server. We do
        // not chain callbacks to forward calls, and the client
        // doesn't need the cross-reference for correctness.
    },
    /// `CB_LAYOUTRECALL` (RFC 8881 §20.3) — ask the client to return
    /// a layout, eventually so the MDS can hand out a fresh one that
    /// excludes a dead DS.
    LayoutRecall {
        layout_type: u32,
        iomode: u32,
        changed: bool,
        recall: LayoutRecall,
    },
}

/// Body of a `CB_LAYOUTRECALL` (the `clora_recall` discriminated
/// union in RFC 8881 §20.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutRecall {
    /// Recall a specific byte range of one file.
    File {
        fh: Vec<u8>,
        offset: u64,
        length: u64,
        stateid: StateId,
    },
    /// Recall everything for a particular fsid.
    Fsid { fsid_major: u64, fsid_minor: u64 },
    /// Recall every layout the client holds.
    All,
}

/// Decoded form of a CB_COMPOUND request (the args the server emits
/// on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CbCompoundCall {
    /// Free-form tag echoed in the reply (RFC 8881 §20.2). We send
    /// empty.
    pub tag: String,
    /// Always 1 (NFSv4.1) for the callback program version we
    /// support.
    pub minorversion: u32,
    /// `cb_callback_ident` — opaque to the client but RFC-mandated.
    /// We hand 0 (server doesn't multiplex multiple sessions onto
    /// one callback program; not needed for FILES-layout recall).
    pub callback_ident: u32,
    pub ops: Vec<CbOp>,
}

/// One result in a CB_COMPOUND reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CbResult {
    Sequence {
        status: Nfs4Status,
        // Body only present on success — we keep the fields flat
        // (None on error) so callers don't have to crack a nested
        // option.
        sessionid: SessionId,
        sequenceid: u32,
        slotid: u32,
        highest_slotid: u32,
        target_highest_slotid: u32,
    },
    LayoutRecall {
        /// `clorr_status`. NFS4_OK on success;
        /// `NFS4ERR_NOMATCHING_LAYOUT` if the client doesn't actually
        /// hold the layout we recalled (treat as "already returned",
        /// not as a real error).
        status: Nfs4Status,
    },
    /// Catch-all for any operation we sent but didn't model a typed
    /// result for. The caller can still see the status; a rich client
    /// might want to re-decode this lazily.
    OtherStatus { opcode: u32, status: Nfs4Status },
}

impl CbResult {
    /// Status of this single op, regardless of variant. A common
    /// enough check that giving it a method beats peeling the enum.
    pub fn status(&self) -> Nfs4Status {
        match self {
            CbResult::Sequence { status, .. } => *status,
            CbResult::LayoutRecall { status } => *status,
            CbResult::OtherStatus { status, .. } => *status,
        }
    }
}

/// Decoded CB_COMPOUND reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CbCompoundReply {
    /// Top-level status. RFC 8881 §20.2 says this matches the status
    /// of the *last* result if everything ran, or the status of the
    /// failing op if one short-circuited.
    pub status: Nfs4Status,
    pub tag: String,
    pub results: Vec<CbResult>,
}

// ---------------------------------------------------------------------------
// Encoders
// ---------------------------------------------------------------------------

impl CbCompoundCall {
    /// Encode the CB_COMPOUND4 args portion of an RPC CALL — i.e. the
    /// bytes after the RPC header. Use [`encode_cb_call`] when you
    /// want a complete RPC frame (header + args).
    pub fn encode(&self) -> Bytes {
        let mut enc = XdrEncoder::new();
        enc.encode_string(&self.tag);
        enc.encode_u32(self.minorversion);
        enc.encode_u32(self.callback_ident);
        enc.encode_u32(self.ops.len() as u32);
        for op in &self.ops {
            encode_cb_op(&mut enc, op);
        }
        enc.finish()
    }
}

fn encode_cb_op(enc: &mut XdrEncoder, op: &CbOp) {
    match op {
        CbOp::Sequence {
            sessionid,
            sequenceid,
            slotid,
            highest_slotid,
            cachethis,
        } => {
            enc.encode_u32(cb_opcode::CB_SEQUENCE);
            enc.encode_sessionid(sessionid);
            enc.encode_u32(*sequenceid);
            enc.encode_u32(*slotid);
            enc.encode_u32(*highest_slotid);
            enc.encode_bool(*cachethis);
            // referring_call_lists<>: empty. Length-prefixed array
            // with zero elements. RFC 8881 §20.9.1 — leaving this off
            // is what the previous inline encoder did, and it
            // mis-framed the rest of the args. Tests cover the fix.
            enc.encode_u32(0);
        }
        CbOp::LayoutRecall {
            layout_type,
            iomode,
            changed,
            recall,
        } => {
            enc.encode_u32(cb_opcode::CB_LAYOUTRECALL);
            enc.encode_u32(*layout_type);
            enc.encode_u32(*iomode);
            enc.encode_bool(*changed);
            match recall {
                LayoutRecall::File {
                    fh,
                    offset,
                    length,
                    stateid,
                } => {
                    enc.encode_u32(layoutrecall_type::FILE);
                    enc.encode_opaque(fh);
                    enc.encode_u64(*offset);
                    enc.encode_u64(*length);
                    enc.encode_stateid(stateid);
                }
                LayoutRecall::Fsid {
                    fsid_major,
                    fsid_minor,
                } => {
                    enc.encode_u32(layoutrecall_type::FSID);
                    enc.encode_u64(*fsid_major);
                    enc.encode_u64(*fsid_minor);
                }
                LayoutRecall::All => {
                    enc.encode_u32(layoutrecall_type::ALL);
                    // void payload
                }
            }
        }
    }
}

/// Encode a complete RPC CALL frame for a CB_COMPOUND, ready to hand
/// to a [`BackChannelWriter`] via `send_record`. The caller picks the
/// `xid` (used to pair the eventual reply); the server-side caller
/// also passes the `cb_program` it persisted on the session at
/// CREATE_SESSION time.
///
/// Auth is fixed to `AUTH_NONE` — Linux's NFSv4.1 client doesn't
/// validate callback creds beyond presence of an AUTH_NONE
/// placeholder, and RPCSEC_GSS on the back-channel is a separate
/// project that can land later without re-shaping this API.
pub fn encode_cb_call(xid: u32, cb_program: u32, args: &CbCompoundCall) -> Bytes {
    let mut enc = XdrEncoder::new();
    // RPC header — see RFC 5531 §9.
    enc.encode_u32(xid);
    enc.encode_u32(MessageType::Call as u32);
    enc.encode_u32(2); // RPC version
    enc.encode_u32(cb_program);
    enc.encode_u32(CB_VERSION);
    enc.encode_u32(cb_procedure::CB_COMPOUND);
    // cred + verf, both AUTH_NONE with empty body.
    enc.encode_u32(AuthFlavor::Null as u32);
    enc.encode_opaque(&[]);
    enc.encode_u32(AuthFlavor::Null as u32);
    enc.encode_opaque(&[]);
    // Args (CB_COMPOUND4args).
    let body = args.encode();
    enc.append_raw(&body);
    enc.finish()
}

// ---------------------------------------------------------------------------
// Decoders
// ---------------------------------------------------------------------------

/// Errors that can come out of decoding a CB_COMPOUND reply. We split
/// "RPC layer rejected the call" from "the call landed but the
/// CB_COMPOUND status is non-OK" so callers can route them
/// differently — the first means "this connection is unusable,"
/// the second means "client is alive but this layout was already
/// gone."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CbReplyError {
    /// XDR decode failed (truncated frame, bad union discriminator,
    /// etc.). The connection state is whatever the writer reports;
    /// we can't tell from here.
    Xdr(String),
    /// RPC says the message wasn't a REPLY, or the reply was DENIED
    /// (auth failure, unsupported program/version).
    RpcRejected { reason: String },
    /// RPC accepted but the accept_status wasn't SUCCESS (e.g.
    /// PROG_UNAVAIL means the client isn't running a callback
    /// program). Either way, the CB body is absent.
    Accept { status: AcceptStatus },
    /// Mismatched xid — the reply we got back wasn't the one we
    /// were waiting for.
    XidMismatch { expected: u32, actual: u32 },
}

impl std::fmt::Display for CbReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CbReplyError::Xdr(s) => write!(f, "CB reply XDR decode error: {}", s),
            CbReplyError::RpcRejected { reason } => write!(f, "CB RPC rejected: {}", reason),
            CbReplyError::Accept { status } => write!(f, "CB RPC accept_status={:?}", status),
            CbReplyError::XidMismatch { expected, actual } => write!(
                f,
                "CB xid mismatch: expected {}, got {}",
                expected, actual
            ),
        }
    }
}

impl std::error::Error for CbReplyError {}

/// Decode a complete RPC reply frame for a CB_COMPOUND CALL we just
/// emitted. `expected_xid` is the same xid passed to
/// [`encode_cb_call`]; mismatches surface as
/// [`CbReplyError::XidMismatch`] so concurrent recall fan-out can't
/// silently consume each other's replies.
pub fn decode_cb_reply(
    bytes: Bytes,
    expected_xid: u32,
) -> Result<CbCompoundReply, CbReplyError> {
    let mut dec = XdrDecoder::new(bytes);

    let xid = dec.decode_u32().map_err(CbReplyError::Xdr)?;
    if xid != expected_xid {
        return Err(CbReplyError::XidMismatch {
            expected: expected_xid,
            actual: xid,
        });
    }

    let msg_type = dec.decode_u32().map_err(CbReplyError::Xdr)?;
    if msg_type != MessageType::Reply as u32 {
        return Err(CbReplyError::RpcRejected {
            reason: format!("expected REPLY, got msg_type={}", msg_type),
        });
    }

    let reply_status = dec.decode_u32().map_err(CbReplyError::Xdr)?;
    if reply_status != ReplyStatus::Accepted as u32 {
        return Err(CbReplyError::RpcRejected {
            reason: format!("reply_status={} (DENIED)", reply_status),
        });
    }

    // Verifier — server's verf, AUTH_NONE empty in our case but we
    // accept whatever the client returned; consume it.
    let _verf_flavor = dec.decode_u32().map_err(CbReplyError::Xdr)?;
    let _verf_body = dec.decode_opaque().map_err(CbReplyError::Xdr)?;

    let accept_status_raw = dec.decode_u32().map_err(CbReplyError::Xdr)?;
    let accept_status = match accept_status_raw {
        0 => AcceptStatus::Success,
        1 => AcceptStatus::ProgUnavail,
        2 => AcceptStatus::ProgMismatch,
        3 => AcceptStatus::ProcUnavail,
        4 => AcceptStatus::GarbageArgs,
        5 => AcceptStatus::SystemErr,
        n => {
            return Err(CbReplyError::RpcRejected {
                reason: format!("unknown accept_status={}", n),
            })
        }
    };
    if !matches!(accept_status, AcceptStatus::Success) {
        // PROG_MISMATCH carries (low, high) version range; we
        // don't use it. Drain the buffer rather than parse it.
        return Err(CbReplyError::Accept {
            status: accept_status,
        });
    }

    // CB_COMPOUND4res body.
    let status = dec.decode_status().map_err(CbReplyError::Xdr)?;
    let tag_bytes = dec.decode_opaque().map_err(CbReplyError::Xdr)?;
    let tag = String::from_utf8(tag_bytes.to_vec()).unwrap_or_default();

    let resarray_len = dec.decode_u32().map_err(CbReplyError::Xdr)? as usize;
    let mut results = Vec::with_capacity(resarray_len);
    for _ in 0..resarray_len {
        results.push(decode_cb_result(&mut dec)?);
    }

    Ok(CbCompoundReply {
        status,
        tag,
        results,
    })
}

fn decode_cb_result(dec: &mut XdrDecoder) -> Result<CbResult, CbReplyError> {
    let opcode = dec.decode_u32().map_err(CbReplyError::Xdr)?;
    let status = dec.decode_status().map_err(CbReplyError::Xdr)?;
    match opcode {
        cb_opcode::CB_SEQUENCE => {
            if status == Nfs4Status::Ok {
                let sessionid = dec.decode_sessionid().map_err(CbReplyError::Xdr)?;
                let sequenceid = dec.decode_u32().map_err(CbReplyError::Xdr)?;
                let slotid = dec.decode_u32().map_err(CbReplyError::Xdr)?;
                let highest_slotid = dec.decode_u32().map_err(CbReplyError::Xdr)?;
                let target_highest_slotid =
                    dec.decode_u32().map_err(CbReplyError::Xdr)?;
                Ok(CbResult::Sequence {
                    status,
                    sessionid,
                    sequenceid,
                    slotid,
                    highest_slotid,
                    target_highest_slotid,
                })
            } else {
                // Error path: union body is void. Surface zeros so
                // callers don't need to special-case.
                Ok(CbResult::Sequence {
                    status,
                    sessionid: SessionId([0; 16]),
                    sequenceid: 0,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                })
            }
        }
        cb_opcode::CB_LAYOUTRECALL => {
            // Body is void on every status — the discriminated union
            // is `switch(nfsstat4) { default: void }`. Nothing more
            // to parse.
            Ok(CbResult::LayoutRecall { status })
        }
        other => Ok(CbResult::OtherStatus {
            opcode: other,
            status,
        }),
    }
}

#[cfg(test)]
mod tests {
    //! Tests treat the encode/decode pair as a black box: build a
    //! request, encode it, then verify the byte stream by *decoding
    //! the RPC framing alongside* and re-decoding the args via a
    //! fresh decoder. For replies, we hand-craft bytes that match
    //! the wire layout of a real Linux v4.1 client's CB reply so
    //! the parser is tested against the shape we'll see in
    //! production, not against itself.

    use super::*;

    fn sample_session_id() -> SessionId {
        SessionId([
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a,
        ])
    }

    fn sample_layout_stateid() -> StateId {
        StateId {
            seqid: 7,
            other: [
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
            ],
        }
    }

    fn sample_recall_call() -> CbCompoundCall {
        CbCompoundCall {
            tag: String::new(),
            minorversion: 1,
            callback_ident: 0,
            ops: vec![
                CbOp::Sequence {
                    sessionid: sample_session_id(),
                    sequenceid: 1,
                    slotid: 0,
                    highest_slotid: 0,
                    cachethis: false,
                },
                CbOp::LayoutRecall {
                    layout_type: 1, // LAYOUT4_NFSV4_1_FILES
                    iomode: 3,      // LAYOUTIOMODE4_ANY
                    changed: true,
                    recall: LayoutRecall::File {
                        fh: vec![0xde, 0xad, 0xbe, 0xef],
                        offset: 0,
                        length: u64::MAX,
                        stateid: sample_layout_stateid(),
                    },
                },
            ],
        }
    }

    /// CB_COMPOUND args: encode → decode round-trips into the same
    /// structure. Catches drift in the per-op wire layout.
    #[test]
    fn cb_compound_args_round_trip() {
        let call = sample_recall_call();
        let bytes = call.encode();

        let mut dec = XdrDecoder::new(bytes);
        let tag = dec.decode_string().unwrap();
        let minorversion = dec.decode_u32().unwrap();
        let callback_ident = dec.decode_u32().unwrap();
        let ops_len = dec.decode_u32().unwrap();
        assert_eq!(tag, "");
        assert_eq!(minorversion, 1);
        assert_eq!(callback_ident, 0);
        assert_eq!(ops_len, 2);

        // Op 1: CB_SEQUENCE.
        assert_eq!(dec.decode_u32().unwrap(), cb_opcode::CB_SEQUENCE);
        assert_eq!(dec.decode_sessionid().unwrap(), sample_session_id());
        assert_eq!(dec.decode_u32().unwrap(), 1); // sequenceid
        assert_eq!(dec.decode_u32().unwrap(), 0); // slotid
        assert_eq!(dec.decode_u32().unwrap(), 0); // highest_slotid
        assert!(!dec.decode_bool().unwrap()); // cachethis
        assert_eq!(dec.decode_u32().unwrap(), 0); // referring_call_lists<>

        // Op 2: CB_LAYOUTRECALL with FILE body.
        assert_eq!(dec.decode_u32().unwrap(), cb_opcode::CB_LAYOUTRECALL);
        assert_eq!(dec.decode_u32().unwrap(), 1); // layout_type
        assert_eq!(dec.decode_u32().unwrap(), 3); // iomode
        assert!(dec.decode_bool().unwrap()); // changed
        assert_eq!(dec.decode_u32().unwrap(), layoutrecall_type::FILE);
        assert_eq!(dec.decode_opaque().unwrap().as_ref(), &[0xde, 0xad, 0xbe, 0xef][..]);
        assert_eq!(dec.decode_u64().unwrap(), 0);
        assert_eq!(dec.decode_u64().unwrap(), u64::MAX);
        assert_eq!(dec.decode_stateid().unwrap(), sample_layout_stateid());
        assert_eq!(dec.remaining(), 0, "no trailing bytes after CB_COMPOUND args");
    }

    /// Full RPC CALL frame: header is well-formed and addressable
    /// to (cb_program, version=1, proc=CB_COMPOUND). This is the
    /// frame the back-channel writer will eventually push.
    #[test]
    fn encode_cb_call_writes_well_formed_rpc_call() {
        let xid = 0xfeed_face;
        let cb_program = 0x40000000;
        let bytes = encode_cb_call(xid, cb_program, &sample_recall_call());

        let mut dec = XdrDecoder::new(bytes);
        assert_eq!(dec.decode_u32().unwrap(), xid);
        assert_eq!(dec.decode_u32().unwrap(), MessageType::Call as u32);
        assert_eq!(dec.decode_u32().unwrap(), 2); // RPC version
        assert_eq!(dec.decode_u32().unwrap(), cb_program);
        assert_eq!(dec.decode_u32().unwrap(), CB_VERSION);
        assert_eq!(dec.decode_u32().unwrap(), cb_procedure::CB_COMPOUND);
        // cred (AUTH_NONE, empty body)
        assert_eq!(dec.decode_u32().unwrap(), AuthFlavor::Null as u32);
        assert_eq!(dec.decode_opaque().unwrap().len(), 0);
        // verf (AUTH_NONE, empty body)
        assert_eq!(dec.decode_u32().unwrap(), AuthFlavor::Null as u32);
        assert_eq!(dec.decode_opaque().unwrap().len(), 0);
        // After the RPC header, the args bytes match what
        // CbCompoundCall::encode() produces standalone — first u32
        // of args is the (empty) tag length.
        let tag_len = dec.decode_u32().unwrap();
        assert_eq!(tag_len, 0);
    }

    /// Successful CB reply: every op returned NFS4_OK. Build the
    /// bytes the client would send back and assert each result
    /// decodes into its typed variant.
    #[test]
    fn decode_cb_reply_success() {
        let xid = 0x1234_5678;
        let bytes = build_synthetic_reply(
            xid,
            ReplyStatus::Accepted as u32,
            AcceptStatus::Success as u32,
            Nfs4Status::Ok,
            &[
                CbResult::Sequence {
                    status: Nfs4Status::Ok,
                    sessionid: sample_session_id(),
                    sequenceid: 1,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 127,
                },
                CbResult::LayoutRecall {
                    status: Nfs4Status::Ok,
                },
            ],
        );

        let reply = decode_cb_reply(bytes, xid).expect("reply decodes");
        assert_eq!(reply.status, Nfs4Status::Ok);
        assert_eq!(reply.tag, "");
        assert_eq!(reply.results.len(), 2);
        match &reply.results[0] {
            CbResult::Sequence {
                status,
                sequenceid,
                target_highest_slotid,
                ..
            } => {
                assert_eq!(*status, Nfs4Status::Ok);
                assert_eq!(*sequenceid, 1);
                assert_eq!(*target_highest_slotid, 127);
            }
            other => panic!("expected Sequence, got {:?}", other),
        }
        assert_eq!(reply.results[1].status(), Nfs4Status::Ok);
    }

    /// Client returned `NFS4ERR_NOMATCHING_LAYOUT` — the call
    /// landed, the CB_SEQUENCE body is present, but the
    /// CB_LAYOUTRECALL response carries an error status with no
    /// body. Decoder must surface the status without trying to
    /// read past the end of the frame.
    #[test]
    fn decode_cb_reply_layoutrecall_no_matching_layout() {
        let xid = 0xaaaa_bbbb;
        let bytes = build_synthetic_reply(
            xid,
            ReplyStatus::Accepted as u32,
            AcceptStatus::Success as u32,
            Nfs4Status::NoMatchingLayout, // top-level status mirrors last op
            &[
                CbResult::Sequence {
                    status: Nfs4Status::Ok,
                    sessionid: sample_session_id(),
                    sequenceid: 5,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 127,
                },
                CbResult::LayoutRecall {
                    status: Nfs4Status::NoMatchingLayout,
                },
            ],
        );

        let reply = decode_cb_reply(bytes, xid).expect("reply decodes");
        assert_eq!(reply.status, Nfs4Status::NoMatchingLayout);
        assert_eq!(reply.results[1].status(), Nfs4Status::NoMatchingLayout);
    }

    /// Mismatched xid — reply parser refuses to confuse one
    /// callback's reply for another's. Important for A.3's
    /// concurrent fan-out.
    #[test]
    fn decode_cb_reply_rejects_xid_mismatch() {
        let bytes = build_synthetic_reply(
            42,
            ReplyStatus::Accepted as u32,
            AcceptStatus::Success as u32,
            Nfs4Status::Ok,
            &[],
        );
        let err = decode_cb_reply(bytes, 99).unwrap_err();
        assert!(matches!(
            err,
            CbReplyError::XidMismatch {
                expected: 99,
                actual: 42
            }
        ));
    }

    /// PROG_UNAVAIL — the client isn't running a callback program
    /// on this connection. Surface as `Accept` so the caller can
    /// drop the writer rather than retry.
    #[test]
    fn decode_cb_reply_prog_unavail_surfaces_accept_error() {
        let bytes = build_synthetic_reply(
            7,
            ReplyStatus::Accepted as u32,
            AcceptStatus::ProgUnavail as u32,
            Nfs4Status::Ok, // ignored — body absent on non-success
            &[],
        );
        let err = decode_cb_reply(bytes, 7).unwrap_err();
        match err {
            CbReplyError::Accept { status } => {
                assert!(matches!(status, AcceptStatus::ProgUnavail));
            }
            other => panic!("expected Accept, got {:?}", other),
        }
    }

    /// Build a synthetic CB reply frame matching the layout the
    /// client emits. Only used by tests; lives next to them so the
    /// "what wire bytes look like" knowledge stays inside this
    /// module.
    fn build_synthetic_reply(
        xid: u32,
        reply_status: u32,
        accept_status: u32,
        cb_status: Nfs4Status,
        results: &[CbResult],
    ) -> Bytes {
        let mut enc = XdrEncoder::new();
        enc.encode_u32(xid);
        enc.encode_u32(MessageType::Reply as u32);
        enc.encode_u32(reply_status);
        // Verifier: AUTH_NONE.
        enc.encode_u32(AuthFlavor::Null as u32);
        enc.encode_opaque(&[]);
        enc.encode_u32(accept_status);
        if accept_status != AcceptStatus::Success as u32 {
            // Non-SUCCESS terminates the frame here. PROG_MISMATCH
            // would have a (low, high) version pair; we don't
            // emit one — the decoder's drain logic ignores any
            // trailing bytes.
            return enc.finish();
        }
        // CB_COMPOUND body.
        enc.encode_u32(cb_status.to_u32());
        enc.encode_string(""); // tag
        enc.encode_u32(results.len() as u32);
        for r in results {
            match r {
                CbResult::Sequence {
                    status,
                    sessionid,
                    sequenceid,
                    slotid,
                    highest_slotid,
                    target_highest_slotid,
                } => {
                    enc.encode_u32(cb_opcode::CB_SEQUENCE);
                    enc.encode_u32(status.to_u32());
                    if *status == Nfs4Status::Ok {
                        enc.encode_sessionid(sessionid);
                        enc.encode_u32(*sequenceid);
                        enc.encode_u32(*slotid);
                        enc.encode_u32(*highest_slotid);
                        enc.encode_u32(*target_highest_slotid);
                    }
                }
                CbResult::LayoutRecall { status } => {
                    enc.encode_u32(cb_opcode::CB_LAYOUTRECALL);
                    enc.encode_u32(status.to_u32());
                    // void union body
                }
                CbResult::OtherStatus { opcode, status } => {
                    enc.encode_u32(*opcode);
                    enc.encode_u32(status.to_u32());
                }
            }
        }
        enc.finish()
    }
}
