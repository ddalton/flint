//! The subtree lease, held for ONE BARRIER's commit section (design
//! 2026-09-13, `docs/plans/flint-lean-writer-lease-and-gated-assessment.md`
//! §4), with a FIFO ticket so W writers wait at most W barriers.
//!
//! What the cell arbitrates is who may INSTALL a manifest. Nothing
//! else: checkout, sync, status and the uploads of a barrier hold
//! nothing, because every one of those is guarded by the object's own
//! etag (`If-Match`) and installs no citation. Between barriers nobody
//! holds the cell, so a second writer on the same workspace is Ready in
//! checkout time and publishes every floor — the life-long lease this
//! replaces made it wait the first writer's LIFETIME in
//! ContainerCreating (`verbs.rs` used to call it "a DEADLOCK dressed as
//! mutual exclusion", and had already taken checkout out of it).
//!
//! Deliberately NOT `tier::epoch::claim`: that path (a) runs the
//! bucket-wide MPU takeover sweep, which a project-scoped proxy denies
//! (the sweep is an operator-side job in lean — plan §2.4), and (b)
//! gates self-recognition on the hub's state-directory occupancy lock.
//! Lean's self-recognition token is the PERSISTED INCARNATION ID: it is
//! emptyDir-scoped, so only the same pod's restarted container inherits
//! it — which is exactly the one case where immediate self-supersede is
//! safe. A replacement pod gets a fresh id and must wait out the quiet
//! polls; that observation ({last_token, quiet_polls}) persists so a
//! container restart RESUMES it instead of resetting the clock.
//!
//! The claim step is one read plus at most one CAS. Its verdicts:
//!
//! - a FRESH cell: acquire.
//! - a cell that names THIS incarnation at THIS epoch: our own acquire
//!   whose response was lost — adopt it.
//! - a RELEASED cell reserved for nobody, or for us: acquire (a clean
//!   handoff; no manifest rotation).
//! - a RELEASED cell reserved for someone else: queue up once, and wait
//!   — unless the reservation has stood unclaimed across
//!   `HANDOFF_QUIET_POLLS` spaced observations (the reserved holder
//!   died), in which case take it.
//! - a cell HELD by someone else (or by us at an epoch we never
//!   recorded: an orphaned acquire): queue up once, and wait — unless it
//!   has stood still across `QUIET_POLLS` spaced observations (the
//!   holder died inside its commit section), in which case DEPOSE it and
//!   rotate the manifest, exactly the straggler fence there always was.
//!
//! An observation is "spaced" when at least `QUIET_SPACING_SECS` passed
//! since the previous counted one. The wait loop polls faster than
//! that (`CLAIM_POLL_SECS`), so a release is seen within a second, but
//! deadness is still judged over a minute of a token that does not move.
//! A waiter's own enqueue moves the token; the holder's next renew or
//! handoff 412s, re-reads, and adopts the longer queue (`renew`,
//! `release`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use flint_store::{EpochLease, EpochState, GenerationStamps, ObjectStore, PutCondition, StoreError};

use super::state::Incarnation;
use super::{manifest, LeanError, LeanResult, Syncer};

/// Spaced quiet observations before a HELD cell's holder is judged dead
/// and deposed (6 x 10 s: the takeover threshold the drills were run at).
pub const QUIET_POLLS: u32 = 6;
/// Spaced quiet observations before a RELEASED cell's reservation is
/// judged abandoned (the reserved waiter died before claiming).
pub const HANDOFF_QUIET_POLLS: u32 = 2;
/// Minimum spacing between two observations that COUNT toward either
/// threshold. Polls in between only look for a release or a handoff.
pub const QUIET_SPACING_SECS: u64 = 10;
/// The wait loop's poll cadence: one GET of a few-hundred-byte cell.
pub const CLAIM_POLL_SECS: u64 = 1;
/// Longest a barrier waits for the cell before failing (and being
/// retried at the next floor). Comfortably past the deposal threshold
/// plus the spacing it needs, so a dead holder is always deposed within
/// one wait; a LIVE holder that never releases is a bug, and the
/// deadline is what keeps it from being a hang.
pub const CLAIM_DEADLINE_SECS: u64 = 150;
/// The writer heartbeat's interval (`heartbeat`), fixed and independent
/// of the floor. Its readers judge staleness in minutes — the gateway
/// and the operator both at five — and none of them is a fence: the cell
/// detects a dead holder from its own token, and a stale reading costs
/// a UI a slower answer or a conflict record, never bytes. It was
/// min(floor, 30) s, which at a 5 s floor was one PUT every 5 s per idle
/// writer for readers that look every few minutes.
pub const HEARTBEAT_SECS: u64 = 60;

