//! pNFS callback fan-out: CB_LAYOUTRECALL over the back-channel.
//!
//! The shape of one recall today is:
//!
//! ```text
//!   look up session_id → BackChannelWriter (dispatcher's
//!     back_channels registry, populated by BIND_CONN_TO_SESSION)
//!   look up session_id → cb_program          (Session record,
//!     populated by CREATE_SESSION csa_cb_program)
//!   build CB_COMPOUND { CB_SEQUENCE, CB_LAYOUTRECALL(file) }
//!   writer.send_cb_compound(...) → await reply (typed CbCompoundReply)
//! ```
//!
//! All four of those moving parts already exist by the time A.3
//! ships:
//!
//! * Phase A.1 plumbed `BackChannelWriter` and the dispatcher's
//!   `back_channels` registry.
//! * Phase A.2 added `Session.cb_program` and the typed
//!   `CbCompoundCall`/`CbCompoundReply` round-trip.
//! * `BackChannelWriter::send_cb_compound` (this PR) glues them
//!   together with the inflight-xid registry and read-loop reply
//!   routing.
//!
//! `CallbackManager` itself is the seam pNFS code uses — Phase A.4
//! will call into it from the device heartbeat to fire recalls on
//! DS death.
//!
//! # Protocol references
//! * RFC 8881 §20.3 — CB_LAYOUTRECALL operation.
//! * RFC 8881 §12.5.5 — Layout recall semantics.
//! * RFC 8881 §20.9   — CB_SEQUENCE (must precede CB_LAYOUTRECALL).

