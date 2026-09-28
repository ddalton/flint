//! The proxy's wire codec toward the hubs (nfs-proxy design §3), and the
//! splice of a hub's reply into the downstream one.
//!
//! The crate had no NFSv4.1 client: these encode only what the PROXY
//! itself says to a hub (the RPC call header, `SEQUENCE`, `PUTROOTFH`,
//! the session ops that set up a backend client, `GETFH` to learn a
//! hub's root) and decode only those ops' results. Every other op goes
//! as the client's original bytes, and every other result comes back
//! opaque: the proxy never decodes a READ, WRITE, GETATTR or READDIR.

use bytes::Bytes;

use crate::nfs::rpc::Auth;
use crate::nfs::v4::compound::{ChannelAttrs, CompoundResponse, OperationResult, SequenceResult};
use crate::nfs::v4::protocol::{opcode, Nfs4FileHandle, Nfs4Status, SessionId};
use crate::nfs::v4::xdr::{Nfs4XdrDecoder, Nfs4XdrEncoder};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};

pub const NFS_PROGRAM: u32 = 100003;
pub const NFS_V4: u32 = 4;
pub const PROC_COMPOUND: u32 = 1;

/// An ONC-RPC CALL for NFSv4 COMPOUND, `cred` as the client presented it
/// (§7: the proxy copies the caller's AUTH_SYS into each backend call;
/// RPCSEC_GSS cannot pass through and is refused before this).
pub fn encode_call(xid: u32, cred: &Auth, compound: &[u8]) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_u32(xid);
    e.encode_u32(0); // CALL
    e.encode_u32(2); // RPC version
    e.encode_u32(NFS_PROGRAM);
    e.encode_u32(NFS_V4);
    e.encode_u32(PROC_COMPOUND);
    cred.encode(&mut e);
    Auth::null().encode(&mut e); // verifier: AUTH_NONE
    e.append_raw(compound);
    e.finish().to_vec()
}

/// `SEQUENCE4args`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqArgs {
    pub sessionid: SessionId,
    pub sequenceid: u32,
    pub slotid: u32,
    pub highest_slotid: u32,
    pub cachethis: bool,
}

pub fn op_sequence(a: &SeqArgs) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_u32(opcode::SEQUENCE);
    e.encode_sessionid(&a.sessionid);
    e.encode_u32(a.sequenceid);
    e.encode_u32(a.slotid);
    e.encode_u32(a.highest_slotid);
    e.encode_bool(a.cachethis);
    e.finish().to_vec()
}

pub fn op_putrootfh() -> Vec<u8> {
    opcode::PUTROOTFH.to_be_bytes().to_vec()
}

pub fn op_getfh() -> Vec<u8> {
    opcode::GETFH.to_be_bytes().to_vec()
}

/// `EXCHANGE_ID4args`, SP4_NONE, no impl id. `flags` as the downstream
/// client sent them (§4: the backend client mirrors it).
pub fn op_exchange_id(verifier: [u8; 8], owner: &[u8], flags: u32) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_u32(opcode::EXCHANGE_ID);
    e.encode_verifier(&verifier);
    e.encode_opaque(owner);
    e.encode_u32(flags);
    e.encode_u32(0); // SP4_NONE
    e.encode_u32(0); // eia_client_impl_id<1>: none
    e.finish().to_vec()
}

fn encode_channel_attrs(e: &mut XdrEncoder, a: &ChannelAttrs) {
    e.encode_u32(a.header_pad_size);
    e.encode_u32(a.max_request_size);
    e.encode_u32(a.max_response_size);
    e.encode_u32(a.max_response_size_cached);
    e.encode_u32(a.max_operations);
    e.encode_u32(a.max_requests);
    e.encode_u32(0); // ca_rdma_ird<1>: TCP
}

