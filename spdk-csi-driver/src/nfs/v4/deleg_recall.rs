//! The delegation recall ladder — "recall-or-die" (design §5.4,
//! modeled by formal/FlintDelegRecall.tla; the LadderRecheck and
//! RebindRearm constants are THIS file's obligations).
//!
//! One task per recalled record, single-flight by construction: the
//! mutation fence transitions a record out of `Granted` exactly once
//! and returns its `RecallOrder` exactly once; this driver owns the
//! record's fate from there — DELEGRETURN (the client cooperates),
//! disown (the client provably never held it), or revocation with the
//! SEQ4 bit raised (the client finds out on its next lease renewal,
//! the one RPC delegations don't eliminate).
//!
//! Every wakeup re-snapshots the record and no-ops on gone/changed:
//! dropping a JoinHandle detaches, it does not cancel, so a task that
//! outlives its record must never act on a re-granted successor (the
//! NoRecheck mutation run is the counterexample for skipping this).
//!
//! The sender is a trait so the ladder's decisions — rung spacing,
//! deadline arithmetic, reply classification, the disown re-probe,
//! the CB_PATH_DOWN window — are tested against a scripted mock on
//! tokio's paused clock, not a TCP rig. `CallbackManager` implements
//! it over the real back-channel in pnfs/mds/callback.rs.

use super::back_channel::CallbackError;
use super::cb_compound::{CbCompoundReply, CbResult};
use super::protocol::{seq4_status, Nfs4Status, StateId};
use super::state::deleg_meter::{RecallOutcome, RevokeReason};
use super::state::{DelegState, RecallOrder, StateManager};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// The one send the ladder needs. Implemented by `CallbackManager`
/// (client-addressed: every session, writer failover within each).
pub trait RecallSender: Send + Sync + 'static {
    fn send_recall(
        &self,
        client_id: u64,
        stateid: StateId,
        truncate: bool,
        fh: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<CbCompoundReply, CallbackError>> + Send>>;
}

/// Ladder timing. Defaults are the design's numbers; tests shrink
/// nothing — they run on tokio's paused clock instead, so the REAL
/// durations are what's exercised.
#[derive(Debug, Clone)]
pub struct RecallLadderConfig {
    /// Revoke deadline, from FIRST SUCCESSFUL TRANSMIT — not from
    /// conflict detection: slot-0 serialization to one slow client
    /// must not revoke delegations that were never asked for.
    pub revoke_deadline: Duration,
    /// Resend rungs after the first transmit (timeout/DELAY replies).
    pub rungs: [Duration; 2],
    /// The CB_PATH_DOWN window: how long an UNREACHABLE client (no
    /// transport accepted the write, or no channel at all) is given
    /// before revocation. Retries inside the window are how a rebind
    /// converts a would-be revocation into a completed recall.
    pub path_down_window: Duration,
    /// Retry spacing while the path is down (each retry is the rearm
    /// probe — a rebound writer makes the next one succeed).
    pub path_retry: Duration,
    /// Delay before the disown re-probe (a BAD_STATEID answer may be
    /// the CB_RECALL crossing the granting OPEN reply — the client
    /// may be about to install the delegation it just denied).
    pub disown_probe_delay: Duration,
}

impl Default for RecallLadderConfig {
    fn default() -> Self {
        Self {
            revoke_deadline: Duration::from_secs(90),
            rungs: [Duration::from_secs(30), Duration::from_secs(60)],
            path_down_window: Duration::from_secs(30),
            path_retry: Duration::from_secs(5),
            disown_probe_delay: Duration::from_secs(2),
        }
    }
}

/// Spawns and drives recall tasks. Cheap to clone-by-Arc; one per
/// server posture.
pub struct RecallDriver {
    state_mgr: Arc<StateManager>,
    sender: Arc<dyn RecallSender>,
    cfg: RecallLadderConfig,
}

/// What one reply means for the ladder — precedence per design §5.3.
enum Classified {
    /// DELAY at ANY level (CB_SEQUENCE or CB_RECALL): retry a rung,
    /// never revoke on a client that answers DELAY.
    Delay,
    /// CB_RECALL NFS4_OK: acknowledged, DELEGRETURN expected.
    Acked,
    /// BADHANDLE/BAD_STATEID: the disown rule — never insta-drop.
    Disown,
    /// Definitive refusal (NOTSUPP, BADXDR, a malformed reply): the
    /// client cannot or will not run our recall. Revoke.
    Refused,
}

