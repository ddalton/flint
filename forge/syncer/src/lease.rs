//! The repository lease: one writer, judged against the store's clock.
//!
//! Lean's claim loop, its lost-response rule and its takeover rotation,
//! re-typed on the syncer. They are copied deliberately rather than
//! imported: lean's are methods on `Sidecar` and reach for a manifest,
//! a stage and a gauge file that forge has none of. What is shared is
//! the PROTOCOL, which is the part the models in `lean/formal/` check.
//!
//! Two rules earn their length here.
//!
//! The **heartbeat runs on a timer, not on a push** (design §5). A
//! server that renewed only when a client pushed would let a quiet
//! repository's lease lapse, and a straggler from a roll would then
//! see a dead cell and take over while the first server was still
//! answering fetches.
//!
//! A **412 on the renew is not yet a deposal**. The renew CAS is
//! `If-Match` on our own token, and only two things move that token: a
//! successor's acquire, or our own previous renew whose RESPONSE was
//! lost. One read tells them apart, and treating the second as the
//! first once made a live lean sidecar fence itself into silence for
//! the rest of its tenant's life (audit 2026-09-03, finding 2).

use flint_store::{EpochLease, StoreError};

use super::{snapshot, ForgeError, ForgeResult, Syncer};

/// Quiet polls required before superseding a foreign holder: its token
/// must not advance across this many observations.
pub const QUIET_POLLS: u32 = 6;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Incarnation {
    pub holder_id: String,
    pub epoch: u64,
    pub last_token: Option<String>,
    pub quiet_polls: u32,
}

#[derive(Debug)]
pub enum ClaimOutcome {
    /// Fresh cell, clean-released cell, our own cell, or a holder
    /// judged dead: claimed. A takeover from an unreleased foreign
    /// holder has ALREADY rotated the snapshot by the time this
    /// returns.
    Claimed(EpochLease),
    /// Call again after the heartbeat interval; the observation is
    /// persisted, so a container restart resumes the count instead of
    /// resetting it.
    Waiting { quiet_polls: u32 },
}

fn incarnation_path(sc: &Syncer) -> std::path::PathBuf {
    sc.cfg.state_dir.join("incarnation.json")
}

pub fn load_incarnation(sc: &Syncer) -> ForgeResult<Option<Incarnation>> {
    match std::fs::read(incarnation_path(sc)) {
        Ok(b) => Ok(serde_json::from_slice(&b).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn save_incarnation(sc: &Syncer, inc: &Incarnation) -> ForgeResult<()> {
    std::fs::create_dir_all(&sc.cfg.state_dir)?;
    let body = serde_json::to_vec_pretty(inc)
        .map_err(|e| ForgeError::State(format!("incarnation will not serialise: {e}")))?;
    let tmp = incarnation_path(sc).with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, incarnation_path(sc))?;
    Ok(())
}

/// One claim step: at most one read and one acquire. The caller loops
/// on `Waiting` at the heartbeat cadence.
pub async fn claim_step(sc: &mut Syncer) -> ForgeResult<ClaimOutcome> {
    sc.check_fence()?;
    let key = sc.cfg.epoch_key();
    let mut inc = load_incarnation(sc)?.unwrap_or_else(|| Incarnation {
        holder_id: sc.holder_id.clone(),
        epoch: 0,
        last_token: None,
        quiet_polls: 0,
    });
    // A restarted container inherits the persisted id; a replacement
    // pod's fresh id is what forces it down the takeover path.
    sc.holder_id = inc.holder_id.clone();

    let observed = sc.store.epoch_read(&key).await?;
    match observed {
        None => match sc.store.epoch_acquire(&key, &inc.holder_id, None).await {
            Ok(lease) => {
                inc.epoch = lease.epoch;
                inc.last_token = None;
                inc.quiet_polls = 0;
                save_incarnation(sc, &inc)?;
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
                // Rotation is for the unreleased-foreign takeover
                // alone: a possibly-live straggler may still hold a
                // valid `If-Match` on the snapshot. A released cell is
                // a clean handoff, and self-recognition means our own
                // previous process died with its writes.
                let rotate = !ours && !state.released;
                match sc.store.epoch_acquire(&key, &inc.holder_id, Some(&state)).await {
                    Ok(lease) => {
                        if rotate {
                            let cell = snapshot::rotate_for_takeover(
                                sc.store.as_ref(),
                                &sc.cfg,
                                lease.epoch,
                                &inc.holder_id,
                            )
                            .await?;
                            sc.cell = Some(cell);
                        }
                        inc.epoch = lease.epoch;
                        inc.last_token = None;
                        inc.quiet_polls = 0;
                        save_incarnation(sc, &inc)?;
                        sc.lease = Some(lease.clone());
                        Ok(ClaimOutcome::Claimed(lease))
                    }
                    Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                        // Lost the supersede race; restart observation.
                        inc.last_token = None;
                        inc.quiet_polls = 0;
                        save_incarnation(sc, &inc)?;
                        Ok(ClaimOutcome::Waiting { quiet_polls: 0 })
                    }
                    Err(e) => Err(e.into()),
                }
            } else {
                inc.quiet_polls = if quiet { inc.quiet_polls + 1 } else { 0 };
                inc.last_token = Some(state.token.clone());
                let polls = inc.quiet_polls;
                save_incarnation(sc, &inc)?;
                Ok(ClaimOutcome::Waiting { quiet_polls: polls })
            }
        }
    }
}

/// What this syncer is observed to be doing, for the lease cell — the
/// operator's only evidence of what the binary in that pod is actually
/// running, on a request that is already being paid for every 10 s.
fn observed_echo(sc: &Syncer) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "syncer_version": super::SYNCER_VERSION,
        "snapshot_seq": sc.cell.as_ref().map(|c| c.snap.seq).unwrap_or(0),
        "refs": sc.cell.as_ref().map(|c| c.snap.refs.len()).unwrap_or(0),
        "packs": sc.cell.as_ref().map(|c| c.snap.packs.len()).unwrap_or(0),
        "last_push_unix": sc.last_push_unix,
    }))
    .ok()
}