use crate::nfs::v4::back_channel::{BackChannelWriter, CallbackError};
use crate::nfs::v4::cb_compound::{CbCompoundCall, CbCompoundReply, CbOp, LayoutRecall};
use crate::nfs::v4::protocol::{SessionId, StateId};
use crate::nfs::v4::state::StateManager;
use crate::pnfs::mds::layout::LayoutStateId;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Default per-call timeout for CB CALLs. RFC 8881 §20.4 ("recall
/// response time") doesn't mandate a value; 10s matches Linux nfsd.
pub const DEFAULT_CB_TIMEOUT: Duration = Duration::from_secs(10);

/// pNFS callback fan-out manager.
///
/// Borrows the dispatcher's per-session back-channel writer registry
/// and the [`StateManager`] (for `Session.cb_program` lookup); both
/// are `Arc`-shared so the manager itself can be cheap to clone /
/// pass around. Construction is failure-free; the actual CB send
/// path can fail a few different ways, all surfaced as
/// [`CallbackError`].
pub struct CallbackManager {
    back_channels: Arc<DashMap<SessionId, Arc<BackChannelWriter>>>,
    state_mgr: Arc<StateManager>,
    timeout: Duration,
}

impl CallbackManager {
    /// `back_channels` is the dispatcher's per-session writer
    /// registry; `state_mgr` is the source of truth for `cb_program`
    /// (stored on `Session` since A.2). Per-call timeout defaults to
    /// [`DEFAULT_CB_TIMEOUT`]; tests can override via
    /// [`with_timeout`].
    pub fn new(
        back_channels: Arc<DashMap<SessionId, Arc<BackChannelWriter>>>,
        state_mgr: Arc<StateManager>,
    ) -> Self {
        Self {
            back_channels,
            state_mgr,
            timeout: DEFAULT_CB_TIMEOUT,
        }
    }

    /// Override the per-call timeout (tests).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send a CB_LAYOUTRECALL for one specific layout to the client
    /// behind `session_id`. Returns the parsed reply on success;
    /// [`CallbackError`] otherwise.
    ///
    /// The returned reply may itself carry a non-OK status (e.g.
    /// `NFS4ERR_NOMATCHING_LAYOUT` when the client already returned
    /// the layout). Callers should treat that as a successful
    /// outcome — the layout is gone from the client either way.
    pub async fn send_layoutrecall(
        &self,
        session_id: &SessionId,
        layout_stateid: &LayoutStateId,
        layout_type: u32,
        iomode: u32,
        changed: bool,
    ) -> Result<CbCompoundReply, CallbackError> {
        // Resolve the writer first — if the session never bound a
        // back-channel, there's nothing to send.
        let writer = match self.back_channels.get(session_id) {
            Some(w) => Arc::clone(w.value()),
            None => {
                warn!(
                    "CB_LAYOUTRECALL: no back-channel for session {:?}",
                    session_id,
                );
                return Err(CallbackError::ConnectionClosed);
            }
        };

        // Resolve cb_program from the session. If the client
        // CREATE_SESSION'd with cb_program=0 ("I won't host
        // callbacks"), bail out — sending a CALL with program=0
        // would just bounce.
        let cb_program = match self.state_mgr.sessions.get_session(session_id) {
            Some(s) if s.cb_program != 0 => s.cb_program,
            Some(_) => {
                warn!(
                    "CB_LAYOUTRECALL: session {:?} advertised cb_program=0",
                    session_id,
                );
                return Err(CallbackError::ConnectionClosed);
            }
            None => {
                warn!("CB_LAYOUTRECALL: session {:?} not found", session_id);
                return Err(CallbackError::ConnectionClosed);
            }
        };

        // Crack the 16-byte LayoutStateId blob (seqid:4 + other:12,
        // big-endian) into the typed StateId the CB encoder takes;
        // wire layout is identical.
        let stateid = StateId {
            seqid: u32::from_be_bytes([
                layout_stateid[0],
                layout_stateid[1],
                layout_stateid[2],
                layout_stateid[3],
            ]),
            other: {
                let mut o = [0u8; 12];
                o.copy_from_slice(&layout_stateid[4..16]);
                o
            },
        };

        let call = CbCompoundCall {
            tag: String::new(),
            minorversion: 1,
            callback_ident: 0,
            ops: vec![
                CbOp::Sequence {
                    sessionid: *session_id,
                    // Slot 0, seqid 1 — back-channel slot tracking
                    // is its own follow-up; for now we serialize
                    // recalls per-session through the writer's
                    // mutex and use a single slot.
                    sequenceid: 1,
                    slotid: 0,
                    highest_slotid: 0,
                    cachethis: false,
                },
                CbOp::LayoutRecall {
                    layout_type,
                    iomode,
                    changed,
                    recall: LayoutRecall::File {
                        // Empty FH = "any layout for this session"
                        // — Linux's client treats this as a session-
                        // wide return, which matches what we want
                        // when a DS dies. Per-file recall is an
                        // optimisation we can layer later.
                        fh: Vec::new(),
                        offset: 0,
                        length: u64::MAX,
                        stateid,
                    },
                },
            ],
        };

        info!(
            "📢 CB_LAYOUTRECALL → session {:?} (cb_program={}, type={}, iomode={})",
            session_id, cb_program, layout_type, iomode,
        );
        let reply = writer
            .send_cb_compound(cb_program, &call, self.timeout)
            .await?;
        info!(
            "✅ CB_LAYOUTRECALL ← session {:?}: status={:?}, {} results",
            session_id,
            reply.status,
            reply.results.len(),
        );
        Ok(reply)
    }

    /// Fire one CB_LAYOUTRECALL per `(session_id, layout_stateid)`
    /// pair. The pairs come from
    /// [`LayoutManager::recall_layouts_for_device`], which already
    /// scoped each layout to its issuing session — we just route
    /// each one to the right back-channel.
    ///
    /// Returns one [`RecallResult`] per input pair, in the same
    /// order. The caller (typically the heartbeat-monitor's
    /// `fan_out_recalls`) inspects each result to decide whether
    /// to forcibly revoke the layout server-side: a `TimedOut`,
    /// `NoChannel`, or `Transport` outcome means the client
    /// either didn't get the recall or won't reply, so the layout
    /// is at risk of staying live with a dead DS — RFC 5661
    /// §12.5.5.2 lets us revoke immediately. `Acked` outcomes get
    /// a soft post-deadline timer instead (also handled by the
    /// caller).
    ///
    /// `device_id` is used only for logging — the routing is fully
    /// driven by the input pairs.
    pub async fn recall_layouts_for_device(
        &self,
        device_id: &str,
        recalls: &[(SessionId, LayoutStateId)],
    ) -> Vec<RecallResult> {
        if recalls.is_empty() {
            return Vec::new();
        }
        info!(
            "📢 Fanning out {} CB_LAYOUTRECALL(s) for failed device: {}",
            recalls.len(),
            device_id,
        );

        let mut results = Vec::with_capacity(recalls.len());
        for (session_id, stateid) in recalls {
            let outcome = match self
                .send_layoutrecall(
                    session_id,
                    stateid,
                    1, // LAYOUT4_NFSV4_1_FILES
                    3, // LAYOUTIOMODE4_ANY
                    true,
                )
                .await
            {
                Ok(_reply) => RecallOutcome::Acked,
                Err(CallbackError::Timeout) => RecallOutcome::TimedOut,
                Err(CallbackError::ConnectionClosed) => RecallOutcome::NoChannel,
                Err(e) => {
                    let msg = e.to_string();
                    warn!(
                        "CB_LAYOUTRECALL to session {:?} failed: {}",
                        session_id, msg,
                    );
                    RecallOutcome::Transport(msg)
                }
            };
            results.push(RecallResult {
                session_id: *session_id,
                stateid: *stateid,
                outcome,
            });
        }
        let acked = results.iter().filter(|r| matches!(r.outcome, RecallOutcome::Acked)).count();
        info!(
            "📊 Device {} fan-out: {}/{} recalls acked",
            device_id,
            acked,
            results.len(),
        );
        results
    }
}

/// Outcome of one CB_LAYOUTRECALL CALL. Used by the heartbeat
/// monitor to decide whether to forcibly revoke each layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallOutcome {
    /// Client replied. Either NFS4_OK or NFS4ERR_NOMATCHING_LAYOUT
    /// — both mean "layout is gone from the client side." The
    /// caller may still want to apply a soft post-recall deadline
    /// for the eventual LAYOUTRETURN.
    Acked,
    /// `CallbackError::Timeout` — no reply within the per-call
    /// deadline. RFC 5661 §12.5.5.2: server MAY revoke.
    TimedOut,
    /// No back-channel was registered for this session (or
    /// cb_program=0). The recall couldn't even leave the server,
    /// so the client never knew — revoke server-side rather than
    /// leave a dangling layout.
    NoChannel,
    /// Some other error: transport failure, RPC rejected,
    /// reply-decode error. Treat the same as `TimedOut` for
    /// revocation purposes; the message is preserved for logs.
    Transport(String),
}

