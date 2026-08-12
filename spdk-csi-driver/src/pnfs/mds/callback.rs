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
use crate::nfs::v4::protocol::{Nfs4Status, SessionId, StateId};
use crate::nfs::v4::state::StateManager;
use crate::pnfs::mds::layout::LayoutStateId;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

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
    back_channels: Arc<DashMap<SessionId, Vec<Arc<BackChannelWriter>>>>,
    state_mgr: Arc<StateManager>,
    timeout: Duration,
    /// Per-session back-channel slot state: the sequenceid last sent on
    /// slot 0.
    ///
    /// RFC 8881 §2.10.6.1 requires each reuse of a slot to carry
    /// `previous + 1`, and requires the replier to treat a repeat as a
    /// retry. This was hardcoded to 1, so the SECOND CB_COMPOUND a
    /// session ever received — and every one after it — looked like a
    /// replay of the first; with `cachethis: false` a conforming client
    /// answers NFS4ERR_RETRY_UNCACHED_REP and aborts the compound AT
    /// CB_SEQUENCE, so CB_LAYOUTRECALL never runs (audit C2).
    ///
    /// The mutex is held ACROSS the reply await, not just the send. The
    /// old comment claimed the writer's mutex serialised recalls; it
    /// does not — `send_record` releases before awaiting, so two
    /// callers could have two CB_COMPOUNDs outstanding on slot 0 at
    /// once. One slot per session is plenty for recalls, and holding it
    /// across the round-trip is what makes the sequence well-defined.
    slots: Arc<DashMap<SessionId, Arc<tokio::sync::Mutex<u32>>>>,
}

impl CallbackManager {
    /// `back_channels` is the dispatcher's per-session writer
    /// registry; `state_mgr` is the source of truth for `cb_program`
    /// (stored on `Session` since A.2). Per-call timeout defaults to
    /// [`DEFAULT_CB_TIMEOUT`]; tests can override via
    /// [`with_timeout`].
    pub fn new(
        back_channels: Arc<DashMap<SessionId, Vec<Arc<BackChannelWriter>>>>,
        state_mgr: Arc<StateManager>,
    ) -> Self {
        Self {
            back_channels,
            state_mgr,
            timeout: DEFAULT_CB_TIMEOUT,
            slots: Arc::new(DashMap::new()),
        }
    }

