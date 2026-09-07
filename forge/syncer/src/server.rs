//! The serving loop: claim, restore, then batch until deposed.
//!
//! Everything that touches the bucket happens on this one task, which
//! is what "the syncer holds the writer lock" means concretely: there
//! is no lock, there is one owner. Hooks queue on a channel, the
//! heartbeat is a timer on the same `select!`, and the sweep and the
//! repack run between batches rather than beside them.
//!
//! The loop's exits are all deliberate. A fence exits non-zero so the
//! pod restarts and restores — a deposed server must stop answering
//! fetches too, since it can no longer prove the refs it would serve.
//! A refusal exits `EXIT_REFUSED`, which the delivery treats as final:
//! restarting a server whose bucket names a pack that does not exist
//! just produces the same refusal in a crash loop that reads, to a
//! tenant, as "starting".

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::batch::{self, PushReport};
use super::policy::Policy;
use super::status::{self, Phase, Shared};
use super::uds::{self, HookResponse, Incoming};
use super::{bundle, fold, follow, lease, restore, ForgeError, ForgeResult, Syncer};

pub struct ServerOpts {
    pub socket: PathBuf,
    /// Where the rendered branch policy is re-read from between
    /// batches. `None` = the policy passed in is fixed for the life of
    /// the process, which is the rigs' posture.
    ///
    /// It exists because the operator cannot write into the
    /// repository's `emptyDir`: the document arrives on a read-only
    /// ConfigMap mount, and a mount updates in place. Re-reading is
    /// what makes a branch-policy edit take effect without rolling the
    /// server and dropping every clone in flight.
    pub policy_dir: Option<PathBuf>,
    /// `host:port` for the status listener the operator polls. `None`
    /// disables it — which also disables the idle ladder, since a poll
    /// that cannot be made Holds.
    pub status_addr: Option<String>,
    pub policy: Policy,
    /// The legible export (§9). `None` = off, which is the default: a
    /// repository that nobody mounts as a workspace pays nothing for
    /// the option.
    pub export: Option<super::export::ExportConfig>,
    /// Clone bundles (§8). `None` = off; a repository nobody clones in
    /// a storm pays nothing for the option.
    pub bundle: Option<super::bundle::BundleConfig>,
    /// Pruning merged, quiet agent branches (§7). `None` = off, and
    /// off is the default: a branch is somebody's work until it is
    /// contained in the integration branch.
    pub prune: Option<super::prune::PruneConfig>,
    /// Git LFS. `None` = the batch API answers 404, which is what a
    /// repository with no large binaries wants: a client that never
    /// asks pays nothing, and one that does is told plainly.
    pub lfs: Option<LfsOpts>,
}

#[derive(Debug, Clone)]
pub struct LfsOpts {
    /// How long a transfer URL is good for.
    pub ttl_secs: u64,
}