/// One outcome per recall pair. Order matches the input order so
/// the caller can re-pair with the request side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallResult {
    pub session_id: SessionId,
    pub stateid: LayoutStateId,
    pub outcome: RecallOutcome,
}

#[cfg(test)]
mod tests {
    //! Integration-style tests against a real loopback TCP pair.
    //! The "client" side is hand-rolled to read the CB CALL the
    //! server emits, decode it enough to confirm shape, then write
    //! a CB REPLY back. Drives the whole send-and-await path:
    //! dispatcher writer → record-marker framing → mock-client
    //! parse + reply → server read loop → inflight registry →
    //! decoder.

    use super::*;
    use crate::nfs::rpc::{AcceptStatus, AuthFlavor, MessageType, ReplyStatus};
    use crate::nfs::v4::cb_compound::CbResult;
    use crate::nfs::v4::protocol::{cb_opcode, Nfs4Status};
    use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
    use bytes::{Bytes, BytesMut};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
    use tokio::net::{TcpListener, TcpStream};

    /// Make a (writer, server-read-half) pair on a loopback socket
    /// plus the *client* halves so the test can drive both sides.
    /// Returns:
    ///   * `writer`   — the BackChannelWriter the server would
    ///     normally use to push CB CALLs.
    ///   * `server_read` — the read half on the server side, which
    ///     a real server's `handle_tcp_connection` would consume.
    ///   * `client_read` / `client_write` — the read/write halves
    ///     a mock client uses to receive the CALL and emit a REPLY.
    async fn pair() -> (
        Arc<BackChannelWriter>,
        tokio::net::tcp::OwnedReadHalf,
        tokio::net::tcp::OwnedReadHalf,
        tokio::net::tcp::OwnedWriteHalf,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = TcpStream::connect(addr);
        let accept = listener.accept();
        let (server_res, accept_res) = tokio::join!(connect, accept);
        let server_stream = server_res.unwrap();
        let (client_stream, _) = accept_res.unwrap();
        let (server_read, server_write) = server_stream.into_split();
        let (client_read, client_write) = client_stream.into_split();
        let writer = BackChannelWriter::new(BufWriter::with_capacity(4096, server_write));
        (writer, server_read, client_read, client_write)
    }