pub enum ClaimOutcome {
    /// Fresh, released-for-us, deposed, or adopted: held.
    Claimed(EpochLease),
    /// Not ours yet. `quiet_polls` is the spaced-observation count
    /// against the current verdict's threshold; `behind` names the
    /// holder or the reserved waiter we are waiting on.
    Waiting { quiet_polls: u32, behind: Option<String> },
}

/// Say so, loudly, if another product also writes this prefix.
///
/// Detection, deliberately NOT enforcement — see the forge twin. A
/// lean workspace and a forge repository on one prefix arbitrate on
/// different cells and will never fence each other, so without this
/// nothing anywhere reports the condition.
///
/// Note what is NOT a finding: forge's legible export IS a lean
/// workspace, published by forge rather than by an agent's syncer, so
/// a syncer reading an export prefix finds only its own kind of cell.
/// And an export nested under a repository's prefix puts this cell at
/// `<prefix>/inner/.flint/lean/epoch`, which is not the key forge
/// probes — the exact-key probe is what keeps that from reading as a
/// collision.
pub async fn warn_if_prefix_is_shared(sc: &Syncer) {
    // A published mirror does not probe. Two reasons, and the second is
    // the load-bearing one:
    //
    // Forge spawns a barrier per export, so this would be a recurring
    // read for a condition that cannot change between two barriers of
    // the same publisher. And forge echoes only the child lines
    // containing "barrier" (export.rs:389), so a warning raised here
    // would be read by nobody — a check whose output is discarded is
    // worse than no check, because it looks like coverage.
    //
    // The prefix is still probed, ONCE, by the publisher at startup:
    // see `warn_if_export_prefix_is_shared` in forge's twin. Moved, not
    // dropped — an export prefix is a prefix two products both write
    // legitimately, which makes it likelier than most to have a third
    // pointed at it.
    if sc.cfg.sole_writer {
        return;
    }
    let found = flint_store::layout::neighbours(
        sc.store.as_ref(),
        &sc.cfg.prefix,
        flint_store::layout::Writer::LeanWorkspace,
    )
    .await
    .unwrap_or_default();
    for f in found {
        eprintln!("flint-sync: {}", f.report());
    }
}

/// This pod's identity, minted once and persisted in the state
/// directory (emptyDir-scoped: a restarted container inherits it, a
/// replacement pod does not).
pub fn incarnation(sc: &Syncer) -> LeanResult<Incarnation> {
    match sc.state.load_incarnation()? {
        Some(i) => Ok(i),
        None => {
            let i = Incarnation {
                holder_id: format!("lean-{}", uuid::Uuid::new_v4()),
                epoch: 0,
                last_token: None,
                quiet_polls: 0,
            };
            sc.state.save_incarnation(&i)?;
            Ok(i)
        }
    }
}

