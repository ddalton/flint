//! The proxy server (nfs-proxy design §3, §4): terminate the client's
//! session, route each COMPOUND, answer the pseudo-root, forward the
//! rest to one hub, splice the reply.
//!
//! Downstream sessions are the crate's own: a [`CompoundDispatcher`]
//! over an EMPTY export and a SQLite state backend answers `EXCHANGE_ID`,
//! `CREATE_SESSION`, `DESTROY_*`, `BIND_CONN_TO_SESSION`,
//! `RECLAIM_COMPLETE`, and validates every `SEQUENCE` (slot, seqid,
//! lease renewal). It never sees a client's other ops: the proxy hands
//! it the `SEQUENCE` alone and routes the rest.
//!
//! Exactly-once (§4 "Slots and exactly-once"): the proxy stores no reply
//! bodies. Each downstream slot remembers `(seqid, hub, backend seqid)`;
//! the dispatcher reports a retransmission as `RETRY_UNCACHED_REP` (it
//! cached nothing), and the proxy re-sends it to the SAME hub with the
//! SAME backend `(slot, seqid)`, so the hub's reply cache answers it.
//! A proxy-only compound is idempotent and is recomputed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use super::backend::{BackendClient, BackendError, BackendSession, HubConn};
use super::pseudo::{self, LocalCtx};
use super::route::{self, Disp};
use super::table::{HubRow, IdentityRule, Table, View};
use super::wire::{self, HubReply, Splice};
use crate::nfs::ingress::{NextRecord, RecordReader};
use crate::nfs::rpc::{AuthFlavor, AuthStat, CallMessage, ReplyBuilder};
use crate::nfs::v4::compound::{ChannelAttrs, CompoundRequest, Operation, OperationResult, SequenceResult};
use crate::nfs::v4::dispatcher::CompoundDispatcher;
use crate::nfs::v4::filehandle::FileHandleManager;
use crate::nfs::v4::operations::lockops::LockManager;
use crate::nfs::v4::protocol::{opcode, Nfs4Status, SessionId};
use crate::nfs::v4::state::StateManager;
use crate::nfs::xdr::XdrDecoder;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    pub listen: String,
    /// The persisted client table (`proxy.db`) and the empty export the
    /// session dispatcher is built over.
    pub state_dir: PathBuf,
    #[serde(default)]
    pub hubs: Vec<HubRow>,
    #[serde(default)]
    pub identities: Vec<IdentityRule>,
}

/// The revocation bits a hub's `sr_status_flags` may carry through to the
/// client (§4 Leases: a revocation must not be dropped on the way). NEVER
/// `SEQ4_STATUS_ADMIN_STATE_REVOKED` (0x20): Linux treats it client-wide,
/// and behind the proxy the client is every workspace a node mounts
/// (census Part 3). Channel bits (CB_PATH_DOWN, ...) describe the
/// proxy's backend session, not the client's.
const HUB_FLAGS_PASSED: u32 = 0x08 | 0x10 | 0x40;

/// What a downstream slot last carried.
#[derive(Debug, Clone, Copy)]
struct DownSlot {
    seq: u32,
    hub: Option<u64>,
    bseq: u32,
}

pub struct Proxy {
    table: Arc<Table>,
    disp: Arc<CompoundDispatcher>,
    state: Arc<StateManager>,
    conns: Mutex<HashMap<u64, Arc<HubConn>>>,
    clients: Mutex<HashMap<(u64, u64), Arc<BackendClient>>>,
    sessions: Mutex<HashMap<([u8; 16], u64), Arc<BackendSession>>>,
    /// Serialises the slow path that CREATES backend clients/sessions.
    setup: tokio::sync::Mutex<()>,
    slots: Mutex<HashMap<([u8; 16], u32), DownSlot>>,
    /// §3: compounds refused because they needed a second target. The
    /// first drill must show this at zero for the Linux client.
    pub second_target: AtomicU64,
}

