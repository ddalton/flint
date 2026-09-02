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
//! The FIRST transmit is batched per client (design §5.4, "batch
//! per-client recalls into one CB_COMPOUND where ca_maxoperations
//! permits"): `rm -rf` over forty delegated files is forty orders for
//! one client, and forty serialized slot-0 round trips would put the
//! fortieth holder's 90s deadline a long way behind the first's. One
//! compound carries as many CB_RECALLs as the client's back channel
//! allows; the reply is split positionally and each record's ladder
//! then runs on its own — rungs and deadlines are per record, and
//! resends are rare enough not to be worth re-batching. Against
//! Linux this is inert: it advertises a back-channel
//! ca_maxoperations of 2 (NFS4_MAX_BACK_CHANNEL_OPS), which is
//! CB_SEQUENCE plus exactly one CB_RECALL, so the chunk size is 1
//! and the wire is byte-identical to the unbatched path. Clients
//! with a wider back channel get the fan-in.
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

/// One CB_RECALL's arguments — one op of a batched compound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallItem {
    pub stateid: StateId,
    pub truncate: bool,
    pub fh: Vec<u8>,
}

impl From<&RecallOrder> for RecallItem {
    fn from(o: &RecallOrder) -> Self {
        Self {
            stateid: o.stateid,
            truncate: o.truncate,
            fh: o.fh.clone(),
        }
    }
}

/// The one send the ladder needs. Implemented by `CallbackManager`
/// (client-addressed: every session, writer failover within each).
pub trait RecallSender: Send + Sync + 'static {
    /// ONE CB_COMPOUND to `client_id`: CB_SEQUENCE followed by one
    /// CB_RECALL per item, in order. The reply's CB_RECALL results are
    /// positional; a compound that stops at a failing op carries fewer
    /// results than items, and the ladder resends the tail
    /// individually. Callers never pass more items than
    /// [`recall_batch_limit`](Self::recall_batch_limit) allows.
    fn send_recalls(
        &self,
        client_id: u64,
        items: Vec<RecallItem>,
    ) -> Pin<Box<dyn Future<Output = Result<CbCompoundReply, CallbackError>> + Send>>;

    /// How many CB_RECALLs one compound to this client may carry —
    /// the back channel's `ca_maxoperations` minus the CB_SEQUENCE.
    /// Floor 1. The default is 1 (no batching), which is also the
    /// honest answer for a Linux client.
    fn recall_batch_limit(&self, _client_id: u64) -> usize {
        1
    }

    /// One recall. Every resend on the ladder goes through here.
    fn send_recall(
        &self,
        client_id: u64,
        stateid: StateId,
        truncate: bool,
        fh: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<CbCompoundReply, CallbackError>> + Send>> {
        self.send_recalls(
            client_id,
            vec![RecallItem {
                stateid,
                truncate,
                fh,
            }],
        )
    }
}

/// `CallbackError` holds an `io::Error` and is not `Clone`; a batch
/// outcome has to be handed to every record it covered.
fn clone_cb_error(e: &CallbackError) -> CallbackError {
    match e {
        CallbackError::Timeout => CallbackError::Timeout,
        CallbackError::Transport(io) => {
            CallbackError::Transport(std::io::Error::new(io.kind(), io.to_string()))
        }
        CallbackError::Reply(r) => CallbackError::Reply(r.clone()),
        CallbackError::ConnectionClosed => CallbackError::ConnectionClosed,
    }
}

/// Split one batched reply into per-record replies, positionally.
///
/// `None` for a record means "the compound stopped before reaching
/// your op" — a SIBLING failed, the client never saw this recall, and
/// it must be sent again on its own. A compound that failed at
/// CB_SEQUENCE itself (no CB_RECALL result at all) is every record's
/// failure, and each gets the sequence-level reply to classify —
/// DELAY there is DELAY for all; anything else is a refusal for all,
/// exactly as the unbatched ladder treats it.
///
/// Each synthesized reply carries only ITS record's CB_RECALL result
/// next to the CB_SEQUENCE result: `classify` treats a DELAY on any
/// result as DELAY, and a sibling's DELAY must not become this
/// record's.
pub(crate) fn split_batch_reply(reply: &CbCompoundReply, n: usize) -> Vec<Option<CbCompoundReply>> {
    let seq = reply
        .results
        .iter()
        .find(|r| matches!(r, CbResult::Sequence { .. }))
        .cloned();
    let recalls: Vec<Nfs4Status> = reply
        .results
        .iter()
        .filter_map(|r| match r {
            CbResult::Recall { status } => Some(*status),
            _ => None,
        })
        .collect();
    let seq_delay = seq.as_ref().map(|s| s.status() == Nfs4Status::Delay).unwrap_or(false)
        || (recalls.is_empty() && reply.status == Nfs4Status::Delay);

    (0..n)
        .map(|i| {
            let mut results = Vec::with_capacity(2);
            if let Some(s) = &seq {
                results.push(s.clone());
            }
            match recalls.get(i) {
                Some(status) => {
                    results.push(CbResult::Recall { status: *status });
                    Some(CbCompoundReply {
                        status: if seq_delay { Nfs4Status::Delay } else { *status },
                        tag: reply.tag.clone(),
                        results,
                    })
                }
                None if recalls.is_empty() => Some(CbCompoundReply {
                    status: reply.status,
                    tag: reply.tag.clone(),
                    results,
                }),
                None => None,
            }
        })
        .collect()
}