/// Renew the held lease. A 412 that is not our own lost response is
/// the fence, and a fence stops READS as well as writes.
pub async fn renew(sc: &mut Syncer) -> ForgeResult<()> {
    sc.check_fence()?;
    let key = sc.cfg.epoch_key();
    let lease = sc.lease()?.clone();
    let echo = observed_echo(sc);
    match sc.store.epoch_renew(&key, &lease, echo.as_deref()).await {
        Ok(l) => {
            sc.lease = Some(l);
            Ok(())
        }
        Err(StoreError::PreconditionFailed(e)) => {
            match sc.store.epoch_read(&key).await {
                Ok(Some(state))
                    if state.holder_id == lease.holder_id
                        && state.epoch == lease.epoch
                        && !state.released =>
                {
                    eprintln!(
                        "flint-forge: renew 412 on a cell that is still ours (epoch {}): a lost \
                         renew response — adopting its token, not fencing",
                        state.epoch
                    );
                    sc.lease = Some(EpochLease {
                        holder_id: state.holder_id,
                        epoch: state.epoch,
                        token: state.token,
                    });
                    Ok(())
                }
                _ => Err(sc.fence(format!("deposed at renew: {e}"))),
            }
        }
        // 401/403 is not contention and no retry fixes it, but it is
        // also not a deposal: keep serving reads, keep trying. The
        // renewals we are now missing are what a challenger will read
        // as a dead holder, so it is reported at every attempt rather
        // than once.
        Err(e @ StoreError::Auth(_)) => {
            eprintln!("flint-forge: lease renewal refused by the store: {e}");
            Err(e.into())
        }
        Err(e) => Err(e.into()),
    }
}

/// Clean release (the `preStop` path): a successor supersedes at once
/// instead of waiting out six quiet polls.
pub async fn release(sc: &mut Syncer) -> ForgeResult<()> {
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

/// The claim precondition, on the data plane: with a project id
/// stamped, refuse to serve a prefix whose claim cell names another
/// project. The operator's refuse-foreign is advisory here — a CR it
/// has not yet judged still resolves to its spec — so the syncer reads
/// the durable cell itself, one GET before the first claim step.
pub async fn verify_claim(sc: &Syncer) -> ForgeResult<()> {
    let Some(mine) = sc.cfg.project_id.as_deref() else { return Ok(()) };
    let key = sc.cfg.claim_key();
    match sc.store.get_whole(&key, None).await {
        Ok((_, body)) => {
            let doc: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|e| ForgeError::State(format!("claim cell {key} is unparseable: {e}")))?;
            match doc.get("project_id").and_then(|v| v.as_str()) {
                Some(p) if p == mine => Ok(()),
                Some(p) => Err(ForgeError::Refused(format!(
                    "prefix {} is claimed by project {p:?}; this repository is project {mine:?} — \
                     refusing to serve or publish over another project's repository",
                    sc.cfg.prefix
                ))),
                None => Err(ForgeError::State(format!("claim cell {key} names no project_id"))),
            }
        }
        Err(StoreError::NotFound(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