/// `CREATE_SESSION4args`: no back channel is asked for (v1 relays no
/// callbacks, §2), AUTH_NONE callback security.
pub fn op_create_session(clientid: u64, sequence: u32, fore: &ChannelAttrs) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_u32(opcode::CREATE_SESSION);
    e.encode_u64(clientid);
    e.encode_u32(sequence);
    e.encode_u32(0); // csa_flags: no persist, no back channel
    encode_channel_attrs(&mut e, fore);
    encode_channel_attrs(&mut e, &ChannelAttrs { max_requests: 1, ..ChannelAttrs::default() });
    e.encode_u32(0x4000_0000); // csa_cb_program (unused: no back channel)
    e.encode_u32(1); // csa_sec_parms<>: one entry,
    e.encode_u32(0); // AUTH_NONE
    e.finish().to_vec()
}

pub fn op_reclaim_complete(one_fs: bool) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_u32(opcode::RECLAIM_COMPLETE);
    e.encode_bool(one_fs);
    e.finish().to_vec()
}

pub fn op_destroy_session(sid: &SessionId) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_u32(opcode::DESTROY_SESSION);
    e.encode_sessionid(sid);
    e.finish().to_vec()
}

pub fn op_destroy_clientid(clientid: u64) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_u32(opcode::DESTROY_CLIENTID);
    e.encode_u64(clientid);
    e.finish().to_vec()
}

/// `COMPOUND4args` from pre-encoded ops, in order.
pub fn encode_compound(tag: &[u8], minor_version: u32, ops: &[&[u8]]) -> Vec<u8> {
    let mut e = XdrEncoder::new();
    e.encode_opaque(tag);
    e.encode_u32(minor_version);
    e.encode_u32(ops.len() as u32);
    for op in ops {
        e.append_raw(op);
    }
    e.finish().to_vec()
}

/// Strip the RPC reply header; the COMPOUND4res body on success. Any
/// other outcome (denied, a non-SUCCESS accept_stat) is an error: the
/// hub refused the CALL itself, not an op.
pub fn decode_reply(record: Bytes, xid: u32) -> Result<Bytes, String> {
    let mut d = XdrDecoder::new(record);
    let got = d.decode_u32()?;
    if got != xid {
        return Err(format!("reply xid {got:#x} != {xid:#x}"));
    }
    if d.decode_u32()? != 1 {
        return Err("not a REPLY".into());
    }
    match d.decode_u32()? {
        0 => {}
        1 => return Err("MSG_DENIED".into()),
        s => return Err(format!("bad reply_stat {s}")),
    }
    let _verf = Auth::decode(&mut d)?;
    match d.decode_u32()? {
        0 => Ok(d.into_remaining_bytes()),
        s => Err(format!("accept_stat {s}")),
    }
}

fn expect_op(d: &mut XdrDecoder, op: u32) -> Result<Nfs4Status, String> {
    let got = d.decode_u32()?;
    if got != op {
        return Err(format!("result opcode {got} where {op} was expected"));
    }
    d.decode_status()
}

/// COMPOUND4res header: (raw status, numres). The tag is skipped.
fn compound_header(d: &mut XdrDecoder) -> Result<(u32, u32), String> {
    let status = d.decode_u32()?;
    let _tag = d.decode_opaque()?;
    let numres = d.decode_u32()?;
    Ok((status, numres))
}

pub fn decode_sequence_res(d: &mut XdrDecoder) -> Result<(Nfs4Status, Option<SequenceResult>), String> {
    let status = expect_op(d, opcode::SEQUENCE)?;
    if status != Nfs4Status::Ok {
        return Ok((status, None));
    }
    Ok((
        status,
        Some(SequenceResult {
            sessionid: d.decode_sessionid()?,
            sequenceid: d.decode_u32()?,
            slotid: d.decode_u32()?,
            highest_slotid: d.decode_u32()?,
            target_highest_slotid: d.decode_u32()?,
            status_flags: d.decode_u32()?,
        }),
    ))
}

