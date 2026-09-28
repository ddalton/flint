//! The proxy as an NFSv4.1 CLIENT of each hub (nfs-proxy design §4).
//!
//! [`HubConn`] is one TCP connection to one hub, shared by every backend
//! client on it: calls are multiplexed by xid, replies may come back in
//! any order. [`BackendClient`] is the proxy's identity toward ONE hub on
//! behalf of ONE downstream client — `co_ownerid = "flint-proxy/" ‖ the
//! client's`, verifier = the client's — so a client's open and lock
//! owners stay within one clientid on every hub, and a client reboot
//! (new verifier) reaches each hub as the same case 5 a direct mount
//! would. [`BackendSession`] backs ONE downstream session on that hub:
//! downstream slot s is backend slot s, so a retransmission can be sent
//! with the same backend (slot, seqid) and the hub's reply cache answers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use super::wire::{self, HubReply, SeqArgs};
use crate::nfs::ingress::{NextRecord, RecordReader};
use crate::nfs::rpc::{Auth, AuthFlavor};
use crate::nfs::v4::compound::ChannelAttrs;
use crate::nfs::v4::protocol::{Nfs4FileHandle, Nfs4Status, SessionId};
use crate::nfs::xdr::XdrEncoder;

#[derive(Debug)]
pub enum BackendError {
    /// The connection is gone (refused, reset, closed): the hub may be
    /// parked or restarting. The caller wakes it / retries (§4).
    Down(String),
    /// The hub answered, but not as NFSv4.1 would.
    Protocol(String),
    /// A session-setup op failed with this status.
    Status(&'static str, Nfs4Status),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Down(s) => write!(f, "hub down: {s}"),
            BackendError::Protocol(s) => write!(f, "hub protocol: {s}"),
            BackendError::Status(op, s) => write!(f, "{op}: {s:?}"),
        }
    }
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Bytes>>>>;

/// One connection to one hub; calls multiplexed by xid.
pub struct HubConn {
    addr: String,
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending: Pending,
    next_xid: AtomicU32,
    closed: Arc<AtomicBool>,
}

impl HubConn {
    pub async fn connect(addr: &str) -> Result<Arc<Self>, BackendError> {
        let stream = TcpStream::connect(addr).await.map_err(|e| BackendError::Down(format!("{addr}: {e}")))?;
        let _ = stream.set_nodelay(true);
        let (mut rd, wr) = stream.into_split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let conn = Arc::new(HubConn {
            addr: addr.to_string(),
            writer: tokio::sync::Mutex::new(wr),
            pending: pending.clone(),
            // xids only need to be unique per connection
            next_xid: AtomicU32::new(rand::random::<u32>() | 1),
            closed: closed.clone(),
        });
        let label = format!("hub {addr}");
        tokio::spawn(async move {
            let mut rr = RecordReader::new(label.clone());
            loop {
                match rr.next(&mut rd, None).await {
                    Ok(NextRecord::Record(rec)) if rec.len() >= 4 => {
                        let xid = u32::from_be_bytes(rec[0..4].try_into().unwrap());
                        match pending.lock().unwrap().remove(&xid) {
                            Some(tx) => {
                                let _ = tx.send(rec);
                            }
                            // A reply nobody waits for (the caller gave
                            // up), or a callback: v1 relays none (§2).
                            None => debug!("{label}: unmatched record xid {xid:#x}"),
                        }
                    }
                    Ok(NextRecord::Record(_)) => {}
                    Ok(NextRecord::Closed) | Ok(NextRecord::IdleClosed) => break,
                    Err(e) => {
                        warn!("{label}: read failed: {e}");
                        break;
                    }
                }
            }
            closed.store(true, Ordering::SeqCst);
            // Dropping the senders fails every waiter: no call hangs on a
            // dead connection.
            pending.lock().unwrap().clear();
        });
        Ok(conn)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// One COMPOUND; the COMPOUND4res body on an RPC success.
    pub async fn call(&self, cred: &Auth, compound: &[u8]) -> Result<Bytes, BackendError> {
        if self.is_closed() {
            return Err(BackendError::Down(format!("{}: connection closed", self.addr)));
        }
        let xid = self.next_xid.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(xid, tx);
        let msg = wire::encode_call(xid, cred, compound);
        let mut rec = Vec::with_capacity(4 + msg.len());
        rec.extend_from_slice(&(0x8000_0000u32 | msg.len() as u32).to_be_bytes());
        rec.extend_from_slice(&msg);
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.write_all(&rec).await {
                self.pending.lock().unwrap().remove(&xid);
                return Err(BackendError::Down(format!("{}: {e}", self.addr)));
            }
        }
        // The reader may have exited between the check above and the
        // insert: it cleared the map before our sender went in.
        if self.is_closed() {
            self.pending.lock().unwrap().remove(&xid);
        }
        let reply = rx.await.map_err(|_| BackendError::Down(format!("{}: connection closed", self.addr)))?;
        wire::decode_reply(reply, xid).map_err(BackendError::Protocol)
    }
}

