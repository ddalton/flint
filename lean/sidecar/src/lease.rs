//! The subtree lease (plan P4: `tier::epoch` re-scoped to the lean
//! subtree cell), with lean's own claim loop.
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

use std::sync::Arc;

use flint_store::{EpochLease, ObjectStore, StoreError};

use super::state::Incarnation;
use super::{manifest, LeanError, LeanResult, Sidecar};

/// Quiet polls required before superseding a foreign holder (the
/// token must not advance across this many observations).
pub const QUIET_POLLS: u32 = 6;

pub enum ClaimOutcome {
    /// Fresh cell or clean-released cell: claimed immediately.
    Claimed(EpochLease),
    /// Foreign holder's token advanced (or not enough quiet polls yet):
    /// call again after the heartbeat interval. The observation is
    /// persisted.
    Waiting { quiet_polls: u32 },
}

/// One claim step. The caller loops on `Waiting` at its poll cadence;
/// each call performs at most one read + one acquire.
pub async fn claim_step(sc: &mut Sidecar) -> LeanResult<ClaimOutcome> {
    let store: &Arc<dyn ObjectStore> = &sc.store;
    let key = sc.cfg.epoch_key();
    let mut inc = sc.state.load_incarnation()?.unwrap_or_else(|| Incarnation {
        holder_id: format!("lean-{}", uuid::Uuid::new_v4()),
        epoch: 0,
        last_token: None,
        quiet_polls: 0,
    });

    let observed = store.epoch_read(&key).await?;
    match observed {
        None => match store.epoch_acquire(&key, &inc.holder_id, None).await {
            Ok(lease) => {
                inc.epoch = lease.epoch;
                inc.last_token = None;
                inc.quiet_polls = 0;
                sc.state.save_incarnation(&inc)?;
                sc.lease = Some(lease.clone());
                Ok(ClaimOutcome::Claimed(lease))
            }
            Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                Ok(ClaimOutcome::Waiting { quiet_polls: 0 })
            }
            Err(e) => Err(e.into()),
        },
        Some(state) => {
            let ours = state.holder_id == inc.holder_id;
            let quiet = inc.last_token.as_deref() == Some(state.token.as_str());
            if ours || state.released || (quiet && inc.quiet_polls + 1 >= QUIET_POLLS) {
                // Self-recognition (same emptyDir), a clean release, or
                // a lease judged dead across QUIET_POLLS observations.
                //
                // Rotation is needed ONLY for the unreleased-foreign
                // takeover (a possibly-live straggler mid-barrier). A
                // released cell is a clean handoff — the holder's final
                // barrier completed before release — and self-
                // recognition means the previous container's process
                // (and any in-flight write of its) died with it.
                // Rotating on those paths is pure manifest churn: at
                // 100k+ entries it is a multi-MB GET+PUT per claim, it
                // double-bumps seq, and it defeats the no-change
                // barrier's early exit (measured on the 0b rig).
                let rotate = !ours && !state.released;
                match store.epoch_acquire(&key, &inc.holder_id, Some(&state)).await {
                    Ok(lease) => {
                        if rotate {
                            manifest::rotate_for_takeover(store.as_ref(), &sc.cfg, lease.epoch)
                                .await?;
                        }
                        inc.epoch = lease.epoch;
                        inc.last_token = None;
                        inc.quiet_polls = 0;
                        sc.state.save_incarnation(&inc)?;
                        sc.lease = Some(lease.clone());
                        Ok(ClaimOutcome::Claimed(lease))
                    }
                    Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                        // Lost the supersede race; restart observation.
                        inc.last_token = None;
                        inc.quiet_polls = 0;
                        sc.state.save_incarnation(&inc)?;
                        Ok(ClaimOutcome::Waiting { quiet_polls: 0 })
                    }
                    Err(e) => Err(e.into()),
                }
            } else {
                inc.quiet_polls = if quiet { inc.quiet_polls + 1 } else { 0 };
                inc.last_token = Some(state.token.clone());
                let polls = inc.quiet_polls;
                sc.state.save_incarnation(&inc)?;
                Ok(ClaimOutcome::Waiting { quiet_polls: polls })
            }
        }
    }
}

