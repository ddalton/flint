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

use std::sync::Arc;

use flint_store::{EpochLease, ObjectStore, StoreError};

use super::status::{Facts, Phase, Shared};
use super::{snapshot, ForgeError, ForgeResult, Hold, Syncer};

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
                sc.hold.set_lease(lease.clone());
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
                // Rotation is for every claim but a released cell's: a
                // possibly-live straggler may still hold a valid
                // `If-Match` on the snapshot. A released cell is a
                // clean handoff — its holder fenced itself before it
                // wrote the mark. Self-recognition is NOT exempt: the
                // incarnation of ours that died may have been a
                // successor that died between its takeover and its
                // rotation, with the straggler from the epoch before
                // still live (`formal/ForgeSync.tla`'s second strict
                // counterexample). The first shape rotated on the
                // foreign takeover alone.
                let rotate = !state.released;
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
                        sc.hold.set_lease(lease.clone());
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
fn observed_echo(f: &Facts) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "syncer_version": super::SYNCER_VERSION,
        "snapshot_seq": f.snapshot_seq,
        "refs": f.refs,
        "packs": f.packs,
        "last_push_unix": f.last_push_unix,
    }))
    .ok()
}

/// The batch's own renewal (design §4 step 3): one per batch, before
/// anything is uploaded, so a deposed server learns it before it pays
/// for the upload. Goes through the same serialised path as the
/// renewer task.
pub async fn renew(sc: &mut Syncer) -> ForgeResult<()> {
    let echo = observed_echo(&super::status::facts(sc, Phase::Pushing));
    renew_shared(sc.store.as_ref(), &sc.cfg.epoch_key(), &sc.hold, echo).await
}