fn classify(reply: &CbCompoundReply) -> Classified {
    // DELAY wins at any level.
    if reply.status == Nfs4Status::Delay
        || reply.results.iter().any(|r| r.status() == Nfs4Status::Delay)
    {
        return Classified::Delay;
    }
    // The CB_RECALL op's own result, wherever it sits behind
    // CB_SEQUENCE.
    let recall_status = reply.results.iter().find_map(|r| match r {
        CbResult::Recall { status } => Some(*status),
        _ => None,
    });
    match recall_status {
        Some(Nfs4Status::Ok) => Classified::Acked,
        Some(Nfs4Status::BadStateId) | Some(Nfs4Status::BadHandle) => Classified::Disown,
        // NOTSUPP, BADXDR, anything else the client answered — or a
        // reply that never reached CB_RECALL (CB_SEQUENCE failed with
        // a non-DELAY error): refusal.
        _ => Classified::Refused,
    }
}

impl RecallDriver {
    pub fn new(state_mgr: Arc<StateManager>, sender: Arc<dyn RecallSender>) -> Self {
        Self {
            state_mgr,
            sender,
            cfg: RecallLadderConfig::default(),
        }
    }

    pub fn with_config(mut self, cfg: RecallLadderConfig) -> Self {
        self.cfg = cfg;
        self
    }

    /// Spawn one ladder task per order. Callers hand this the
    /// `recalls` from a `FenceOutcome::Conflict` — each order is
    /// returned by the fence exactly once, which is what makes the
    /// task single-flight.
    pub fn spawn_recalls(self: &Arc<Self>, orders: Vec<RecallOrder>) {
        for order in orders {
            let driver = Arc::clone(self);
            tokio::spawn(async move {
                driver.drive_one(order).await;
            });
        }
    }

    /// Still live under recall? None = returned/disowned/torn down or
    /// already revoked — either way this task is finished.
    fn still_pending(&self, stateid: &StateId) -> Option<DelegState> {
        self.state_mgr
            .delegations
            .snapshot(stateid)
            .map(|s| s.state)
            .filter(|s| matches!(s, DelegState::RecallPending | DelegState::RecallAcked))
    }

    /// `cb_recall_sent_total` counts CB_RECALL calls that actually
    /// reached the wire — Ok and Timeout both mean the call went out;
    /// a Transport error means nothing did. Counting attempts that
    /// never left would make the sent/acked ratio look like client
    /// misbehaviour when the truth is a dead back-channel, which the
    /// path_down outcome already reports honestly.
    fn note_sent(&self) {
        self.state_mgr.delegations.meter().note_recall_sent();
    }

    fn note_outcome(&self, o: crate::nfs::v4::state::deleg_meter::RecallOutcome) {
        self.state_mgr.delegations.meter().note_outcome(o);
    }

    fn revoke(
        &self,
        order: &RecallOrder,
        reason: crate::nfs::v4::state::deleg_meter::RevokeReason,
        first_transmit: Option<Instant>,
        why: &str,
    ) {
        if let Some(client) = self.state_mgr.delegations.revoke(&order.stateid) {
            let meter = self.state_mgr.delegations.meter();
            meter.note_revoked(reason);
            // §10's histogram is first-transmit -> RETURNED/REVOKED.
            // A revoke with no first transmit (the back-channel never
            // carried the call) has no latency to report, and inventing
            // 0 there would make the p99 look healthy precisely when
            // the recall path was dead.
            if let Some(ft) = first_transmit {
                meter.observe_recall_latency_ms(ft.elapsed().as_millis() as u64);
            }
            warn!(
                "deleg ladder: REVOKING {:?} held by client {} ({})",
                order.stateid, client, why
            );
            // Both managers carry the revocation: the delegation
            // table's tombstone blocks re-grants; the stateid entry's
            // revoked flag is what TEST_STATEID/FREE_STATEID key off.
            let _ = self.state_mgr.stateids.revoke(&order.stateid);
            self.state_mgr
                .raise_seq_flags(client, seq4_status::RECALLABLE_STATE_REVOKED);
        }
    }