pub async fn run(mut sc: Syncer, opts: ServerOpts) -> ForgeResult<()> {
    restore::check_git_floor(&sc).await?;
    lease::verify_claim(&sc).await?;
    // Diagnostic, never a gate: prevention belongs to whatever assigns
    // prefixes, and this only makes a failure of it audible.
    lease::warn_if_prefix_is_shared(&sc).await;
    if let Some(ex) = opts.export.as_ref() {
        lease::warn_if_export_prefix_is_shared(&sc, &ex.prefix).await;
    }

    let shared: Shared = Arc::new(Mutex::new(status::facts(&sc, Phase::Starting)));
    if let Some(addr) = opts.status_addr.clone() {
        let shared = shared.clone();
        // The LFS batch API answers on the same listener, because it
        // needs exactly what this process has and the door does not:
        // the bucket credentials. The bytes themselves never come here
        // — the response hands the client a presigned URL.
        let lfs = opts.lfs.as_ref().map(|l| {
            Arc::new(LfsCtx {
                store: sc.store.clone(),
                prefix: sc.cfg.prefix.clone(),
                ttl_secs: l.ttl_secs,
            })
        });
        tokio::spawn(async move {
            if let Err(e) = serve_http(&addr, shared, lfs).await {
                // Not fatal, and deliberately loud: without it the
                // ladder holds this repository awake forever, which is
                // a cost rather than a fault.
                eprintln!("flint-forge: status listener on {addr} stopped: {e}");
            }
        });
    }

    // ── claim ────────────────────────────────────────────────────────
    publish(&shared, &sc, Phase::ClaimingEpoch);
    loop {
        match lease::claim_step(&mut sc).await? {
            lease::ClaimOutcome::Claimed(l) => {
                eprintln!("flint-forge: holding {} at epoch {}", sc.cfg.prefix, l.epoch);
                break;
            }
            lease::ClaimOutcome::Waiting { quiet_polls } => {
                eprintln!(
                    "flint-forge: another server holds {} ({quiet_polls}/{} quiet polls)",
                    sc.cfg.prefix,
                    lease::QUIET_POLLS
                );
                // X14's cheap half: bring the repository down and prove
                // it WHILE waiting, so a claim costs the pushes missed
                // rather than the repository.
                //
                // Only while the holder's token is still moving. Once a
                // poll comes back quiet this process may be about to
                // take over a dead server, and a takeover that first
                // downloads 40 GiB is the outage this exists to remove,
                // rebuilt one step earlier.
                if sc.cfg.prewarm && quiet_polls == 0 {
                    match follow::warm(&mut sc).await {
                        Ok(r) if r.moved() => {
                            eprintln!("flint-forge: warm: {}", r.line())
                        }
                        Ok(_) => {}
                        // Never fatal, and never a reason not to claim:
                        // the restore after the claim reconciles
                        // whatever this left behind.
                        Err(e) => eprintln!("flint-forge: warm pass deferred: {e}"),
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(sc.cfg.heartbeat_secs)).await;
            }
        }
    }

    // ── the heartbeat, before anything long runs ─────────────────────
    // On its own task, so the restore below, every batch and every
    // export beat through — the loop's own timer arm never could, and
    // the token was measured silent for 125 s during a 10 GiB push.
    // Progress-gated during a restore or a push (`Hold`).
    let renewer = lease::spawn_renewer(
        sc.store.clone(),
        sc.cfg.epoch_key(),
        sc.hold.clone(),
        shared.clone(),
        std::time::Duration::from_secs(sc.cfg.heartbeat_secs),
    );

    // ── what a predecessor left in flight ────────────────────────────
    // Parts a crashed or deposed server uploaded and never completed
    // are billed until aborted, and nothing of OURS is in flight yet.
    match super::sweep::abort_orphaned_uploads(&sc).await {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "flint-forge: aborted {n} multipart upload(s) a previous server left in flight"
        ),
        // Hygiene, not a gate: a listing permission the pod lacks must
        // not become a crash loop. The between-batches sweep retries.
        Err(e) => eprintln!(
            "flint-forge: could not sweep in-flight uploads ({e}); they stay billed until a \
             sweep succeeds"
        ),
    }

    // ── restore ──────────────────────────────────────────────────────
    publish(&shared, &sc, Phase::Importing);
    let restored = restore::restore(&mut sc).await?;
    eprintln!("flint-forge: restored {}", restored.line());
    let branch = sc.cfg.default_branch.clone();
    restore::set_default_branch(&sc, &branch).await?;
    // The bundle advertisement lives in the repository's local config,
    // which the restore just recreated empty. Put it back BEFORE
    // serving: a wake from idle-to-zero is exactly the moment a storm
    // arrives, and an unadvertised bundle makes the storm lever inert
    // precisely when it is needed.
    if let Some(bcfg) = opts.bundle.as_ref() {
        if let Err(e) = bundle::readvertise(&mut sc, bcfg, super::now_unix()).await {
            // Not fatal: a repository that serves without an
            // advertisement is slow under a storm, not wrong.
            eprintln!("flint-forge: could not re-advertise the bundle ({e}); clones will come \
                       from this server until the next cut");
        }
    }
    // Under tiers the full LIST sweep runs here — what a crashed
    // incarnation or a deposed straggler left — and then once per
    // `sweep_every_secs`, never with a fold in flight; the ledger sweep
    // on the tick covers what folds unname. (The control rule sweeps
    // after each repack, as it always did.)
    if sc.cfg.fold_factor > 0 {
        match super::sweep::sweep(&mut sc).await {
            Ok(_) => sc.last_full_sweep_unix = super::now_unix(),
            Err(e @ ForgeError::Fenced(_)) => return Err(e),
            Err(e) => eprintln!("flint-forge: start-up sweep deferred: {e}"),
        }
    }
    publish(&shared, &sc, Phase::Serving);

    // ── serve ────────────────────────────────────────────────────────
    // The fold task beside the loop reports on this channel; the loop
    // owns the receiver so the select below borrows nothing of `sc`.
    let (fold_tx, mut fold_rx) = mpsc::channel::<fold::FoldResult>(4);
    let (tx, mut rx) = mpsc::channel::<Incoming>(256);
    {
        let socket = opts.socket.clone();
        tokio::spawn(async move {
            if let Err(e) = uds::serve(&socket, tx).await {
                eprintln!("flint-forge: hook socket stopped: {e}");
            }
        });
    }

    // Housekeeping — bundles and the branch pruner — runs on its own
    // slower timer. Both are cheap to decline, but declining them at
    // the heartbeat's rate would put a subprocess and a log line every
    // ten seconds behind a repository nobody is using.
    let mut maintenance = tokio::time::interval(std::time::Duration::from_secs(60));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    maintenance.tick().await;

    // The renewer deposed is this loop's exit; it wakes on the fence
    // from whatever it is awaiting.
    let mut fenced_rx = sc.hold.subscribe();

    // A clean release on SIGTERM: a successor claims at once instead of
    // waiting out six quiet polls, which is the difference between a
    // roll that costs one request and one that costs a minute of them.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(ForgeError::Io)?;
    let mut policy = opts.policy.clone();
    let mut last_prune: u64 = 0;
    let mut next_id: u64 = 0;
    loop {
        tokio::select! {
            _ = term.recv() => {
                publish(&shared, &sc, Phase::Draining);
                // A fold in flight named nothing yet: kill its task and
                // drop its scratch; the successor plans it again.
                fold::abort(&mut sc);
                match lease::release(&mut sc).await {
                    Ok(()) => eprintln!("flint-forge: lease released; a successor may claim at once"),
                    // Not fatal: the successor waits out the quiet
                    // polls instead, which is slower and still correct.
                    Err(e) => eprintln!("flint-forge: could not release the lease cleanly: {e}"),
                }
                renewer.abort();
                publish(&shared, &sc, Phase::Released);
                return Ok(());
            }
            // The heartbeat is the renewer task's (`lease::spawn_renewer`),
            // not this loop's: it beats through a batch, a restore and
            // an export, which a timer arm here never could. What this
            // loop owns is the consequence — the renewer deposed is the
            // fence, and a fenced server stops serving reads too.
            res = fenced_rx.changed() => {
                let why = match res {
                    Ok(()) => fenced_rx.borrow().clone().unwrap_or_else(|| "fenced".into()),
                    Err(_) => "the lease cell went away".into(),
                };
                publish(&shared, &sc, Phase::Draining);
                return Err(ForgeError::Fenced(why));
            }
            _ = maintenance.tick() => {
                if sc.cfg.fold_factor > 0 {
                    fold_tick(&mut sc, &fold_tx).await?;
                }
                // What a container restart on this same emptyDir may
                // skip proving. Taken on the slow tick and not per
                // push: it is a local write proportional to the ref
                // map, and nothing waits on it.
                if let Err(e) = follow::checkpoint(&sc, super::now_unix()) {
                    eprintln!("flint-forge: could not checkpoint the proof ({e})");
                }
                if let Some(bcfg) = opts.bundle.as_ref() {
                    match super::bundle::maybe_run(&mut sc, bcfg, super::now_unix()).await {
                        // Stashed for the next batch's CAS, never one
                        // of its own.
                        Ok(Some(name)) => sc.pending_bundle = Some(name),
                        Ok(None) => {}
                        Err(e @ ForgeError::Fenced(_)) => return Err(e),
                        Err(e) => eprintln!("flint-forge: bundle deferred: {e}"),
                    }
                }
                if let Some(pcfg) = opts.prune.as_ref() {
                    if prune_due(&mut last_prune, pcfg, super::now_unix()) {
                        match super::prune::candidates(&sc, pcfg, super::now_unix()).await {
                            Ok(dead) if !dead.is_empty() => {
                                eprintln!(
                                    "flint-forge: pruning {} merged agent branch(es) quiet for \
                                     more than {}s",
                                    dead.len(),
                                    pcfg.after_secs
                                );
                                // Through the ordinary batch: one CAS,
                                // one transaction. A ref this process
                                // moved outside that path would be a
                                // ref the bucket does not know about.
                                let push = batch::PushRequest {
                                    id: 0,
                                    principal: "system:flint-forge".into(),
                                    options: vec![],
                                    commands: dead,
                                };
                                if let Err(e) =
                                    batch::run_batch(&mut sc, vec![push], &policy).await
                                {
                                    if matches!(e, ForgeError::Fenced(_)) {
                                        return Err(e);
                                    }
                                    eprintln!("flint-forge: prune deferred: {e}");
                                } else {
                                    publish(&shared, &sc, Phase::Serving);
                                }
                            }
                            Ok(_) => {}
                            Err(e) => eprintln!("flint-forge: prune deferred: {e}"),
                        }
                    }
                }
            }
            res = fold_rx.recv() => {
                if let Some(res) = res {
                    fold_landed(&mut sc, res, &fold_tx).await?;
                }
            }
            incoming = rx.recv() => {
                let Some(first) = incoming else {
                    return Err(ForgeError::State("the hook socket closed".into()));
                };
                let waiting = collect(&mut rx, first, &sc, &mut next_id).await;
                // Re-read before judging, so a policy edit is in force
                // for the very next push. A document that has become
                // unreadable keeps the last GOOD one and says so: the
                // operator renders this from a typed struct, so an
                // unparseable file is a hand edit, and refusing every
                // push over one would turn a typo into an outage.
                if let Some(dir) = opts.policy_dir.as_deref() {
                    match Policy::load(dir) {
                        Ok(Some(p)) => policy = p,
                        Ok(None) => {}
                        Err(e) => eprintln!(
                            "flint-forge: {e}; keeping the policy this server started with"
                        ),
                    }
                }
                run_and_report(&mut sc, waiting, &policy, &shared, &fold_tx).await?;
                // AFTER the report, and never before it. The export is
                // derived data: a push is acknowledged on the strength
                // of the pack and the snapshot, and nothing about
                // republishing a tree may delay or fail that. A failure
                // here is logged and retried on the next batch.
                if let Some(ex) = opts.export.as_ref() {
                    // A fresh pod has no `flint-sync` baseline, and
                    // without one every object in the export prefix
                    // looks like a stranger's: lean parks all of them
                    // and the published workspace freezes for good.
                    let bkey = sc.cfg.export_baseline_key();
                    if let Err(e) =
                        super::export::rehydrate_baseline(sc.store.as_ref(), &bkey, ex).await
                    {
                        eprintln!("flint-forge: export baseline not rehydrated: {e}");
                    }
                    match super::export::maybe_run(&sc.git, ex, super::now_unix()).await {
                        // Stashed, not written: the NEXT batch's single
                        // CAS carries it (see `Syncer::pending_exported_commit`).
                        Ok(Some(commit)) => {
                            let ep = sc.lease().map(|l| l.epoch).unwrap_or(0);
                            if let Err(e) = super::export::preserve_baseline(
                                sc.store.as_ref(),
                                &bkey,
                                ex,
                                ep,
                            )
                            .await
                            {
                                eprintln!("flint-forge: export baseline not preserved: {e}");
                            }
                            sc.pending_exported_commit = Some(commit)
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("flint-forge: export deferred: {e}"),
                    }
                }
            }
        }
    }
}

fn log_plan(plan: &fold::Plan) {
    match plan {
        fold::Plan::Fold { inputs } => {
            eprintln!("flint-forge: folding {} tier pack(s) beside the loop", inputs.len())
        }
        fold::Plan::Base { inputs } => eprintln!(
            "flint-forge: rebuilding the base from {} named pack(s) beside the loop",
            inputs.len()
        ),
    }
}

/// The fold task's result: its error is logged and the fold cleared;
/// its pack is committed on the loop, and a commit whose CAS neither
/// landed nor 412'd is fatal — the process restarts and restores rather
/// than leave an unnamed pack for the next batch to upload on a push's
/// path.
async fn fold_landed(
    sc: &mut Syncer,
    res: fold::FoldResult,
    fold_tx: &mpsc::Sender<fold::FoldResult>,
) -> ForgeResult<()> {
    if let Some(e) = res.error.as_ref() {
        eprintln!("flint-forge: fold failed beside the loop: {e}");
        fold::abort(sc);
        return Ok(());
    }
    match fold::commit(sc, res, super::now_unix()).await {
        Ok(_) => {}
        Err(e @ ForgeError::Fenced(_)) => return Err(e),
        Err(e) => return Err(e),
    }
    match fold::maybe_spawn(sc, fold_tx.clone(), super::now_unix()) {
        Ok(Some(plan)) => log_plan(&plan),
        Ok(None) => {}
        Err(e @ ForgeError::Fenced(_)) => return Err(e),
        Err(e) => eprintln!("flint-forge: fold not planned: {e}"),
    }
    Ok(())
}

/// The tick's fold work: the stall detector, retention's unlinks, the
/// ledger sweep (capped), the plan, and the full LIST sweep when due
/// and no fold is in flight.
async fn fold_tick(sc: &mut Syncer, fold_tx: &mpsc::Sender<fold::FoldResult>) -> ForgeResult<()> {
    let now = super::now_unix();
    fold::check_stall(sc, now);
    if let Err(e) = fold::unlink_retained(sc, now) {
        eprintln!("flint-forge: retained packs not unlinked: {e}");
    }
    match fold::sweep_ledger(sc, now, 64).await {
        Ok(_) => {}
        Err(e @ ForgeError::Fenced(_)) => return Err(e),
        Err(e) => eprintln!("flint-forge: ledger sweep deferred: {e}"),
    }
    match fold::maybe_spawn(sc, fold_tx.clone(), now) {
        Ok(Some(plan)) => log_plan(&plan),
        Ok(None) => {}
        Err(e @ ForgeError::Fenced(_)) => return Err(e),
        Err(e) => eprintln!("flint-forge: fold not planned: {e}"),
    }
    if sc.fold.is_none() && now.saturating_sub(sc.last_full_sweep_unix) >= sc.cfg.sweep_every_secs {
        match super::sweep::sweep(sc).await {
            Ok(_) => sc.last_full_sweep_unix = now,
            Err(e @ ForgeError::Fenced(_)) => return Err(e),
            Err(e) => eprintln!("flint-forge: sweep deferred: {e}"),
        }
    }
    Ok(())
}

/// The pruner's own interval, tracked separately from the maintenance
/// tick so that turning the pass down to daily does not also turn the
/// bundle cadence down.
fn prune_due(last: &mut u64, cfg: &super::prune::PruneConfig, now: u64) -> bool {
    if *last > 0 && now.saturating_sub(*last) < cfg.every_secs {
        return false;
    }
    *last = now;
    true
}

/// One waiting hook: its push and the channel its report goes back on.
struct Waiting {
    id: u64,
    reply: tokio::sync::oneshot::Sender<HookResponse>,
    push: batch::PushRequest,
}

/// Hold the batch open for the window, or until it is full.
///
/// This is where the design's throughput arithmetic lands: the batch
/// pays one lease renewal, one snapshot CAS and one ref transaction
/// however many pushes it carries, so the cost per push FALLS as the
/// fleet gets busier. A per-push sync would have paid three dependent
/// S3 round trips each.
async fn collect(
    rx: &mut mpsc::Receiver<Incoming>,
    first: Incoming,
    sc: &Syncer,
    next_id: &mut u64,
) -> Vec<Waiting> {
    let mut out = Vec::new();
    let push_of = |inc: Incoming, id: u64| Waiting {
        id,
        reply: inc.reply,
        push: uds::to_push(id, inc.request),
    };
    *next_id += 1;
    out.push(push_of(first, *next_id));
    // X20: a batch is what arrived while the previous batch ran, not
    // what arrives during a fixed wait. Pushes queue on the channel for
    // as long as a batch takes, so group commit under load is free and
    // a lone push pays nothing — the 400 ms window it used to pay was
    // 0.48 s of the 0.58 s a 1 KiB push cost on the wire (the walgit
    // comparison's P1). What is already queued is drained without
    // waiting; a window > 0 is kept as a knob for a caller that wants
    // the old behaviour.
    if sc.cfg.batch_window_ms == 0 {
        while out.len() < sc.cfg.batch_max {
            match rx.try_recv() {
                Ok(inc) => {
                    *next_id += 1;
                    out.push(push_of(inc, *next_id));
                }
                Err(_) => break,
            }
        }
        return out;
    }
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(sc.cfg.batch_window_ms);
    while out.len() < sc.cfg.batch_max {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(inc)) => {
                *next_id += 1;
                out.push(push_of(inc, *next_id));
            }
            // Channel closed, or the window elapsed: run what we have.
            Ok(None) | Err(_) => break,
        }
    }
    out
}