fn opcode_at(args: &Bytes, r: &std::ops::Range<usize>) -> u32 {
    args.get(r.start..r.start + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).unwrap_or(0)
}

/// The client's tag, raw (echoed byte-for-byte, RFC 8881 §16.2.3).
fn raw_tag(args: &Bytes) -> Bytes {
    let mut d = XdrDecoder::new(args.clone());
    d.decode_opaque().unwrap_or_default()
}

/// The error result for an op the proxy refuses. Every result's error arm
/// is `opcode, status` except SETATTR's, which also carries `attrsset`.
fn refusal(opcode: u32, status: Nfs4Status) -> OperationResult {
    match opcode {
        opcode::SETATTR => OperationResult::SetAttr(status, vec![]),
        opcode::LOOKUP => OperationResult::Lookup(status),
        _ => OperationResult::Unsupported { opcode, status },
    }
}

enum Piece {
    Res(OperationResult),
    Hub(HubReply),
}

impl Proxy {
    pub async fn new(cfg: &ProxyConfig) -> Result<Arc<Self>, String> {
        let root = cfg.state_dir.join("empty-root");
        std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        let db = cfg.state_dir.join("proxy.db");
        let backend: Arc<dyn crate::state_backend::StateBackend> = Arc::new(
            crate::state_backend::SqliteBackend::open(&db).map_err(|e| format!("{}: {e}", db.display()))?,
        );
        let state = Arc::new(StateManager::new("flint-nfs-proxy", backend.clone()));
        state.load_from_backend(false).await.map_err(|e| format!("load {}: {e}", db.display()))?;
        let fh = Arc::new(FileHandleManager::new(root));
        let locks = Arc::new(LockManager::new());
        let disp = Arc::new(CompoundDispatcher::new(fh, state.clone(), locks));
        let table = Table::new(cfg.hubs.clone(), cfg.identities.clone())?;
        Ok(Arc::new(Proxy {
            table,
            disp,
            state,
            conns: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            setup: tokio::sync::Mutex::new(()),
            slots: Mutex::new(HashMap::new()),
            second_target: AtomicU64::new(0),
        }))
    }

    pub fn table(&self) -> &Arc<Table> {
        &self.table
    }

    pub async fn serve(self: Arc<Self>, listen: &str) -> std::io::Result<()> {
        let l = TcpListener::bind(listen).await?;
        info!("flint-nfs-proxy listening on {}", l.local_addr()?);
        self.serve_on(l).await
    }

    pub async fn serve_on(self: Arc<Self>, l: TcpListener) -> std::io::Result<()> {
        loop {
            let (s, peer) = l.accept().await?;
            let p = self.clone();
            tokio::spawn(async move { p.connection(s, peer).await });
        }
    }

    async fn connection(self: Arc<Self>, s: TcpStream, peer: SocketAddr) {
        let _ = s.set_nodelay(true);
        let (mut rd, wr) = s.into_split();
        let wr = Arc::new(tokio::sync::Mutex::new(wr));
        let mut rr = RecordReader::new(format!("client {peer}"));
        debug!("client {peer} connected");
        loop {
            let rec = match rr.next(&mut rd, None).await {
                Ok(NextRecord::Record(r)) => r,
                Ok(_) => break,
                Err(e) => {
                    debug!("client {peer}: {e}");
                    break;
                }
            };
            let p = self.clone();
            let wr = wr.clone();
            // Requests on one connection run concurrently: the client
            // pipelines across slots, and one slow hub must not stall the
            // replies from another.
            tokio::spawn(async move {
                if let Some(reply) = p.call(rec, peer).await {
                    write_record(&wr, &reply).await;
                }
            });
        }
        debug!("client {peer} gone");
    }