    async fn drive_one(&self, order: RecallOrder) {
        let started = Instant::now();
        let mut first_transmit: Option<Instant> = None;
        let mut disown_seen = false;
        let mut rung = 0usize;

        loop {
            // The recheck (model: LadderRecheck). Gone or revoked or —
            // impossibly — re-granted: stop touching it.
            if self.still_pending(&order.stateid).is_none() {
                debug!(
                    "deleg ladder: {:?} resolved out from under the task — done",
                    order.stateid
                );
                return;
            }

            // The deadline check lives at the loop top so a wakeup AT
            // the deadline revokes instead of spending it on one more
            // send the client can no longer honor in time.
            if let Some(ft) = first_transmit {
                if Instant::now() >= ft + self.cfg.revoke_deadline {
                    self.note_outcome(RecallOutcome::Timeout);
                    self.revoke(
                        &order,
                        RevokeReason::Deadline,
                        first_transmit,
                        "recall deadline expired",
                    );
                    return;
                }
            }

            let outcome = self
                .sender
                .send_recall(
                    order.client_id,
                    order.stateid,
                    order.truncate,
                    order.fh.clone(),
                )
                .await;

            match outcome {
                Ok(reply) => {
                    // A reply proves transmission. Path is up.
                    self.note_sent();
                    if first_transmit.is_none() {
                        first_transmit = Some(Instant::now());
                        self.state_mgr.delegations.note_first_transmit(&order.stateid);
                    }
                    self.state_mgr
                        .lower_seq_flags(order.client_id, seq4_status::CB_PATH_DOWN);

                    match classify(&reply) {
                        Classified::Acked => {
                            self.state_mgr.delegations.note_recall_acked(&order.stateid);
                            self.note_outcome(RecallOutcome::Acked);
                            // Wait out the deadline for the DELEGRETURN.
                            let deadline =
                                first_transmit.unwrap() + self.cfg.revoke_deadline;
                            tokio::time::sleep_until(deadline).await;
                            if self.still_pending(&order.stateid).is_some() {
                                // Outcome stays `acked` — the client DID
                                // answer. The revoke reason is what says
                                // it then failed to return, and keeping
                                // the two axes separate is what lets a
                                // rig tell a rude client from a dead one.
                                self.revoke(
                                    &order,
                                    RevokeReason::Deadline,
                                    first_transmit,
                                    "acked but never returned",
                                );
                            }
                            return;
                        }
                        Classified::Delay => {
                            // Fall through to the rung wait below.
                        }
                        Classified::Disown => {
                            if disown_seen {
                                // Re-probe confirmed: the client does
                                // not hold it. Not a revocation — no
                                // SEQ4 bit, no tombstone.
                                self.note_outcome(RecallOutcome::ClientDisowns);
                                if self.state_mgr.delegations.resolve_disown(&order.stateid)
                                {
                                    info!(
                                        "deleg ladder: {:?} disowned twice — dropped",
                                        order.stateid
                                    );
                                }
                                return;
                            }
                            disown_seen = true;
                            tokio::time::sleep(self.cfg.disown_probe_delay).await;
                            continue;
                        }
                        Classified::Refused => {
                            self.note_outcome(RecallOutcome::Refused);
                            self.revoke(
                                &order,
                                RevokeReason::Refused,
                                first_transmit,
                                "client refused the recall",
                            );
                            return;
                        }
                    }
                }
                Err(CallbackError::Timeout) => {
                    // The CALL went out; the reply didn't come back.
                    self.note_sent();
                    if first_transmit.is_none() {
                        first_transmit = Some(Instant::now());
                        self.state_mgr.delegations.note_first_transmit(&order.stateid);
                    }
                    // Fall through to the rung wait below.
                }
                Err(CallbackError::Transport(_)) | Err(CallbackError::ConnectionClosed) => {
                    // Unreachable: no transport accepted the write (or
                    // no channel at all). The CB_PATH_DOWN window, with
                    // retries as the rearm probe — a rebind makes the
                    // next retry succeed (model: RebindRearm).
                    self.state_mgr
                        .raise_seq_flags(order.client_id, seq4_status::CB_PATH_DOWN);
                    let window_over = started.elapsed() >= self.cfg.path_down_window;
                    let past_deadline = first_transmit
                        .map(|t| t.elapsed() >= self.cfg.revoke_deadline)
                        .unwrap_or(false);
                    if past_deadline || (first_transmit.is_none() && window_over) {
                        if self.still_pending(&order.stateid).is_some() {
                            self.note_outcome(RecallOutcome::PathDown);
                            self.revoke(
                                &order,
                                RevokeReason::ChannelDead,
                                first_transmit,
                                "back-channel down past the window",
                            );
                        }
                        return;
                    }
                    tokio::time::sleep(self.cfg.path_retry).await;
                    continue;
                }
                Err(e) => {
                    // Reply-decode failures and other channel wreckage:
                    // the client answered garbage to a recall. Refusal.
                    warn!(
                        "deleg ladder: {:?} recall reply unusable ({:?})",
                        order.stateid, e
                    );
                    self.note_outcome(RecallOutcome::Refused);
                    self.revoke(
                        &order,
                        RevokeReason::Refused,
                        first_transmit,
                        "unusable recall reply",
                    );
                    return;
                }
            }

            // Timeout/DELAY: resend AT first_transmit + rung offsets
            // (the design's "+30s and +60s"), then nothing more until
            // the deadline. `first_transmit` is Some on every path
            // that reaches here.
            let ft = first_transmit.unwrap();
            let deadline = ft + self.cfg.revoke_deadline;
            let wake = match self.cfg.rungs.get(rung) {
                // A rung whose moment already passed (a slow reply ate
                // the gap) resends immediately.
                Some(offset) => std::cmp::min(std::cmp::max(ft + *offset, Instant::now()), deadline),
                // Rungs exhausted: sleep to the deadline; the loop top
                // then revokes.
                None => deadline,
            };
            rung += 1;
            tokio::time::sleep_until(wake).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::state::{FenceOutcome, FileId};
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Scripted sender: pops the next outcome per call, records the
    /// paused-clock instant of every attempt.
    struct MockSender {
        script: Mutex<Vec<Result<CbCompoundReply, CallbackError>>>,
        calls: Mutex<Vec<Instant>>,
    }

    impl MockSender {
        fn new(script: Vec<Result<CbCompoundReply, CallbackError>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                calls: Mutex::new(Vec::new()),
            })
        }
        fn call_times(&self) -> Vec<Instant> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RecallSender for Arc<MockSender> {
        fn send_recall(
            &self,
            _client_id: u64,
            _stateid: StateId,
            _truncate: bool,
            _fh: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<CbCompoundReply, CallbackError>> + Send>>
        {
            self.calls.lock().unwrap().push(Instant::now());
            let mut script = self.script.lock().unwrap();
            let next = if script.is_empty() {
                // Script exhausted: keep answering the last-known
                // shape's most boring cousin — unreachable.
                Err(CallbackError::ConnectionClosed)
            } else {
                script.remove(0)
            };
            Box::pin(async move { next })
        }
    }

    fn reply(recall_status: Nfs4Status) -> Result<CbCompoundReply, CallbackError> {
        Ok(CbCompoundReply {
            status: recall_status,
            tag: String::new(),
            results: vec![
                CbResult::Sequence {
                    status: Nfs4Status::Ok,
                    sessionid: crate::nfs::v4::protocol::SessionId([0; 16]),
                    sequenceid: 1,
                    slotid: 0,
                    highest_slotid: 0,
                    target_highest_slotid: 0,
                },
                CbResult::Recall {
                    status: recall_status,
                },
            ],
        })
    }

    fn sid(n: u8) -> StateId {
        let mut other = [0u8; 12];
        other[0] = n;
        StateId { seqid: 1, other }
    }

    const F: FileId = FileId { dev: 1, ino: 500 };

    /// Grant + fence: returns (state_mgr, stateid, the fence's orders).
    fn granted_and_recalled() -> (Arc<StateManager>, StateId, Vec<RecallOrder>) {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let stateid = state_mgr
            .delegations
            .try_grant(F, 7, vec![0xf], PathBuf::from("/f"), || true, || sid(1))
            .unwrap();
        let orders = match state_mgr.delegations.mutation_fence(F, Some(8), false) {
            FenceOutcome::Conflict { recalls, .. } => recalls,
            FenceOutcome::Clear(_) => panic!("expected a conflict"),
        };
        (state_mgr, stateid, orders)
    }

    fn driver(state_mgr: &Arc<StateManager>, sender: Arc<MockSender>) -> Arc<RecallDriver> {
        Arc::new(RecallDriver::new(Arc::clone(state_mgr), Arc::new(sender)))
    }

    #[tokio::test(start_paused = true)]
    async fn a_cooperating_client_is_never_revoked() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        let sender = MockSender::new(vec![reply(Nfs4Status::Ok)]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::task::yield_now().await;
        // Acked; the client DELEGRETURNs well inside the window.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::RecallAcked
        );
        state_mgr.delegations.return_delegation(&stateid).unwrap();
        // Let the deadline pass; the task must find the record gone
        // and do NOTHING.
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(state_mgr.delegations.snapshot(&stateid).is_none());
        assert_eq!(state_mgr.seq_flags(7), 0, "no SEQ4 bit for a clean return");
    }

    #[tokio::test(start_paused = true)]
    async fn acked_but_never_returned_revokes_at_the_deadline_with_seq4() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        let sender = MockSender::new(vec![reply(Nfs4Status::Ok)]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        // Just before the deadline: still only acked.
        tokio::time::sleep(Duration::from_secs(89)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::RecallAcked
        );
        assert_eq!(state_mgr.seq_flags(7), 0);
        // Past it: revoked, tombstone retained, SEQ4 bit raised.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::Revoked
        );
        assert_ne!(
            state_mgr.seq_flags(7) & seq4_status::RECALLABLE_STATE_REVOKED,
            0,
            "revocation must raise the SEQ4 bit — silent revocation is the named worst case"
        );

        // §10 wiring. Unit tests on DelegMeter prove the meter counts;
        // only this proves the PRODUCTION path calls it. A metric that
        // is never incremented reads exactly like a quiet server, and
        // every §9 rig leg asserts on these numbers — so an unwired
        // counter would make those legs pass by describing silence.
        let m = state_mgr.delegations.meter();
        assert_eq!(m.outcome_count(RecallOutcome::Acked), 1, "the client DID ack");
        assert_eq!(
            m.revoked_count(RevokeReason::Deadline),
            1,
            "and was revoked for never returning — outcome and reason are separate axes"
        );
        assert_eq!(m.revoked_count(RevokeReason::ChannelDead), 0);
        assert_eq!(
            m.seq4_count(seq4_status::RECALLABLE_STATE_REVOKED),
            1,
            "the SEQ4 raise is counted once, not once per ladder retry"
        );
        // first transmit -> revoke spans the 90s deadline, so the
        // latency sample must land in a bucket that reflects that, not
        // in the fast path.
        assert_eq!(m.latency_count(), 1);
        assert!(
            m.latency_percentile_ms(0.99).unwrap() >= 60_000,
            "a 90s deadline revoke is not a sub-minute recall: {:?}",
            m.latency_percentile_ms(0.99)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delay_replies_walk_the_rungs_then_succeed() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        let sender = MockSender::new(vec![
            reply(Nfs4Status::Delay),
            reply(Nfs4Status::Delay),
            reply(Nfs4Status::Ok),
        ]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(89)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::RecallAcked,
            "the third attempt's OK must land before the deadline"
        );
        let times = sender.call_times();
        assert_eq!(times.len(), 3);
        // Resends AT +30s and +60s from the first transmit.
        assert_eq!(times[1] - times[0], Duration::from_secs(30));
        assert_eq!(times[2] - times[0], Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn the_meter_separates_a_dead_channel_from_a_rude_client() {
        // The two failure modes a fleet operator must be able to tell
        // apart from the counters alone: nobody answered (path_down /
        // channel_dead) versus answered and refused (refused/refused).
        // If these collapsed into one number, "delegations are being
        // revoked" would not say whether the network or the client is
        // at fault.
        let (state_mgr, _stateid, orders) = granted_and_recalled();
        let sender = MockSender::new(
            (0..40).map(|_| Err(CallbackError::ConnectionClosed)).collect(),
        );
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(200)).await;

        let m = state_mgr.delegations.meter();
        assert_eq!(m.outcome_count(RecallOutcome::PathDown), 1);
        assert_eq!(m.revoked_count(RevokeReason::ChannelDead), 1);
        assert_eq!(m.outcome_count(RecallOutcome::Refused), 0, "nobody refused anything");
        assert_eq!(m.revoked_count(RevokeReason::Deadline), 0);
        // Nothing ever reached the wire, so there is no recall latency
        // to report — and reporting 0ms here would make a dead
        // back-channel look like the fastest recall the server ever did.
        assert_eq!(m.latency_count(), 0);
        assert_eq!(m.latency_percentile_ms(0.99), None);
        assert_eq!(
            m.cb_recall_sent.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a call that no transport accepted was never sent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_client_gets_the_window_not_instant_revoke() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        // Never reachable.
        let sender = MockSender::new(vec![]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        // Inside the window: CB_PATH_DOWN raised, NOT revoked.
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::RecallPending
        );
        assert_ne!(state_mgr.seq_flags(7) & seq4_status::CB_PATH_DOWN, 0);
        // Window over: revoked.
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::Revoked
        );
        assert_ne!(
            state_mgr.seq_flags(7) & seq4_status::RECALLABLE_STATE_REVOKED,
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebind_inside_the_window_converts_revocation_into_recall() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        // Down for three retries (~15s), then the writer rebinds.
        let sender = MockSender::new(vec![
            Err(CallbackError::ConnectionClosed),
            Err(CallbackError::ConnectionClosed),
            Err(CallbackError::ConnectionClosed),
            reply(Nfs4Status::Ok),
        ]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::RecallAcked,
            "the rebound path must complete the recall"
        );
        // The path came back: CB_PATH_DOWN lowered again.
        assert_eq!(state_mgr.seq_flags(7) & seq4_status::CB_PATH_DOWN, 0);
        // And no revocation happens later (clean return).
        state_mgr.delegations.return_delegation(&stateid).unwrap();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(state_mgr.seq_flags(7), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn one_disown_gets_a_reprobe_two_drop_the_record_without_revoking() {
        // Double disown: dropped, no tombstone, no SEQ4.
        let (state_mgr, stateid, orders) = granted_and_recalled();
        let sender = MockSender::new(vec![
            reply(Nfs4Status::BadStateId),
            reply(Nfs4Status::BadStateId),
        ]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(
            state_mgr.delegations.snapshot(&stateid).is_none(),
            "double disown drops the record entirely"
        );
        assert_eq!(state_mgr.seq_flags(7), 0, "a disown is not a revocation");

        // Single disown then OK: the re-probe saves the record (the
        // recall raced the granting OPEN reply and the client had it
        // installed by the second ask).
        let (state_mgr2, stateid2, orders2) = granted_and_recalled();
        let sender2 = MockSender::new(vec![
            reply(Nfs4Status::BadStateId),
            reply(Nfs4Status::Ok),
        ]);
        let d2 = driver(&state_mgr2, Arc::clone(&sender2));
        d2.spawn_recalls(orders2);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(
            state_mgr2.delegations.snapshot(&stateid2).unwrap().state,
            DelegState::RecallAcked
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_definitive_refusal_revokes_immediately() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        let sender = MockSender::new(vec![reply(Nfs4Status::NotSupp)]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::Revoked
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_record_returned_before_the_task_runs_is_left_alone() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        // The client DELEGRETURNs before the ladder's first attempt.
        state_mgr.delegations.return_delegation(&stateid).unwrap();
        let sender = MockSender::new(vec![reply(Nfs4Status::Ok)]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(200)).await;
        assert!(sender.call_times().is_empty(), "no send for a resolved record");
        assert_eq!(state_mgr.seq_flags(7), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn timeouts_resend_on_rungs_and_revoke_at_the_deadline() {
        let (state_mgr, stateid, orders) = granted_and_recalled();
        // Every attempt times out (the CALL goes out, no reply).
        let sender = MockSender::new(vec![
            Err(CallbackError::Timeout),
            Err(CallbackError::Timeout),
            Err(CallbackError::Timeout),
        ]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(95)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&stateid).unwrap().state,
            DelegState::Revoked,
            "a transmitted-but-unanswered recall revokes at the deadline"
        );
        let times = sender.call_times();
        assert_eq!(times.len(), 3, "first transmit + two rung resends");
    }
}