/// Renew the held lease. A 412 that is not our own lost response is
/// the fence, and a fence stops READS as well as writes.
///
/// Serialised on the hold's gate: the renewer task and the batch may
/// both ask, and two renews in flight would 412 each other.
pub async fn renew_shared(
    store: &dyn ObjectStore,
    key: &str,
    hold: &Hold,
    echo: Option<String>,
) -> ForgeResult<()> {
    hold.check_fence()?;
    let _serial = hold.gate().await;
    let lease = hold.lease().ok_or_else(|| ForgeError::State("no lease held".into()))?;
    match store.epoch_renew(key, &lease, echo.as_deref()).await {
        Ok(l) => {
            hold.set_lease(l);
            Ok(())
        }
        Err(StoreError::PreconditionFailed(e)) => {
            match store.epoch_read(key).await {
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
                    hold.set_lease(EpochLease {
                        holder_id: state.holder_id,
                        epoch: state.epoch,
                        token: state.token,
                    });
                    Ok(())
                }
                _ => Err(hold.fence(format!("deposed at renew: {e}"))),
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

/// The heartbeat, on its own task, from the claim until a fence or a
/// release. The rule it applies is `Hold`'s: unconditional while
/// serving, progress-gated while a phase that must move is reported.
///
/// Returns the task handle. A clean release marks the hold released,
/// which stops the task at its next tick; aborting the handle after
/// the release just makes that immediate.
pub fn spawn_renewer(
    store: Arc<dyn ObjectStore>,
    key: String,
    hold: Arc<Hold>,
    shared: Shared,
    heartbeat: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(heartbeat);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; the claim just renewed for us.
        ticker.tick().await;
        let mut last_seen = hold.progress();
        let mut quiet_ticks: u32 = 0;
        // X13: the holder's own term. Six heartbeats without a landed
        // renewal is exactly the window after which a challenger may
        // have claimed, so readiness is withdrawn at that point and
        // restored by the next renewal that lands.
        let term = heartbeat * QUIET_POLLS;
        let mut was_overdue = false;
        let refresh = |was_overdue: &mut bool| {
            let overdue = hold.renewal_overdue(term);
            if let Ok(mut g) = shared.lock() {
                g.last_renew_unix = hold.last_renew_unix();
                g.renewal_overdue = overdue;
            }
            if overdue && !*was_overdue {
                eprintln!(
                    "flint-forge: no renewal has landed for {:.0}s (the term is {:.0}s): readiness \
                     withdrawn until one does — reads stop, the lease is not given up",
                    hold.since_renew().map(|d| d.as_secs_f64()).unwrap_or(0.0),
                    term.as_secs_f64()
                );
            } else if !overdue && *was_overdue {
                eprintln!("flint-forge: a renewal landed; readiness restored, serving again");
            }
            *was_overdue = overdue;
        };
        loop {
            ticker.tick().await;
            if hold.fenced().is_some() || hold.is_released() {
                return;
            }
            let facts = shared.lock().ok().map(|g| g.clone());
            let phase = facts.as_ref().map(|f| f.phase).unwrap_or(Phase::Serving);
            let seen = hold.progress();
            if phase.must_progress() && seen == last_seen {
                // Nothing moved since the last renewal. Letting the
                // token go quiet is the point: the quiet polls a
                // challenger counts are the only takeover a wedged
                // server can get, and renewing for it would keep the
                // repository unavailable for as long as the pod lived.
                quiet_ticks += 1;
                if quiet_ticks == 1 || quiet_ticks.is_multiple_of(QUIET_POLLS) {
                    eprintln!(
                        "flint-forge: {} has moved nothing since the last renewal \
                         ({quiet_ticks} heartbeat(s)); the token stays quiet so a challenger can \
                         take over a wedged server",
                        phase.as_str()
                    );
                }
                refresh(&mut was_overdue);
                continue;
            }
            quiet_ticks = 0;
            last_seen = seen;
            let echo = facts.as_ref().and_then(observed_echo);
            match renew_shared(store.as_ref(), &key, &hold, echo).await {
                Ok(()) => {}
                // The hold is fenced; the serving loop wakes on it.
                Err(ForgeError::Fenced(_)) => return,
                // An auth pause or a transient store fault: keep
                // trying, and keep the lease — nobody can take it while
                // the store is down for everyone. Reads continue only
                // within the term (X13): past it this holder may have
                // been deposed by a challenger that CAN reach the
                // store, and `refresh` withdraws readiness.
                Err(e) => eprintln!("flint-forge: heartbeat: {e}"),
            }
            refresh(&mut was_overdue);
        }
    })
}

/// Clean release (the `preStop` path): a successor supersedes at once
/// instead of waiting out six quiet polls. Marks the hold released
/// under the gate, so the renewer cannot renew into a released cell.
pub async fn release(sc: &mut Syncer) -> ForgeResult<()> {
    let key = sc.cfg.epoch_key();
    let _serial = sc.hold.gate().await;
    sc.hold.mark_released();
    if let Some(lease) = sc.hold.take_lease() {
        match sc.store.epoch_release(&key, &lease).await {
            Ok(()) => Ok(()),
            Err(StoreError::PreconditionFailed(_)) => Ok(()), // already deposed
            Err(e) => Err(e.into()),
        }
    } else {
        Ok(())
    }
}

/// Say so, loudly, if another product also writes this prefix.
///
/// Detection, deliberately NOT enforcement. Prevention belongs to
/// whatever assigns prefixes — an admission policy, a GitOps path — and
/// refusing here would turn a diagnostic into an outage the first time
/// a stale cell outlived the workspace that wrote it. What this buys is
/// that the condition stops being SILENT: today a forge server and a
/// lean sidecar on one prefix both acquire, both are right that they
/// hold their own cell, and neither logs a line (drill C1).
///
/// Never fatal, and never fatal by accident: a probe that cannot read
/// the store returns nothing rather than an error.
pub async fn warn_if_prefix_is_shared(sc: &Syncer) {
    let found = flint_store::layout::neighbours(
        sc.store.as_ref(),
        &sc.cfg.prefix,
        flint_store::layout::Writer::ForgeRepository,
    )
    .await
    .unwrap_or_default();
    for f in found {
        eprintln!("flint-forge: {}", f.report());
    }
}

/// The same probe, aimed at the prefix this repository EXPORTS to.
///
/// The export is a lean workspace that forge publishes, so what would
/// be foreign there is a forge repository rooted on it — somebody
/// else's `keyPrefix` pointed at our mirror. The operator refuses that
/// when both sides are CRs it can see; it cannot see a repository in
/// another cluster, and the bucket is not cluster-scoped.
///
/// Called once at startup rather than by the barrier itself: the
/// spawned `flint-sync` skips its own probe when it is publishing a
/// mirror, because a per-export read would be recurring and its
/// warning would be swallowed by the line filter in `run_barrier`.
pub async fn warn_if_export_prefix_is_shared(sc: &Syncer, export_prefix: &str) {
    let found = flint_store::layout::neighbours(
        sc.store.as_ref(),
        export_prefix,
        flint_store::layout::Writer::LeanWorkspace,
    )
    .await
    .unwrap_or_default();
    for f in found {
        eprintln!("flint-forge: on the export prefix — {}", f.report());
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