async fn run_and_report(
    sc: &mut Syncer,
    waiting: Vec<Waiting>,
    policy: &Policy,
    shared: &Shared,
    fold_tx: &mpsc::Sender<fold::FoldResult>,
) -> ForgeResult<()> {
    let pushes: Vec<batch::PushRequest> = waiting.iter().map(|w| w.push.clone()).collect();
    // A phase that must move: the renewer renews it only while the
    // batch's progress counter advances.
    publish(shared, sc, Phase::Pushing);
    let outcome = batch::run_batch(sc, pushes, policy).await;
    match outcome {
        Ok(reports) => {
            deliver(waiting, reports);
            publish(shared, sc, Phase::Serving);
            if sc.cfg.fold_factor == 0 {
                // The CONTROL rule (X18): between batches, never beside
                // them — the full repack with the loop inside its upload.
                match restore::maybe_repack(sc).await {
                    Ok(true) => {
                        publish(shared, sc, Phase::Sweeping);
                        if let Err(e) = super::sweep::sweep(sc).await {
                            eprintln!("flint-forge: sweep deferred: {e}");
                        }
                        publish(shared, sc, Phase::Serving);
                    }
                    Ok(false) => {}
                    Err(e @ ForgeError::Fenced(_)) => return Err(e),
                    Err(e) => eprintln!("flint-forge: repack deferred: {e}"),
                }
            } else {
                // Tiers: a fold's bytes are never on a push's path. The
                // plan runs here and on the tick; the task runs beside
                // the loop; only its commit (the fold arm) is on it.
                match fold::maybe_spawn(sc, fold_tx.clone(), super::now_unix()) {
                    Ok(Some(plan)) => log_plan(&plan),
                    Ok(None) => {}
                    Err(e @ ForgeError::Fenced(_)) => return Err(e),
                    Err(e) => eprintln!("flint-forge: fold not planned: {e}"),
                }
            }
            Ok(())
        }
        Err(e) => {
            // Nothing was acknowledged. Every push in the batch is told
            // why, and a fence takes the process down with it: the
            // client sees a failed push and retries into a server that
            // has restored.
            let reason = e.to_string();
            for w in waiting {
                let results = w
                    .push
                    .commands
                    .iter()
                    .map(|c| batch::CommandResult::Ng {
                        name: c.name.clone(),
                        reason: reason.clone(),
                    })
                    .collect();
                let _ = w.reply.send(HookResponse { results });
            }
            publish(shared, sc, Phase::Draining);
            Err(e)
        }
    }
}

