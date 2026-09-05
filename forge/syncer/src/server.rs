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
use super::status::{self, Facts, Phase};
use super::uds::{self, HookResponse, Incoming};
use super::{lease, restore, ForgeError, ForgeResult, Syncer};

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
}

/// Shared with the status listener. A `Mutex` over facts, not over the
/// syncer: the status path must never be able to block a batch.
type Shared = Arc<Mutex<Facts>>;

pub async fn run(mut sc: Syncer, opts: ServerOpts) -> ForgeResult<()> {
    restore::check_git_floor(&sc).await?;
    lease::verify_claim(&sc).await?;

    let shared: Shared = Arc::new(Mutex::new(status::facts(&sc, Phase::Starting)));
    if let Some(addr) = opts.status_addr.clone() {
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_status(&addr, shared).await {
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
                tokio::time::sleep(std::time::Duration::from_secs(sc.cfg.heartbeat_secs)).await;
            }
        }
    }

    // ── restore ──────────────────────────────────────────────────────
    publish(&shared, &sc, Phase::Importing);
    restore::restore(&mut sc).await?;
    let branch = sc.cfg.default_branch.clone();
    restore::set_default_branch(&sc, &branch).await?;
    publish(&shared, &sc, Phase::Serving);

    // ── serve ────────────────────────────────────────────────────────
    let (tx, mut rx) = mpsc::channel::<Incoming>(256);
    {
        let socket = opts.socket.clone();
        tokio::spawn(async move {
            if let Err(e) = uds::serve(&socket, tx).await {
                eprintln!("flint-forge: hook socket stopped: {e}");
            }
        });
    }

    let mut heartbeat =
        tokio::time::interval(std::time::Duration::from_secs(sc.cfg.heartbeat_secs));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; the claim just renewed for us.
    heartbeat.tick().await;

    // A clean release on SIGTERM: a successor claims at once instead of
    // waiting out six quiet polls, which is the difference between a
    // roll that costs one request and one that costs a minute of them.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(ForgeError::Io)?;
    let mut policy = opts.policy.clone();
    let mut next_id: u64 = 0;
    loop {
        tokio::select! {
            _ = term.recv() => {
                publish(&shared, &sc, Phase::Draining);
                match lease::release(&mut sc).await {
                    Ok(()) => eprintln!("flint-forge: lease released; a successor may claim at once"),
                    // Not fatal: the successor waits out the quiet
                    // polls instead, which is slower and still correct.
                    Err(e) => eprintln!("flint-forge: could not release the lease cleanly: {e}"),
                }
                publish(&shared, &sc, Phase::Released);
                return Ok(());
            }
            // The heartbeat runs whether or not pushes arrive. A
            // server that renewed only inside a push would let a quiet
            // repository's lease lapse and leave a straggler unfenced.
            _ = heartbeat.tick() => {
                match lease::renew(&mut sc).await {
                    Ok(()) => publish(&shared, &sc, Phase::Serving),
                    Err(e @ ForgeError::Fenced(_)) => {
                        publish(&shared, &sc, Phase::Draining);
                        return Err(e);
                    }
                    Err(e) => {
                        // An auth pause or a transient store fault:
                        // keep serving reads, keep trying. The lease is
                        // still ours until it is not.
                        eprintln!("flint-forge: heartbeat: {e}");
                    }
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
                run_and_report(&mut sc, waiting, &policy, &shared).await?;
                // AFTER the report, and never before it. The export is
                // derived data: a push is acknowledged on the strength
                // of the pack and the snapshot, and nothing about
                // republishing a tree may delay or fail that. A failure
                // here is logged and retried on the next batch.
                if let Some(ex) = opts.export.as_ref() {
                    match super::export::maybe_run(&sc.git, ex, super::now_unix()).await {
                        // Stashed, not written: the NEXT batch's single
                        // CAS carries it (see `Syncer::pending_exported_commit`).
                        Ok(Some(commit)) => sc.pending_exported_commit = Some(commit),
                        Ok(None) => {}
                        Err(e) => eprintln!("flint-forge: export deferred: {e}"),
                    }
                }
            }
        }
    }
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
    if sc.cfg.batch_window_ms == 0 {
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
) -> ForgeResult<()> {
    let pushes: Vec<batch::PushRequest> = waiting.iter().map(|w| w.push.clone()).collect();
    let outcome = batch::run_batch(sc, pushes, policy).await;
    match outcome {
        Ok(reports) => {
            deliver(waiting, reports);
            publish(shared, sc, Phase::Serving);
            // Between batches, never beside them: git's own auto-gc is
            // off precisely so that nothing but this task writes
            // `objects/pack/` (design §10).
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

/// The smallest HTTP server that answers the ladder's poll.
///
/// A dependency-free listener rather than a web framework: the surface
/// is one route, it is reachable only on the pod network, and the
/// operator's client sends `GET /status HTTP/1.1` with a `Host` header
/// and reads a body it length-checks.
async fn serve_status(addr: &str, shared: Shared) -> ForgeResult<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let shared = shared.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let Ok(Some(request)) = lines.next_line().await else { return };
            let mut parts = request.split_whitespace();
            let method = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("");
            let (code, body) = if method != "GET" {
                (405, b"method not allowed\n".to_vec())
            } else if path == "/status" || path.starts_with("/status?") {
                let facts = shared.lock().map(|g| g.clone());
                match facts {
                    Ok(f) => (
                        200,
                        serde_json::to_vec(&status::document(&f, super::now_unix()))
                            .unwrap_or_default(),
                    ),
                    Err(_) => (500, b"status unavailable\n".to_vec()),
                }
            } else if path == "/healthz" {
                (200, b"ok\n".to_vec())
            } else {
                (404, b"not found\n".to_vec())
            };
            let head = format!(
                "HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                if code == 200 { "OK" } else { "Error" },
                body.len()
            );
            let _ = w.write_all(head.as_bytes()).await;
            let _ = w.write_all(&body).await;
            let _ = w.flush().await;
        });
    }
}