/// One claim step. The caller loops on `Waiting` at its poll cadence;
/// each call performs one read and at most one CAS (an acquire, or an
/// enqueue). `count` says whether this observation is spaced far
/// enough from the last counted one to advance the quiet thresholds —
/// the loop decides that from its own clock, so the judgement of
/// deadness stays tied to wall time however fast the loop polls.
pub async fn claim_step(sc: &mut Syncer, count: bool) -> LeanResult<ClaimOutcome> {
    let store: &Arc<dyn ObjectStore> = &sc.store;
    let key = sc.cfg.epoch_key();
    let mut inc = incarnation(sc)?;

    let observed = store.epoch_read(&key).await?;
    let Some(state) = observed else {
        return match store.epoch_acquire(&key, &inc.holder_id, None).await {
            Ok(lease) => {
                inc.epoch = lease.epoch;
                inc.last_token = None;
                inc.quiet_polls = 0;
                sc.state.save_incarnation(&inc)?;
                sc.lease = Some(lease.clone());
                sc.trace("claim", serde_json::json!({"verdict": "claimed", "how": "fresh", "epoch": lease.epoch}));
                Ok(ClaimOutcome::Claimed(lease))
            }
            Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                Ok(ClaimOutcome::Waiting { quiet_polls: 0, behind: None })
            }
            Err(e) => Err(e.into()),
        };
    };

    // Self-recognition needs the EPOCH too (review 2026-09-12, lease-3 /
    // audit #7): a cell naming this holder at an epoch this incarnation
    // never recorded is our own acquire whose response was lost, or
    // whose rotation failed after it landed — the straggler it deposed
    // may still be mid-barrier, so that path takes the takeover
    // rotation like any other.
    let same_holder = state.holder_id == inc.holder_id;
    let ours = same_holder && state.epoch == inc.epoch && !state.released;
    let quiet = inc.last_token.as_deref() == Some(state.token.as_str());
    let counted_quiet = if count {
        if quiet {
            inc.quiet_polls + 1
        } else {
            0
        }
    } else {
        inc.quiet_polls
    };

    if ours {
        // Our own acquire whose response was lost, or a cell a previous
        // container of this pod is still named on at the epoch we
        // recorded: adopt it IN PLACE — token and queue as they stand,
        // no write. (Re-acquiring would move the epoch for nothing and
        // make every reader of the cell count a barrier that never ran.)
        let lease = EpochLease {
            holder_id: state.holder_id,
            epoch: state.epoch,
            token: state.token,
            waiters: state.waiters,
        };
        inc.last_token = None;
        inc.quiet_polls = 0;
        sc.state.save_incarnation(&inc)?;
        sc.lease = Some(lease.clone());
        sc.trace("claim", serde_json::json!({"verdict": "claimed", "how": "adopted-own", "epoch": lease.epoch}));
        return Ok(ClaimOutcome::Claimed(lease));
    }

    // The verdict. `take` = acquire now; `rotate` = the acquire is a
    // deposal of a possibly-live straggler and must fence it.
    let (take, rotate, behind) = if same_holder && !state.released {
        // Orphaned own: our acquire landed and its response was lost, or
        // its rotation failed after it landed. The straggler it deposed
        // may still be mid-commit, so this takes the rotation too.
        (true, true, None)
    } else if state.released {
        match state.handoff.as_deref() {
            None => (true, false, None),
            Some(h) if h == inc.holder_id => (true, false, None),
            Some(h) => (counted_quiet >= HANDOFF_QUIET_POLLS, false, Some(h.to_string())),
        }
    } else {
        // Held by someone else, or by us at an epoch we never recorded.
        (counted_quiet >= QUIET_POLLS, true, Some(state.holder_id.clone()))
    };

    if take {
        // Rotation is needed ONLY for the unreleased-foreign takeover (a
        // possibly-live straggler mid-commit). A released cell is a
        // clean handoff — the holder's barrier completed before its
        // release — and self-recognition means the previous container's
        // process (and any in-flight write of its) died with it.
        // Rotating on those paths is pure manifest churn: at 100k+
        // entries it is a multi-MB GET+PUT per claim, it double-bumps
        // seq, and it defeats the no-change barrier's early exit
        // (measured on the 0b rig). Under the per-barrier lease that
        // would be every boundary.
        let prior = serde_json::json!({"holder": state.holder_id, "epoch": state.epoch, "released": state.released,
            "handoff": state.handoff, "waiters": state.waiters});
        let how = if rotate && same_holder {
            "orphaned-own"
        } else if rotate {
            "deposed"
        } else if state.handoff.as_deref().map(|h| h != inc.holder_id).unwrap_or(false) {
            "skipped-handoff"
        } else {
            "released"
        };
        return match store.epoch_acquire(&key, &inc.holder_id, Some(&state)).await {
            Ok(lease) => {
                if rotate {
                    manifest::rotate_for_takeover(store.as_ref(), &sc.cfg, lease.epoch).await?;
                }
                inc.epoch = lease.epoch;
                inc.last_token = None;
                inc.quiet_polls = 0;
                sc.state.save_incarnation(&inc)?;
                sc.lease = Some(lease.clone());
                sc.trace("claim", serde_json::json!({"verdict": "claimed", "how": how, "epoch": lease.epoch, "prior": prior}));
                Ok(ClaimOutcome::Claimed(lease))
            }
            Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                // Lost the race (a rival acquire, or an enqueue moved the
                // token); restart the observation.
                inc.last_token = None;
                inc.quiet_polls = 0;
                sc.state.save_incarnation(&inc)?;
                Ok(ClaimOutcome::Waiting { quiet_polls: 0, behind })
            }
            Err(e) => Err(e.into()),
        };
    }

    // Not ours yet: make sure we are in the queue, ONCE. The enqueue
    // moves the token, so the observation restarts from the token we
    // wrote — our own append must not read as the holder's heartbeat,
    // and it must not reset a count that a rival's append did not.
    let token = if state.waiters.iter().any(|w| w == &inc.holder_id) {
        state.token.clone()
    } else {
        match store.epoch_enqueue(&key, &state, &inc.holder_id).await {
            Ok(after) => after.token,
            Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                // Somebody else moved the cell first; observe again.
                state.token.clone()
            }
            Err(e) => return Err(e.into()),
        }
    };
    if count {
        inc.quiet_polls = counted_quiet;
        inc.last_token = Some(token);
        sc.state.save_incarnation(&inc)?;
    }
    Ok(ClaimOutcome::Waiting { quiet_polls: counted_quiet, behind })
}