/// The proxy's own credential for the calls it originates (session setup,
/// `RECLAIM_COMPLETE`, learning a root). Client ops carry the client's.
pub fn proxy_cred() -> Auth {
    let mut e = XdrEncoder::new();
    e.encode_u32(0); // stamp
    e.encode_string("flint-proxy");
    e.encode_u32(0); // uid
    e.encode_u32(0); // gid
    e.encode_u32(0); // gids<>
    Auth { flavor: AuthFlavor::Unix, body: e.finish() }
}

/// The backend `co_ownerid` for a downstream client's (§4).
pub fn backend_owner(downstream: &[u8]) -> Vec<u8> {
    let mut o = b"flint-proxy/".to_vec();
    o.extend_from_slice(downstream);
    o
}

/// One backend session: backs one downstream session on one hub.
pub struct BackendSession {
    pub sessionid: SessionId,
    /// The last seqid SENT on each backend slot.
    seqs: Mutex<Vec<u32>>,
}

impl BackendSession {
    /// The seqid for a NEW request on `slot` (not a retransmission).
    pub fn next_seq(&self, slot: u32) -> Option<u32> {
        let mut s = self.seqs.lock().unwrap();
        let v = s.get_mut(slot as usize)?;
        *v = v.wrapping_add(1);
        Some(*v)
    }

    pub fn slots(&self) -> u32 {
        self.seqs.lock().unwrap().len() as u32
    }
}

/// The proxy's identity toward one hub for one downstream client.
pub struct BackendClient {
    pub conn: Arc<HubConn>,
    pub clientid: u64,
    /// The next CREATE_SESSION sequence (RFC 8881 §18.36: starts at the
    /// EXCHANGE_ID result's `eir_sequenceid`).
    cs_seq: Mutex<u32>,
    reclaimed: AtomicBool,
}

async fn one_op(conn: &HubConn, op: Vec<u8>) -> Result<Bytes, BackendError> {
    conn.call(&proxy_cred(), &wire::encode_compound(b"flint-proxy", 1, &[&op])).await
}

impl BackendClient {
    /// EXCHANGE_ID as `flint-proxy/‖owner` with the downstream verifier.
    pub async fn register(conn: Arc<HubConn>, owner: &[u8], verifier: [u8; 8], flags: u32) -> Result<Arc<Self>, BackendError> {
        // CONFIRMED_R is a reply flag; the rest mirror the client.
        let flags = flags & !0x8000_0000;
        let body = one_op(&conn, wire::op_exchange_id(verifier, &backend_owner(owner), flags)).await?;
        let (s, ok) = wire::decode_exchange_id_reply(body).map_err(BackendError::Protocol)?;
        let ok = ok.ok_or(BackendError::Status("EXCHANGE_ID", s))?;
        Ok(Arc::new(BackendClient { conn, clientid: ok.clientid, cs_seq: Mutex::new(ok.sequenceid), reclaimed: AtomicBool::new(false) }))
    }

    /// A backend session on the downstream session's own fore channel
    /// (its slot count, sizes and op limit), so anything the client may
    /// send fits: downstream slot s = backend slot s. A hub that grants
    /// fewer slots is refused: the identity mapping would break.
    pub async fn create_session(&self, fore: &ChannelAttrs) -> Result<Arc<BackendSession>, BackendError> {
        let seq = *self.cs_seq.lock().unwrap();
        let slots = fore.max_requests;
        let body = one_op(&self.conn, wire::op_create_session(self.clientid, seq, fore)).await?;
        let (s, ok) = wire::decode_create_session_reply(body).map_err(BackendError::Protocol)?;
        let ok = ok.ok_or(BackendError::Status("CREATE_SESSION", s))?;
        *self.cs_seq.lock().unwrap() = seq.wrapping_add(1);
        if ok.fore_max_requests < slots {
            return Err(BackendError::Protocol(format!(
                "hub granted {} slots, the downstream session has {slots}",
                ok.fore_max_requests
            )));
        }
        let sess = Arc::new(BackendSession { sessionid: ok.sessionid, seqs: Mutex::new(vec![0; slots as usize]) });
        if !self.reclaimed.swap(true, Ordering::SeqCst) {
            // §4: the proxy sends RECLAIM_COMPLETE on its backend clients
            // itself; it never reclaims (state survives a proxy restart
            // on the hub, under the same owner and verifier).
            // COMPLETE_ALREADY after a proxy restart is fine.
            self.sequenced(&sess, 0, &[&wire::op_reclaim_complete(false)]).await?;
            debug!("backend RECLAIM_COMPLETE on {}: clientid {:#x}", self.conn.addr(), self.clientid);
        }
        Ok(sess)
    }