    async fn call(&self, rec: Bytes, peer: SocketAddr) -> Option<Bytes> {
        let (call, args) = match CallMessage::decode_with_args(rec) {
            Ok(x) => x,
            Err(e) => {
                debug!("client {peer}: undecodable call: {e}");
                return None;
            }
        };
        if call.program != wire::NFS_PROGRAM {
            return Some(ReplyBuilder::prog_unavail(call.xid));
        }
        if call.version != wire::NFS_V4 {
            // PROG_MISMATCH carries the supported range: 4..4.
            let mut e = crate::nfs::xdr::XdrEncoder::new();
            for w in [call.xid, 1, 0, 0, 0, 2, wire::NFS_V4, wire::NFS_V4] {
                e.encode_u32(w);
            }
            return Some(e.finish());
        }
        if matches!(call.cred.flavor, AuthFlavor::RpcsecGss) {
            // §7: GSS is bound to the server principal and cannot pass
            // through; krb5 at the proxy is v2.
            return Some(ReplyBuilder::auth_error(call.xid, AuthStat::TooWeak));
        }
        match call.procedure {
            0 => Some(ReplyBuilder::success(call.xid).finish()),
            1 => {
                let view = self.table.view(peer.ip());
                let body = match self.compound(&view, &call, args).await {
                    Ok(b) => b,
                    Err(()) => return Some(ReplyBuilder::garbage_args(call.xid)),
                };
                let mut rb = ReplyBuilder::success(call.xid);
                rb.encoder().append_raw(&body);
                Some(rb.finish())
            }
            _ => Some(ReplyBuilder::proc_unavail(call.xid)),
        }
    }

    /// Hand a whole compound to the session dispatcher (session-only
    /// compounds, and every malformed shape: it answers them exactly as a
    /// hub would).
    async fn to_dispatcher(&self, call: &CallMessage, req: CompoundRequest) -> Bytes {
        let resp = self
            .disp
            .dispatch_compound_with_cred(req, call.cred.principal(), call.cred.unix_uid_gid(), call.cred.unix_gids(), None, false)
            .await;
        let slot = resp.cache_slot;
        let bytes = resp.encode();
        if let Some((sid, slot)) = slot {
            self.disp.cache_slot_reply(&sid, slot, bytes.clone());
        }
        bytes
    }