/// The wait loop: claim the cell for a commit section, or give up
/// after `CLAIM_DEADLINE_SECS` so a holder that never releases turns
/// into a failed (and retried) barrier rather than a hang.
pub async fn claim(sc: &mut Syncer) -> LeanResult<EpochLease> {
    let started = Instant::now();
    let mut last_counted: Option<Instant> = None;
    let mut reported: Option<String> = None;
    let spacing = Duration::from_secs(sc.cfg.claim_quiet_spacing_secs);
    let deadline = Duration::from_secs(sc.cfg.claim_deadline_secs);
    let poll = Duration::from_secs(sc.cfg.claim_poll_secs);
    loop {
        let count = last_counted.map(|t| t.elapsed() >= spacing).unwrap_or(true);
        match claim_step(sc, count).await? {
            ClaimOutcome::Claimed(lease) => return Ok(lease),
            ClaimOutcome::Waiting { quiet_polls, behind } => {
                if count {
                    last_counted = Some(Instant::now());
                    sc.trace("claim", serde_json::json!({"verdict": "waiting", "behind": behind, "quiet_polls": quiet_polls,
                        "waited_ms": started.elapsed().as_millis() as u64}));
                    // Once per spaced observation, not once per second.
                    let line = format!(
                        "flint-sync: waiting for the publish fence behind {} (quiet {quiet_polls})",
                        behind.as_deref().unwrap_or("a rival claim")
                    );
                    if reported.as_deref() != Some(line.as_str()) {
                        eprintln!("{line}");
                        reported = Some(line);
                    }
                }
                if started.elapsed() >= deadline {
                    sc.trace("claim", serde_json::json!({"verdict": "deadline", "behind": behind,
                        "waited_ms": started.elapsed().as_millis() as u64}));
                    return Err(LeanError::State(format!(
                        "could not acquire the publish fence within {}s \
                         (behind {}); this barrier is abandoned and retried at the next floor — \
                         its uploads are durable and the next barrier adopts them",
                        deadline.as_secs(),
                        behind.as_deref().unwrap_or("a rival claim")
                    )));
                }
                tokio::time::sleep(poll).await;
            }
        }
    }
}