fn deliver(waiting: Vec<Waiting>, reports: Vec<PushReport>) {
    for w in waiting {
        let results = reports
            .iter()
            .find(|r| r.id == w.id)
            .map(|r| r.results.clone())
            .unwrap_or_default();
        // A hook that hung up gets no report and needs none: its
        // client's push already failed.
        let _ = w.reply.send(HookResponse { results });
    }
}

fn publish(shared: &Shared, sc: &Syncer, phase: Phase) {
    if let Ok(mut g) = shared.lock() {
        *g = status::facts(sc, phase);
    }
}

/// The smallest HTTP server that answers the ladder's poll — and, when
/// LFS is on, the batch API.
///
/// A dependency-free listener rather than a web framework: the surface
/// is four routes, it is reachable only on the pod network, and the
/// clients are the operator's poll and the git container's runner
/// relaying one JSON POST.
async fn serve_http(addr: &str, shared: Shared, lfs: Option<Arc<LfsCtx>>) -> ForgeResult<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let shared = shared.clone();
        let lfs = lfs.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut reader = BufReader::new(r);
            let Some((method, path, headers)) = read_head(&mut reader).await else { return };

            let (code, ctype, body) = if method == "GET" && (path == "/status" || path.starts_with("/status?")) {
                let facts = shared.lock().map(|g| g.clone());
                match facts {
                    Ok(f) => (
                        200,
                        "application/json",
                        serde_json::to_vec(&status::document(&f, super::now_unix())).unwrap_or_default(),
                    ),
                    Err(_) => (500, "text/plain", b"status unavailable\n".to_vec()),
                }
            } else if method == "GET" && path == "/healthz" {
                // "Am I serving", not "am I alive": a headless Service
                // publishes DNS only for READY pods, so a restoring
                // server is simply not resolvable and the door holds.
                // The decision is `Facts::serving`, which also withdraws
                // readiness when no renewal has landed for the term
                // (X13) — the body says which.
                let (serving, why) = shared
                    .lock()
                    .map(|f| {
                        (
                            f.serving(),
                            if f.renewal_overdue { "not serving: no lease renewal within the term\n" } else { "not serving\n" },
                        )
                    })
                    .unwrap_or((false, "status unavailable\n"));
                if serving {
                    (200, "text/plain", b"ok\n".to_vec())
                } else {
                    (503, "text/plain", why.as_bytes().to_vec())
                }
            } else if method == "POST" && path.starts_with("/lfs/objects/") {
                let len = headers
                    .get("content-length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                // A batch is JSON describing at most a thousand
                // objects. Anything larger is not a batch.
                if len > 4 << 20 {
                    (413, super::lfs::LFS_MEDIA_TYPE, lfs_error("the batch request is too large"))
                } else {
                    let mut buf = vec![0u8; len];
                    if reader.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    match lfs.as_ref() {
                        None => (
                            404,
                            super::lfs::LFS_MEDIA_TYPE,
                            lfs_error("git LFS is not enabled for this repository"),
                        ),
                        Some(ctx) => {
                            let verify_url = headers.get("x-forge-lfs-verify").cloned();
                            handle_lfs(ctx, &path, &buf, verify_url.as_deref()).await
                        }
                    }
                }
            } else {
                (404, "text/plain", b"not found\n".to_vec())
            };

            let head = format!(
                "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                if code < 400 { "OK" } else { "Error" },
                body.len()
            );
            let _ = w.write_all(head.as_bytes()).await;
            let _ = w.write_all(&body).await;
            let _ = w.flush().await;
        });
    }
}

