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
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use super::backend::{BackendClient, BackendError, BackendSession, HubConn};
use super::pseudo::{self, LocalCtx};
use super::route::{self, Disp};
use super::table::{HubRow, IdentityRule, Peer, Table, View};
use super::tls::{self, Tls, TlsConfig};
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
    /// RPC-with-TLS (design §6a). Set, EVERY connection must upgrade: a
    /// call on a connection that did not is refused `AUTH_TOOWEAK`.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// The revocation bits a hub's `sr_status_flags` may carry through to the
/// client: RECALLABLE_STATE_REVOKED (0x40) only, which Linux answers by
/// testing its delegations one stateid at a time. EXPIRED_ALL (0x08),
/// EXPIRED_SOME (0x10) and ADMIN (0x20) _STATE_REVOKED start a
/// client-wide recovery (`nfs41_handle_sequence_flag_errors`), and behind
/// the proxy the client is every workspace a node mounts: each of the
/// three, surfaced after one hub lost its state, cost another workspace
/// its lock and its writes (step3-drills.sh `revoke`, Linux 6.12; census
/// Part 3 for 0x20 on knfsd). Those losses reach the client as the hub's
/// per-op stateid errors instead, which it recovers per state (same
/// drill). Channel bits (CB_PATH_DOWN, ...) describe the proxy's backend
/// session, not the client's.
const HUB_FLAGS_PASSED: u32 = 0x40;

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
    tls: Option<Arc<Tls>>,
}

/// Ids no client or session can have: both are counters from 1, and a
/// session id is `counter ‖ clientid` (`SessionManager::create_session`).
const NO_SESSION: SessionId = SessionId([0xFF; 16]);
const NO_CLIENT: u64 = u64::MAX;

/// The owner prefix of a TLS identity: a digest, so the owner stays
/// bounded whatever the certificate names, and STABLE — it is persisted
/// in proxy.db and sent to the hubs. None on plaintext.
fn identity_prefix(peer: &Peer) -> Option<Vec<u8>> {
    use sha2::{Digest, Sha256};
    if peer.uris.is_empty() {
        return None;
    }
    let mut uris = peer.uris.clone();
    uris.sort();
    let d = Sha256::digest(uris.join("\n").as_bytes());
    let hex: String = d[..8].iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("id:{hex}/").into_bytes())
}

/// Does a registered owner belong to this identity? A plaintext
/// connection owns what no certificate registered.
fn owner_is(owner: &[u8], prefix: Option<&[u8]>) -> bool {
    match prefix {
        Some(p) => owner.starts_with(p),
        None => !owner.starts_with(b"id:"),
    }
}