/// What this syncer is OBSERVED to be doing, for the heartbeat and the
/// cell (boundary-verbs plan §2.6). Computed from local files only — the
/// same store-free discipline as `write_gauges`, and for the same
/// reason: this rides writes that already happen, so it must not add a
/// request to the one tick every idle workspace in the fleet pays (leg
/// B8's oracle counts them).
fn observed_echo(sc: &Syncer) -> Option<String> {
    let g = sc.load_gauges().ok()?;
    let (seq, unix) = g.last_boundary.as_ref().map(|b| (b.seq, b.unix)).unwrap_or((0, 0));
    // The pre-flight verdict lives in the marker the AGENT reads; the
    // operator has no other way to see it, so it rides the echo.
    let caps: Option<super::control::Capabilities> =
        std::fs::read(sc.cfg.control_dir().join(super::control::CAPABILITIES))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
    serde_json::to_string(&flint_store::LeaseEcho {
        syncer_version: super::SYNCER_VERSION.to_string(),
        protocol: super::SENTINEL_PROTOCOL,
        last_cited_seq: seq,
        last_cited_unix: unix,
        sentinel_verbs_active: caps.map(|c| !c.verbs.is_empty()).unwrap_or(false),
        metrics_bound: sc.load_metrics_posture().filter(|m| m.enabled).map(|m| m.bound),
    })
    .ok()
}

/// Re-read the cell after a 412 and, if it still names this holder at
/// this epoch unreleased, hand back its current token and queue: the
/// only things that move OUR cell's token are our own writes whose
/// response was lost and the waiters' enqueues, and neither is a
/// takeover. Anything else is the fence.
async fn still_ours(sc: &Syncer, key: &str, lease: &EpochLease) -> Option<EpochLease> {
    match sc.store.epoch_read(key).await {
        Ok(Some(state))
            if state.holder_id == lease.holder_id && state.epoch == lease.epoch && !state.released =>
        {
            Some(EpochLease {
                holder_id: state.holder_id,
                epoch: state.epoch,
                token: state.token,
                waiters: state.waiters,
            })
        }
        _ => None,
    }
}

/// Renew the held lease inside a commit section; a 412 that is not our
/// own moved token means deposed — the caller must abandon the barrier
/// (self-fence).
///
/// The renewal also carries the observed-state echo (§2.6).
pub async fn renew(sc: &mut Syncer) -> LeanResult<()> {
    let key = sc.cfg.epoch_key();
    let lease = sc
        .lease
        .clone()
        .ok_or_else(|| LeanError::State("renew without a lease".into()))?;
    let echo = observed_echo(sc);
    match sc.store.epoch_renew(&key, &lease, echo.as_deref()).await {
        Ok(l) => {
            sc.lease = Some(l);
            // The renewal is a probe of our own credentials, so it is
            // also a place a pause can be observed to have ENDED.
            // Best-effort: a gauge that failed to write must not fail a
            // renewal that succeeded.
            let _ = sc.clear_auth_pause();
            Ok(())
        }
        Err(StoreError::PreconditionFailed(e)) => {
            // A 412 is not yet a deposal. The renew CAS is If-Match on
            // OUR token, and three things move that token: a
            // successor's acquire, a waiter's enqueue, or our own
            // previous write whose RESPONSE was lost. Only the first is
            // a takeover, and treating the others as one made a live
            // holder fence itself (audit 2026-09-03, finding 2). One
            // read tells the cases apart.
            match still_ours(sc, &key, &lease).await {
                Some(adopted) => {
                    eprintln!(
                        "flint-sync: renew 412 on a cell that is still ours (epoch {}, {} queued): \
                         adopting its token, not fencing",
                        adopted.epoch,
                        adopted.waiters.len()
                    );
                    // Review 2026-09-12, lease-2: the adoption alone
                    // writes nothing, so the token would stand still
                    // for a whole takeover threshold and a waiter could
                    // count a live holder dead. One renew moves it; if
                    // that write fails the adopted token stands and the
                    // next tick tries again.
                    sc.lease = Some(
                        sc.store
                            .epoch_renew(&key, &adopted, echo.as_deref())
                            .await
                            .unwrap_or(adopted),
                    );
                    let _ = sc.clear_auth_pause();
                    Ok(())
                }
                None => {
                    sc.lease = None;
                    Err(LeanError::Fenced(format!("deposed at renew: {e}")))
                }
            }
        }
        // 401/403 is not contention and not a bucket fault; retrying
        // cannot fix it. Record when the pause began while we still can:
        // nothing we write to the STORE can carry this, because the
        // request that would carry it is the one being refused (design
        // §6.3).
        Err(e @ StoreError::Auth(_)) => {
            let _ = sc.note_auth_pause();
            Err(e.into())
        }
        Err(e) => Err(e.into()),
    }
}