/// What the LFS handler needs, and it is exactly what the door has not
/// got: a store handle for this repository's bucket.
struct LfsCtx {
    store: Arc<dyn flint_store::ObjectStore>,
    prefix: String,
    ttl_secs: u64,
}

fn lfs_error(message: &str) -> Vec<u8> {
    serde_json::to_vec(&super::lfs::BatchError { message: message.to_string() }).unwrap_or_default()
}

/// The request line and headers, lower-cased by name.
async fn read_head(
    reader: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Option<(String, String, std::collections::HashMap<String, String>)> {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = std::collections::HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).await.ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Some((method, path, headers))
}

async fn handle_lfs(
    ctx: &LfsCtx,
    path: &str,
    body: &[u8],
    verify_url: Option<&str>,
) -> (u16, &'static str, Vec<u8>) {
    use super::lfs;
    if path.starts_with("/lfs/objects/batch") {
        let req: lfs::BatchRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                return (400, lfs::LFS_MEDIA_TYPE, lfs_error(&format!("unparseable batch: {e}")))
            }
        };
        match lfs::batch(ctx.store.as_ref(), &ctx.prefix, &req, ctx.ttl_secs).await {
            Ok(mut res) => {
                // The verify href is the door's to supply: only the
                // door knows the URL the client actually reached, and
                // handing back one built from a pod's own address
                // would send the client somewhere it cannot go. With
                // no door in front, the action is dropped rather than
                // guessed.
                for obj in res.objects.iter_mut() {
                    match verify_url {
                        Some(u) => {
                            if let Some(a) = obj.actions.get_mut("verify") {
                                a.href = u.to_string();
                            }
                        }
                        None => {
                            obj.actions.remove("verify");
                        }
                    }
                }
                (200, lfs::LFS_MEDIA_TYPE, serde_json::to_vec(&res).unwrap_or_default())
            }
            Err(e) => (422, lfs::LFS_MEDIA_TYPE, serde_json::to_vec(&e).unwrap_or_default()),
        }
    } else if path.starts_with("/lfs/objects/verify") {
        let spec: lfs::ObjectSpec = match serde_json::from_slice(body) {
            Ok(s) => s,
            Err(e) => {
                return (400, lfs::LFS_MEDIA_TYPE, lfs_error(&format!("unparseable verify: {e}")))
            }
        };
        match lfs::verify(ctx.store.as_ref(), &ctx.prefix, &spec).await {
            Ok(()) => (200, lfs::LFS_MEDIA_TYPE, b"{}".to_vec()),
            Err((code, message)) => (code, lfs::LFS_MEDIA_TYPE, lfs_error(&message)),
        }
    } else {
        (404, lfs::LFS_MEDIA_TYPE, lfs_error("no such LFS route"))
    }
}