/// The parts of `EXCHANGE_ID4resok` the proxy uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeIdOk {
    pub clientid: u64,
    pub sequenceid: u32,
    pub flags: u32,
}

/// A whole session-setup reply (`EXCHANGE_ID` alone): the op's status,
/// and its result when OK.
pub fn decode_exchange_id_reply(body: Bytes) -> Result<(Nfs4Status, Option<ExchangeIdOk>), String> {
    let mut d = XdrDecoder::new(body);
    let (_, numres) = compound_header(&mut d)?;
    if numres == 0 {
        return Err("EXCHANGE_ID: no result".into());
    }
    let status = expect_op(&mut d, opcode::EXCHANGE_ID)?;
    if status != Nfs4Status::Ok {
        return Ok((status, None));
    }
    let ok = ExchangeIdOk { clientid: d.decode_u64()?, sequenceid: d.decode_u32()?, flags: d.decode_u32()? };
    Ok((status, Some(ok)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSessionOk {
    pub sessionid: SessionId,
    pub sequenceid: u32,
    /// The hub's fore channel: `max_requests` bounds the backend slots.
    pub fore_max_requests: u32,
    pub fore_max_response_size_cached: u32,
}

pub fn decode_create_session_reply(body: Bytes) -> Result<(Nfs4Status, Option<CreateSessionOk>), String> {
    let mut d = XdrDecoder::new(body);
    let (_, numres) = compound_header(&mut d)?;
    if numres == 0 {
        return Err("CREATE_SESSION: no result".into());
    }
    let status = expect_op(&mut d, opcode::CREATE_SESSION)?;
    if status != Nfs4Status::Ok {
        return Ok((status, None));
    }
    let sessionid = d.decode_sessionid()?;
    let sequenceid = d.decode_u32()?;
    let _flags = d.decode_u32()?;
    let fore = ChannelAttrs::decode(&mut d)?;
    Ok((
        status,
        Some(CreateSessionOk {
            sessionid,
            sequenceid,
            fore_max_requests: fore.max_requests,
            fore_max_response_size_cached: fore.max_response_size_cached,
        }),
    ))
}

/// A backend `SEQUENCE, PUTROOTFH, GETFH` reply: the hub's root handle.
pub fn decode_root_fh_reply(body: Bytes) -> Result<Result<Nfs4FileHandle, Nfs4Status>, String> {
    let mut d = XdrDecoder::new(body);
    let (_, numres) = compound_header(&mut d)?;
    let (s, _) = decode_sequence_res(&mut d)?;
    if s != Nfs4Status::Ok || numres < 2 {
        return Ok(Err(s));
    }
    let s = expect_op(&mut d, opcode::PUTROOTFH)?;
    if s != Nfs4Status::Ok || numres < 3 {
        return Ok(Err(s));
    }
    let s = expect_op(&mut d, opcode::GETFH)?;
    if s != Nfs4Status::Ok {
        return Ok(Err(s));
    }
    Ok(Ok(d.decode_filehandle()?))
}

/// A hub's reply to a forwarded compound, taken apart just enough to
/// splice: its `SEQUENCE` result, the synthesised `PUTROOTFH`'s status,
/// and everything after them, opaque.
#[derive(Debug, Clone)]
pub struct HubReply {
    /// The hub's COMPOUND status, raw (a code this crate does not name
    /// must still round-trip).
    pub status: u32,
    pub seq: (Nfs4Status, Option<SequenceResult>),
    /// `Some` iff a `PUTROOTFH` was synthesised and the hub answered it.
    pub putrootfh: Option<Nfs4Status>,
    /// The results of the client's own ops, as the hub encoded them.
    pub tail: Bytes,
    pub tail_count: u32,
}

pub fn parse_hub_reply(body: Bytes, putrootfh: bool) -> Result<HubReply, String> {
    let mut d = XdrDecoder::new(body);
    let (status, numres) = compound_header(&mut d)?;
    if numres == 0 {
        return Err("hub reply has no SEQUENCE result".into());
    }
    let seq = decode_sequence_res(&mut d)?;
    let mut used = 1;
    let mut root = None;
    if putrootfh && numres > 1 {
        root = Some(expect_op(&mut d, opcode::PUTROOTFH)?);
        used += 1;
    }
    Ok(HubReply { status, seq, putrootfh: root, tail: d.into_remaining_bytes(), tail_count: numres - used })
}

/// The downstream `COMPOUND4res`, assembled in op order from the proxy's
/// own results and at most one opaque run of a hub's.
pub struct Splice {
    body: Vec<u8>,
    count: u32,
    /// The last result's status, raw: the compound's status (RFC 8881
    /// §16.2.3: the status of the last op executed).
    last: u32,
}

impl Splice {
    pub fn new() -> Self {
        Splice { body: Vec::new(), count: 0, last: 0 }
    }

    /// One of the proxy's own results.
    pub fn push(&mut self, r: OperationResult) {
        let status = r.status() as u32;
        let mut e = XdrEncoder::new();
        CompoundResponse::encode_result(&mut e, r);
        self.body.extend_from_slice(&e.finish());
        self.count += 1;
        self.last = status;
    }

    /// The hub's results for the client's ops, verbatim.
    pub fn push_hub(&mut self, h: &HubReply) {
        if h.tail_count == 0 {
            return;
        }
        self.body.extend_from_slice(&h.tail);
        self.count += h.tail_count;
        self.last = h.status;
    }

    /// The compound stopped (an op failed): nothing more may be pushed.
    pub fn stopped(&self) -> bool {
        self.last != 0
    }

    pub fn finish(self, tag: &[u8]) -> Vec<u8> {
        let mut e = XdrEncoder::new();
        e.encode_u32(self.last);
        e.encode_opaque(tag);
        e.encode_u32(self.count);
        e.append_raw(&self.body);
        e.finish().to_vec()
    }
}

impl Default for Splice {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::rpc::CallMessage;
    use crate::nfs::v4::compound::{CompoundRequest, ExchangeIdResult, CreateSessionResult, Operation};

    fn sid(b: u8) -> SessionId {
        SessionId([b; 16])
    }

    /// The hub's OWN decoder reads what the proxy encodes: every op the
    /// proxy originates, in one compound, inside a real RPC call.
    #[test]
    fn the_hubs_decoder_reads_every_op_the_proxy_originates() {
        let seq = SeqArgs { sessionid: sid(7), sequenceid: 9, slotid: 2, highest_slotid: 3, cachethis: true };
        let fore = ChannelAttrs { max_requests: 64, ..ChannelAttrs::default() };
        let ops = [
            op_sequence(&seq),
            op_putrootfh(),
            op_getfh(),
            op_exchange_id(*b"verifier", b"flint-proxy/linux-node-1", 0x3),
            op_create_session(0xC1, 1, &fore),
            op_reclaim_complete(false),
            op_destroy_session(&sid(8)),
            op_destroy_clientid(0xC1),
        ];
        let refs: Vec<&[u8]> = ops.iter().map(|o| o.as_slice()).collect();
        let body = encode_compound(b"proxy", 1, &refs);
        let cred = Auth { flavor: crate::nfs::rpc::AuthFlavor::Unix, body: Bytes::from_static(&[0; 20]) };
        let call = encode_call(0x1234, &cred, &body);

        let (msg, args) = CallMessage::decode_with_args(Bytes::from(call)).unwrap();
        assert_eq!((msg.xid, msg.program, msg.version, msg.procedure), (0x1234, NFS_PROGRAM, 4, 1));
        assert_eq!(msg.cred.body, cred.body);
        let req = CompoundRequest::decode(XdrDecoder::new(args)).unwrap();
        assert_eq!((req.tag.as_str(), req.minor_version), ("proxy", 1));
        let o = &req.operations;
        assert_eq!(o.len(), 8, "{o:?}");
        assert!(matches!(&o[0], Operation::Sequence { sessionid, sequenceid: 9, slotid: 2, highest_slotid: 3, cachethis: true } if *sessionid == sid(7)));
        assert!(matches!(o[1], Operation::PutRootFh));
        assert!(matches!(o[2], Operation::GetFh));
        match &o[3] {
            Operation::ExchangeId { clientowner, flags: 3, state_protect: 0, .. } => {
                assert_eq!(clientowner.verifier, u64::from_be_bytes(*b"verifier"));
                assert_eq!(clientowner.id, b"flint-proxy/linux-node-1");
            }
            x => panic!("{x:?}"),
        }
        match &o[4] {
            Operation::CreateSession { clientid: 0xC1, sequence: 1, fore_chan_attrs, back_chan_attrs, .. } => {
                assert_eq!(fore_chan_attrs.max_requests, 64);
                assert_eq!(back_chan_attrs.max_requests, 1);
            }
            x => panic!("{x:?}"),
        }
        assert!(matches!(o[5], Operation::ReclaimComplete(false)));
        assert!(matches!(&o[6], Operation::DestroySession(s) if *s == sid(8)));
        assert!(matches!(o[7], Operation::DestroyClientId(0xC1)));
    }

    fn hub_body(results: Vec<OperationResult>) -> Bytes {
        let status = results.last().map(|r| r.status()).unwrap_or(Nfs4Status::Ok);
        CompoundResponse { status, tag: "t".into(), results, raw_reply: None, cache_slot: None }.encode()
    }

    fn seq_ok(flags: u32) -> OperationResult {
        OperationResult::Sequence(
            Nfs4Status::Ok,
            Some(SequenceResult {
                sessionid: sid(1),
                sequenceid: 5,
                slotid: 0,
                highest_slotid: 0,
                target_highest_slotid: 7,
                status_flags: flags,
            }),
        )
    }

    /// And the proxy reads what the hub's OWN encoder writes.
    #[test]
    fn the_proxy_reads_the_hubs_session_setup_replies() {
        let b = hub_body(vec![OperationResult::ExchangeId(
            Nfs4Status::Ok,
            Some(ExchangeIdResult { clientid: 0xAB, sequenceid: 1, flags: 0x10003, server_owner: "hub".into(), server_scope: b"s".to_vec() }),
        )]);
        assert_eq!(decode_exchange_id_reply(b).unwrap(), (Nfs4Status::Ok, Some(ExchangeIdOk { clientid: 0xAB, sequenceid: 1, flags: 0x10003 })));

        let fore = ChannelAttrs { max_requests: 32, max_response_size_cached: 4096, ..ChannelAttrs::default() };
        let b = hub_body(vec![OperationResult::CreateSession(
            Nfs4Status::Ok,
            Some(CreateSessionResult { sessionid: sid(4), sequenceid: 1, flags: 0, fore_chan_attrs: fore, back_chan_attrs: ChannelAttrs::default() }),
        )]);
        let (s, ok) = decode_create_session_reply(b).unwrap();
        assert_eq!(s, Nfs4Status::Ok);
        assert_eq!(ok.unwrap(), CreateSessionOk { sessionid: sid(4), sequenceid: 1, fore_max_requests: 32, fore_max_response_size_cached: 4096 });

        let b = hub_body(vec![OperationResult::CreateSession(Nfs4Status::StaleClientId, None)]);
        assert_eq!(decode_create_session_reply(b).unwrap(), (Nfs4Status::StaleClientId, None));

        let root = Nfs4FileHandle { data: vec![3, 1, 2, 3] };
        let b = hub_body(vec![seq_ok(0), OperationResult::PutRootFh(Nfs4Status::Ok), OperationResult::GetFh(Nfs4Status::Ok, Some(root.clone()))]);
        assert_eq!(decode_root_fh_reply(b).unwrap().unwrap().data, root.data);
    }

    #[test]
    fn decode_reply_takes_the_header_off_and_refuses_what_is_not_success() {
        let mut ok = vec![];
        for w in [0x55u32, 1, 0, 0, 0, 0] {
            ok.extend_from_slice(&w.to_be_bytes());
        }
        ok.extend_from_slice(b"body");
        assert_eq!(&decode_reply(Bytes::from(ok.clone()), 0x55).unwrap()[..], b"body");
        assert!(decode_reply(Bytes::from(ok.clone()), 0x56).is_err(), "xid mismatch");
        let mut garbage = ok.clone();
        garbage[20..24].copy_from_slice(&4u32.to_be_bytes()); // GARBAGE_ARGS
        assert!(decode_reply(Bytes::from(garbage), 0x55).is_err());
    }

    /// The crossing, end to end on the wire: the client sent `SEQUENCE,
    /// PUTFH(/), LOOKUP ws, GETFH, GETATTR`; the hub ran `SEQUENCE,
    /// PUTROOTFH, GETFH, GETATTR`. The client must see FIVE results in its
    /// own op order, the hub's GETFH/GETATTR bytes untouched.
    #[test]
    fn the_splice_gives_the_client_its_own_op_order_with_the_hubs_bytes_verbatim() {
        let fh = Nfs4FileHandle { data: vec![3; 50] };
        let hub = hub_body(vec![
            seq_ok(0),
            OperationResult::PutRootFh(Nfs4Status::Ok),
            OperationResult::GetFh(Nfs4Status::Ok, Some(fh.clone())),
            OperationResult::PutRootFh(Nfs4Status::Ok), // stands in for GETATTR: opaque either way
        ]);
        let h = parse_hub_reply(hub, true).unwrap();
        assert_eq!(h.putrootfh, Some(Nfs4Status::Ok));
        assert_eq!(h.tail_count, 2);

        let mut s = Splice::new();
        s.push(seq_ok(0));
        s.push(OperationResult::PutFh(Nfs4Status::Ok));
        s.push(OperationResult::Lookup(h.putrootfh.unwrap()));
        s.push_hub(&h);
        let out = s.finish(b"client-tag");

        let expect = CompoundResponse {
            status: Nfs4Status::Ok,
            tag: "client-tag".into(),
            results: vec![
                seq_ok(0),
                OperationResult::PutFh(Nfs4Status::Ok),
                OperationResult::Lookup(Nfs4Status::Ok),
                OperationResult::GetFh(Nfs4Status::Ok, Some(fh)),
                OperationResult::PutRootFh(Nfs4Status::Ok),
            ],
            raw_reply: None,
            cache_slot: None,
        }
        .encode();
        assert_eq!(out, expect.to_vec());
    }

    #[test]
    fn a_hub_failure_ends_the_downstream_compound_with_the_hubs_status() {
        let hub = hub_body(vec![seq_ok(0), OperationResult::PutFh(Nfs4Status::Ok), OperationResult::Lookup(Nfs4Status::NoEnt)]);
        let h = parse_hub_reply(hub, false).unwrap();
        assert_eq!(h.putrootfh, None);
        let mut s = Splice::new();
        s.push(seq_ok(0));
        s.push_hub(&h);
        assert!(s.stopped());
        let out = s.finish(b"t");
        assert_eq!(&out[0..4], &(Nfs4Status::NoEnt as u32).to_be_bytes());
        let mut d = XdrDecoder::new(Bytes::from(out));
        let (_, numres) = compound_header(&mut d).unwrap();
        assert_eq!(numres, 3);
    }

    #[test]
    fn a_failed_synthesised_putrootfh_leaves_no_tail() {
        let hub = hub_body(vec![seq_ok(0), OperationResult::PutRootFh(Nfs4Status::Delay)]);
        let h = parse_hub_reply(hub, true).unwrap();
        assert_eq!((h.putrootfh, h.tail_count), (Some(Nfs4Status::Delay), 0));
    }
}