    /// The slot-0 sequence lock for a session, created on first use.
    fn slot(&self, session_id: &SessionId) -> Arc<tokio::sync::Mutex<u32>> {
        Arc::clone(
            self.slots
                .entry(*session_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(0)))
                .value(),
        )
    }

    /// Drop a session's slot state — its sequence restarts from 1 for a
    /// fresh session, which is what a new session id means.
    pub fn forget_session(&self, session_id: &SessionId) {
        self.slots.remove(session_id);
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
        // Empty FH + whole range = the session-wide form (see the
        // `recall` field below).
        self.send_layoutrecall_range(
            session_id,
            layout_stateid,
            Vec::new(),
            0,
            u64::MAX,
            layout_type,
            iomode,
            changed,
        )
        .await
    }

    /// CB_LAYOUTRECALL scoped to ONE file and ONE byte range.
    ///
    /// The dead-DS path deliberately sends an empty FH, which Linux
    /// treats as "return everything for this session" — right when a
    /// device died, since every layout touching it is suspect. A
    /// truncate is the opposite case: exactly one file changed, and
    /// dropping the client's layouts for unrelated files on every
    /// SETATTR(size) would be a self-inflicted performance bug. Pass
    /// the layout's own filehandle and `[new_size, ..)` instead.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_layoutrecall_range(
        &self,
        session_id: &SessionId,
        layout_stateid: &LayoutStateId,
        fh: Vec<u8>,
        offset: u64,
        length: u64,
        layout_type: u32,
        iomode: u32,
        changed: bool,
    ) -> Result<CbCompoundReply, CallbackError> {
        // Resolve the writer first — if the session never bound a
        // back-channel, there's nothing to send.
        // Any bound transport will do — they all reach the same client.
        let writer = match self.back_channels.get(session_id).and_then(|w| w.first().cloned()) {
            Some(w) => w,
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
        // …and with it the session's MINOR VERSION, which the callback
        // header must carry: Linux resolves the callback's client by
        // (address, sessionid, minorversion), so a 4.2 mount sent a
        // minorversion=1 CB_COMPOUND answers BADSESSION before any
        // callback op is looked at (rig-found — it had been costing
        // every recall to a 4.2 client, which is every flint mount).
        let (cb_program, cb_minorversion) = match self.state_mgr.sessions.get_session(session_id) {
            Some(s) if s.cb_program != 0 => (s.cb_program, s.minorversion),
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

        // Hold slot 0 for the whole round-trip; `seq` is the value we are
        // about to send and becomes the session's new high-water mark only
        // if the call actually goes out.
        let slot = self.slot(session_id);
        let mut seq_guard = slot.lock().await;
        let seq = seq_guard.wrapping_add(1);
        let seq = if seq == 0 { 1 } else { seq };

        let call = CbCompoundCall {
            tag: String::new(),
            minorversion: cb_minorversion,
            callback_ident: 0,
            ops: vec![
                CbOp::Sequence {
                    sessionid: *session_id,
                    // One slot, strictly increasing. RFC 8881 §2.10.6.1.
                    sequenceid: seq,
                    slotid: 0,
                    highest_slotid: 0,
                    cachethis: false,
                },
                CbOp::LayoutRecall {
                    layout_type,
                    iomode,
                    changed,
                    recall: LayoutRecall::File {
                        // Empty FH = "any layout for this session" —
                        // Linux's client treats this as a session-wide
                        // return, which is what the dead-DS fan-out
                        // wants. The truncate path passes a real FH and
                        // range (see send_layoutrecall_range).
                        fh,
                        offset,
                        length,
                        stateid,
                    },
                },
            ],
        };

        let cb_cred = self
            .state_mgr
            .sessions
            .get_session(session_id)
            .and_then(|s| s.cb_cred.clone());
        info!(
            "📢 CB_LAYOUTRECALL → session {:?} (cb_program={}, type={}, iomode={}, cred={})",
            session_id,
            cb_program,
            layout_type,
            iomode,
            // Name the flavour: a DENIED reply is otherwise a guessing game
            // about which of {what we sent, what was offered} is wrong, and
            // that guess cost a live drill once already (C8).
            match cb_cred.as_ref() {
                Some(crate::nfs::v4::compound::CallbackSecParms::Sys { uid, gid, .. }) =>
                    format!("AUTH_SYS uid={} gid={}", uid, gid),
                Some(crate::nfs::v4::compound::CallbackSecParms::Gss) => "RPCSEC_GSS (UNSUPPORTED — will be denied)".to_string(),
                _ => "AUTH_NONE".to_string(),
            },
        );
        let reply = match writer
            .send_cb_compound(cb_program, cb_cred.as_ref(), &call, self.timeout)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // The call may or may not have reached the client. Advance
                // anyway: re-sending `seq` after a timeout would look like a
                // retry of a request the client may have already executed,
                // and RFC 8881 §2.10.6.1's retry semantics make that the
                // more dangerous of the two readings.
                *seq_guard = seq;
                return Err(e);
            }
        };
        *seq_guard = seq;
        drop(seq_guard);
        info!(
            "✅ CB_LAYOUTRECALL ← session {:?}: status={:?}, {} results",
            session_id,
            reply.status,
            reply.results.len(),
        );
        Ok(reply)
    }

    /// Tell one client that a device it cached has changed — the
    /// online half of block-volume expansion (design doc §7).
    ///
    /// The client caches a pNFS device, its LENGTH included, from
    /// GETDEVICEINFO; a grown volume is invisible until that cache is
    /// dropped, and layouts past the old end are granted by the server
    /// and immediately returned by the client (rig-proven). Linux's
    /// `nfs4_callback_devicenotify` responds to BOTH change and delete
    /// by deleting the cached deviceid, which makes the next LAYOUTGET
    /// re-fetch it — exactly the effect we want.
    ///
    /// Best-effort by construction: a client with no back-channel, a
    /// dead session, or a refusal leaves the volume in the documented
    /// "recycle the mount" state. It never affects the expand's own
    /// success — the capacity IS there either way.
    /// Reach a CLIENT rather than a session: try each session the client
    /// currently holds until one has a back-channel and answers.
    ///
    /// This is the resolution step that lets the notify address book be
    /// durable. A back-channel is a live TCP writer and can never be
    /// persisted; a session id does not survive an MDS restart either
    /// (startup drops persisted sessions so the kernel re-CREATE_SESSIONs
    /// on BADSESSION). The client id survives both, so the book records
    /// the client and the session is looked up HERE, from live state, at
    /// the moment of sending.
    ///
    /// Trying every session matters for the same reason: after a restart
    /// or a trunked mount, the session that fetched the device is not
    /// the session that can be reached now.
    pub async fn send_notify_deviceid_to_client(
        &self,
        client_id: u64,
        layout_type: u32,
        deviceid: [u8; 16],
        notify_type: u32,
    ) -> Result<CbCompoundReply, CallbackError> {
        let sessions = self.state_mgr.sessions.get_client_sessions(client_id);
        if sessions.is_empty() {
            debug!("CB_NOTIFY_DEVICEID: client {} holds no session", client_id);
            return Err(CallbackError::ConnectionClosed);
        }
        let mut last = CallbackError::ConnectionClosed;
        for sid in &sessions {
            match self
                .send_notify_deviceid(sid, layout_type, deviceid, notify_type)
                .await
            {
                Ok(reply) => return Ok(reply),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub async fn send_notify_deviceid(
        &self,
        session_id: &SessionId,
        layout_type: u32,
        deviceid: [u8; 16],
        notify_type: u32,
    ) -> Result<CbCompoundReply, CallbackError> {
        let writer = match self.back_channels.get(session_id).and_then(|w| w.first().cloned()) {
            Some(w) => w,
            None => {
                debug!("CB_NOTIFY_DEVICEID: no back-channel for session {:?}", session_id);
                return Err(CallbackError::ConnectionClosed);
            }
        };
        let (cb_program, cb_minorversion) = match self.state_mgr.sessions.get_session(session_id) {
            Some(s) if s.cb_program != 0 => (s.cb_program, s.minorversion),
            _ => return Err(CallbackError::ConnectionClosed),
        };

        let slot = self.slot(session_id);
        let mut seq_guard = slot.lock().await;
        let seq = seq_guard.wrapping_add(1);
        let seq = if seq == 0 { 1 } else { seq };

        let call = CbCompoundCall {
            tag: String::new(),
            minorversion: cb_minorversion,
            callback_ident: 0,
            ops: vec![
                CbOp::Sequence {
                    sessionid: *session_id,
                    sequenceid: seq,
                    slotid: 0,
                    highest_slotid: 0,
                    cachethis: false,
                },
                CbOp::NotifyDeviceId {
                    changes: vec![crate::nfs::v4::cb_compound::DeviceIdNotify {
                        notify_type,
                        layout_type,
                        deviceid,
                        // "Act now" rather than "at your convenience":
                        // the client is holding geometry we know is
                        // stale, and every write past the old end goes
                        // down the MDS lane until it re-fetches.
                        immediate: true,
                    }],
                },
            ],
        };

        let cb_cred = self
            .state_mgr
            .sessions
            .get_session(session_id)
            .and_then(|s| s.cb_cred.clone());
        info!(
            "📢 CB_NOTIFY_DEVICEID → session {:?} (type={}, layout_type={}, dev={:02x?})",
            session_id,
            notify_type,
            layout_type,
            &deviceid[..4],
        );
        let reply = match writer
            .send_cb_compound(cb_program, cb_cred.as_ref(), &call, self.timeout)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                *seq_guard = seq;
                return Err(e);
            }
        };
        *seq_guard = seq;
        drop(seq_guard);
        // A refusal is worth a WARN of its own: the failure is otherwise
        // SILENT (the client keeps serving the stale device and nothing
        // else complains), and NFS4ERR_INVAL specifically means our
        // encoding was rejected — the one thing a wire change can get
        // wrong and never notice.
        if reply.status != crate::nfs::v4::protocol::Nfs4Status::Ok {
            warn!(
                "⚠️ CB_NOTIFY_DEVICEID ← session {:?}: status={:?} — the client kept its \
                 cached device (a grown volume stays invisible to it until the mount is \
                 recycled)",
                session_id, reply.status,
            );
        } else {
            info!("✅ CB_NOTIFY_DEVICEID ← session {:?}: accepted", session_id);
        }
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
                Ok(reply) => classify_reply(&reply),
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

/// Fire one per-file CB_LAYOUTRECALL for each layout on a file whose
/// size is changing, covering `[new_size, ..)`.
///
/// The REVOCATION POLICY DIFFERS from the dead-DS fan-out, and the
/// difference is the point. There, an `Acked` layout gets a soft
/// post-recall deadline before forcible revocation, because the client
/// may still have a legitimate LAYOUTCOMMIT to land. Here the bytes
/// past `new_size` are going away by definition, so a grace period is
/// not politeness — it is exactly the exposure window the recall exists
/// to close. Every layout is revoked server-side as soon as its recall
/// attempt returns, whatever the outcome:
///
///   Acked      the client dropped it; revoking is bookkeeping.
///   TimedOut   } the client either never heard or will not answer, and
///   NoChannel  } RFC 5661 §12.5.5.2 lets us revoke immediately. It may
///   Transport  } still be reading — which is why the caller must not
///              } lift the truncate-dirty gate until the fanout lands.
///
/// Returns the outcomes for logging; the caller does not need to act on
/// them, which is the whole simplification.
pub async fn recall_layouts_for_truncate(
    callbacks: &CallbackManager,
    layout_manager: &crate::pnfs::mds::layout::LayoutManager,
    file_ident: &str,
    new_size: u64,
    recalls: &[(SessionId, LayoutStateId, Vec<u8>)],
) -> Vec<RecallResult> {
    if recalls.is_empty() {
        return Vec::new();
    }
    info!(
        "📢 {} CB_LAYOUTRECALL(s) for truncated file {} → covering [{}, ..)",
        recalls.len(),
        file_ident,
        new_size,
    );

    // CONCURRENT ACROSS SESSIONS, serial within one.
    //
    // This loop used to be a plain `for ... .await`, which cost one full
    // CB timeout per unresponsive holder, one after another. Measured on
    // runat with synthetic holders that accept callbacks and never answer:
    // 1 holder blocked the truncate 10.46s, 3 holders blocked it 30.43s —
    // linear, at DEFAULT_CB_TIMEOUT each. The truncating client's SETATTR
    // pays that bill, and on an RWX volume the holder count is the number
    // of consumers.
    //
    // Per-SESSION ordering is still required and still enforced: a back
    // channel negotiates ca_maxrequests=1, so two CB_COMPOUNDs to one
    // session would collide on slot 0. `send_layoutrecall_range` takes
    // that session's slot mutex and holds it across the reply, so two
    // recalls to the SAME session serialize here exactly as before —
    // only different sessions overlap.
    //
    // The revoke-before-proceeding property is preserved per layout: each
    // task revokes its own layout as soon as its own recall returns, so no
    // layout stays live while other round-trips are outstanding. That was
    // the reason the loop was sequential, and it does not require
    // sequencing — only that revoke follows its own recall.
    //
    // join_all preserves input order in the result vector, so the refusal
    // logging below is unchanged and deterministic.
    let results: Vec<RecallResult> = futures::future::join_all(recalls.iter().map(
        |(session_id, stateid, fh)| async move {
        let outcome = match callbacks
            .send_layoutrecall_range(
                session_id,
                stateid,
                fh.clone(),
                new_size,
                u64::MAX,
                1, // LAYOUT4_NFSV4_1_FILES
                3, // LAYOUTIOMODE4_ANY
                true,
            )
            .await
        {
            Ok(reply) => classify_reply(&reply),
            Err(CallbackError::Timeout) => RecallOutcome::TimedOut,
            Err(CallbackError::ConnectionClosed) => RecallOutcome::NoChannel,
            Err(e) => {
                let msg = e.to_string();
                warn!(
                    "CB_LAYOUTRECALL (truncate) to session {:?} failed: {}",
                    session_id, msg,
                );
                RecallOutcome::Transport(msg)
            }
        };
        // Unconditional, and immediately after THIS recall returns: a
        // layout left live while other round-trips are outstanding is a
        // layout that can still reach the bytes we are about to delete.
        // Note this is per-layout, not global ordering — which is why
        // running the recalls concurrently does not weaken it.
        if layout_manager.revoke_layout(stateid) {
            debug!(
                "🚫 revoked layout {:?} on truncate of {} (recall outcome {:?})",
                &stateid[0..4],
                file_ident,
                outcome,
            );
        }
        RecallResult {
            session_id: *session_id,
            stateid: *stateid,
            outcome,
        }
    },
    ))
    .await;
    let acked = results
        .iter()
        .filter(|r| matches!(r.outcome, RecallOutcome::Acked))
        .count();
    // Refusals get their own line, loudly. The whole reason two RFC
    // violations survived this long is that they were counted as acks.
    for r in results.iter() {
        if let RecallOutcome::Refused(why) = &r.outcome {
            warn!(
                "❌ CB_LAYOUTRECALL for {} REFUSED by session {:?}: {} — the client still \
                 believes it holds the layout and can read past the new EOF; the \
                 server-side revoke below does NOT bind it",
                file_ident, r.session_id, why,
            );
        }
    }
    if acked == results.len() {
        info!(
            "📊 Truncate recall for {}: {}/{} acked, all revoked server-side",
            file_ident,
            acked,
            results.len(),
        );
    } else {
        warn!(
            "📊 Truncate recall for {}: only {}/{} acked — {} client(s) may still be \
             reading past the new EOF",
            file_ident,
            acked,
            results.len(),
            results.len() - acked,
        );
    }
    results
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
    /// The client answered and REFUSED. `decode_cb_reply` returns Ok
    /// for any NFS4 status once the RPC layer accepted, so without
    /// this arm a rejection is indistinguishable from success — which
    /// is how two RFC violations in the recall encoding survived a
    /// gate, a test suite and a code review, all of them reading
    /// "1/1 acked" (audit C3, 2026-07-31).
    ///
    /// NFS4ERR_NOMATCHING_LAYOUT is NOT refusal: the client is telling
    /// us it already returned the layout, which is the outcome we
    /// wanted. Everything else means the recall did not take effect.
    Refused(String),
}

/// Classify a decoded CB reply. The layout is gone from the client on
/// NFS4_OK and on NFS4ERR_NOMATCHING_LAYOUT and on nothing else.
///
/// Checks the per-op results, not just the top level: a CB_COMPOUND
/// that fails at CB_SEQUENCE short-circuits, so CB_LAYOUTRECALL never
/// runs and there is no LayoutRecall result at all — the case a
/// hardcoded back-channel slot produces, and the one a top-level-only
/// check is least likely to notice.
fn classify_reply(reply: &CbCompoundReply) -> RecallOutcome {
    use crate::nfs::v4::cb_compound::CbResult;

    let mut saw_recall = false;
    for r in &reply.results {
        match r {
            CbResult::Sequence { status, .. } if *status != Nfs4Status::Ok => {
                return RecallOutcome::Refused(format!("CB_SEQUENCE {:?}", status));
            }
            CbResult::LayoutRecall { status } => {
                saw_recall = true;
                match status {
                    Nfs4Status::Ok | Nfs4Status::NoMatchingLayout => {}
                    other => {
                        return RecallOutcome::Refused(format!("CB_LAYOUTRECALL {:?}", other))
                    }
                }
            }
            _ => {}
        }
    }
    if !saw_recall {
        return RecallOutcome::Refused(format!(
            "CB_LAYOUTRECALL never ran (compound status {:?}, {} result(s))",
            reply.status,
            reply.results.len(),
        ));
    }
    RecallOutcome::Acked
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
        fixture_state_minor(cb_program, 1)
    }

    fn fixture_state_minor(cb_program: u32, minorversion: u32) -> (Arc<StateManager>, SessionId) {
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
            None,
            minorversion,
        );
        (state_mgr, session.session_id)
    }

    /// THE MINOR VERSION IS NOT COSMETIC — rig-found, and it had been
    /// silently breaking every callback to every flint mount.
    ///
    /// Linux's callback service takes `cps->minorversion` from OUR
    /// CB_COMPOUND header and resolves the client with
    /// `nfs4_find_client_sessionid(net, addr, sessionid, cps->minorversion)`,
    /// which requires `clp->cl_minorversion == minorversion`. flint
    /// mounts are vers=4.2, so a hardcoded minorversion=1 matched NO
    /// client: the reply was NFS4ERR_BADSESSION and the callback op
    /// never ran. Both send paths must echo the session's own minor
    /// version.
    #[tokio::test]
    async fn callbacks_echo_the_sessions_minor_version_not_a_hardcoded_one() {
        for minor in [1u32, 2u32] {
            let (writer, server_read, client_read, mut client_write) = pair().await;
            let (state_mgr, session_id) = fixture_state_minor(0x40000000, minor);
            let back_channels = Arc::new(DashMap::new());
            back_channels.insert(session_id, vec![Arc::clone(&writer)]);
            let cb_mgr = CallbackManager::new(back_channels, Arc::clone(&state_mgr))
                .with_timeout(Duration::from_secs(5));
            let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

            let seen = Arc::new(std::sync::Mutex::new(u32::MAX));
            let seen_c = Arc::clone(&seen);
            let mock_client = tokio::spawn(async move {
                let mut r = BufReader::new(client_read);
                let call = read_record(&mut r).await;
                // RPC header is 10 u32s (xid, msg_type, rpcvers, prog,
                // vers, proc, cred{flavor,len}, verf{flavor,len}); the
                // CB_COMPOUND body then opens with the tag's length
                // (0) and the minorversion.
                let at = |i: usize| {
                    u32::from_be_bytes([call[i], call[i + 1], call[i + 2], call[i + 3]])
                };
                let mut off = 40;
                let taglen = at(off) as usize;
                off += 4 + taglen.div_ceil(4) * 4;
                *seen_c.lock().unwrap() = at(off);
                let xid = at(0);
                write_record(&mut client_write, build_reply(xid, Nfs4Status::Ok)).await;
            });

            let stateid = [0u8; 16];
            let _ = cb_mgr.send_layoutrecall(&session_id, &stateid, 1, 3, true).await;
            mock_client.await.unwrap();
            assert_eq!(
                *seen.lock().unwrap(),
                minor,
                "CB_COMPOUND must carry the session's minor version"
            );
        }
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
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);

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

    /// CB_NOTIFY_DEVICEID end to end over the back channel: the call
    /// goes out with the right op, the client's status comes back
    /// parsed, and the WIRE BYTES are the ones a Linux client accepts
    /// (the byte-level shape is pinned in `cb_compound`'s tests; this
    /// one covers routing, sequencing and the reply parse).
    #[tokio::test]
    async fn send_notify_deviceid_round_trip() {
        let (writer, server_read, client_read, mut client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);

        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);
        let cb_mgr = CallbackManager::new(Arc::clone(&back_channels), Arc::clone(&state_mgr))
            .with_timeout(Duration::from_secs(5));
        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        let seen_op = Arc::new(std::sync::Mutex::new(0u32));
        let seen = Arc::clone(&seen_op);
        let mock_client = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let call = read_record(&mut r).await;
            let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
            // The second op's opcode: find it by scanning for the value
            // rather than hand-counting the RPC header offsets.
            let want = cb_opcode::CB_NOTIFY_DEVICEID.to_be_bytes();
            if call.windows(4).any(|w| w == want) {
                *seen.lock().unwrap() = cb_opcode::CB_NOTIFY_DEVICEID;
            }
            let mut enc = XdrEncoder::new();
            enc.encode_u32(xid);
            enc.encode_u32(MessageType::Reply as u32);
            enc.encode_u32(ReplyStatus::Accepted as u32);
            enc.encode_u32(AuthFlavor::Null as u32);
            enc.encode_opaque(&[]);
            enc.encode_u32(AcceptStatus::Success as u32);
            enc.encode_u32(Nfs4Status::Ok.to_u32());
            enc.encode_opaque(&[]); // tag
            enc.encode_u32(2);
            enc.encode_u32(cb_opcode::CB_SEQUENCE);
            enc.encode_u32(Nfs4Status::Ok.to_u32());
            for _ in 0..8 {
                enc.encode_u32(0);
            }
            enc.encode_u32(cb_opcode::CB_NOTIFY_DEVICEID);
            enc.encode_u32(Nfs4Status::Ok.to_u32());
            write_record(&mut client_write, enc.finish()).await;
        });

        let reply = cb_mgr
            .send_notify_deviceid(
                &session_id,
                5,
                [0x5au8; 16],
                crate::nfs::v4::cb_compound::deviceid_notify_type::CHANGE,
            )
            .await
            .expect("CB_NOTIFY_DEVICEID succeeds");

        assert_eq!(reply.status, Nfs4Status::Ok);
        assert_eq!(reply.results.len(), 2);
        assert!(matches!(reply.results[1], CbResult::NotifyDeviceId { .. }));
        assert_eq!(
            *seen_op.lock().unwrap(),
            cb_opcode::CB_NOTIFY_DEVICEID,
            "the CALL must actually carry op 14"
        );
        mock_client.await.unwrap();
    }

    /// THE RESOLUTION STEP THE DURABLE NOTIFY BOOK RESTS ON: address a
    /// CLIENT and reach it through whatever session it holds NOW.
    ///
    /// This is exactly the post-restart shape. The book remembers client
    /// 42; the session it originally fetched under is gone (startup
    /// drops persisted sessions so the kernel re-CREATE_SESSIONs), and
    /// the client is now on a different session id with the only live
    /// back-channel. Sending to the remembered session would fail; the
    /// client-addressed send must find the new one.
    #[tokio::test]
    async fn a_client_is_reached_through_whatever_session_it_holds_now() {
        let (writer, server_read, client_read, mut client_write) = pair().await;
        let (state_mgr, dead_session) = fixture_state(0x40000000);

        // The client re-established: a SECOND session for client 42,
        // and only THAT one has a back-channel.
        let live = state_mgr.sessions.create_session(
            42, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16, 0x40000000, None, 1,
        );
        assert_ne!(live.session_id, dead_session);
        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(live.session_id, vec![Arc::clone(&writer)]);
        let cb_mgr = CallbackManager::new(Arc::clone(&back_channels), Arc::clone(&state_mgr))
            .with_timeout(Duration::from_secs(5));

        // The remembered session cannot be reached at all.
        assert!(cb_mgr
            .send_notify_deviceid(
                &dead_session,
                5,
                [0x5au8; 16],
                crate::nfs::v4::cb_compound::deviceid_notify_type::CHANGE,
            )
            .await
            .is_err());

        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);
        let mock_client = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            let call = read_record(&mut r).await;
            let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
            let mut enc = XdrEncoder::new();
            enc.encode_u32(xid);
            enc.encode_u32(MessageType::Reply as u32);
            enc.encode_u32(ReplyStatus::Accepted as u32);
            enc.encode_u32(AuthFlavor::Null as u32);
            enc.encode_opaque(&[]);
            enc.encode_u32(AcceptStatus::Success as u32);
            enc.encode_u32(Nfs4Status::Ok.to_u32());
            enc.encode_opaque(&[]);
            enc.encode_u32(2);
            enc.encode_u32(cb_opcode::CB_SEQUENCE);
            enc.encode_u32(Nfs4Status::Ok.to_u32());
            for _ in 0..8 {
                enc.encode_u32(0);
            }
            enc.encode_u32(cb_opcode::CB_NOTIFY_DEVICEID);
            enc.encode_u32(Nfs4Status::Ok.to_u32());
            write_record(&mut client_write, enc.finish()).await;
        });

        let reply = cb_mgr
            .send_notify_deviceid_to_client(
                42,
                5,
                [0x5au8; 16],
                crate::nfs::v4::cb_compound::deviceid_notify_type::CHANGE,
            )
            .await
            .expect("the client is reachable through its CURRENT session");
        assert_eq!(reply.status, Nfs4Status::Ok);
        mock_client.await.unwrap();

        // A client that holds no session at all is an error, not a
        // silent success — the caller counts accepted vs attempted.
        assert!(cb_mgr
            .send_notify_deviceid_to_client(
                999,
                5,
                [0x5au8; 16],
                crate::nfs::v4::cb_compound::deviceid_notify_type::CHANGE,
            )
            .await
            .is_err());
    }

    /// AUDIT C2. RFC 8881 §2.10.6.1: each reuse of a back-channel slot
    /// carries `previous + 1`. This was hardcoded to 1, so the SECOND
    /// CB_COMPOUND on a session looked like a replay of the first and a
    /// conforming client aborts at CB_SEQUENCE — CB_LAYOUTRECALL never
    /// running. One truncate can emit several recalls to one session, so
    /// "the second one" is routine, not an edge case.
    #[tokio::test]
    async fn back_channel_sequenceid_advances_per_session() {
        let (writer, server_read, client_read, mut client_write) = pair().await;
        let (state_mgr, session_id) = fixture_state(0x40000000);
        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);
        let cb_mgr = CallbackManager::new(Arc::clone(&back_channels), Arc::clone(&state_mgr))
            .with_timeout(Duration::from_secs(5));
        let _loop_handle = spawn_read_loop(Arc::clone(&writer), server_read);

        // Mock client: answer three calls, recording the CB_SEQUENCE
        // sequenceid it saw on each. Layout of the CB_COMPOUND args puts
        // the sequenceid right after tag/minorversion/callback_ident/
        // opcount/op + the 16-byte sessionid, so find it by scanning for
        // the session id rather than hardcoding an offset.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let sid_bytes = session_id.0;
        let mock = tokio::spawn(async move {
            let mut r = BufReader::new(client_read);
            for _ in 0..3 {
                let call = read_record(&mut r).await;
                let xid = u32::from_be_bytes([call[0], call[1], call[2], call[3]]);
                let sid = &sid_bytes[..];
                let at = call
                    .windows(16)
                    .position(|w| w == sid)
                    .expect("sessionid appears in the CB_SEQUENCE args");
                let seq_at = at + 16;
                tx.send(u32::from_be_bytes([
                    call[seq_at],
                    call[seq_at + 1],
                    call[seq_at + 2],
                    call[seq_at + 3],
                ]))
                .unwrap();
                write_record(&mut client_write, build_reply(xid, Nfs4Status::Ok)).await;
            }
        });

        for _ in 0..3 {
            let _ = cb_mgr.send_layoutrecall(&session_id, &[7u8; 16], 1, 3, true).await;
        }
        mock.await.unwrap();

        let mut on_the_wire = Vec::new();
        while let Ok(v) = rx.try_recv() {
            on_the_wire.push(v);
        }
        assert_eq!(
            on_the_wire,
            vec![1, 2, 3],
            "RFC 8881 §2.10.6.1: each slot reuse carries previous+1; a repeat is a replay",
        );
        assert_eq!(*cb_mgr.slot(&session_id).lock().await, 3);

        // A different session has its own slot, starting over.
        let other = SessionId([0xEE; 16]);
        assert_eq!(*cb_mgr.slot(&other).lock().await, 0);

        // A session that goes away resets — a new session id is a new sequence.
        cb_mgr.forget_session(&session_id);
        assert_eq!(*cb_mgr.slot(&session_id).lock().await, 0);
    }

    /// AUDIT C3. `decode_cb_reply` returns Ok for any NFS4 status once the
    /// RPC layer accepted, so a blind `Ok(_) => Acked` scores a REFUSAL as
    /// a success. That is how two RFC violations in the recall encoding
    /// survived a gate, a test suite and a review — every log said
    /// "1/1 acked".
    #[test]
    fn classify_reply_separates_refusal_from_ack() {
        use crate::nfs::v4::cb_compound::CbResult;
        let seq_ok = |status| CbResult::Sequence {
            status,
            sessionid: SessionId([0u8; 16]),
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            target_highest_slotid: 0,
        };
        let reply = |status, results| CbCompoundReply { status, tag: String::new(), results };

        // Success.
        assert_eq!(
            classify_reply(&reply(
                Nfs4Status::Ok,
                vec![seq_ok(Nfs4Status::Ok), CbResult::LayoutRecall { status: Nfs4Status::Ok }],
            )),
            RecallOutcome::Acked,
        );
        // "I already returned it" is the outcome we wanted, not a refusal.
        assert_eq!(
            classify_reply(&reply(
                Nfs4Status::NoMatchingLayout,
                vec![
                    seq_ok(Nfs4Status::Ok),
                    CbResult::LayoutRecall { status: Nfs4Status::NoMatchingLayout },
                ],
            )),
            RecallOutcome::Acked,
        );
        // A refused recall.
        assert!(matches!(
            classify_reply(&reply(
                Nfs4Status::Delay,
                vec![seq_ok(Nfs4Status::Ok), CbResult::LayoutRecall { status: Nfs4Status::Delay }],
            )),
            RecallOutcome::Refused(_),
        ));
        // The C2 shape: the compound short-circuits AT CB_SEQUENCE, so there
        // is no LayoutRecall result at all. A top-level-only check would
        // miss this one.
        assert!(matches!(
            classify_reply(&reply(Nfs4Status::BadSession, vec![seq_ok(Nfs4Status::BadSession)])),
            RecallOutcome::Refused(_),
        ));
        // Nothing ran at all.
        assert!(matches!(
            classify_reply(&reply(Nfs4Status::ServerFault, vec![])),
            RecallOutcome::Refused(_),
        ));
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
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);

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
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);

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
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);

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
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);
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
            .create_session(1, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16, 0x40000000, None, 1)
            .session_id;
        let session_b = state_mgr
            .sessions
            .create_session(2, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16, 0x40000000, None, 1)
            .session_id;

        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_a, vec![Arc::clone(&writer_a)]);
        back_channels.insert(session_b, vec![Arc::clone(&writer_b)]);

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
            .create_session(1, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16, 0x40000000, None, 1)
            .session_id;
        let back_channels = Arc::new(DashMap::new());
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);
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
        back_channels.insert(session_id, vec![Arc::clone(&writer)]);
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

    /// Truncate recalls to DIFFERENT sessions must overlap.
    ///
    /// This is the regression guard for a measured production hazard: the
    /// fan-out was a plain sequential `for ... .await`, so every silent
    /// holder cost a full CB timeout one after another. On runat, with
    /// synthetic holders that accept callbacks and never answer, one
    /// holder blocked the truncate 10.46s and three blocked it 30.43s —
    /// linear. The truncating client's SETATTR pays it, and on an RWX
    /// volume the holder count is the consumer count.
    ///
    /// Three silent sessions at a 300ms timeout: sequential would need
    /// ~900ms, concurrent ~300ms. The bound is deliberately loose (600ms)
    /// — this asserts "overlapping", not a stopwatch reading, so it does
    /// not turn into a flaky test on a loaded CI box. It still fails
    /// decisively if the concurrency is ever removed.
    #[tokio::test]
    async fn truncate_recalls_to_distinct_sessions_overlap() {
        use crate::pnfs::mds::layout::LayoutManager;
        use crate::pnfs::config::LayoutPolicy as ConfigLayoutPolicy;

        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let back_channels: Arc<DashMap<SessionId, Vec<Arc<BackChannelWriter>>>> =
            Arc::new(DashMap::new());

        let mut recalls = Vec::new();
        let mut keepalive = Vec::new();
        let mut drains = Vec::new();
        for i in 0..3u64 {
            let session = state_mgr.sessions.create_session(
                100 + i, 0, 0, 64 * 1024, 64 * 1024, 16 * 1024, 16, 16,
                0x4000_0000, None, 1,
            );
            let (writer, server_read, client_read, client_write) = pair().await;
            back_channels.insert(session.session_id, vec![Arc::clone(&writer)]);
            let h = spawn_read_loop(Arc::clone(&writer), server_read);
            // Drain the CALL and never reply — each session burns the full
            // timeout, which is the whole point.
            drains.push(tokio::spawn(async move {
                let mut r = BufReader::new(client_read);
                let _ = read_record(&mut r).await;
            }));
            keepalive.push((writer, client_write, h));
            recalls.push((session.session_id, [i as u8 + 1; 16], vec![0xABu8; 8]));
        }

        let cb_mgr = CallbackManager::new(Arc::clone(&back_channels), Arc::clone(&state_mgr))
            .with_timeout(Duration::from_millis(300));
        // The layout map is empty, so revoke_layout is a no-op per
        // stateid — this test is about the recall timing, not revocation.
        let lm = LayoutManager::new(
            Arc::new(crate::pnfs::mds::DeviceRegistry::new()),
            ConfigLayoutPolicy::Stripe,
            1 << 20,
            crate::state_backend::memory_backend(),
        );

        let t0 = std::time::Instant::now();
        let results = recall_layouts_for_truncate(&cb_mgr, &lm, "id:test", 0, &recalls).await;
        let elapsed = t0.elapsed();

        assert_eq!(results.len(), 3, "every recall must be reported");
        for r in &results {
            assert_eq!(r.outcome, RecallOutcome::TimedOut);
        }
        assert!(
            elapsed < Duration::from_millis(600),
            "3 silent sessions took {:?} at a 300ms timeout — the recalls are \
             running SEQUENTIALLY again, so a truncate now costs one CB timeout \
             per unresponsive holder (measured live: 1 holder 10.5s, 3 holders 30.4s)",
            elapsed,
        );
        for d in drains {
            let _ = d.await;
        }
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