#[cfg(test)]
mod collect_tests {
    //! X20 on virtual time. Every test here has a control: the timed
    //! window (> 0) must still wait, or the tests would pass against a
    //! collector that ignored the knob altogether.
    use super::super::{gitcmd::RefUpdate, ForgeConfig};
    use super::*;

    fn syncer(window_ms: u64) -> (Syncer, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = ForgeConfig::new("t/r", dir.path().join("repo.git"));
        cfg.batch_window_ms = window_ms;
        cfg.batch_max = 4;
        let store: Arc<dyn flint_store::ObjectStore> =
            Arc::new(flint_store::memory::MemoryStore::new());
        (Syncer::new(store, cfg, "collect-test".into()), dir)
    }

    fn incoming(n: u64) -> Incoming {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        Incoming {
            request: uds::HookRequest {
                principal: "tester".into(),
                options: vec![],
                commands: vec![RefUpdate {
                    name: format!("refs/heads/b{n}"),
                    old_oid: "0".repeat(40),
                    new_oid: format!("{n:040x}"),
                }],
            },
            reply,
        }
    }

    /// A lone push at window 0 is a batch of one, and the collector
    /// returns without the clock moving.
    #[tokio::test(start_paused = true)]
    async fn a_lone_push_pays_no_window() {
        let (sc, _d) = syncer(0);
        let (_tx, mut rx) = mpsc::channel::<Incoming>(8);
        let mut id = 0;
        let t0 = tokio::time::Instant::now();
        let got = collect(&mut rx, incoming(1), &sc, &mut id).await;
        assert_eq!(got.len(), 1);
        assert_eq!(tokio::time::Instant::now(), t0, "no wait at window 0");
    }