    async fn compound(&self, view: &View, call: &CallMessage, args: Bytes) -> Result<Bytes, ()> {
        let (req, ranges) = CompoundRequest::decode_with_ranges(XdrDecoder::new(args.clone())).map_err(|_| ())?;
        let first_is_seq = matches!(req.operations.first(), Some(Operation::Sequence { .. }));
        if !first_is_seq || !(1..=2).contains(&req.minor_version) || !req.tag_valid {
            let ops: Vec<u32> = ranges.iter().map(|r| opcode_at(&args, r)).collect();
            let body = self.to_dispatcher(call, req).await;
            self.after_session_ops(&ops, &body);
            return Ok(body);
        }
        let plan = route::route(view, &req.operations[1..]);
        if plan.second_target {
            let n = self.second_target.fetch_add(1, Ordering::Relaxed) + 1;
            warn!("compound needs a second target (count {n}): {:?}", plan.disp);
        }
        // A compound of session ops after SEQUENCE (RECLAIM_COMPLETE,
        // DESTROY_SESSION, ...) is the dispatcher's, whole.
        if !plan.disp.is_empty() && plan.disp.iter().all(|d| *d == Disp::Session) {
            return Ok(self.to_dispatcher(call, req).await);
        }
        let tag = raw_tag(&args);
        let minor = req.minor_version;
        let mut ops = req.operations;
        let seq_op = ops.remove(0);
        let Operation::Sequence { sessionid, sequenceid, slotid, cachethis, .. } = seq_op else { unreachable!() };
        let seq_only = CompoundRequest { tag: String::new(), tag_valid: true, minor_version: minor, operations: vec![seq_op], wire_size: 0 };
        let seq_body = self
            .disp
            .dispatch_compound_with_cred(seq_only, call.cred.principal(), call.cred.unix_uid_gid(), call.cred.unix_gids(), None, false)
            .await
            .encode();
        let mut d = XdrDecoder::new(seq_body.clone());
        let (_, _, _) = (d.decode_u32(), d.decode_opaque(), d.decode_u32());
        let (st, seq_res) = wire::decode_sequence_res(&mut d).map_err(|_| ())?;

        // New request, or a retransmission of one the proxy remembers.
        let key = (sessionid.0, slotid);
        let (mut seq_res, resend) = match (st, seq_res) {
            (Nfs4Status::Ok, Some(r)) => (r, None),
            (Nfs4Status::RetryUncachedRep, _) => {
                let last = self.slots.lock().unwrap().get(&key).copied();
                match last {
                    Some(l) if l.seq == sequenceid => {
                        let max = self.state.sessions.session_limits(&sessionid).map(|s| s.fore_chan_maxrequests).unwrap_or(1);
                        let r = SequenceResult {
                            sessionid,
                            sequenceid,
                            slotid,
                            highest_slotid: max.saturating_sub(1),
                            target_highest_slotid: max.saturating_sub(1),
                            status_flags: 0,
                        };
                        (r, Some(l))
                    }
                    // Nothing to replay against: the dispatcher's answer.
                    _ => return Ok(rewrap(&seq_body, &tag)),
                }
            }
            // BADSESSION, SEQ_MISORDERED, ...: the dispatcher's answer.
            _ => return Ok(rewrap(&seq_body, &tag)),
        };

        let mut pieces: Vec<Piece> = Vec::new();
        let mut local = LocalCtx::default();
        let mut stopped = false;
        let mut i = 0;
        let mut used_hub: Option<(u64, u32)> = None;
        while i < plan.disp.len() && !stopped {
            let op = &ops[i];
            let r = &ranges[i + 1];
            match &plan.disp[i] {
                Disp::Local => {
                    let res = pseudo::answer(view, &mut local, op, opcode_at(&args, r));
                    stopped = res.status() != Nfs4Status::Ok;
                    pieces.push(Piece::Res(res));
                    i += 1;
                }
                Disp::Session => {
                    // Mixed with routed ops: not a shape a client sends.
                    self.second_target.fetch_add(1, Ordering::Relaxed);
                    pieces.push(Piece::Res(refusal(opcode_at(&args, r), Nfs4Status::ServerFault)));
                    stopped = true;
                }
                Disp::Refuse(s) => {
                    pieces.push(Piece::Res(refusal(opcode_at(&args, r), *s)));
                    stopped = true;
                }
                Disp::Cross | Disp::Forward => {
                    // The whole forwarded run: [Cross] Forward*.
                    let cross = plan.disp[i] == Disp::Cross;
                    let start = if cross { i + 1 } else { i };
                    let mut end = start;
                    while end < plan.disp.len() && plan.disp[end] == Disp::Forward {
                        end += 1;
                    }
                    let hub = plan.hub.expect("a forwarded op has a hub");
                    let slices: Vec<&[u8]> = (start..end).map(|k| &args[ranges[k + 1].clone()]).collect();
                    let first_opcode = opcode_at(&args, &ranges[i + 1]);
                    let bseq = resend.filter(|l| l.hub == Some(hub)).map(|l| l.bseq);
                    match self
                        .forward(view, sessionid, hub, slotid, bseq, cachethis, call, &tag, minor, plan.putrootfh, &slices)
                        .await
                    {
                        Ok((bseq, reply)) => {
                            used_hub = Some((hub, bseq));
                            seq_res.status_flags |= reply.seq.1.as_ref().map(|s| s.status_flags).unwrap_or(0) & HUB_FLAGS_PASSED;
                            if cross {
                                let s = reply.putrootfh.unwrap_or(Nfs4Status::ServerFault);
                                pieces.push(Piece::Res(OperationResult::Lookup(s)));
                                stopped = s != Nfs4Status::Ok;
                            }
                            if !stopped {
                                stopped = reply.status != 0;
                                pieces.push(Piece::Hub(reply));
                            }
                        }
                        Err(e) => {
                            // Step 3 wakes a parked hub; until then the
                            // client retries on DELAY.
                            warn!("hub {hub:#x}: {e}");
                            pieces.push(Piece::Res(refusal(first_opcode, Nfs4Status::Delay)));
                            stopped = true;
                        }
                    }
                    i = end;
                }
            }
        }
        self.slots.lock().unwrap().insert(
            key,
            DownSlot { seq: sequenceid, hub: used_hub.map(|u| u.0), bseq: used_hub.map(|u| u.1).unwrap_or(0) },
        );

        let mut s = Splice::new();
        s.push(OperationResult::Sequence(Nfs4Status::Ok, Some(seq_res)));
        for p in pieces {
            match p {
                Piece::Res(r) => s.push(r),
                Piece::Hub(h) => s.push_hub(&h),
            }
        }
        Ok(Bytes::from(s.finish(&tag)))
    }

