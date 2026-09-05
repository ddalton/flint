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
use super::{bundle, lease, restore, ForgeError, ForgeResult, Syncer};

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

/// Shared with the status listener. A `Mutex` over facts, not over the
/// syncer: the status path must never be able to block a batch.
type Shared = Arc<Mutex<Facts>>;

pub async fn run(mut sc: Syncer, opts: ServerOpts) -> ForgeResult<()> {
    restore::check_git_floor(&sc).await?;
    lease::verify_claim(&sc).await?;

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
                tokio::time::sleep(std::time::Duration::from_secs(sc.cfg.heartbeat_secs)).await;
            }
        }
    }

    // ── restore ──────────────────────────────────────────────────────
    publish(&shared, &sc, Phase::Importing);
    restore::restore(&mut sc).await?;
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

    // Housekeeping — bundles and the branch pruner — runs on its own
    // slower timer. Both are cheap to decline, but declining them at
    // the heartbeat's rate would put a subprocess and a log line every
    // ten seconds behind a repository nobody is using.
    let mut maintenance = tokio::time::interval(std::time::Duration::from_secs(60));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    maintenance.tick().await;

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
    let mut last_prune: u64 = 0;
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
            _ = maintenance.tick() => {
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

/// The smallest HTTP server that answers the ladder's poll — and, when
/// LFS is on, the batch API.
///
/// A dependency-free listener rather than a web framework: the surface
/// is four routes, it is reachable only on the pod network, and the
/// clients are the operator's poll and nginx forwarding one JSON POST.
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
                let serving = shared
                    .lock()
                    .map(|f| f.phase == Phase::Serving && f.fenced.is_none())
                    .unwrap_or(false);
                if serving {
                    (200, "text/plain", b"ok\n".to_vec())
                } else {
                    (503, "text/plain", b"not serving\n".to_vec())
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