    /// Read one record-marker-framed message off `r`. Returns the
    /// payload (without the 4-byte marker).
    async fn read_record(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Bytes {
        let mut marker = [0u8; 4];
        r.read_exact(&mut marker).await.unwrap();
        let len = (u32::from_be_bytes(marker) & 0x7FFF_FFFF) as usize;
        let mut body = BytesMut::with_capacity(len);
        body.resize(len, 0);
        r.read_exact(&mut body[..]).await.unwrap();
        body.freeze()
    }

    /// Write one record-marker-framed message onto `w`.
    async fn write_record(w: &mut tokio::net::tcp::OwnedWriteHalf, payload: Bytes) {
        let len = payload.len() as u32;
        let marker = 0x8000_0000u32 | len;
        w.write_all(&marker.to_be_bytes()).await.unwrap();
        w.write_all(&payload).await.unwrap();
        w.flush().await.unwrap();
    }

    /// Build a synthetic CB_COMPOUND reply (RPC envelope + body)
    /// matching `xid`. Two ops: CB_SEQUENCE OK, CB_LAYOUTRECALL
    /// with `recall_status`. This is what a real Linux v4.1
    /// callback handler would emit.
    fn build_reply(xid: u32, recall_status: Nfs4Status) -> Bytes {
        let mut enc = XdrEncoder::new();
        enc.encode_u32(xid);
        enc.encode_u32(MessageType::Reply as u32);
        enc.encode_u32(ReplyStatus::Accepted as u32);
        // verifier: AUTH_NONE, empty body
        enc.encode_u32(AuthFlavor::Null as u32);
        enc.encode_opaque(&[]);
        enc.encode_u32(AcceptStatus::Success as u32);
        // CB_COMPOUND4res
        enc.encode_u32(recall_status.to_u32()); // top-level status mirrors last op
        enc.encode_opaque(&[]); // tag
        enc.encode_u32(2); // resarray<>.len
        // CB_SEQUENCE result OK. CB_SEQUENCE4resok layout:
        // sessionid (16 bytes = 4 u32s) + sequenceid + slotid +
        // highest_slotid + target_highest_slotid = 8 u32s total.
        enc.encode_u32(cb_opcode::CB_SEQUENCE);
        enc.encode_u32(Nfs4Status::Ok.to_u32());
        for _ in 0..8 {
            enc.encode_u32(0);
        }
        // CB_LAYOUTRECALL result
        enc.encode_u32(cb_opcode::CB_LAYOUTRECALL);
        enc.encode_u32(recall_status.to_u32());
        enc.finish()
    }

    /// Spawn a "server read loop" that mimics handle_tcp_connection's
    /// REPLY routing: read records, dispatch by msg_type, deliver
    /// REPLYs to the writer's inflight registry. Returns the join
    /// handle so the test can cancel it on completion.
    fn spawn_read_loop(
        writer: Arc<BackChannelWriter>,
        server_read: tokio::net::tcp::OwnedReadHalf,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut r = BufReader::new(server_read);
            loop {
                let body = match try_read_record(&mut r).await {
                    Some(b) => b,
                    None => break,
                };
                if body.len() < 8 {
                    continue;
                }
                let msg_type =
                    u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                if msg_type == 1 {
                    let xid =
                        u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    writer.deliver_reply(xid, body);
                }
            }
            writer.drop_all_inflight();
        })
    }

    /// Like `read_record` but returns None on clean EOF instead of
    /// panicking — the loop spawned above needs to terminate
    /// gracefully when the test drops the client side.
    async fn try_read_record(
        r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    ) -> Option<Bytes> {
        let mut marker = [0u8; 4];
        r.read_exact(&mut marker).await.ok()?;
        let len = (u32::from_be_bytes(marker) & 0x7FFF_FFFF) as usize;
        let mut body = BytesMut::with_capacity(len);
        body.resize(len, 0);
        r.read_exact(&mut body[..]).await.ok()?;
        Some(body.freeze())
    }

    /// Make a StateManager + Session with a known cb_program so the
    /// CallbackManager can resolve it. SessionId is fixed so the
    /// test can register the same id in `back_channels`.
    fn fixture_state(cb_program: u32) -> (Arc<StateManager>, SessionId) {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let session = state_mgr.sessions.create_session(
            42,                 // client_id
            0,                  // sequence
            0,                  // flags
            64 * 1024,          // max_request
            64 * 1024,          // max_response
            16 * 1024,          // max_response_cached
            16,                 // max_ops
            16,                 // max_requests
            cb_program,
        );
        (state_mgr, session.session_id)
    }

    /// Happy path: send a CB_LAYOUTRECALL, mock client replies OK,
    /// the awaiting send_layoutrecall returns the parsed reply.
    /// Verifies end-to-end: call shape on the wire, REPLY routing,
    /// reply parse.
    #[tokio::test]
    async fn send_layoutrecall_round_trip() {
        let (writer, server_read, client_read, mut client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);

        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, Arc::clone(&writer));

        let cb_mgr = CallbackManager::new(Arc::clone(&back_channels), Arc::clone(&state_mgr))
            .with_timeout(Duration::from_secs(5));

        // Spawn the "server read loop" — routes inbound REPLYs
        // back to the writer's inflight registry.
        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        // Mock client: read the CALL, peek the xid, write a reply.
        let mock_client = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let call = read_record(&mut r).await;
            // First u32 of the RPC body is xid.
            let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
            // Echo back a successful CB reply.
            write_record(&mut client_write, build_reply(xid, Nfs4Status::Ok)).await;
        });

        let stateid = [
            0u8, 0, 0, 1, // seqid = 1
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
        ];
        let reply = cb_mgr
            .send_layoutrecall(&session_id, &stateid, 1, 3, true)
            .await
            .expect("CB_LAYOUTRECALL succeeds");

        assert_eq!(reply.status, Nfs4Status::Ok);
        assert_eq!(reply.results.len(), 2);
        assert_eq!(reply.results[1].status(), Nfs4Status::Ok);
        assert!(matches!(reply.results[1], CbResult::LayoutRecall { .. }));

        mock_client.await.unwrap();
    }

    /// Client returns NFS4ERR_NOMATCHING_LAYOUT — call still
    /// succeeds (transport-wise) but the recalled-status is
    /// surfaced via the parsed reply. This is the "client already
    /// returned this layout" path the caller should treat as a
    /// successful outcome.
    #[tokio::test]
    async fn send_layoutrecall_no_matching_layout_is_ok() {
        let (writer, server_read, client_read, mut client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);

        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, Arc::clone(&writer));

        let cb_mgr = CallbackManager::new(back_channels, state_mgr)
            .with_timeout(Duration::from_secs(5));

        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        let mock = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let call = read_record(&mut r).await;
            let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
            write_record(
                &mut client_write,
                build_reply(xid, Nfs4Status::NoMatchingLayout),
            )
            .await;
        });

        let stateid = [0u8; 16];
        let reply = cb_mgr
            .send_layoutrecall(&session_id, &stateid, 1, 3, true)
            .await
            .expect("transport succeeds");
        assert_eq!(reply.results[1].status(), Nfs4Status::NoMatchingLayout);
        mock.await.unwrap();
    }

    /// Mock client never replies → caller times out. The xid is
    /// forgotten on this path; a stale reply arriving later is
    /// quietly ignored by the read loop.
    #[tokio::test]
    async fn send_layoutrecall_times_out_when_client_silent() {
        let (writer, server_read, client_read, _client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);

        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, Arc::clone(&writer));

        let cb_mgr = CallbackManager::new(back_channels, state_mgr)
            .with_timeout(Duration::from_millis(150));

        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        // Drain the CALL but never reply. Drop the read half at
        // end of scope so the read loop terminates cleanly.
        let drain = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let _ = read_record(&mut r).await;
        });

        let stateid = [0u8; 16];
        let err = cb_mgr
            .send_layoutrecall(&session_id, &stateid, 1, 3, true)
            .await
            .unwrap_err();
        assert!(matches!(err, CallbackError::Timeout), "got {:?}", err);
        drain.await.unwrap();
    }

    /// No back-channel registered for this session → fail fast
    /// with `ConnectionClosed`. Distinguishes "client opted out"
    /// from "wire error mid-flight."
    #[tokio::test]
    async fn send_layoutrecall_no_back_channel() {
        let (state_mgr, session_id) = fixture_state(0x40000000);
        let back_channels = Arc::new(DashMap::new());
        let cb_mgr = CallbackManager::new(back_channels, state_mgr);

        let stateid = [0u8; 16];
        let err = cb_mgr
            .send_layoutrecall(&session_id, &stateid, 1, 3, true)
            .await
            .unwrap_err();
        assert!(matches!(err, CallbackError::ConnectionClosed), "got {:?}", err);
    }

    /// Drives only the inflight cleanup path: when the connection's
    /// read loop exits without delivering a reply, awaiting callers
    /// see `ConnectionClosed`, not a hang and not a timeout.
    /// Important because real connection drops happen when a client
    /// goes away mid-recall.
    #[tokio::test]
    async fn send_layoutrecall_connection_closed_mid_call() {
        let (writer, server_read, client_read, client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);

        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, Arc::clone(&writer));

        let cb_mgr = CallbackManager::new(back_channels, state_mgr)
            .with_timeout(Duration::from_secs(5));

        let loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        // Mock client: read the CALL, *then* drop both halves —
        // simulates the client process exiting before responding.
        let mock = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let _ = read_record(&mut r).await;
            drop(r);
            drop(client_write);
        });

        let stateid = [0u8; 16];
        let err = cb_mgr
            .send_layoutrecall(&session_id, &stateid, 1, 3, true)
            .await
            .unwrap_err();
        assert!(
            matches!(err, CallbackError::ConnectionClosed),
            "got {:?}", err,
        );
        mock.await.unwrap();
        let _ = loop_handle.await;
    }

    /// Decoder sanity: the call we emit on the wire is what we
    /// said it was. Unlike A.2's tests (which exercise the encoder
    /// against itself), this test reads bytes off a real socket
    /// then re-decodes. Catches regressions where the writer
    /// adds/elides framing.
    #[tokio::test]
    async fn emitted_call_decodes_to_layoutrecall_file() {
        let (writer, server_read, client_read, mut client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);
        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, Arc::clone(&writer));
        let cb_mgr = CallbackManager::new(back_channels, state_mgr)
            .with_timeout(Duration::from_secs(5));
        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        let stateid = [
            0u8, 0, 0, 7, // seqid = 7
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ];

        // Mock client: read the CALL, decode the inner CB_COMPOUND
        // args, *then* reply OK.
        let inspect = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let call = read_record(&mut r).await;
            let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
            // Skip RPC header: xid(4) type(4) rpcvers(4) prog(4)
            // vers(4) proc(4) cred_flavor(4) cred_body(4 + len)
            // verf_flavor(4) verf_body(4 + len). With AUTH_NONE
            // both bodies are 4 bytes (length=0).
            let mut dec = XdrDecoder::new(call.clone());
            for _ in 0..6 {
                dec.decode_u32().unwrap();
            }
            for _ in 0..2 {
                dec.decode_u32().unwrap();
                dec.decode_opaque().unwrap();
            }
            // Now CB_COMPOUND args.
            let _tag = dec.decode_string().unwrap();
            assert_eq!(dec.decode_u32().unwrap(), 1); // minorversion
            assert_eq!(dec.decode_u32().unwrap(), 0); // callback_ident
            assert_eq!(dec.decode_u32().unwrap(), 2); // ops len
            // Op 1: CB_SEQUENCE
            assert_eq!(dec.decode_u32().unwrap(), cb_opcode::CB_SEQUENCE);
            // Op 2 starts after CB_SEQUENCE body — we trust the
            // A.2 round-trip test for the byte-by-byte detail and
            // just confirm the second opcode is CB_LAYOUTRECALL.
            // Sessionid (16 bytes) + seqid + slotid + highest_slotid
            // + cachethis + referring_call_lists<>.len(=0).
            for _ in 0..(16 / 4) {
                dec.decode_u32().unwrap();
            }
            for _ in 0..5 {
                dec.decode_u32().unwrap();
            }
            assert_eq!(dec.decode_u32().unwrap(), cb_opcode::CB_LAYOUTRECALL);

            write_record(&mut client_write, build_reply(xid, Nfs4Status::Ok)).await;
        });

        cb_mgr
            .send_layoutrecall(&session_id, &stateid, 1, 3, true)
            .await
            .unwrap();
        inspect.await.unwrap();
    }

    /// Two clients on two separate back-channels, three layouts:
    /// client A owns 2, client B owns 1. The fan-out should
    /// produce exactly 3 CALLs — A gets two, B gets one — and
    /// each CALL goes to the right writer (asserted by counting
    /// the bytes that come out each socket).
    #[tokio::test]
    async fn recall_layouts_for_device_routes_per_session() {
        let (writer_a, server_read_a, client_read_a, mut client_write_a) = pair().await;
        let (writer_b, server_read_b, client_read_b, mut client_write_b) = pair().await;
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let session_a = state_mgr
            .sessions
            .create_session(1, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16, 0x40000000)
            .session_id;
        let session_b = state_mgr
            .sessions
            .create_session(2, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16, 0x40000000)
            .session_id;

        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_a, Arc::clone(&writer_a));
        back_channels.insert(session_b, Arc::clone(&writer_b));

        let cb_mgr = CallbackManager::new(back_channels, state_mgr)
            .with_timeout(Duration::from_secs(5));

        // Read loops for both writers.
        let _loop_a = spawn_read_loop(Arc::clone(&writer_a), server_read_a);
        let _loop_b = spawn_read_loop(Arc::clone(&writer_b), server_read_b);

        // Mock client A: respond OK to its 2 inbound calls.
        let mock_a = tokio::spawn(async move {
            let mut r = BufReader::new(client_read_a);
            let mut count = 0;
            for _ in 0..2 {
                let call = read_record(&mut r).await;
                let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
                write_record(&mut client_write_a, build_reply(xid, Nfs4Status::Ok)).await;
                count += 1;
            }
            count
        });
        // Mock client B: 1 inbound call.
        let mock_b = tokio::spawn(async move {
            let mut r = BufReader::new(client_read_b);
            let call = read_record(&mut r).await;
            let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
            write_record(&mut client_write_b, build_reply(xid, Nfs4Status::Ok)).await;
            1
        });

        let stateid_a1 = [1u8; 16];
        let stateid_a2 = [2u8; 16];
        let stateid_b1 = [3u8; 16];
        let recalls = vec![
            (session_a, stateid_a1),
            (session_a, stateid_a2),
            (session_b, stateid_b1),
        ];

        let results = cb_mgr.recall_layouts_for_device("ds-dead", &recalls).await;
        assert_eq!(results.len(), 3);
        for r in &results {
            assert_eq!(r.outcome, RecallOutcome::Acked);
        }
        // Per-pair routing: the (session, stateid) pairs in `results`
        // must match the input pairs in order so the caller can
        // pair them with the requests they originated.
        for (i, r) in results.iter().enumerate() {
            assert_eq!(r.session_id, recalls[i].0);
            assert_eq!(r.stateid, recalls[i].1);
        }

        let count_a = mock_a.await.unwrap();
        let count_b = mock_b.await.unwrap();
        assert_eq!(count_a, 2, "client A should have received 2 calls");
        assert_eq!(count_b, 1, "client B should have received 1 call");
    }

    /// Empty input is a no-op and doesn't even hit the back-channel.
    /// Important: the heartbeat path may compute zero pairs (e.g.
    /// the dead device had no live layouts) and we shouldn't
    /// accidentally fan out to every registered session.
    #[tokio::test]
    async fn recall_layouts_for_device_empty_is_noop() {
        let (writer, _server_read, _client_read, _client_write) = pair().await;
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let session_id = state_mgr
            .sessions
            .create_session(1, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16, 0x40000000)
            .session_id;
        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, Arc::clone(&writer));
        let cb_mgr = CallbackManager::new(back_channels, state_mgr);

        let results = cb_mgr.recall_layouts_for_device("ds-dead", &[]).await;
        assert!(results.is_empty());
    }

    /// Timeout outcome surfaces as RecallOutcome::TimedOut so the
    /// heartbeat caller can revoke the layout (Phase A.5). Wires the
    /// short-timeout fixture against a silent mock client and checks
    /// the typed outcome.
    #[tokio::test]
    async fn recall_layouts_for_device_surfaces_timeout() {
        let (writer, server_read, client_read, _client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);
        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, Arc::clone(&writer));
        let cb_mgr = CallbackManager::new(back_channels, state_mgr)
            .with_timeout(Duration::from_millis(150));
        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        // Drain the CALL but never reply.
        let drain = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let _ = read_record(&mut r).await;
        });

        let stateid = [9u8; 16];
        let results = cb_mgr
            .recall_layouts_for_device("ds-dead", &[(session_id, stateid)])
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, RecallOutcome::TimedOut);
        assert_eq!(results[0].stateid, stateid);
        drain.await.unwrap();
    }

    /// No back-channel registered for the session → outcome is
    /// `NoChannel` (the heartbeat path treats this the same as
    /// TimedOut for revocation purposes).
    #[tokio::test]
    async fn recall_layouts_for_device_surfaces_no_channel() {
        let (state_mgr, session_id) = fixture_state(0x40000000);
        let back_channels = Arc::new(DashMap::new());
        let cb_mgr = CallbackManager::new(back_channels, state_mgr);

        let stateid = [42u8; 16];
        let results = cb_mgr
            .recall_layouts_for_device("ds-dead", &[(session_id, stateid)])
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, RecallOutcome::NoChannel);
    }
}