/// What this sidecar is OBSERVED to be doing, for the heartbeat cell
/// (boundary-verbs plan §2.6). Computed from local files only — the
/// same store-free discipline as `write_gauges`, and for the same
/// reason: this rides the renewal, so it must not add a request to the
/// one tick every idle workspace in the fleet pays (leg B8's oracle
/// counts them).
fn observed_echo(sc: &Sidecar) -> Option<String> {
    let g = sc.load_gauges().ok()?;
    // The count comes from the stage itself rather than the gauges
    // snapshot: it is the exposure number an operator pages on, and
    // gauges lag by up to a floor.
    let staged = sc.load_stage().map(|s| s.entries.len() as u64).unwrap_or(0);
    let (seq, unix) = g.last_boundary.as_ref().map(|b| (b.seq, b.unix)).unwrap_or((0, 0));
    // The pre-flight verdict lives in the marker the AGENT reads; the
    // operator has no other way to see it, so it rides the echo.
    let caps: Option<super::control::Capabilities> =
        std::fs::read(sc.cfg.control_dir().join(super::control::CAPABILITIES))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
    serde_json::to_string(&flint_store::LeaseEcho {
        sidecar_version: super::SIDECAR_VERSION.to_string(),
        protocol: super::SENTINEL_PROTOCOL,
        active_boundary_mode: sc.cfg.boundary_mode.as_str().to_string(),
        last_cited_seq: seq,
        last_cited_unix: unix,
        staged_uncited_count: staged,
        sentinel_verbs_active: caps.map(|c| !c.verbs.is_empty()).unwrap_or(false),
        metrics_bound: sc.load_metrics_posture().filter(|m| m.enabled).map(|m| m.bound),
    })
    .ok()
}

/// Renew the held lease; a 412 means deposed — the caller must stop
/// publishing (self-fence).
///
/// The renewal also carries the observed-state echo (§2.6): the
/// operator's only evidence of what the sidecar binary is ACTUALLY
/// running, on a request that was already being paid for.
pub async fn renew(sc: &mut Sidecar) -> LeanResult<()> {
    let key = sc.cfg.epoch_key();
    let lease = sc
        .lease
        .clone()
        .ok_or_else(|| LeanError::State("renew without a lease".into()))?;
    let echo = observed_echo(sc);
    match sc.store.epoch_renew(&key, &lease, echo.as_deref()).await {
        Ok(l) => {
            sc.lease = Some(l);
            // The renewal is the only scheduled probe of our own
            // credentials, so it is also the only place a pause can be
            // observed to have ENDED. Best-effort: a gauge that failed
            // to write must not fail a renewal that succeeded.
            let _ = sc.clear_auth_pause();
            Ok(())
        }
        Err(StoreError::PreconditionFailed(e)) => {
            // A 412 is not yet a deposal. The renew CAS is If-Match on
            // OUR token, and only two things move that token: a
            // successor's acquire, or our own previous renew whose
            // RESPONSE was lost (the PUT landed, the client saw a
            // timeout). The second is not a takeover, and treating it as
            // one made a live holder fence itself — exit 0, and under
            // the CSI delivery nothing restarted it, so the workspace
            // went unpublished for the rest of the tenant's life
            // (audit 2026-09-03, finding 2). One read tells the cases
            // apart: a cell that still names this holder at this epoch,
            // unreleased, can only have been written by us, so its token
            // is ours to adopt. Anything else is the fence.
            match sc.store.epoch_read(&key).await {
                Ok(Some(state))
                    if state.holder_id == lease.holder_id
                        && state.epoch == lease.epoch
                        && !state.released =>
                {
                    eprintln!(
                        "flint-sync: renew 412 on a cell that is still ours (epoch {}): a lost \
                         renew response — adopting its token, not fencing",
                        state.epoch
                    );
                    sc.lease = Some(EpochLease {
                        holder_id: state.holder_id,
                        epoch: state.epoch,
                        token: state.token,
                    });
                    let _ = sc.clear_auth_pause();
                    Ok(())
                }
                _ => {
                    sc.lease = None;
                    Err(LeanError::Fenced(format!("deposed at renew: {e}")))
                }
            }
        }
        // 401/403 is not contention and not a bucket fault; retrying
        // cannot fix it. We keep serving local files and keep trying —
        // but the renewals we are now missing are exactly what a
        // challenger reads as a dead holder, so record when the pause
        // began while we still can. Nothing we write to the STORE can
        // carry this: the request that would carry it is the one being
        // refused (design §6.3).
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
pub async fn verify_claim(sc: &Sidecar) -> LeanResult<()> {
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

/// Clean release (the preStop path): a successor supersedes immediately
/// instead of waiting out the lease.
pub async fn release(sc: &mut Sidecar) -> LeanResult<()> {
    let key = sc.cfg.epoch_key();
    if let Some(lease) = sc.lease.take() {
        match sc.store.epoch_release(&key, &lease).await {
            Ok(()) => Ok(()),
            Err(StoreError::PreconditionFailed(_)) => Ok(()), // already deposed
            Err(e) => Err(e.into()),
        }
    } else {
        Ok(())
    }
}