/// The claim precondition (audit 2026-09-03, finding 5). With a project
/// id stamped (`FLINT_SYNC_PROJECT_ID`), refuse to run over a prefix
/// whose claim cell names ANOTHER project. The operator's refuse-foreign
/// was advisory on the data plane: a CR the operator had not yet judged
/// resolved to its spec, and the syncer checked out and republished over
/// the foreign project's manifest. The cell is durable in the bucket, so
/// the data plane can read it — one GET before the first claim step.
/// Absent cell ⇒ a fresh prefix, claimable; unstamped ⇒ no check (the
/// pre-operator posture, which the drill runs). A foreign claim is
/// `LeanError::Refused`: the process exits `EXIT_REFUSED`, which the
/// delivery treats as final (tear down, name the reason) rather than
/// as one more crash to restart.
pub async fn verify_claim(sc: &Syncer) -> LeanResult<()> {
    let Some(mine) = sc.cfg.project_id.as_deref() else { return Ok(()) };
    let key = sc.cfg.claim_key();
    match sc.store.get_whole(&key, None).await {
        Ok((_, body)) => {
            let doc: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|e| LeanError::State(format!("claim cell {key} is unparseable: {e}")))?;
            match doc.get("project_id").and_then(|v| v.as_str()) {
                Some(p) if p == mine => Ok(()),
                Some(p) => Err(LeanError::Refused(format!(
                    "prefix {} is claimed by project {p:?}; this workspace is project {mine:?} — \
                     refusing to check out or publish over another project's data (delete the \
                     standing claim explicitly if this reuse is intended)",
                    sc.cfg.prefix
                ))),
                None => Err(LeanError::State(format!("claim cell {key} names no project_id"))),
            }
        }
        Err(StoreError::NotFound(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Release at the end of a commit section: the cell is handed to the
/// queue head and keeps this barrier's echo. A 412 is re-read once —
/// a waiter's enqueue moved the token — and retried with the adopted
/// token; a cell that is no longer ours is nobody's to release.
pub async fn release(sc: &mut Syncer) -> LeanResult<()> {
    let key = sc.cfg.epoch_key();
    let Some(lease) = sc.lease.take() else { return Ok(()) };
    let echo = observed_echo(sc);
    sc.trace("release", serde_json::json!({"epoch": lease.epoch, "waiters_at_claim": lease.waiters}));
    match sc.store.epoch_handoff(&key, &lease, echo.as_deref()).await {
        Ok(()) => Ok(()),
        Err(StoreError::PreconditionFailed(_)) => match still_ours(sc, &key, &lease).await {
            Some(adopted) => match sc.store.epoch_handoff(&key, &adopted, echo.as_deref()).await {
                Ok(()) => Ok(()),
                Err(StoreError::PreconditionFailed(_)) => Ok(()), // deposed meanwhile
                Err(e) => Err(e.into()),
            },
            None => Ok(()), // already deposed: the cell is the successor's
        },
        Err(e) => Err(e.into()),
    }
}

/// A restarted container that finds the cell HELD by its own
/// incarnation releases it: it holds nothing in memory, the intent
/// journal replays whatever the previous container left, and a cell
/// left held would cost every other writer a 60 s deposal wait.
pub async fn release_stale_own(sc: &mut Syncer) -> LeanResult<()> {
    let key = sc.cfg.epoch_key();
    let inc = incarnation(sc)?;
    if let Some(state) = sc.store.epoch_read(&key).await? {
        if state.holder_id == inc.holder_id && !state.released {
            eprintln!(
                "flint-sync: the publish fence was left held by a previous container of this pod \
                 (epoch {}); releasing it",
                state.epoch
            );
            sc.lease = Some(EpochLease {
                holder_id: state.holder_id,
                epoch: state.epoch,
                token: state.token,
                waiters: state.waiters,
            });
            return release(sc).await;
        }
    }
    Ok(())
}

/// The per-writer heartbeat: `<prefix>/.flint/lean/writers/<holder_id>`,
/// written unconditionally at startup and every `HEARTBEAT_SECS` by the
/// run loop's own arm. That arm shares the loop with the barrier, so a
/// barrier that waits for the fence (up to `CLAIM_DEADLINE_SECS`) or
/// uploads for minutes holds its writer's heartbeat back for as long —
/// which is why the readers' windows are minutes. With the cell at rest
/// between barriers this is the ONLY liveness a reader can see — the
/// operator's `observedWriters` and the gateway's "is anyone here to
/// cite it" both read this prefix. One small PUT per interval per
/// writer.
pub async fn heartbeat(sc: &mut Syncer) -> LeanResult<()> {
    let inc = incarnation(sc)?;
    let key = sc.cfg.writer_key(&inc.holder_id);
    let body = Bytes::from(
        serde_json::to_vec(&WriterHeartbeat {
            holder_id: inc.holder_id.clone(),
            unix: super::now_unix(),
            echo: observed_echo(sc),
        })
        .map_err(|e| LeanError::State(format!("heartbeat: {e}")))?,
    );
    let crc = flint_store::crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 0,
        epoch: inc.epoch,
        flush_uuid: "heartbeat".into(),
        boundary_source: None,
        posix: None,
    };
    match sc.store.put_whole(&key, body, &PutCondition::Unconditional, &stamps, crc).await {
        Ok(_) => {
            let _ = sc.clear_auth_pause();
            Ok(())
        }
        Err(e @ StoreError::Auth(_)) => {
            let _ = sc.note_auth_pause();
            Err(e.into())
        }
        Err(e) => Err(e.into()),
    }
}

/// Clean shutdown: take the heartbeat down so readers stop counting
/// this writer at once instead of after it goes stale. Best effort.
pub async fn retire_heartbeat(sc: &Syncer) -> LeanResult<()> {
    let inc = incarnation(sc)?;
    match sc.store.delete(&sc.cfg.writer_key(&inc.holder_id)).await {
        Ok(()) | Err(StoreError::NotFound(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The heartbeat object's body. `echo` is the same `LeaseEcho` the cell
/// carries, so one parser serves both.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WriterHeartbeat {
    pub holder_id: String,
    pub unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo: Option<String>,
}

/// Writers whose heartbeat is fresher than `stale_secs` by the STORE's
/// clock, for the operator and the gateway. `now` is the caller's
/// clock; compare generously (the callers pass minutes, not seconds) —
/// a node clock behind the store's reads a live writer as stale, never
/// as live, so the error is on the safe side.
pub async fn live_writers(
    store: &dyn ObjectStore,
    cfg: &super::LeanConfig,
    now: u64,
    stale_secs: u64,
) -> LeanResult<Vec<String>> {
    let prefix = cfg.writers_prefix();
    let mut out = vec![];
    for o in store.list(&prefix).await? {
        let fresh = o.last_modified_unix.map(|t| now.saturating_sub(t) <= stale_secs).unwrap_or(true);
        if fresh {
            out.push(o.key.trim_start_matches(&prefix).to_string());
        }
    }
    Ok(out)
}

// An observed cell, re-exported for the callers that fence on one.
pub type Observed = EpochState;