    /// Pushes that queued while a batch ran are drained into the next
    /// batch, without waiting and up to `batch_max`.
    #[tokio::test(start_paused = true)]
    async fn what_queued_during_a_batch_is_the_next_batch() {
        let (sc, _d) = syncer(0);
        let (tx, mut rx) = mpsc::channel::<Incoming>(8);
        for n in 2..=6 {
            tx.send(incoming(n)).await.unwrap();
        }
        let mut id = 0;
        let t0 = tokio::time::Instant::now();
        let got = collect(&mut rx, incoming(1), &sc, &mut id).await;
        assert_eq!(got.len(), 4, "batch_max bounds the drain");
        assert_eq!(tokio::time::Instant::now(), t0, "the drain does not wait");
        let next_first = rx.try_recv().unwrap();
        let rest = collect(&mut rx, next_first, &sc, &mut id).await;
        assert_eq!(rest.len(), 2, "the remainder is the batch after");
        assert_eq!(id, 6);
    }

    /// The control: a window > 0 still waits for it, so the two tests
    /// above are about the knob's value and not about a collector that
    /// never waits.
    #[tokio::test(start_paused = true)]
    async fn a_positive_window_still_waits() {
        let (sc, _d) = syncer(400);
        let (tx, mut rx) = mpsc::channel::<Incoming>(8);
        let mut id = 0;
        let t0 = tokio::time::Instant::now();
        // A sender stays alive in this scope: a closed channel ends the
        // window early by design, and this test is about the timer.
        let late_tx = tx.clone();
        let late = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            late_tx.send(incoming(2)).await.unwrap();
        });
        let got = collect(&mut rx, incoming(1), &sc, &mut id).await;
        late.await.unwrap();
        assert_eq!(got.len(), 2, "a push inside the window joins the batch");
        let waited = tokio::time::Instant::now() - t0;
        assert!(
            waited >= std::time::Duration::from_millis(400),
            "the window is waited out ({waited:?})"
        );
        drop(tx);
    }
}