/// A first-transmit outcome obtained by the batch send, handed to the
/// record's ladder in place of its own first send.
struct Primed {
    outcome: Result<CbCompoundReply, CallbackError>,
    /// The rearm epoch read BEFORE the batch send — the ladder's
    /// park-on-transport-failure needs the pre-send value.
    rearm_epoch: u64,
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

/// Read a duration from the environment, in whole seconds, falling
/// back to the design's number. A value of 0 is ignored rather than
/// honoured: a zero deadline would revoke every delegation the instant
/// it was recalled, which is not a tuning anyone means.
fn env_secs(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

impl RecallLadderConfig {
    /// The ladder's timings, overridable for rigs and for operators who
    /// have measured their own clients.
    ///
    /// The defaults ARE the design's numbers and stay that way; this
    /// only makes them reachable. The reason it exists: a client's
    /// DELAY-retry budget can be far shorter than our 90s deadline —
    /// pynfs gives a compound 10 retries at 1s and then gives up — so a
    /// conformance leg that watches a revocation happen cannot run at
    /// the production deadline at all. Being unable to exercise the
    /// revoke path against a real client is worse than being able to
    /// exercise it at 5s.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            revoke_deadline: env_secs("FLINT_NFS_DELEG_REVOKE_SECS", d.revoke_deadline),
            // Rungs are left alone: they are offsets INSIDE the
            // deadline, and the wake calculation already clamps them to
            // it, so a shortened deadline simply skips them.
            rungs: d.rungs,
            path_down_window: env_secs(
                "FLINT_NFS_DELEG_PATH_DOWN_SECS",
                d.path_down_window,
            ),
            path_retry: env_secs("FLINT_NFS_DELEG_PATH_RETRY_SECS", d.path_retry),
            disown_probe_delay: d.disown_probe_delay,
        }
    }
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

/// Wire the production recall path. Call once, at server start.
///
/// This is not optional plumbing. Until it runs,
/// `StateManager::recall_machinery_ready()` is false and the grant
/// gate's rule 1 refuses EVERY delegation — deliberately, because a
/// delegation the server cannot recall is the stale-forever trap the
/// whole design exists to avoid. The consequence is that a server
/// missing this call is INERT with the flag on, and its only symptom
/// is that no grants ever happen: a silence indistinguishable from a
/// workload that simply never qualified. (It was missing from both
/// binaries until 2026-09-01. `install_recall_spawner` had callers
/// only in `#[cfg(test)]` code, so every unit test wired it by hand
/// and passed, while pynfs against the real server answered "Could
/// not get delegation" ten times out of ten.)
///
/// Takes the `CallbackManager` rather than building one, and the
/// difference matters: the manager owns the per-session slot-0
/// mutexes. Two managers over the same back-channel registry would
/// each keep their own sequence counter for a session and both send
/// on slot 0, which is precisely the CB_SEQUENCE misordering the
/// mutex exists to prevent. The MDS already has one; the standalone
/// server builds its own and then has exactly one.
pub fn install_recall_machinery(
    state_mgr: &Arc<StateManager>,
    callbacks: Arc<crate::pnfs::mds::callback::CallbackManager>,
) {
    let cfg = RecallLadderConfig::from_env();
    let driver = Arc::new(
        RecallDriver::new(Arc::clone(state_mgr), Arc::new(callbacks)).with_config(cfg.clone()),
    );
    state_mgr.install_recall_spawner(Arc::new(move |orders| driver.spawn_recalls(orders)));
    info!(
        "deleg: recall machinery installed — grants are now possible          (revoke deadline {}s, CB_PATH_DOWN window {}s)",
        cfg.revoke_deadline.as_secs(),
        cfg.path_down_window.as_secs(),
    );
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

    /// Spawn the ladders for a fence's orders. Callers hand this the
    /// `recalls` from a `FenceOutcome::Conflict` — each order is
    /// returned by the fence exactly once, which is what makes every
    /// task single-flight.
    ///
    /// Orders for one client are grouped and their FIRST transmit
    /// batched into compounds of at most `recall_batch_limit` recalls;
    /// a group of one (or a limit of one) is the plain per-record
    /// ladder with no batch task in between.
    pub fn spawn_recalls(self: &Arc<Self>, orders: Vec<RecallOrder>) {
        // Group by client, preserving first-seen order so a rig
        // reading the log sees recalls in fence order.
        let mut groups: Vec<(u64, Vec<RecallOrder>)> = Vec::new();
        for order in orders {
            match groups.iter_mut().find(|(c, _)| *c == order.client_id) {
                Some((_, v)) => v.push(order),
                None => groups.push((order.client_id, vec![order])),
            }
        }
        for (client_id, group) in groups {
            let limit = self.sender.recall_batch_limit(client_id).max(1);
            let mut group = group;
            while !group.is_empty() {
                let take = limit.min(group.len());
                let chunk: Vec<RecallOrder> = group.drain(..take).collect();
                let driver = Arc::clone(self);
                if chunk.len() == 1 {
                    let order = chunk.into_iter().next().unwrap();
                    tokio::spawn(async move {
                        driver.drive_one(order, None).await;
                    });
                } else {
                    tokio::spawn(async move {
                        driver.drive_batch(chunk).await;
                    });
                }
            }
        }
    }

    /// One compound for a client's chunk, then one ladder per record
    /// primed with its slice of the reply. The ladders are spawned
    /// rather than joined so a slow record cannot hold up its
    /// siblings' rungs.
    async fn drive_batch(self: Arc<Self>, chunk: Vec<RecallOrder>) {
        let client_id = chunk[0].client_id;
        let n = chunk.len();
        let rearm = self.state_mgr.delegations.rearm_signal(client_id);
        let rearm_epoch = rearm.epoch();
        let items: Vec<RecallItem> = chunk.iter().map(RecallItem::from).collect();
        debug!(
            "deleg ladder: batching {} recalls to client {} in one CB_COMPOUND",
            n, client_id
        );
        let outcome = self.sender.send_recalls(client_id, items).await;
        self.state_mgr.delegations.meter().note_recall_batch(n as u64);

        let primed: Vec<Option<Primed>> = match &outcome {
            Ok(reply) => split_batch_reply(reply, n)
                .into_iter()
                .map(|r| {
                    r.map(|reply| Primed {
                        outcome: Ok(reply),
                        rearm_epoch,
                    })
                })
                .collect(),
            Err(e) => (0..n)
                .map(|_| {
                    Some(Primed {
                        outcome: Err(clone_cb_error(e)),
                        rearm_epoch,
                    })
                })
                .collect(),
        };
        for (order, primed) in chunk.into_iter().zip(primed) {
            let driver = Arc::clone(&self);
            tokio::spawn(async move {
                driver.drive_one(order, primed).await;
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

    async fn drive_one(&self, order: RecallOrder, mut primed: Option<Primed>) {
        let started = Instant::now();
        let mut first_transmit: Option<Instant> = None;
        let mut disown_seen = false;
        let mut rung = 0usize;
        // Held for the task's whole life so the signal exists across
        // every window we might be woken in — minting it lazily at the
        // first failure would leave a gap where a rebind fires into an
        // empty map and is lost.
        let rearm = self.state_mgr.delegations.rearm_signal(order.client_id);

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

            // Read BEFORE the send: a rebind that lands while the send
            // is in flight must not be slept through (see RearmSignal).
            // A primed first transmit (the batch already sent it)
            // carries the epoch its own send read.
            let (rearm_epoch, outcome) = match primed.take() {
                Some(p) => (p.rearm_epoch, p.outcome),
                None => {
                    let epoch = rearm.epoch();
                    let outcome = self
                        .sender
                        .send_recall(
                            order.client_id,
                            order.stateid,
                            order.truncate,
                            order.fh.clone(),
                        )
                        .await;
                    (epoch, outcome)
                }
            };

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
                    // Park on the rearm signal, not just the timer: a
                    // reconnect re-drives here immediately instead of
                    // costing the fenced writer another `path_retry` of
                    // DELAY cycles. The timer remains the floor, so a
                    // client that never comes back still walks the
                    // window out to the revoke.
                    if rearm.wait(rearm_epoch, self.cfg.path_retry).await {
                        debug!(
                            "deleg ladder: {:?} rearmed by a back-channel rebind — retrying now",
                            order.stateid
                        );
                    }
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

    /// Scripted sender: pops the next outcome per CALL (one compound,
    /// batched or not), records the paused-clock instant and the
    /// batch size of every attempt.
    ///
    /// A scripted reply carrying exactly ONE CB_RECALL result answers
    /// a batched call of N items with that result replicated N times
    /// — so the single-record scripts below need no rewriting, and a
    /// batch test that wants per-position statuses builds them with
    /// `reply_multi`.
    struct MockSender {
        script: Mutex<Vec<Result<CbCompoundReply, CallbackError>>>,
        calls: Mutex<Vec<Instant>>,
        batch_sizes: Mutex<Vec<usize>>,
        limit: std::sync::atomic::AtomicUsize,
    }

    impl MockSender {
        fn new(script: Vec<Result<CbCompoundReply, CallbackError>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                calls: Mutex::new(Vec::new()),
                batch_sizes: Mutex::new(Vec::new()),
                limit: std::sync::atomic::AtomicUsize::new(1),
            })
        }
        fn with_batch_limit(self: Arc<Self>, limit: usize) -> Arc<Self> {
            self.limit.store(limit, std::sync::atomic::Ordering::Relaxed);
            self
        }
        fn call_times(&self) -> Vec<Instant> {
            self.calls.lock().unwrap().clone()
        }
        fn batch_sizes(&self) -> Vec<usize> {
            self.batch_sizes.lock().unwrap().clone()
        }
    }

    impl RecallSender for Arc<MockSender> {
        fn send_recalls(
            &self,
            _client_id: u64,
            items: Vec<RecallItem>,
        ) -> Pin<Box<dyn Future<Output = Result<CbCompoundReply, CallbackError>> + Send>>
        {
            self.calls.lock().unwrap().push(Instant::now());
            self.batch_sizes.lock().unwrap().push(items.len());
            let mut script = self.script.lock().unwrap();
            let next = if script.is_empty() {
                // Script exhausted: keep answering the last-known
                // shape's most boring cousin — unreachable.
                Err(CallbackError::ConnectionClosed)
            } else {
                script.remove(0)
            };
            let next = match next {
                Ok(mut reply) if items.len() > 1 => {
                    let recalls: Vec<CbResult> = reply
                        .results
                        .iter()
                        .filter(|r| matches!(r, CbResult::Recall { .. }))
                        .cloned()
                        .collect();
                    if recalls.len() == 1 {
                        for _ in 1..items.len() {
                            reply.results.push(recalls[0].clone());
                        }
                    }
                    Ok(reply)
                }
                other => other,
            };
            Box::pin(async move { next })
        }

        fn recall_batch_limit(&self, _client_id: u64) -> usize {
            self.limit.load(std::sync::atomic::Ordering::Relaxed)
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

    /// A batched reply with per-position CB_RECALL statuses — the
    /// shape a client answers when it processed some recalls and
    /// stopped at the first failing one (a shorter list than the
    /// batch is exactly that).
    fn reply_multi(statuses: &[Nfs4Status]) -> Result<CbCompoundReply, CallbackError> {
        let mut results = vec![CbResult::Sequence {
            status: Nfs4Status::Ok,
            sessionid: crate::nfs::v4::protocol::SessionId([0; 16]),
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            target_highest_slotid: 0,
        }];
        for s in statuses {
            results.push(CbResult::Recall { status: *s });
        }
        Ok(CbCompoundReply {
            status: *statuses.last().unwrap_or(&Nfs4Status::Ok),
            tag: String::new(),
            results,
        })
    }

    fn sid(n: u8) -> StateId {
        let mut other = [0u8; 12];
        other[0] = n;
        StateId { seqid: 1, other }
    }

    const F: FileId = FileId { dev: 1, ino: 500 };

    /// `n` grants to `client` on distinct files, each then fenced by
    /// client 8 — the shape of `rm -rf` over a holder's warm set. The
    /// orders come back in ONE Vec because a real fence over a
    /// directory would hand them over per file; the driver's grouping
    /// is what this exercises.
    fn granted_and_recalled_n(
        client: u64,
        n: u8,
    ) -> (Arc<StateManager>, Vec<StateId>, Vec<RecallOrder>) {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let mut sids = Vec::new();
        let mut orders = Vec::new();
        for i in 1..=n {
            let ident = FileId { dev: 1, ino: 500 + i as u64 };
            let s = state_mgr
                .delegations
                .try_grant(ident, client, vec![i], PathBuf::from(format!("/f{i}")), || true, || sid(i))
                .unwrap();
            sids.push(s);
            match state_mgr.delegations.mutation_fence(ident, Some(8), false) {
                FenceOutcome::Conflict { recalls, .. } => orders.extend(recalls),
                FenceOutcome::Clear(_) => panic!("expected a conflict"),
            }
        }
        (state_mgr, sids, orders)
    }

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

/// The part of a source file that is NOT the trailing `mod tests`.
    ///
    /// Cutting at the first `#[cfg(test)]` is WRONG and was wrong here: a
    /// `#[cfg(test)]` attribute on a single production helper appears at
    /// ioops.rs:459, so a scan that stopped there covered 459 lines of a
    /// 4000-line file and its red proof passed. Cut only at the attribute
    /// that actually introduces a module.
    fn production_source(src: &str) -> &str {
        let lines: Vec<&str> = src.lines().collect();
        let mut off = 0usize;
        for (i, l) in lines.iter().enumerate() {
            if l.trim() == "#[cfg(test)]" {
                let next = lines[i + 1..]
                    .iter()
                    .find(|x| !x.trim().is_empty())
                    .map(|x| x.trim_start())
                    .unwrap_or("");
                if next.starts_with("mod ") || next.starts_with("pub mod ") {
                    return &src[..off];
                }
            }
            off += l.len() + 1;
        }
        src
    }

    /// Every production server must install the recall machinery.
    ///
    /// `NfsServer` gets a direct behavioural test (server_v4.rs's
    /// `a_constructed_server_can_actually_recall`). `MdsServer::new`
    /// needs a backend, a config tree and a DS registry, so there is
    /// no cheap way to construct one here — and "too expensive to
    /// test" is exactly the gap the missing wiring lived in. A source
    /// scan is coarse, but it fails when someone deletes the call,
    /// which is the failure that actually happened.
    ///
    /// Scoped to the text BEFORE any `#[cfg(test)]`, so a test that
    /// wires the spawner by hand — as every unit test did while both
    /// binaries shipped without it — cannot satisfy this. Comment
    /// lines are stripped first, and that is not fastidiousness: the
    /// first version of this test matched the bare identifier, and its
    /// red proof PASSED, because deleting the call left behind a
    /// comment two lines above it that mentioned the function by name.
    /// A scan that a comment can satisfy is a scan that proves nothing.
    #[test]
    fn every_production_server_installs_the_recall_machinery() {
        const SERVERS: &[&str] = &["src/nfs/server_v4.rs", "src/pnfs/mds/server.rs"];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for rel in SERVERS {
            let src = std::fs::read_to_string(root.join(rel))
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
            let production = production_source(&src);
            let code: String = production
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let re = regex::Regex::new(r"install_recall_machinery\s*\(").unwrap();
            assert!(
                re.is_match(&code),
                "{rel} never CALLS install_recall_machinery, so its grant gate \
                 refuses every delegation — silently, because a refused grant \
                 looks exactly like a workload that never qualified",
            );
        }
    }

    /// The design's numbers are the DEFAULTS, and stay so.
    ///
    /// `from_env` exists to let a rig shorten the deadline, which is
    /// exactly the kind of knob that quietly becomes the production
    /// value. This pins the unset case against §5.4 directly, so
    /// changing the shipped behaviour has to be a deliberate edit here
    /// and not a side effect of making something testable.
    #[test]
    fn the_shipped_ladder_timings_are_the_designs_numbers() {
        let d = RecallLadderConfig::default();
        assert_eq!(d.revoke_deadline, Duration::from_secs(90));
        assert_eq!(d.rungs, [Duration::from_secs(30), Duration::from_secs(60)]);
        assert_eq!(d.path_down_window, Duration::from_secs(30));
        assert_eq!(d.path_retry, Duration::from_secs(5));

        // A zero is refused rather than honoured: "revoke immediately"
        // is not a tuning anyone means by writing 0, and it would make
        // every recall a revocation.
        assert_eq!(env_secs("FLINT_NFS_DELEG_NO_SUCH_VAR", d.revoke_deadline), d.revoke_deadline);
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
    async fn a_rebind_wakes_a_parked_ladder_instead_of_waiting_out_the_retry_timer() {
        // CONTROL FIRST, and it is not a formality: the treatment leg
        // below asserts the second attempt lands 1s after the first,
        // and that number means nothing unless the timer alone would
        // have put it somewhere else. This leg pins where.
        let (sm_c, _sid_c, orders_c) = granted_and_recalled();
        let sender_c = MockSender::new(vec![
            Err(CallbackError::ConnectionClosed),
            reply(Nfs4Status::Ok),
        ]);
        let d_c = driver(&sm_c, Arc::clone(&sender_c));
        d_c.spawn_recalls(orders_c);
        tokio::time::sleep(Duration::from_secs(8)).await;
        let t_c = sender_c.call_times();
        assert_eq!(t_c.len(), 2);
        assert_eq!(
            t_c[1] - t_c[0],
            Duration::from_secs(5),
            "control: with no rebind the retry timer is the only wake",
        );
        assert_eq!(sm_c.delegations.meter().rearm_total(), 0);

        // TREATMENT: identical script, but the client rebinds at +1s.
        let (sm, stateid, orders) = granted_and_recalled();
        let sender = MockSender::new(vec![
            Err(CallbackError::ConnectionClosed),
            reply(Nfs4Status::Ok),
        ]);
        let d = driver(&sm, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(1)).await;
        sm.delegations.note_rearm(7);
        tokio::time::sleep(Duration::from_secs(1)).await;

        let t = sender.call_times();
        assert_eq!(t.len(), 2, "the rebind re-drove the recall");
        assert_eq!(
            t[1] - t[0],
            Duration::from_secs(1),
            "the rebind, not the 5s timer, is what woke the ladder",
        );
        assert_eq!(
            sm.delegations.snapshot(&stateid).unwrap().state,
            DelegState::RecallAcked,
        );
        assert_eq!(sm.delegations.meter().rearm_total(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebind_landing_before_the_park_is_caught_by_the_epoch() {
        // The race the epoch exists for. The ladder reads the epoch,
        // its send fails, and the rebind lands in the gap BEFORE it
        // parks — so `notify_waiters` fires with no waiter registered
        // and wakes nobody. Only the counter can catch this, and it is
        // the single epoch read in `wait` that does: delete it and
        // this test sleeps out the hour below.
        let sm = Arc::new(StateManager::new_in_memory(""));
        let sig = sm.delegations.rearm_signal(7);
        let since = sig.epoch();
        sm.delegations.note_rearm(7);

        // An hour of budget makes the failure unmissable: without the
        // epoch re-read, the paused clock jumps the whole hour and
        // `wait` comes back false.
        let t0 = Instant::now();
        assert!(
            sig.wait(since, Duration::from_secs(3600)).await,
            "a rebind that landed before the park must not be slept through",
        );
        assert_eq!(Instant::now() - t0, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn cb_path_down_outlives_the_ladder_and_only_a_rebind_takes_it_down() {
        let (state_mgr, _stateid, orders) = granted_and_recalled();
        let session = state_mgr
            .sessions
            .create_session(7, 0, 0, 10, 4096, 4096, 8, 10, 0x4000_0000, None, 1);
        // Never reachable: the ladder walks the window out and revokes.
        let sender = MockSender::new(vec![]);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(60)).await;

        // The ladder has revoked and RETURNED. The bit it raised is
        // still on every SEQUENCE reply and — before this change —
        // nothing left in the server had any reason to lower it. That
        // is the state a client sits in once its network heals: told
        // to repair a path that is already fine, for the rest of its
        // life. RFC 8881 §2.10.4 makes BIND_CONN_TO_SESSION the
        // client's response to the bit, so the stuck bit drives the
        // repair in a loop.
        assert_ne!(
            state_mgr.seq_flags(7) & seq4_status::CB_PATH_DOWN,
            0,
            "precondition: the ladder left the bit up",
        );

        state_mgr.note_back_channel_bound(&session.session_id);
        assert_eq!(state_mgr.seq_flags(7) & seq4_status::CB_PATH_DOWN, 0);

        // ...and it clears ONLY that. The client still holds a revoked
        // delegation it has not been told about, and that bit is a
        // different fact with a different resolution (FREE_STATEID).
        assert_ne!(
            state_mgr.seq_flags(7) & seq4_status::RECALLABLE_STATE_REVOKED,
            0,
            "a healed back-channel does not un-revoke anything",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebind_is_scoped_to_the_client_that_rebound() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let s7 = state_mgr
            .sessions
            .create_session(7, 0, 0, 10, 4096, 4096, 8, 10, 0x4000_0000, None, 1);
        state_mgr.raise_seq_flags(7, seq4_status::CB_PATH_DOWN);
        state_mgr.raise_seq_flags(8, seq4_status::CB_PATH_DOWN);

        state_mgr.note_back_channel_bound(&s7.session_id);
        assert_eq!(state_mgr.seq_flags(7) & seq4_status::CB_PATH_DOWN, 0);
        assert_ne!(
            state_mgr.seq_flags(8) & seq4_status::CB_PATH_DOWN,
            0,
            "one client's reconnect says nothing about another's path",
        );

        // An unknown session resolves to no client and must be inert
        // rather than clearing something at random.
        let bogus = crate::nfs::v4::protocol::SessionId([0xab; 16]);
        state_mgr.note_back_channel_bound(&bogus);
        assert_ne!(state_mgr.seq_flags(8) & seq4_status::CB_PATH_DOWN, 0);
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

    // ── Per-client recall batching (design §5.4) ─────────────────────

    /// `split_batch_reply` is the whole correctness of batching: every
    /// record must get ITS OWN result and nothing of its siblings'.
    #[test]
    fn a_batched_reply_is_split_positionally_and_a_siblings_delay_stays_its_own() {
        let r = reply_multi(&[Nfs4Status::Ok, Nfs4Status::Delay, Nfs4Status::BadStateId]).unwrap();
        let parts = split_batch_reply(&r, 3);
        assert_eq!(parts.len(), 3);
        let p0 = parts[0].as_ref().unwrap();
        let p1 = parts[1].as_ref().unwrap();
        let p2 = parts[2].as_ref().unwrap();
        assert!(matches!(classify(p0), Classified::Acked), "position 0 is OK");
        assert!(matches!(classify(p1), Classified::Delay), "position 1 is DELAY");
        assert!(matches!(classify(p2), Classified::Disown), "position 2 is BAD_STATEID");
        // Each synthesized reply carries exactly [Sequence, own Recall].
        for p in [p0, p1, p2] {
            assert_eq!(p.results.len(), 2);
        }
        // A compound that STOPPED after two ops leaves the third with
        // no result: None, to be resent alone — never a sibling's
        // status.
        let short = reply_multi(&[Nfs4Status::Ok, Nfs4Status::NotSupp]).unwrap();
        let parts = split_batch_reply(&short, 3);
        assert!(parts[0].is_some() && parts[1].is_some());
        assert!(parts[2].is_none(), "the unreached op is nobody's answer");
        // A failure AT CB_SEQUENCE (no recall ran) is everyone's.
        let seq_only = CbCompoundReply {
            status: Nfs4Status::BadSession,
            tag: String::new(),
            results: vec![CbResult::Sequence {
                status: Nfs4Status::BadSession,
                sessionid: crate::nfs::v4::protocol::SessionId([0; 16]),
                sequenceid: 1,
                slotid: 0,
                highest_slotid: 0,
                target_highest_slotid: 0,
            }],
        };
        let parts = split_batch_reply(&seq_only, 2);
        for p in &parts {
            assert!(matches!(classify(p.as_ref().unwrap()), Classified::Refused));
        }
        let mut seq_delay = seq_only.clone();
        seq_delay.status = Nfs4Status::Delay;
        seq_delay.results[0] = CbResult::Sequence {
            status: Nfs4Status::Delay,
            sessionid: crate::nfs::v4::protocol::SessionId([0; 16]),
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            target_highest_slotid: 0,
        };
        for p in split_batch_reply(&seq_delay, 2) {
            assert!(matches!(classify(&p.unwrap()), Classified::Delay));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_clients_recalls_share_one_compound_when_its_back_channel_allows() {
        let (state_mgr, sids, orders) = granted_and_recalled_n(7, 3);
        assert_eq!(orders.len(), 3);
        let sender = MockSender::new(vec![reply(Nfs4Status::Ok)]).with_batch_limit(8);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(sender.batch_sizes(), vec![3], "one compound carried all three");
        for s in &sids {
            assert_eq!(
                state_mgr.delegations.snapshot(s).unwrap().state,
                DelegState::RecallAcked,
                "every record in the batch is acked from its own result"
            );
        }
        let m = state_mgr.delegations.meter();
        assert_eq!(m.recall_batches(), 1);
        assert_eq!(m.recall_batched_ops(), 3);
        assert_eq!(
            m.cb_recall_sent.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "sent counts CB_RECALL ops, not compounds — the sent/acked ratio stays per op"
        );
        assert_eq!(m.outcome_count(RecallOutcome::Acked), 3);
        // The ladders are still independent: all three return, none
        // is revoked at the deadline.
        for s in &sids {
            state_mgr.delegations.return_delegation(s).unwrap();
        }
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(state_mgr.seq_flags(7), 0);
    }

    /// THE LINUX PIN. A Linux client advertises back-channel
    /// ca_maxoperations 2 (CB_SEQUENCE + one op), so its batch limit is
    /// 1 and the driver must send three compounds — byte-identical to
    /// the unbatched ladder. Without this leg the test above would
    /// pass against a driver that batched regardless of the limit,
    /// which a Linux client answers with NFS4ERR_TOO_MANY_OPS.
    #[tokio::test(start_paused = true)]
    async fn a_back_channel_of_two_ops_gets_no_batching() {
        let (state_mgr, sids, orders) = granted_and_recalled_n(7, 3);
        let sender = MockSender::new(vec![
            reply(Nfs4Status::Ok),
            reply(Nfs4Status::Ok),
            reply(Nfs4Status::Ok),
        ])
        .with_batch_limit(1);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(sender.batch_sizes(), vec![1, 1, 1]);
        for s in &sids {
            assert_eq!(state_mgr.delegations.snapshot(s).unwrap().state, DelegState::RecallAcked);
        }
        assert_eq!(state_mgr.delegations.meter().recall_batches(), 0, "a compound of one is not a batch");
    }

    #[tokio::test(start_paused = true)]
    async fn a_group_larger_than_the_limit_is_chunked() {
        let (state_mgr, sids, orders) = granted_and_recalled_n(7, 5);
        let sender = MockSender::new(vec![
            reply(Nfs4Status::Ok),
            reply(Nfs4Status::Ok),
            reply(Nfs4Status::Ok),
        ])
        .with_batch_limit(2);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(sender.batch_sizes(), vec![2, 2, 1]);
        for s in &sids {
            assert_eq!(state_mgr.delegations.snapshot(s).unwrap().state, DelegState::RecallAcked);
        }
        let m = state_mgr.delegations.meter();
        assert_eq!(m.recall_batches(), 2, "the trailing single is not a batch");
        assert_eq!(m.recall_batched_ops(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn two_clients_get_two_compounds() {
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let mut orders = Vec::new();
        for (i, client) in [(1u8, 7u64), (2, 7), (3, 9), (4, 9)] {
            let ident = FileId { dev: 1, ino: 600 + i as u64 };
            state_mgr
                .delegations
                .try_grant(ident, client, vec![i], PathBuf::from(format!("/g{i}")), || true, || sid(i))
                .unwrap();
            match state_mgr.delegations.mutation_fence(ident, Some(8), false) {
                FenceOutcome::Conflict { recalls, .. } => orders.extend(recalls),
                FenceOutcome::Clear(_) => panic!(),
            }
        }
        let sender = MockSender::new(vec![reply(Nfs4Status::Ok), reply(Nfs4Status::Ok)])
            .with_batch_limit(8);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(sender.batch_sizes(), vec![2, 2], "one compound per client, never across clients");
        assert_eq!(state_mgr.delegations.meter().recall_batches(), 2);
    }

    /// A batch where the SECOND recall is disowned: the compound stops
    /// there, so the first is acked from its result, the second walks
    /// the disown re-probe on its own ladder, and the third — never
    /// reached — is resent alone rather than inheriting anyone's
    /// answer.
    #[tokio::test(start_paused = true)]
    async fn a_failing_sibling_stops_the_compound_and_the_tail_is_resent_alone() {
        let (state_mgr, sids, orders) = granted_and_recalled_n(7, 3);
        let sender = MockSender::new(vec![
            reply_multi(&[Nfs4Status::Ok, Nfs4Status::BadStateId]),
            // The two follow-ups, in whatever order they land: the
            // third record's solo resend (immediate) and the second
            // record's re-probe (+2s). Both OK.
            reply(Nfs4Status::Ok),
            reply(Nfs4Status::Ok),
        ])
        .with_batch_limit(8);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&sids[0]).unwrap().state,
            DelegState::RecallAcked,
            "first record: acked from position 0"
        );
        assert_eq!(
            state_mgr.delegations.snapshot(&sids[1]).unwrap().state,
            DelegState::RecallPending,
            "second record: disowned once, awaiting its re-probe — NOT dropped, NOT revoked"
        );
        assert_eq!(
            state_mgr.delegations.snapshot(&sids[2]).unwrap().state,
            DelegState::RecallAcked,
            "third record: unreached in the compound, resent alone, acked"
        );
        assert_eq!(sender.batch_sizes(), vec![3, 1], "so far: the batch and the solo resend");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            state_mgr.delegations.snapshot(&sids[1]).unwrap().state,
            DelegState::RecallAcked,
            "the re-probe's OK saves the second record"
        );
        assert_eq!(sender.batch_sizes(), vec![3, 1, 1]);
        assert_eq!(state_mgr.seq_flags(7), 0, "nothing was revoked");
    }

    /// A batch whose compound never reached the client is every
    /// record's path-down: each ladder parks in the window with the
    /// epoch the batch read, and a rebind re-drives them all.
    #[tokio::test(start_paused = true)]
    async fn a_batch_that_finds_no_transport_parks_every_record_and_a_rebind_re_drives_them() {
        let (state_mgr, sids, orders) = granted_and_recalled_n(7, 3);
        let sender = MockSender::new(vec![
            Err(CallbackError::ConnectionClosed),
            // After the rebind each record retries ALONE (resends are
            // per record); three OKs.
            reply(Nfs4Status::Ok),
            reply(Nfs4Status::Ok),
            reply(Nfs4Status::Ok),
        ])
        .with_batch_limit(8);
        let d = driver(&state_mgr, Arc::clone(&sender));
        d.spawn_recalls(orders);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(sender.batch_sizes(), vec![3]);
        assert_ne!(state_mgr.seq_flags(7) & seq4_status::CB_PATH_DOWN, 0);
        for s in &sids {
            assert_eq!(state_mgr.delegations.snapshot(s).unwrap().state, DelegState::RecallPending);
        }
        state_mgr.delegations.note_rearm(7);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(sender.batch_sizes(), vec![3, 1, 1, 1], "the rebind woke all three parked ladders");
        for s in &sids {
            assert_eq!(state_mgr.delegations.snapshot(s).unwrap().state, DelegState::RecallAcked);
        }
        assert_eq!(state_mgr.seq_flags(7) & seq4_status::CB_PATH_DOWN, 0);
    }
}