    /// Backend bookkeeping after a session op the dispatcher ran: a
    /// destroyed session or client no longer needs its backends.
    fn after_session_ops(&self, ops: &[u32], _body: &Bytes) {
        if ops.iter().any(|o| *o == opcode::DESTROY_SESSION || *o == opcode::DESTROY_CLIENTID) {
            // Step 3: DESTROY_SESSION/_CLIENTID on the backends. Until
            // then, a backend session outlives its downstream one and the
            // hub reaps it at lease expiry.
            debug!("downstream session/client destroyed; backends left to lease expiry");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        view: &View,
        sid: SessionId,
        hub: u64,
        slot: u32,
        bseq: Option<u32>,
        cachethis: bool,
        call: &CallMessage,
        tag: &[u8],
        minor: u32,
        putrootfh: bool,
        ops: &[&[u8]],
    ) -> Result<(u32, HubReply), BackendError> {
        for attempt in 0..2 {
            let (client, sess) = self.backend(view, sid, hub).await?;
            let r = client.forward(&sess, slot, bseq, cachethis, &call.cred, tag, minor, putrootfh, ops).await;
            let stale = match &r {
                Ok((_, reply)) => matches!(
                    reply.seq.0,
                    Nfs4Status::BadSession | Nfs4Status::DeadSession | Nfs4Status::StaleClientId | Nfs4Status::Expired
                ),
                Err(BackendError::Down(_)) => true,
                Err(_) => false,
            };
            if !stale || attempt == 1 {
                return r.and_then(|(b, reply)| {
                    if reply.seq.0 != Nfs4Status::Ok {
                        return Err(BackendError::Status("SEQUENCE", reply.seq.0));
                    }
                    Ok((b, reply))
                });
            }
            // §4 "Hub restarts": the session (or client) is gone on the
            // hub; drop ours and establish again, once.
            info!("hub {hub:#x}: backend state stale, re-establishing");
            self.forget_hub(hub);
        }
        unreachable!()
    }

    fn forget_hub(&self, hub: u64) {
        let conn = self.conns.lock().unwrap().get(&hub).cloned();
        if conn.as_ref().is_some_and(|c| c.is_closed()) {
            self.conns.lock().unwrap().remove(&hub);
        }
        self.clients.lock().unwrap().retain(|(_, h), _| *h != hub);
        self.sessions.lock().unwrap().retain(|(_, h), _| *h != hub);
    }

    async fn backend(&self, view: &View, sid: SessionId, hub: u64) -> Result<(Arc<BackendClient>, Arc<BackendSession>), BackendError> {
        let limits = self
            .state
            .sessions
            .session_limits(&sid)
            .ok_or_else(|| BackendError::Protocol("downstream session vanished".into()))?;
        let ckey = (limits.client_id, hub);
        let skey = (sid.0, hub);
        let fast = |p: &Proxy| {
            let c = p.clients.lock().unwrap().get(&ckey).cloned()?;
            let s = p.sessions.lock().unwrap().get(&skey).cloned()?;
            (!c.conn.is_closed()).then_some((c, s))
        };
        if let Some(x) = fast(self) {
            return Ok(x);
        }
        let _g = self.setup.lock().await;
        if let Some(x) = fast(self) {
            return Ok(x);
        }
        let row = view.hub(hub).ok_or_else(|| BackendError::Protocol(format!("hub {hub:#x} not in the table")))?;
        let conn = {
            let cur = self.conns.lock().unwrap().get(&hub).cloned();
            match cur {
                Some(c) if !c.is_closed() => c,
                _ => {
                    let c = HubConn::connect(&row.address).await?;
                    self.conns.lock().unwrap().insert(hub, c.clone());
                    // A new connection: every backend on the old one goes.
                    self.clients.lock().unwrap().retain(|(_, h), _| *h != hub);
                    self.sessions.lock().unwrap().retain(|(_, h), _| *h != hub);
                    c
                }
            }
        };
        let client = match self.clients.lock().unwrap().get(&ckey).cloned() {
            Some(c) => Some(c),
            None => None,
        };
        let client = match client {
            Some(c) => c,
            None => {
                let down = self
                    .state
                    .clients
                    .get_client(limits.client_id)
                    .ok_or_else(|| BackendError::Protocol("downstream client vanished".into()))?;
                let c = BackendClient::register(conn, &down.owner, down.verifier.to_be_bytes(), down.flags).await?;
                info!("backend client on {} for downstream {:#x}: clientid {:#x}", row.name, limits.client_id, c.clientid);
                self.clients.lock().unwrap().insert(ckey, c.clone());
                c
            }
        };
        let fore = ChannelAttrs {
            header_pad_size: 0,
            max_request_size: limits.fore_chan_maxrequestsize,
            max_response_size: limits.fore_chan_maxresponsesize,
            max_response_size_cached: limits.fore_chan_maxresponsesize_cached,
            // the crossing adds a PUTROOTFH but replaces PUTFH + LOOKUP
            max_operations: limits.fore_chan_maxops,
            max_requests: limits.fore_chan_maxrequests,
            rdma_ird: vec![],
        };
        let sess = client.create_session(&fore).await?;
        if !view.is_hub_root_known(hub) {
            let root = client.root_fh(&sess).await?;
            if route::hub_of_fh(&root) != Some(hub) {
                // The table's serverId is not the hub's instance id: every
                // handle it mints would route elsewhere (or nowhere).
                return Err(BackendError::Protocol(format!(
                    "{}: root handle instance {:?} != serverId {hub:#x} (is FLINT_NFS_FSID_FROM_VOLUME / the table right?)",
                    row.name,
                    route::hub_of_fh(&root)
                )));
            }
            self.table.set_root(hub, root);
        }
        self.sessions.lock().unwrap().insert(skey, sess.clone());
        Ok((client, sess))
    }
}

/// A dispatcher reply under the client's own tag (the SEQUENCE-only
/// compound the proxy built had an empty one).
fn rewrap(body: &Bytes, tag: &[u8]) -> Bytes {
    let mut d = XdrDecoder::new(body.clone());
    let status = d.decode_u32().unwrap_or(0);
    let _ = d.decode_opaque();
    let numres = d.decode_u32().unwrap_or(0);
    let rest = d.into_remaining_bytes();
    let mut e = crate::nfs::xdr::XdrEncoder::new();
    e.encode_u32(status);
    e.encode_opaque(tag);
    e.encode_u32(numres);
    e.append_raw(&rest);
    e.finish()
}

async fn write_record(wr: &tokio::sync::Mutex<OwnedWriteHalf>, reply: &[u8]) {
    let mut rec = Vec::with_capacity(4 + reply.len());
    rec.extend_from_slice(&(0x8000_0000u32 | reply.len() as u32).to_be_bytes());
    rec.extend_from_slice(reply);
    let mut w = wr.lock().await;
    if let Err(e) = w.write_all(&rec).await {
        debug!("reply write failed: {e}");
    }
}