/// A TLS handshake that has not finished by then is dropped: the socket
/// is not the client's until it has.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
        if table.wants_certificates() && cfg.tls.is_none() {
            return Err("an identity names `clients:` but `tls` is not configured: no connection could match it".into());
        }
        let tls = cfg.tls.as_ref().map(Tls::load).transpose()?;
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
            tls,
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
        if let Some(t) = &self.tls {
            tokio::spawn(t.clone().reload_loop());
        }
        loop {
            let (s, peer) = l.accept().await?;
            let p = self.clone();
            tokio::spawn(async move { p.connection(s, peer).await });
        }
    }

    async fn connection(self: Arc<Self>, mut s: TcpStream, addr: SocketAddr) {
        let _ = s.set_nodelay(true);
        debug!("client {addr} connected");
        let Some(tls) = self.tls.clone() else {
            let (rd, wr) = s.into_split();
            return self.serve_records(rd, wr, Peer::addr(addr.ip())).await;
        };
        // §6a: the connection must upgrade before it is anyone. Until the
        // probe, a NULL ping is answered and every other call is refused
        // AUTH_TOOWEAK (a plaintext mount fails at once, and says why).
        // RecordReader reads exactly one record, so after the probe the
        // socket's next byte is the client's ClientHello.
        let mut rr = RecordReader::new(format!("client {addr}"));
        loop {
            let rec = match rr.next(&mut s, None).await {
                Ok(NextRecord::Record(r)) => r,
                _ => return,
            };
            let call = match CallMessage::decode_with_args(rec) {
                Ok((call, _)) => call,
                Err(e) => {
                    debug!("client {addr}: undecodable call before TLS: {e}");
                    return;
                }
            };
            let (reply, upgrade) = if tls::is_probe(&call) {
                (tls::starttls_reply(call.xid), true)
            } else if call.procedure == 0 {
                (ReplyBuilder::success(call.xid).finish(), false)
            } else {
                warn!("client {addr}: call on a connection that did not upgrade to TLS: AUTH_TOOWEAK");
                (ReplyBuilder::auth_error(call.xid, AuthStat::TooWeak), false)
            };
            if !write_record_to(&mut s, &reply).await {
                return;
            }
            if upgrade {
                break;
            }
        }
        let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.acceptor().accept(s)).await {
            Ok(Ok(st)) => st,
            Ok(Err(e)) => {
                warn!("client {addr}: TLS handshake failed: {e}");
                return;
            }
            Err(_) => {
                warn!("client {addr}: TLS handshake timed out");
                return;
            }
        };
        let conn = stream.get_ref().1;
        let uris = conn.peer_certificates().and_then(|c| c.first()).map(|c| tls::uri_sans(c)).unwrap_or_default();
        if conn.alpn_protocol() != Some(tls::ALPN_SUNRPC) {
            debug!("client {addr}: no ALPN sunrpc offered");
        }
        info!("client {addr}: TLS up, identity {uris:?}");
        let (rd, wr) = tokio::io::split(stream);
        self.serve_records(rd, wr, Peer { addr: addr.ip(), uris }).await
    }

    async fn serve_records<R, W>(self: Arc<Self>, mut rd: R, wr: W, peer: Peer)
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let peer = Arc::new(peer);
        let wr = Arc::new(tokio::sync::Mutex::new(wr));
        let mut rr = RecordReader::new(format!("client {}", peer.addr));
        loop {
            let rec = match rr.next(&mut rd, None).await {
                Ok(NextRecord::Record(r)) => r,
                Ok(_) => break,
                Err(e) => {
                    debug!("client {}: {e}", peer.addr);
                    break;
                }
            };
            let p = self.clone();
            let wr = wr.clone();
            let peer = peer.clone();
            // Requests on one connection run concurrently: the client
            // pipelines across slots, and one slow hub must not stall the
            // replies from another.
            tokio::spawn(async move {
                if let Some(reply) = p.call(rec, &peer).await {
                    write_record(&wr, &reply).await;
                }
            });
        }
        debug!("client {} gone", peer.addr);
    }

    async fn call(&self, rec: Bytes, peer: &Peer) -> Option<Bytes> {
        let (call, args) = match CallMessage::decode_with_args(rec) {
            Ok(x) => x,
            Err(e) => {
                debug!("client {}: undecodable call: {e}", peer.addr);
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
                let view = self.table.view_of(peer);
                let body = match self.compound(&view, peer, &call, args).await {
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

    /// §6a: a downstream client belongs to the identity that registered
    /// it. Session and client ids are guessable (a counter), and every
    /// connection reaches the one dispatcher, so without this a
    /// certificate for cluster B could drive cluster A's session — its
    /// slots, its lease, its opens and locks on any workspace both may
    /// see. On a TLS connection, EXCHANGE_ID's owner is prefixed with the
    /// identity (two identities never share a client record, and the
    /// hubs see the prefix in the backend owner too), and any session or
    /// clientid a compound names that belongs to ANOTHER identity is
    /// replaced by one that cannot exist: the dispatcher then answers
    /// BADSESSION / STALE_CLIENTID exactly as for an absent one.
    ///
    /// A plaintext connection (no `tls`) has no identity to bind to: an
    /// address is not one (NAT, trunking over several addresses).
    fn bind_to_identity(&self, peer: &Peer, ops: &mut [Operation]) {
        let prefix = identity_prefix(peer);
        let owns_client = |cid: u64| match self.state.clients.get_client(cid) {
            Some(c) => owner_is(&c.owner, prefix.as_deref()),
            None => true, // absent either way
        };
        let owns_session = |sid: &SessionId| match self.state.sessions.get_session(sid) {
            Some(s) => owns_client(s.client_id),
            None => true,
        };
        for op in ops.iter_mut() {
            match op {
                Operation::ExchangeId { clientowner, .. } => {
                    if let Some(p) = &prefix {
                        let mut o = p.clone();
                        o.extend_from_slice(&clientowner.id);
                        clientowner.id = o;
                    }
                }
                Operation::Sequence { sessionid, .. }
                | Operation::DestroySession(sessionid)
                | Operation::BindConnToSession { sessionid, .. } => {
                    if !owns_session(sessionid) {
                        warn!("client {}: names a session of another identity; answered as absent", peer.addr);
                        *sessionid = NO_SESSION;
                    }
                }
                Operation::CreateSession { clientid, .. } | Operation::DestroyClientId(clientid) => {
                    if !owns_client(*clientid) {
                        warn!("client {}: names a clientid of another identity; answered as absent", peer.addr);
                        *clientid = NO_CLIENT;
                    }
                }
                _ => {}
            }
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

    async fn compound(&self, view: &View, peer: &Peer, call: &CallMessage, args: Bytes) -> Result<Bytes, ()> {
        let (mut req, ranges) = CompoundRequest::decode_with_ranges(XdrDecoder::new(args.clone())).map_err(|_| ())?;
        self.bind_to_identity(peer, &mut req.operations);
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
            self.keepalive_tick(every);
        }
    }

    /// One pass: renew every backend of a live downstream client not
    /// renewed within `every`. Each renewal runs in its own task, so one
    /// slow hub does not hold up the rest; the handles are for tests.
    fn keepalive_tick(self: &Arc<Self>, every: std::time::Duration) -> Vec<tokio::task::JoinHandle<()>> {
        let mut spawned = Vec::new();
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
            spawned.push(tokio::spawn(async move {
                match c.keepalive().await {
                    Ok(Nfs4Status::Ok) => debug!("hub {hub:#x}: renewed backend {:#x}", c.clientid),
                    // The hub restarted (its sessions are dropped at
                    // load) or the connection closed. An idle client
                    // sends nothing that reaches the hub, so nothing
                    // but this re-attaches it: left alone, the hub
                    // reaps the backend client one lease later, locks
                    // and all, and reports zero leases, which
                    // hibernation reads as "nobody holds state here"
                    // (step3-drills.sh `idlerestart`).
                    Ok(Nfs4Status::BadSession | Nfs4Status::DeadSession) | Err(BackendError::Down(_)) => {
                        match p.reattach(cid, hub, &c).await {
                            Ok(n) => info!("hub {hub:#x}: backend {:#x} re-attached by the keepalive: clientid {n:#x}", c.clientid),
                            Err(e) => debug!("hub {hub:#x}: keepalive re-attach: {e}"),
                        }
                    }
                    Ok(s) => {
                        info!("hub {hub:#x}: keepalive {s:?}; backend {:#x} dropped, next use re-establishes", c.clientid);
                        p.forget(hub, Some(cid), None);
                    }
                    Err(e) => debug!("hub {hub:#x}: keepalive: {e}"),
                }
            }));
        }
        spawned
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

    /// Register `cid`'s backend client on `hub` again, under the same
    /// owner and verifier: a hub that kept its state (a restart) answers
    /// with the SAME clientid, so the client's opens and locks carry on.
    /// Replaces `old` only if it is still the cached one (a client op may
    /// have re-established it meanwhile). Its data sessions died with the
    /// hub's sessions and are dropped; next use creates new ones.
    async fn reattach(&self, cid: u64, hub: u64, old: &Arc<BackendClient>) -> Result<u64, BackendError> {
        let _g = self.setup.lock().await;
        let cached = self.clients.lock().unwrap().get(&(cid, hub)).cloned();
        if !cached.is_some_and(|c| Arc::ptr_eq(&c, old)) {
            return Ok(old.clientid);
        }
        let row = self.table.hub(hub).ok_or_else(|| BackendError::Protocol(format!("hub {hub:#x} not in the table")))?;
        let down = self
            .state
            .clients
            .get_client(cid)
            .ok_or_else(|| BackendError::Protocol("downstream client vanished".into()))?;
        let conn = {
            let cur = self.conns.lock().unwrap().get(&hub).cloned();
            match cur {
                Some(c) if !c.is_closed() => c,
                _ => {
                    // Not `backend`'s drop-everything-on-this-hub: every
                    // other backend on the old connection fails its own
                    // keepalive and re-attaches the same way.
                    let c = HubConn::connect(&row.address).await?;
                    self.conns.lock().unwrap().insert(hub, c.clone());
                    c
                }
            }
        };
        let c = BackendClient::register(conn, &down.owner, down.verifier.to_be_bytes(), down.flags).await?;
        let sids: Vec<[u8; 16]> = self.state.sessions.get_client_sessions(cid).into_iter().map(|s| s.0).collect();
        self.sessions.lock().unwrap().retain(|(s, h), _| !(*h == hub && sids.contains(s)));
        let n = c.clientid;
        self.clients.lock().unwrap().insert((cid, hub), c);
        Ok(n)
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

async fn write_record<W: AsyncWrite + Unpin>(wr: &tokio::sync::Mutex<W>, reply: &[u8]) {
    let mut w = wr.lock().await;
    write_record_to(&mut *w, reply).await;
}

async fn write_record_to<W: AsyncWrite + Unpin>(w: &mut W, reply: &[u8]) -> bool {
    let mut rec = Vec::with_capacity(4 + reply.len());
    rec.extend_from_slice(&(0x8000_0000u32 | reply.len() as u32).to_be_bytes());
    rec.extend_from_slice(reply);
    match w.write_all(&rec).await {
        Ok(()) => true,
        Err(e) => {
            debug!("reply write failed: {e}");
            false
        }
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
            identities: vec![IdentityRule { name: "t".into(), sources: vec!["127.0.0.1/32".into()], clients: vec![], workspaces: vec!["ws-*".into()] }],
            tls: None,
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

    /// A hub restart drops every session; a client that stays idle sends
    /// nothing that reaches the hub again. The keepalive must re-attach
    /// its backend under the same clientid (state intact), or the hub reaps
    /// it a lease later, locks and all (step3-drills.sh `idlerestart`).
    #[tokio::test]
    async fn the_keepalive_reattaches_an_idle_client_after_the_hub_drops_its_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let (hub_addr, hub_state) = hub_with_state(dir.path()).await;
        let (p, addr) = proxy(dir.path(), &hub_addr).await;
        let (c, sid) = client(&addr).await;
        let x = [seq(sid, 0, 1), op_putfh(route::PSEUDO_ROOT_FH), op_lookup("ws-a"), op_mkdir("a")];
        let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
        let body = c.call(&proxy_cred(), &wire::encode_compound(b"", 2, &refs)).await.unwrap();
        assert_eq!(&body[0..4], &[0, 0, 0, 0]);
        let backend = hub_state.clients.id_of_owner(b"flint-proxy/test-client").unwrap();
        assert!(hub_state.sessions.session_count_for_client(backend) > 0);

        // What the hub's restart does to the backend: its sessions are gone.
        hub_state.sessions.destroy_client_sessions(backend);
        for h in p.keepalive_tick(std::time::Duration::ZERO) {
            h.await.unwrap();
        }
        assert_eq!(hub_state.clients.id_of_owner(b"flint-proxy/test-client"), Some(backend), "the same client, state intact");
        assert!(
            hub_state.sessions.session_count_for_client(backend) > 0,
            "re-attached with a new control session — otherwise nothing renews it again"
        );
        // And the next keepalive renews it.
        for h in p.keepalive_tick(std::time::Duration::ZERO) {
            h.await.unwrap();
        }
        assert!(hub_state.sessions.session_count_for_client(backend) > 0);
    }

    /// §4 Leases, measured (step3-drills.sh `revoke`, Linux 6.12): a hub's
    /// EXPIRED_ALL (0x08), EXPIRED_SOME (0x10) or ADMIN (0x20)
    /// _STATE_REVOKED surfaced downstream cost the lock and the writes of
    /// ANOTHER workspace; RECALLABLE_STATE_REVOKED (0x40) did not. Only
    /// 0x40 may pass; the rest are left to the hub's per-op errors.
    #[tokio::test]
    async fn only_a_per_state_revocation_flag_reaches_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let (hub_addr, hub_state) = hub_with_state(dir.path()).await;
        let (_p, addr) = proxy(dir.path(), &hub_addr).await;
        let (c, sid) = client(&addr).await;
        let flags = |slot_seq: u32| {
            let c = c.clone();
            async move {
                let x = [seq(sid, 0, slot_seq), op_putfh(route::PSEUDO_ROOT_FH), op_lookup("ws-a"), wire::op_getfh()];
                let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
                let body = c.call(&proxy_cred(), &wire::encode_compound(b"", 2, &refs)).await.unwrap();
                let mut d = XdrDecoder::new(body);
                let (_, _, _) = (d.decode_u32(), d.decode_opaque(), d.decode_u32());
                let (st, r) = wire::decode_sequence_res(&mut d).unwrap();
                assert_eq!(st, Nfs4Status::Ok);
                r.unwrap().status_flags
            }
        };
        assert_eq!(flags(1).await, 0);
        let backend = hub_state.clients.id_of_owner(b"flint-proxy/test-client").unwrap();
        hub_state.raise_seq_flags(backend, 0x08 | 0x10 | 0x20 | 0x40);
        assert_eq!(flags(2).await, 0x40, "the client-wide revocation bits must stop at the proxy");
    }

    // ---- §6a RPC-with-TLS ----

    use crate::nfs_proxy::tls::testpki;

    /// A proxy that requires TLS, trusting `client_ca` for clients.
    async fn tls_proxy(dir: &std::path::Path, hub_addr: &str, rules: Vec<IdentityRule>) -> (testpki::Ca, String) {
        let ca = testpki::ca("flint-test-ca");
        let srv = ca.server("proxy");
        let pki = dir.join("pki");
        std::fs::create_dir_all(&pki).unwrap();
        std::fs::write(pki.join("tls.crt"), &srv.cert_pem).unwrap();
        std::fs::write(pki.join("tls.key"), &srv.key_pem).unwrap();
        std::fs::write(pki.join("ca.crt"), &ca.pem).unwrap();
        let cfg = ProxyConfig {
            listen: "127.0.0.1:0".into(),
            lease_secs: 90,
            keepalive: false,
            state_dir: dir.join("proxy"),
            hubs: vec![HubRow { name: "ws-a".into(), address: hub_addr.into(), server_id: HUB_ID, stateid_tag: 10, share: None, wakeable: true }],
            kube: None,
            identities: rules,
            tls: Some(TlsConfig { cert: pki.join("tls.crt"), key: pki.join("tls.key"), client_ca: pki.join("ca.crt"), reload_secs: 3600 }),
        };
        let p = Proxy::new(&cfg).await.unwrap();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(p.serve_on(l));
        (ca, addr)
    }

    fn cert_rule(uri: &str) -> IdentityRule {
        IdentityRule { name: "c".into(), sources: vec![], clients: vec![uri.into()], workspaces: vec!["ws-*".into()] }
    }

    /// One raw RPC call record, NULL or COMPOUND, with this credential flavor.
    fn raw_call(xid: u32, proc_: u32, flavor: u32, args: &[u8]) -> Vec<u8> {
        let mut e = XdrEncoder::new();
        for w in [xid, 0, 2, wire::NFS_PROGRAM, wire::NFS_V4, proc_, flavor, 0, 0, 0] {
            e.encode_u32(w);
        }
        e.append_raw(args);
        let m = e.finish();
        let mut r = (0x8000_0000u32 | m.len() as u32).to_be_bytes().to_vec();
        r.extend_from_slice(&m);
        r
    }

    async fn read_reply(s: &mut TcpStream) -> Bytes {
        let mut rr = RecordReader::new("test".into());
        match tokio::time::timeout(std::time::Duration::from_secs(5), rr.next(s, None)).await {
            Ok(Ok(NextRecord::Record(r))) => r,
            Ok(Ok(_)) => panic!("no reply: the connection closed"),
            Ok(Err(e)) => panic!("no reply: {e}"),
            Err(_) => panic!("no reply within 5 s"),
        }
    }

    /// The RFC 9289 client side: the AUTH_TLS probe, then the handshake
    /// with `leaf` (if any) as the client certificate.
    async fn tls_connect(addr: &str, ca: &testpki::Ca, leaf: Option<&testpki::Leaf>) -> Arc<HubConn> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&raw_call(7, 0, 7, &[])).await.unwrap();
        let r = read_reply(&mut s).await;
        assert_eq!(&r[16..28], &[&8u32.to_be_bytes()[..], b"STARTTLS"].concat()[..], "the probe is answered STARTTLS");
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(ca.pem.as_bytes()).unwrap()).unwrap();
        let b = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots);
        let mut cc = match leaf {
            Some(l) => b
                .with_client_auth_cert(
                    vec![CertificateDer::from_pem_slice(l.cert_pem.as_bytes()).unwrap()],
                    PrivateKeyDer::from_pem_slice(l.key_pem.as_bytes()).unwrap(),
                )
                .unwrap(),
            None => b.with_no_client_auth(),
        };
        cc.alpn_protocols = vec![b"sunrpc".to_vec()];
        let tls = tokio_rustls::TlsConnector::from(Arc::new(cc))
            .connect(ServerName::try_from("proxy").unwrap(), s)
            .await
            .unwrap();
        HubConn::over(addr, tls)
    }

    async fn session_on(c: &Arc<HubConn>) -> Result<SessionId, BackendError> {
        let one = |op: Vec<u8>| {
            let c = c.clone();
            async move { c.call(&proxy_cred(), &wire::encode_compound(b"", 1, &[&op])).await }
        };
        let (_, x) = wire::decode_exchange_id_reply(one(wire::op_exchange_id(*b"verifier", b"tls-client", 0)).await?).unwrap();
        let x = x.unwrap();
        let fore = ChannelAttrs { max_requests: 4, ..ChannelAttrs::default() };
        let (_, cs) = wire::decode_create_session_reply(one(wire::op_create_session(x.clientid, x.sequenceid, &fore)).await?).unwrap();
        Ok(cs.unwrap().sessionid)
    }

    async fn lookup_ws_a(c: &Arc<HubConn>, sid: SessionId) -> Bytes {
        let x = [seq(sid, 0, 1), op_putfh(route::PSEUDO_ROOT_FH), op_lookup("ws-a"), wire::op_getfh()];
        let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
        c.call(&proxy_cred(), &wire::encode_compound(b"", 2, &refs)).await.unwrap()
    }

    /// §6a end to end in-process: probe → STARTTLS → handshake with a
    /// client certificate → the URI SAN picks the identity rule → the
    /// crossing reaches the hub. The control is the SAME proxy and CA
    /// with a certificate naming another client: its root has no ws-a.
    #[tokio::test]
    async fn an_mtls_client_is_its_certificate_and_sees_its_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let hub_addr = hub(dir.path()).await;
        let (ca, addr) = tls_proxy(dir.path(), &hub_addr, vec![cert_rule("spiffe://clusters/a")]).await;

        let c = tls_connect(&addr, &ca, Some(&ca.client(Some("spiffe://clusters/a")))).await;
        let sid = session_on(&c).await.unwrap();
        let body = lookup_ws_a(&c, sid).await;
        assert_eq!(&body[0..4], &[0, 0, 0, 0], "cluster a crosses into ws-a");

        let other = tls_connect(&addr, &ca, Some(&ca.client(Some("spiffe://clusters/b")))).await;
        let sid = session_on(&other).await.unwrap();
        let body = lookup_ws_a(&other, sid).await;
        assert_eq!(&body[0..4], &(Nfs4Status::NoEnt as u32).to_be_bytes(), "cluster b: ws-a does not exist");
    }

    /// §6a: a client belongs to the identity that registered it. Cluster
    /// b, on its own TLS connection, names cluster a's session (ids are a
    /// counter: guessable): the SEQUENCE is BADSESSION, as for an absent
    /// session, and DESTROY_SESSION / CREATE_SESSION / DESTROY_CLIENTID
    /// naming a's ids fail the same way. The control: a's own session
    /// keeps working afterwards, on a's connection.
    #[tokio::test]
    async fn another_identitys_session_and_client_look_absent() {
        let dir = tempfile::tempdir().unwrap();
        let hub_addr = hub(dir.path()).await;
        let rules = vec![cert_rule("spiffe://clusters/a"), cert_rule("spiffe://clusters/b")];
        let (ca, addr) = tls_proxy(dir.path(), &hub_addr, rules).await;
        let a = tls_connect(&addr, &ca, Some(&ca.client(Some("spiffe://clusters/a")))).await;
        let sid_a = session_on(&a).await.unwrap();
        let b = tls_connect(&addr, &ca, Some(&ca.client(Some("spiffe://clusters/b")))).await;
        let _sid_b = session_on(&b).await.unwrap();
        let status = |body: Bytes| u32::from_be_bytes(body[0..4].try_into().unwrap());
        let bad_session = Nfs4Status::BadSession as u32;

        let body = lookup_ws_a(&b, sid_a).await;
        assert_eq!(status(body), bad_session, "b drives a's session: BADSESSION");

        let mut e = XdrEncoder::new();
        e.encode_u32(opcode::DESTROY_SESSION);
        e.append_raw(&sid_a.0);
        let op = e.finish().to_vec();
        let body = b.call(&proxy_cred(), &wire::encode_compound(b"", 1, &[&op])).await.unwrap();
        assert_eq!(status(body), bad_session, "b destroys a's session: BADSESSION");

        let cid_a = u64::from_be_bytes(sid_a.0[8..16].try_into().unwrap());
        let mut e = XdrEncoder::new();
        e.encode_u32(opcode::DESTROY_CLIENTID);
        e.encode_u64(cid_a);
        let op = e.finish().to_vec();
        let body = b.call(&proxy_cred(), &wire::encode_compound(b"", 1, &[&op])).await.unwrap();
        assert_eq!(status(body), Nfs4Status::StaleClientId as u32, "b destroys a's client: STALE_CLIENTID");

        let fore = ChannelAttrs { max_requests: 4, ..ChannelAttrs::default() };
        let body = b.call(&proxy_cred(), &wire::encode_compound(b"", 1, &[&wire::op_create_session(cid_a, 2, &fore)])).await.unwrap();
        assert_eq!(status(body), Nfs4Status::StaleClientId as u32, "b opens a session on a's client: STALE_CLIENTID");

        let x = [seq(sid_a, 0, 1), op_putfh(route::PSEUDO_ROOT_FH), op_lookup("ws-a"), wire::op_getfh()];
        let refs: Vec<&[u8]> = x.iter().map(|o| o.as_slice()).collect();
        let body = a.call(&proxy_cred(), &wire::encode_compound(b"", 2, &refs)).await.unwrap();
        assert_eq!(status(body), 0, "control: a's session is intact and a still crosses into ws-a");
    }

    /// Same host owner, two identities: two clients, not one — the
    /// prefix is part of the owner the dispatcher keys on.
    #[test]
    fn the_identity_prefix_is_stable_and_separates_owners() {
        let p = |u: &[&str]| identity_prefix(&Peer { addr: "10.0.0.1".parse().unwrap(), uris: u.iter().map(|s| s.to_string()).collect() });
        let a = p(&["spiffe://clusters/a"]).unwrap();
        assert_eq!(a, p(&["spiffe://clusters/a"]).unwrap());
        assert_ne!(a, p(&["spiffe://clusters/b"]).unwrap());
        assert_eq!(p(&["u2", "u1"]), p(&["u1", "u2"]), "order of SANs does not matter");
        assert_eq!(p(&[]), None, "plaintext: no prefix");
        // Pinned: the prefix is persisted and sent to hubs, so a change
        // of digest is a change of every client's identity.
        assert_eq!(a, b"id:cfcbe7d9df2b4dca/".to_vec(), "sha256(uri)[..8]");
        assert!(owner_is(&[a.as_slice(), b"Linux NFS"].concat(), Some(&a)));
        assert!(!owner_is(&[a.as_slice(), b"Linux NFS"].concat(), None), "plaintext does not own a certified client");
        assert!(owner_is(b"Linux NFS", None));
    }

    /// §6a: "A connection that does not upgrade is refused." EXCHANGE_ID
    /// in the clear is AUTH_TOOWEAK (MSG_DENIED / AUTH_ERROR); a NULL
    /// ping is still answered (the control: the connection is served,
    /// only refused).
    #[tokio::test]
    async fn a_connection_that_does_not_upgrade_is_refused_tooweak() {
        let dir = tempfile::tempdir().unwrap();
        let hub_addr = hub(dir.path()).await;
        let (_ca, addr) = tls_proxy(dir.path(), &hub_addr, vec![cert_rule("spiffe://clusters/a")]).await;
        let mut s = TcpStream::connect(&addr).await.unwrap();
        s.write_all(&raw_call(1, 0, 1, &[])).await.unwrap();
        let r = read_reply(&mut s).await;
        assert_eq!(&r[4..12], &[0, 0, 0, 1, 0, 0, 0, 0], "control: NULL is accepted");
        let x = wire::op_exchange_id(*b"verifier", b"plain", 0);
        s.write_all(&raw_call(2, 1, 1, &wire::encode_compound(b"", 1, &[&x]))).await.unwrap();
        let r = read_reply(&mut s).await;
        let want: Vec<u8> = [2u32, 1, 1, 1, 5].iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!(r.as_ref(), want.as_slice(), "xid 2, REPLY, MSG_DENIED, AUTH_ERROR, AUTH_TOOWEAK");
    }

    /// The certificate must chain to the configured CA, and there must
    /// be one. A refused handshake surfaces at the client's first call
    /// (TLS 1.3 verifies the client after the client's Finished).
    #[tokio::test]
    async fn a_certificate_from_another_ca_or_none_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let hub_addr = hub(dir.path()).await;
        let (ca, addr) = tls_proxy(dir.path(), &hub_addr, vec![cert_rule("spiffe://clusters/a")]).await;
        let rogue = testpki::ca("rogue");
        let c = tls_connect(&addr, &ca, Some(&rogue.client(Some("spiffe://clusters/a")))).await;
        assert!(session_on(&c).await.is_err(), "a certificate from another CA");
        let c = tls_connect(&addr, &ca, None).await;
        assert!(session_on(&c).await.is_err(), "no certificate");
        let c = tls_connect(&addr, &ca, Some(&ca.client(Some("spiffe://clusters/a")))).await;
        assert!(session_on(&c).await.is_ok(), "control: the CA's certificate is served");
    }

    /// Without `tls`, the probe is a plain NULL: accepted with an EMPTY
    /// verifier, which RFC 9289 §4.1 reads as "no TLS here" — before
    /// AUTH_TLS decoded, the probe got no reply at all and the mount
    /// waited out a timeout.
    #[tokio::test]
    async fn without_tls_the_probe_is_answered_without_starttls() {
        let dir = tempfile::tempdir().unwrap();
        let hub_addr = hub(dir.path()).await;
        let (_p, addr) = proxy(dir.path(), &hub_addr).await;
        let mut s = TcpStream::connect(&addr).await.unwrap();
        s.write_all(&raw_call(9, 0, 7, &[])).await.unwrap();
        let r = read_reply(&mut s).await;
        let want: Vec<u8> = [9u32, 1, 0, 0, 0, 0].iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!(r.as_ref(), want.as_slice(), "accepted, AUTH_NONE verifier with no body, SUCCESS");
        // And a hub answers it the same way.
        let mut h = TcpStream::connect(&hub_addr).await.unwrap();
        h.write_all(&raw_call(9, 0, 7, &[])).await.unwrap();
        assert_eq!(read_reply(&mut h).await.as_ref(), want.as_slice());
    }

    #[test]
    fn a_clients_rule_without_tls_is_a_config_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = ProxyConfig {
            listen: "127.0.0.1:0".into(),
            lease_secs: 90,
            keepalive: false,
            state_dir: dir.path().join("proxy"),
            hubs: vec![],
            kube: None,
            identities: vec![cert_rule("spiffe://clusters/a")],
            tls: None,
        };
        assert!(rt.block_on(Proxy::new(&cfg)).is_err());
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
        assert!(cfg.tls.is_none(), "tls off unless the chart renders it");
    }

    /// The same, with `nfsProxy.tls.enabled` and a `clients:` rule
    /// (`helm template` output verbatim): the paths are where the chart
    /// mounts the Secret and the CA bundle.
    #[test]
    fn the_charts_rendered_tls_config_parses() {
        let rendered = "listen: 0.0.0.0:2049\nstateDir: /var/lib/flint-nfs-proxy\nleaseSecs: 90\nkube:\n  {}\nidentities:\n  - clients:\n    - spiffe://clusters/a\n    name: a\n    workspaces:\n    - ws-*\ntls:\n  cert: /etc/flint-nfs-proxy/tls/tls.crt\n  key: /etc/flint-nfs-proxy/tls/tls.key\n  clientCa: /etc/flint-nfs-proxy/client-ca/ca.crt\n  reloadSecs: 30\n";
        let cfg: ProxyConfig = serde_yaml::from_str(rendered).unwrap();
        let t = cfg.tls.unwrap();
        assert_eq!(t.cert, PathBuf::from("/etc/flint-nfs-proxy/tls/tls.crt"));
        assert_eq!(t.key, PathBuf::from("/etc/flint-nfs-proxy/tls/tls.key"));
        assert_eq!(t.client_ca, PathBuf::from("/etc/flint-nfs-proxy/client-ca/ca.crt"));
        assert_eq!(t.reload_secs, 30);
        assert_eq!(cfg.identities[0].clients, vec!["spiffe://clusters/a"]);
        assert!(cfg.identities[0].sources.is_empty());
    }
}