    /// A proxy-originated compound on `slot`: SEQUENCE + `ops`.
    async fn sequenced(&self, sess: &BackendSession, slot: u32, ops: &[&[u8]]) -> Result<Bytes, BackendError> {
        let seq = sess.next_seq(slot).ok_or_else(|| BackendError::Protocol("slot out of range".into()))?;
        let s = wire::op_sequence(&SeqArgs { sessionid: sess.sessionid, sequenceid: seq, slotid: slot, highest_slotid: slot, cachethis: false });
        let mut all: Vec<&[u8]> = vec![&s];
        all.extend_from_slice(ops);
        self.conn.call(&proxy_cred(), &wire::encode_compound(b"flint-proxy", 1, &all)).await
    }

    /// The hub's root filehandle (`PUTROOTFH, GETFH`).
    pub async fn root_fh(&self, sess: &BackendSession) -> Result<Nfs4FileHandle, BackendError> {
        let body = self.sequenced(sess, 0, &[&wire::op_putrootfh(), &wire::op_getfh()]).await?;
        wire::decode_root_fh_reply(body)
            .map_err(BackendError::Protocol)?
            .map_err(|s| BackendError::Status("PUTROOTFH/GETFH", s))
    }

    /// Forward a client's ops. `seq` is `Some` for a retransmission (the
    /// backend seqid the original was sent with), `None` for a new
    /// request. Returns the backend seqid used, and the hub's reply.
    #[allow(clippy::too_many_arguments)]
    pub async fn forward(
        &self,
        sess: &BackendSession,
        slot: u32,
        seq: Option<u32>,
        cachethis: bool,
        cred: &Auth,
        tag: &[u8],
        minor_version: u32,
        putrootfh: bool,
        ops: &[&[u8]],
    ) -> Result<(u32, HubReply), BackendError> {
        let seq = match seq {
            Some(s) => s,
            None => sess.next_seq(slot).ok_or_else(|| BackendError::Protocol("slot out of range".into()))?,
        };
        let s = wire::op_sequence(&SeqArgs {
            sessionid: sess.sessionid,
            sequenceid: seq,
            slotid: slot,
            highest_slotid: sess.slots() - 1,
            cachethis,
        });
        let root = wire::op_putrootfh();
        let mut all: Vec<&[u8]> = vec![&s];
        if putrootfh {
            all.push(&root);
        }
        all.extend_from_slice(ops);
        let body = self.conn.call(cred, &wire::encode_compound(tag, minor_version, &all)).await?;
        let reply = wire::parse_hub_reply(body, putrootfh).map_err(BackendError::Protocol)?;
        Ok((seq, reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// A fake hub that answers each call's xid with a success reply whose
    /// body is the call's first COMPOUND op word — and answers them in
    /// REVERSE order, so a demultiplexer that matched replies by arrival
    /// order would hand each caller the other's reply.
    async fn reversing_hub(n: usize) -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut calls = Vec::new();
            for _ in 0..n {
                let mut m = [0u8; 4];
                s.read_exact(&mut m).await.unwrap();
                let len = (u32::from_be_bytes(m) & 0x7fff_ffff) as usize;
                let mut rec = vec![0u8; len];
                s.read_exact(&mut rec).await.unwrap();
                calls.push(rec);
            }
            for rec in calls.into_iter().rev() {
                let xid = &rec[0..4];
                let last = &rec[rec.len() - 4..]; // the op word we sent
                let mut r = xid.to_vec();
                for w in [1u32, 0, 0, 0, 0] {
                    r.extend_from_slice(&w.to_be_bytes());
                }
                r.extend_from_slice(last);
                let mut out = (0x8000_0000u32 | r.len() as u32).to_be_bytes().to_vec();
                out.extend_from_slice(&r);
                s.write_all(&out).await.unwrap();
            }
        });
        addr
    }

    #[tokio::test]
    async fn replies_reach_their_own_callers_whatever_order_they_arrive_in() {
        let addr = reversing_hub(3).await;
        let c = HubConn::connect(&addr).await.unwrap();
        let call = |w: u32| {
            let c = c.clone();
            async move { c.call(&Auth::null(), &w.to_be_bytes()).await.unwrap() }
        };
        let (a, b, d) = tokio::join!(call(11), call(22), call(33));
        assert_eq!((&a[..], &b[..], &d[..]), (&11u32.to_be_bytes()[..], &22u32.to_be_bytes()[..], &33u32.to_be_bytes()[..]));
    }

    #[tokio::test]
    async fn a_dead_hub_fails_its_callers_instead_of_hanging_them() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(s);
        });
        let c = HubConn::connect(&addr).await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), c.call(&Auth::null(), &[0, 0, 0, 1])).await;
        assert!(matches!(r, Ok(Err(BackendError::Down(_)))), "{r:?}");
        assert!(c.is_closed());
        assert!(matches!(c.call(&Auth::null(), &[0; 4]).await, Err(BackendError::Down(_))));
    }

    #[test]
    fn the_backend_owner_is_the_clients_behind_a_proxy_prefix() {
        assert_eq!(backend_owner(b"Linux NFSv4.2 node-1"), b"flint-proxy/Linux NFSv4.2 node-1");
    }
}
