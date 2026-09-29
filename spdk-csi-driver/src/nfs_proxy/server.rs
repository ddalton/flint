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

/// §4 "Hub parked": the proxy is the first NFS-side component that can
/// wake a hub. In a cluster this stamps `chert.us/requested-at` on the
/// FlintShare (step 4); with a static table it can only say so.
pub trait Waker: Send + Sync {
    fn wake(&self, hub: &HubRow);
}

pub struct LogWaker;

impl Waker for LogWaker {
    fn wake(&self, hub: &HubRow) {
        warn!("hub {} ({}) refuses connections; no waker in static mode — answering DELAY", hub.name, hub.address);
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KubeSource {
    /// Watch one namespace; unset = every namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_lease_secs() -> u64 {
    90
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    pub listen: String,
    /// The hubs' lease, which the proxy ALSO advertises and enforces.
    /// It cannot be shorter (as design §4 first had it): the kernel
    /// takes its renewal period from the LAST fsinfo it ran, and a
    /// workspace submount's fsinfo is answered by the hub, so a client
    /// renews on the hub's schedule. A shorter proxy lease reaped live
    /// clients between renewals (measured: the step-3 keepalive drill).
    /// The keepalive, every third of this, is what keeps a hub from
    /// expiring a client the proxy still holds.
    #[serde(default = "default_lease_secs")]
    pub lease_secs: u64,
    /// Off only for the keepalive drill's control arm.
    #[serde(default = "default_true")]
    pub keepalive: bool,
    /// The persisted client table (`proxy.db`) and the empty export the
    /// session dispatcher is built over.
    pub state_dir: PathBuf,
    /// Static rows (the box rig, tests). Ignored when `kube` is set.
    #[serde(default)]
    pub hubs: Vec<HubRow>,
    /// Derive the rows from a FlintShare watch (design §6) and wake
    /// parked hubs through the API server.
    #[serde(default)]
    pub kube: Option<KubeSource>,
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
    /// The backend session and seqid it was sent with. A retransmission
    /// reuses them only on that same backend session: after a hub restart
    /// the new session's seqids start over, and the request goes as NEW —
    /// what a direct mount's client does after BADSESSION.
    bsid: [u8; 16],
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
    waker: Arc<dyn Waker>,
    last_wake: Mutex<HashMap<u64, std::time::Instant>>,
    hub_lease: std::time::Duration,
    keepalive: bool,
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
        Self::new_with(cfg, Arc::new(LogWaker)).await
    }

    pub async fn new_with(cfg: &ProxyConfig, waker: Arc<dyn Waker>) -> Result<Arc<Self>, String> {
        // The process lease (`lease::set_lease_time`) is the BINARY's to
        // set, before this: tests share one process.
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
            waker,
            last_wake: Mutex::new(HashMap::new()),
            hub_lease: std::time::Duration::from_secs(cfg.lease_secs),
            keepalive: cfg.keepalive,
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
        if self.keepalive {
            let p = self.clone();
            tokio::spawn(async move { p.keepalive_loop().await });
        }
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
            .dispatch_compound_with_cred(req, call.cred.principal(), call.cred.unix_uid_gid(), call.cred.unix_gids(), None, false, Default::default())
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
            let destroyed = destroyed_by(&req.operations);
            let body = self.to_dispatcher(call, req).await;
            if body.get(0..4) == Some(&[0, 0, 0, 0][..]) {
                self.after_destroy(destroyed).await;
            }
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
            let destroyed = destroyed_by(&req.operations);
            let body = self.to_dispatcher(call, req).await;
            if body.get(0..4) == Some(&[0, 0, 0, 0][..]) {
                self.after_destroy(destroyed).await;
            }
            return Ok(body);
        }
        let tag = raw_tag(&args);
        let minor = req.minor_version;
        let mut ops = req.operations;
        let seq_op = ops.remove(0);
        let Operation::Sequence { sessionid, sequenceid, slotid, cachethis, .. } = seq_op else { unreachable!() };
        let seq_only = CompoundRequest { tag: String::new(), tag_valid: true, minor_version: minor, operations: vec![seq_op], wire_size: 0 };
        let seq_body = self
            .disp
            .dispatch_compound_with_cred(seq_only, call.cred.principal(), call.cred.unix_uid_gid(), call.cred.unix_gids(), None, false, Default::default())
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
        let mut used_hub: Option<(u64, [u8; 16], u32)> = None;
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
                    let again = resend.filter(|l| l.hub == Some(hub)).map(|l| (l.bsid, l.bseq));
                    match self
                        .forward(view, sessionid, hub, slotid, again, cachethis, call, &tag, minor, plan.putrootfh, &slices)
                        .await
                    {
                        Ok((bsid, bseq, reply)) => {
                            used_hub = Some((hub, bsid, bseq));
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
                            // §4: a parked or restarting hub is DELAY, and
                            // a refused connection wakes it.
                            // DELAY only where waiting can help: the hub is
                            // unreachable (parked, restarting). Anything else
                            // is an error the client SEES — a hard mount
                            // retries DELAY forever, and a DELAY that can
                            // never clear wedges every process on the mount.
                            let status = if matches!(e, BackendError::Down(_)) {
                                self.wake(view, hub);
                                Nfs4Status::Delay
                            } else {
                                Nfs4Status::ServerFault
                            };
                            warn!("hub {hub:#x}: {e} → {status:?}");
                            pieces.push(Piece::Res(refusal(first_opcode, status)));
                            stopped = true;
                        }
                    }
                    i = end;
                }
            }
        }
        self.slots.lock().unwrap().insert(
            key,
            DownSlot {
                seq: sequenceid,
                hub: used_hub.map(|u| u.0),
                bsid: used_hub.map(|u| u.1).unwrap_or([0; 16]),
                bseq: used_hub.map(|u| u.2).unwrap_or(0),
            },
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

    /// A downstream session or client the dispatcher destroyed takes its
    /// backends with it: DESTROY_SESSION / DESTROY_CLIENTID on each hub,
    /// as the client would have sent them on a direct mount. Otherwise a
    /// hub keeps the state until its lease runs out, holding its
    /// `activeLeases` (and its idle ladder) up for nothing.
    async fn after_destroy(&self, (sessions, clients): (Vec<SessionId>, Vec<u64>)) {
        for sid in sessions {
            let gone: Vec<(u64, Arc<BackendSession>)> = {
                let mut m = self.sessions.lock().unwrap();
                let keys: Vec<_> = m.keys().filter(|(s, _)| *s == sid.0).copied().collect();
                keys.into_iter().filter_map(|k| m.remove(&k).map(|v| (k.1, v))).collect()
            };
            for (hub, sess) in gone {
                // DESTROY_SESSION is a sole op: any connection to the hub.
                let conn = self.conns.lock().unwrap().get(&hub).cloned();
                if let Some(c) = conn {
                    match BackendClient::destroy_session_on(&c, &sess).await {
                        Ok(s) => debug!("hub {hub:#x}: backend DESTROY_SESSION: {s:?}"),
                        Err(e) => debug!("hub {hub:#x}: backend DESTROY_SESSION: {e}"),
                    }
                }
            }
        }
        for cid in clients {
            let gone: Vec<(u64, Arc<BackendClient>)> = {
                let mut m = self.clients.lock().unwrap();
                let keys: Vec<_> = m.keys().filter(|(c, _)| *c == cid).copied().collect();
                keys.into_iter().filter_map(|k| m.remove(&k).map(|v| (k.1, v))).collect()
            };
            for (hub, c) in gone {
                match c.destroy().await {
                    Ok(s) => info!("hub {hub:#x}: backend client {:#x} destroyed with its downstream: {s:?}", c.clientid),
                    Err(e) => debug!("hub {hub:#x}: backend DESTROY_CLIENTID: {e}"),
                }
            }
        }
    }

    /// §4 Leases: while a downstream client's proxy lease is live, renew
    /// its lease on every hub it holds a backend client on, once per
    /// third of the hub lease. When the downstream lease lapses, stop:
    /// the hub's own expiry and courtesy release run as they do today.
    async fn keepalive_loop(self: Arc<Self>) {
        let every = self.hub_lease / 3;
        let tick = (every / 3).max(std::time::Duration::from_secs(1));
        loop {
            tokio::time::sleep(tick).await;
            let all: Vec<((u64, u64), Arc<BackendClient>)> =
                self.clients.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect();
            for ((cid, hub), c) in all {
                if self.state.clients.get_client(cid).is_none() {
                    // The downstream client is gone (expired and reaped):
                    // forget its backends; the hub expires them itself.
                    self.clients.lock().unwrap().remove(&(cid, hub));
                    continue;
                }
                if !self.state.leases.is_valid(cid) || c.since_renewed() < every {
                    continue;
                }
                let p = self.clone();
                tokio::spawn(async move {
                    match c.keepalive().await {
                        Ok(Nfs4Status::Ok) => debug!("hub {hub:#x}: renewed backend {:#x}", c.clientid),
                        Ok(s) => {
                            info!("hub {hub:#x}: keepalive {s:?}; backend {:#x} dropped, next use re-establishes", c.clientid);
                            p.forget(hub, Some(cid), None);
                        }
                        Err(e) => debug!("hub {hub:#x}: keepalive: {e}"),
                    }
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        view: &View,
        sid: SessionId,
        hub: u64,
        slot: u32,
        resend: Option<([u8; 16], u32)>,
        cachethis: bool,
        call: &CallMessage,
        tag: &[u8],
        minor: u32,
        putrootfh: bool,
        ops: &[&[u8]],
    ) -> Result<([u8; 16], u32, HubReply), BackendError> {
        let cid = self.state.sessions.session_limits(&sid).map(|l| l.client_id);
        for attempt in 0..2 {
            let (client, sess) = self.backend(view, sid, hub).await?;
            let bseq = resend.filter(|(bsid, _)| *bsid == sess.sessionid.0).map(|(_, s)| s);
            let bsid = sess.sessionid.0;
            let r = client
                .forward(&sess, slot, bseq, cachethis, &call.cred, tag, minor, putrootfh, ops)
                .await
                .map(|(b, reply)| (bsid, b, reply));
            let stale = match &r {
                Ok((_, _, reply)) => matches!(
                    reply.seq.0,
                    Nfs4Status::BadSession | Nfs4Status::DeadSession | Nfs4Status::StaleClientId | Nfs4Status::Expired
                ),
                Err(BackendError::Down(_)) => true,
                Err(_) => false,
            };
            if !stale || attempt == 1 {
                return r.and_then(|(bsid, b, reply)| {
                    if reply.seq.0 != Nfs4Status::Ok {
                        return Err(BackendError::Status("SEQUENCE", reply.seq.0));
                    }
                    Ok((bsid, b, reply))
                });
            }
            // §4 "Hub restarts": the session (or client) is gone on the
            // hub; drop ours and establish again, once.
            let what = match &r {
                Ok((_, _, reply)) => format!("{:?}", reply.seq.0),
                Err(e) => e.to_string(),
            };
            info!("hub {hub:#x}: backend state stale ({what}), re-establishing");
            let client_gone = matches!(&r, Ok((_, _, reply)) if matches!(reply.seq.0, Nfs4Status::StaleClientId | Nfs4Status::Expired));
            self.forget(hub, if client_gone { cid } else { None }, Some(sid));
        }
        unreachable!()
    }

    /// Drop backend state after a hub said it is gone: a closed
    /// connection takes everything on that hub; a stale client its own
    /// sessions; a stale session only itself.
    fn forget(&self, hub: u64, client: Option<u64>, session: Option<SessionId>) {
        let conn = self.conns.lock().unwrap().get(&hub).cloned();
        if conn.as_ref().is_some_and(|c| c.is_closed()) {
            self.conns.lock().unwrap().remove(&hub);
            self.clients.lock().unwrap().retain(|(_, h), _| *h != hub);
            self.sessions.lock().unwrap().retain(|(_, h), _| *h != hub);
            return;
        }
        if let Some(cid) = client {
            self.clients.lock().unwrap().remove(&(cid, hub));
            let sids: Vec<[u8; 16]> = self
                .state
                .sessions
                .get_client_sessions(cid)
                .into_iter()
                .map(|s| s.0)
                .collect();
            self.sessions.lock().unwrap().retain(|(s, h), _| !(*h == hub && sids.contains(s)));
        }
        if let Some(sid) = session {
            self.sessions.lock().unwrap().remove(&(sid.0, hub));
        }
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
        let register = || async {
            let down = self
                .state
                .clients
                .get_client(limits.client_id)
                .ok_or_else(|| BackendError::Protocol("downstream client vanished".into()))?;
            let c = BackendClient::register(conn.clone(), &down.owner, down.verifier.to_be_bytes(), down.flags).await?;
            info!("backend client on {} for downstream {:#x}: clientid {:#x}", row.name, limits.client_id, c.clientid);
            self.clients.lock().unwrap().insert(ckey, c.clone());
            Ok::<_, BackendError>(c)
        };
        let cached = self.clients.lock().unwrap().get(&ckey).cloned();
        let mut client = match cached {
            Some(c) => c,
            None => register().await?,
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
        let sess = match client.create_session(&fore).await {
            // The hub no longer knows the cached backend client (its lease
            // lapsed and it was reaped): register again, once. Without
            // this the error surfaced as DELAY, and a hard mount retried
            // it forever (the 2026-09-28 box wedge).
            Err(BackendError::Status(_, Nfs4Status::StaleClientId | Nfs4Status::Expired)) => {
                info!("hub {hub:#x}: backend client {:#x} is gone on the hub; registering again", client.clientid);
                self.clients.lock().unwrap().remove(&ckey);
                client = register().await?;
                client.create_session(&fore).await?
            }
            r => r?,
        };
        if !view.is_hub_root_known(hub) {
            let root = client.root_fh().await?;
            if route::hub_of_fh(&root) != Some(hub) {
                // The table's serverId is not the hub's instance id: every
                // handle it mints would route elsewhere (or nowhere). The
                // usual cause is a share woken from hibernation onto a new
                // disk, whose new serverId the operator has not published
                // yet — which clears by itself, so this is DELAY (Down),
                // not an error. A table that is simply WRONG shows up as
                // this line repeating in the log.
                return Err(BackendError::Down(format!(
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

impl Proxy {
    /// At most once per 30 s per hub: a hard mount retries on DELAY every
    /// few seconds, and the stamp is a write to the API server.
    fn wake(&self, view: &View, hub: u64) {
        let Some(row) = view.hub(hub) else { return };
        let now = std::time::Instant::now();
        let mut last = self.last_wake.lock().unwrap();
        if last.get(&hub).is_some_and(|t| now.duration_since(*t) < std::time::Duration::from_secs(30)) {
            return;
        }
        last.insert(hub, now);
        self.waker.wake(&row);
    }
}

/// The sessions and clients a session-only compound destroys.
fn destroyed_by(ops: &[Operation]) -> (Vec<SessionId>, Vec<u64>) {
    let mut s = Vec::new();
    let mut c = Vec::new();
    for op in ops {
        match op {
            Operation::DestroySession(sid) => s.push(*sid),
            Operation::DestroyClientId(cid) => c.push(*cid),
            _ => {}
        }
    }
    (s, c)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::rpc::Auth;
    use crate::nfs::xdr::XdrEncoder;
    use crate::nfs_proxy::backend::proxy_cred;

    const HUB_ID: u64 = 0xA11CE_0000_0001;

    /// A REAL hub — the crate's own server loop and dispatcher — on a
    /// free port, over a temp export.
    async fn hub(dir: &std::path::Path) -> String {
        hub_with_state(dir).await.0
    }

    async fn hub_with_state(dir: &std::path::Path) -> (String, Arc<StateManager>) {
        let export = dir.join("export");
        std::fs::create_dir_all(&export).unwrap();
        let fh = Arc::new(FileHandleManager::new_with_instance_id(export, "volume".into(), HUB_ID));
        let state = Arc::new(StateManager::new_in_memory("hub"));
        state.stateids.set_stateid_tag(10);
        let disp = Arc::new(CompoundDispatcher::new(fh, state.clone(), Arc::new(LockManager::new())));
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let addr = format!("127.0.0.1:{port}");
        let a = addr.clone();
        let gss = Arc::new(crate::nfs::rpcsec_gss::RpcSecGssManager::new(None));
        tokio::spawn(async move { crate::nfs::server_v4::serve_tcp(&a, disp, gss).await });
        for _ in 0..100 {
            if TcpStream::connect(&addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        (addr, state)
    }

    async fn proxy(dir: &std::path::Path, hub_addr: &str) -> (Arc<Proxy>, String) {
        let cfg = ProxyConfig {
            listen: "127.0.0.1:0".into(),
            lease_secs: 90,
            keepalive: false,
            state_dir: dir.join("proxy"),
            hubs: vec![HubRow { name: "ws-a".into(), address: hub_addr.into(), server_id: HUB_ID, stateid_tag: 10, share: None, wakeable: true }],
            kube: None,
            identities: vec![IdentityRule { name: "t".into(), sources: vec!["127.0.0.1/32".into()], workspaces: vec!["ws-*".into()] }],
        };
        let p = Proxy::new(&cfg).await.unwrap();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(p.clone().serve_on(l));
        (p, addr)
    }

    fn op_putfh(fh: &[u8]) -> Vec<u8> {
        let mut e = XdrEncoder::new();
        e.encode_u32(opcode::PUTFH);
        e.encode_opaque(fh);
        e.finish().to_vec()
    }
    fn op_lookup(name: &str) -> Vec<u8> {
        let mut e = XdrEncoder::new();
        e.encode_u32(opcode::LOOKUP);
        e.encode_string(name);
        e.finish().to_vec()
    }
    fn op_mkdir(name: &str) -> Vec<u8> {
        let mut e = XdrEncoder::new();
        e.encode_u32(opcode::CREATE);
        e.encode_u32(2); // NF4DIR
        e.encode_string(name);
        e.encode_u32(0); // attrmask<>
        e.encode_u32(0); // attrlist
        e.finish().to_vec()
    }

    /// A client session on the proxy: (connection, sessionid).
    async fn client(proxy_addr: &str) -> (Arc<HubConn>, SessionId) {
        client_with(proxy_addr, *b"verifier").await
    }

    async fn client_with(proxy_addr: &str, verifier: [u8; 8]) -> (Arc<HubConn>, SessionId) {
        let c = HubConn::connect(proxy_addr).await.unwrap();
        let one = |op: Vec<u8>| {
            let c = c.clone();
            async move { c.call(&proxy_cred(), &wire::encode_compound(b"", 1, &[&op])).await.unwrap() }
        };
        let (_, x) = wire::decode_exchange_id_reply(one(wire::op_exchange_id(verifier, b"test-client", 0)).await).unwrap();
        let x = x.unwrap();
        let fore = ChannelAttrs { max_requests: 4, ..ChannelAttrs::default() };
        let (_, cs) = wire::decode_create_session_reply(one(wire::op_create_session(x.clientid, x.sequenceid, &fore)).await).unwrap();
        (c, cs.unwrap().sessionid)
    }

    fn seq(sid: SessionId, slot: u32, seq: u32) -> Vec<u8> {
        wire::op_sequence(&wire::SeqArgs { sessionid: sid, sequenceid: seq, slotid: slot, highest_slotid: 3, cachethis: true })
    }

    /// §4 "Slots and exactly-once", the §8 replay drill in-process: a
    /// retransmitted CREATE must be answered from the HUB's reply cache,
    /// not run twice. A second run is observable: mkdir of an existing
    /// name is NFS4ERR_EXIST (the control below proves it).
    #[tokio::test]
    async fn a_retransmitted_create_is_answered_by_the_hubs_reply_cache_not_run_twice() {
        let dir = tempfile::tempdir().unwrap();
        let hub_addr = hub(dir.path()).await;
        let (_p, addr) = proxy(dir.path(), &hub_addr).await;
        let (c, sid) = client(&addr).await;
        let cred = Auth { flavor: AuthFlavor::Unix, body: proxy_cred().body };
        let root = route::PSEUDO_ROOT_FH;

        let x = [seq(sid, 0, 1), op_putfh(root), op_lookup("ws-a"), op_mkdir("d1")];
        let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
        let first = c.call(&cred, &wire::encode_compound(b"t", 2, &refs)).await.unwrap();
        assert_eq!(&first[0..4], &[0, 0, 0, 0], "the CREATE itself succeeds");
        assert!(dir.path().join("export/d1").is_dir(), "and lands in the hub's export");

        let again = c.call(&cred, &wire::encode_compound(b"t", 2, &refs)).await.unwrap();
        assert_eq!(again, first, "the retransmission gets the ORIGINAL reply, byte for byte");

        // Control: the same CREATE as a NEW request on the slot does run
        // again, and says so.
        let y = [seq(sid, 0, 2), op_putfh(root), op_lookup("ws-a"), op_mkdir("d1")];
        let refs: Vec<&[u8]> = y.iter().map(|o| o.as_slice()).collect();
        let rerun = c.call(&cred, &wire::encode_compound(b"t", 2, &refs)).await.unwrap();
        assert_eq!(&rerun[0..4], &(Nfs4Status::Exist as u32).to_be_bytes(), "a re-executed mkdir is EXIST");
    }

    /// The hub's root learned through the control session carries the
    /// table's serverId; a GETFH after the crossing returns it.
    #[tokio::test]
    async fn the_crossing_lands_on_the_hubs_root() {
        let dir = tempfile::tempdir().unwrap();
        let hub_addr = hub(dir.path()).await;
        let (p, addr) = proxy(dir.path(), &hub_addr).await;
        let (c, sid) = client(&addr).await;
        let x = [seq(sid, 1, 1), op_putfh(route::PSEUDO_ROOT_FH), op_lookup("ws-a"), wire::op_getfh()];
        let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
        let body = c.call(&proxy_cred(), &wire::encode_compound(b"", 2, &refs)).await.unwrap();
        assert_eq!(&body[0..4], &[0, 0, 0, 0]);
        // (`backend` refuses a root whose instance is not the serverId.)
        assert!(p.table().view("127.0.0.1".parse().unwrap()).is_hub_root_known(HUB_ID));
    }

    /// §4: a client reboot (same owner, NEW verifier) must REACH the hub
    /// as the same RFC 8881 §18.35.5 case 5 a direct mount sends, so the
    /// hub discards the old incarnation's state — convergence, not only
    /// safety. The backend owner stays "flint-proxy/"‖owner.
    #[tokio::test]
    async fn a_client_reboot_reaches_the_hub_as_a_new_verifier() {
        let dir = tempfile::tempdir().unwrap();
        let (hub_addr, hub_state) = hub_with_state(dir.path()).await;
        let (_p, addr) = proxy(dir.path(), &hub_addr).await;
        let owner = b"flint-proxy/test-client";
        let touch = |c: Arc<HubConn>, sid: SessionId, name: &'static str| async move {
            let x = [seq(sid, 0, 1), op_putfh(route::PSEUDO_ROOT_FH), op_lookup("ws-a"), op_mkdir(name)];
            let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
            let body = c.call(&proxy_cred(), &wire::encode_compound(b"", 2, &refs)).await.unwrap();
            assert_eq!(&body[0..4], &[0, 0, 0, 0]);
        };

        let (c1, s1) = client_with(&addr, *b"boot-one").await;
        touch(c1, s1, "a").await;
        let before = hub_state.clients.id_of_owner(owner).expect("the backend client exists on the hub");
        assert_eq!(hub_state.clients.get_client(before).unwrap().verifier, u64::from_be_bytes(*b"boot-one"));

        // The client reboots: same owner, new verifier.
        let (c2, s2) = client_with(&addr, *b"boot-two").await;
        touch(c2, s2, "b").await;
        let after = hub_state.clients.id_of_owner(owner).expect("still one backend client for the owner");
        let rec = hub_state.clients.get_client(after).unwrap();
        assert_eq!(rec.verifier, u64::from_be_bytes(*b"boot-two"), "the hub saw the reboot");
        assert_ne!(after, before, "and replaced the old incarnation (case 5), not renewed it");
        assert!(hub_state.clients.get_client(before).is_none(), "whose record is gone");
    }

    /// A hub that reaped the proxy's backend client (its lease lapsed and
    /// a conflicting client triggered the courtesy release) answers the
    /// next op BADSESSION, and CREATE_SESSION on the old clientid is
    /// STALE_CLIENTID. The proxy must register again and carry on — not
    /// answer DELAY, which a hard mount retries forever (the 2026-09-28
    /// box wedge ran exactly this path in the keepalive-off control arm).
    #[tokio::test]
    async fn a_backend_client_the_hub_reaped_is_registered_again_not_delayed_forever() {
        let dir = tempfile::tempdir().unwrap();
        let (hub_addr, hub_state) = hub_with_state(dir.path()).await;
        let (_p, addr) = proxy(dir.path(), &hub_addr).await;
        let (c, sid) = client(&addr).await;
        let mkdir = |slot_seq: u32, name: &'static str| {
            let c = c.clone();
            async move {
                let x = [seq(sid, 0, slot_seq), op_putfh(route::PSEUDO_ROOT_FH), op_lookup("ws-a"), op_mkdir(name)];
                let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
                let body = c.call(&proxy_cred(), &wire::encode_compound(b"", 2, &refs)).await.unwrap();
                u32::from_be_bytes(body[0..4].try_into().unwrap())
            }
        };
        assert_eq!(mkdir(1, "a").await, 0);
        let backend = hub_state.clients.id_of_owner(b"flint-proxy/test-client").unwrap();
        // The hub's lease sweep retires the backend client, sessions and all.
        hub_state.cleanup_expired_ids(&[backend]);
        assert!(hub_state.clients.get_client(backend).is_none());

        let st = mkdir(2, "b").await;
        assert_eq!(st, 0, "status {st} (10008 = DELAY, the forever-loop)");
        assert!(dir.path().join("export/b").is_dir());
        assert!(hub_state.clients.id_of_owner(b"flint-proxy/test-client").is_some(), "registered again");
    }

    /// The config the chart renders (flint-lite-operator-chart
    /// templates/nfs-proxy.yaml, `helm template` output verbatim) is the
    /// config this binary parses — kube mode, all namespaces.
    #[test]
    fn the_charts_rendered_config_parses() {
        let rendered = "listen: 0.0.0.0:2049\nstateDir: /var/lib/flint-nfs-proxy\nleaseSecs: 90\nkube:\n  {}\nidentities:\n  - name: local\n    sources:\n    - 10.0.0.0/8\n    workspaces:\n    - ws-*\n";
        let cfg: ProxyConfig = serde_yaml::from_str(rendered).unwrap();
        assert_eq!(cfg.lease_secs, 90);
        assert!(cfg.kube.as_ref().is_some_and(|k| k.namespace.is_none()), "kube mode, every namespace");
        assert!(cfg.hubs.is_empty() && cfg.keepalive);
        assert_eq!(cfg.identities[0].workspaces, vec!["ws-*"]);
        let one_ns = rendered.replace("kube:\n  {}", "kube:\n  namespace: \"workspaces\"");
        let cfg: ProxyConfig = serde_yaml::from_str(&one_ns).unwrap();
        assert_eq!(cfg.kube.unwrap().namespace.as_deref(), Some("workspaces"));
    }
}
