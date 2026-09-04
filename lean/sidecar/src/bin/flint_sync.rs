//! flint-sync: the lean checkout/publish sidecar (plan of record:
//! docs/plans/flint-lean-plan.md). Runs beside an agent container as a
//! native sidecar: checkout gates the agent start; the barrier loop
//! publishes on the flush floor; preStop drains.
//!
//! Subcommands:
//!   checkout   materialize the workspace (restart-matrix aware), exit
//!   barrier    one publish barrier, exit
//!   sync       the HITL sync verb (scan-first), exit
//!   recover-staged  re-cite durable-but-uncited work as one flagged
//!              boundary (gated recovery after pod replacement), exit
//!   ctl <boundary|sync|status>
//!              talk to the UDS door of the sidecar running in THIS
//!              pod (§2.5). A client, not a second sidecar: it takes
//!              no lease and no state lock. Requires FLINT_SYNC_UDS_DOOR.
//!   status     render gauges + pending + lease state as JSON, exit.
//!              Takes NO lease and NO state-dir lock: it exists to
//!              diagnose a workspace whose sidecar is dead or deposed,
//!              and claiming would depose the very sidecar under
//!              diagnosis.
//!   run        claim → checkout → barrier loop (floorSecs) → drain on
//!              SIGTERM → clean lease release
//!
//! Environment:
//!   FLINT_SYNC_BUCKET    (required) bucket name
//!   FLINT_SYNC_PREFIX    (required) subtree key prefix
//!   FLINT_SYNC_ROOT      (required) workspace root
//!   FLINT_SYNC_ENDPOINT  S3 endpoint override (MinIO/proxy rigs)
//!   FLINT_SYNC_FLOOR_SECS         publish cadence floor (default 60)
//!   FLINT_SYNC_MAX_BYTES/_FILES   checkout budgets (0 = unlimited)
//!   FLINT_SYNC_FANOUT             concurrent fetches/uploads (default 32)
//!   FLINT_SYNC_FETCH_INFLIGHT_MB  checkout bytes in flight (default 512)
//!   FLINT_SYNC_BOUNDARY_MODE      cadence|hybrid|gated (default hybrid)
//!   FLINT_SYNC_SENTINELS          auto|off|force (default auto)
//!   FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS  (default 5)
//!   FLINT_SYNC_SENTINEL_HOURLY_BUDGET      work units/hour (default 60)
//!   FLINT_SYNC_SENTINEL_POLL_SECS          (default 1; env-only)
//!   FLINT_SYNC_QUIESCE_BOUND_SECS          gated: quiescence window (30)
//!   FLINT_SYNC_VISIBILITY_LAG_BOUND_SECS   gated: REQUIRED, no default
//!   FLINT_SYNC_STAGED_BACKLOG_CAP_OBJECTS  gated: forced-citation cap (5000)
//!   FLINT_SYNC_STAGED_BACKLOG_CAP_BYTES    gated: forced-citation cap (2 GiB)
//!   FLINT_SYNC_NONCURRENT_RETENTION_DAYS   gated: the backstop's age (30)
//!   FLINT_SYNC_UDS_DOOR                    "true" arms .flint-sync/ctl.sock
//!   FLINT_SYNC_METRICS                     "true" arms /metrics (D15)
//!   FLINT_SYNC_METRICS_PORT                default 9847
//!   FLINT_SYNC_WORKSPACE/_NAMESPACE        the only two metric labels

use std::sync::Arc;
use std::time::Duration;

use flint_lean::lease::{self, ClaimOutcome};
use flint_lean::state::SidecarState;
use flint_lean::{BoundaryMode, LeanConfig, LeanError, Sidecar, SentinelMode};
use flint_store::s3::S3Store;
use flint_store::ObjectStore;
use warp::Filter;

fn env_req(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("flint-sync: {name} is required");
        std::process::exit(2);
    })
}

fn flint_lean_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One retry log line, with a credential refusal called by its name.
///
/// Every serve arm retries a non-fence error, which is right. But
/// "failed (retrying)" describes a 401/403 exactly as it describes a
/// flaky bucket, and only one of the two is fixed by waiting — the
/// diagnosis `StoreError::Auth` exists to make (design §6.3). The
/// consequence is worth spelling out on the line itself: a paused
/// holder stops renewing, and a stopped renewal is precisely what a
/// challenger reads as a dead holder.
fn log_retry(sc: &Sidecar, e: &LeanError, fallback: &str) {
    if !e.is_auth() {
        eprintln!("flint-sync: {fallback}: {e}");
        return;
    }
    let paused = sc
        .load_gauges()
        .ok()
        .and_then(|g| g.auth_paused_since_unix)
        .map(|t| flint_lean_now().saturating_sub(t))
        .unwrap_or(0);
    eprintln!(
        "flint-sync: REFUSED reason=auth arm={fallback} paused_secs={paused}: {e} \
         — the store rejected our credentials. Not contention, not a lease \
         conflict; retrying does not fix it. Local files keep serving and \
         staged work is intact. Check the credential broker and the projected \
         token (and this node's clock — a skewed one answers 403 too). \
         Renewals have STOPPED: a challenger that can still reach the store \
         may depose this live writer."
    );
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let bucket = env_req("FLINT_SYNC_BUCKET");
    let prefix = env_req("FLINT_SYNC_PREFIX");
    let root = env_req("FLINT_SYNC_ROOT");
    let endpoint = std::env::var("FLINT_SYNC_ENDPOINT").ok();

    let store = match S3Store::connect(bucket, endpoint).await {
        Ok(s) => Arc::new(s) as Arc<dyn ObjectStore>,
        Err(e) => {
            eprintln!("flint-sync: store connect: {e}");
            std::process::exit(1);
        }
    };
    let mut cfg = LeanConfig::new(&prefix, &root);
    cfg.floor_secs = env_u64("FLINT_SYNC_FLOOR_SECS", 60);
    cfg.max_bytes = env_u64("FLINT_SYNC_MAX_BYTES", 0);
    cfg.max_files = env_u64("FLINT_SYNC_MAX_FILES", 0);
    cfg.fanout = env_u64("FLINT_SYNC_FANOUT", 32).max(1) as usize;
    cfg.project_id = std::env::var("FLINT_SYNC_PROJECT_ID").ok().filter(|p| !p.is_empty());
    cfg.fetch_inflight_max_bytes =
        env_u64("FLINT_SYNC_FETCH_INFLIGHT_MB", 512).max(1) * 1024 * 1024;
    if let Ok(m) = std::env::var("FLINT_SYNC_BOUNDARY_MODE") {
        match BoundaryMode::parse(&m) {
            Some(bm) => cfg.boundary_mode = bm,
            None => {
                eprintln!("flint-sync: FLINT_SYNC_BOUNDARY_MODE={m:?} is not cadence|hybrid|gated");
                std::process::exit(2);
            }
        }
    }
    if let Ok(m) = std::env::var("FLINT_SYNC_SENTINELS") {
        match SentinelMode::parse(&m) {
            Some(sm) => cfg.sentinel_mode = sm,
            None => {
                eprintln!("flint-sync: FLINT_SYNC_SENTINELS={m:?} is not auto|off|force");
                std::process::exit(2);
            }
        }
    }
    cfg.sentinel_min_interval_secs = env_u64("FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS", 5);
    cfg.sentinel_hourly_budget = env_u64("FLINT_SYNC_SENTINEL_HOURLY_BUDGET", 60);
    cfg.sentinel_poll_secs = env_u64("FLINT_SYNC_SENTINEL_POLL_SECS", 1).max(1);
    cfg.quiesce_bound_secs = env_u64("FLINT_SYNC_QUIESCE_BOUND_SECS", 30);
    // The backlog caps and the retention days were config fields the
    // binary never read — knobs that exist and do NOTHING, the class
    // this codebase keeps paying for. The caps bound the preStop drain
    // by construction (D10 sizes the pod's grace against exactly these
    // numbers), and the retention is what the citation GC's noncurrent
    // gauge is measured against.
    cfg.staged_backlog_cap_objects = env_u64("FLINT_SYNC_STAGED_BACKLOG_CAP_OBJECTS", 5_000);
    cfg.staged_backlog_cap_bytes =
        env_u64("FLINT_SYNC_STAGED_BACKLOG_CAP_BYTES", 2 * 1024 * 1024 * 1024);
    cfg.noncurrent_retention_days = env_u64("FLINT_SYNC_NONCURRENT_RETENTION_DAYS", 30);
    cfg.visibility_lag_bound_secs =
        std::env::var("FLINT_SYNC_VISIBILITY_LAG_BOUND_SECS").ok().and_then(|v| v.parse().ok());
    // Gated is refused without a lag bound: unbounded staleness must be
    // impossible by construction, not by convention (§2.4.1).
    if cfg.boundary_mode == BoundaryMode::Gated && cfg.visibility_lag_bound_secs.is_none() {
        eprintln!(
            "flint-sync: boundaryMode=gated requires FLINT_SYNC_VISIBILITY_LAG_BOUND_SECS \
             (unbounded citation staleness is refused)"
        );
        std::process::exit(2);
    }

    // Also dispatched before the state directory is opened, and for a
    // stronger reason: `ctl` is a CLIENT of the running sidecar. Taking
    // the occupancy lock — or the lease — would fight the very process
    // it is asking to do something.
    if cmd == "ctl" {
        let verb = std::env::args().nth(2).unwrap_or_else(|| "status".into());
        let (method, path) = match verb.as_str() {
            "boundary" => ("POST", "/v1/boundary"),
            "sync" => ("POST", "/v1/sync"),
            "status" => ("GET", "/v1/status"),
            other => {
                eprintln!("flint-sync ctl: unknown verb {other:?} (boundary|sync|status)");
                std::process::exit(2);
            }
        };
        let sock = flint_lean::uds::socket_path(&cfg.state_dir());
        match ctl_call(&sock, method, path).await {
            Ok(body) => {
                println!("{body}");
                return;
            }
            Err(e) => {
                eprintln!("flint-sync ctl: {} ({e})", sock.display());
                std::process::exit(1);
            }
        }
    }

    // Dispatched before the state directory is opened: a live sidecar
    // holds the occupancy flock, and `status` must work WHILE it does.
    if cmd == "status" {
        match flint_lean::status_report(&cfg) {
            Ok(r) => {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
                return;
            }
            Err(e) => {
                eprintln!("flint-sync: status: {e}");
                std::process::exit(1);
            }
        }
    }

    let state = match SidecarState::open(cfg.state_dir()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("flint-sync: state dir: {e}");
            std::process::exit(1);
        }
    };
    let mut sc = Sidecar { store, cfg, state, lease: None, noted_not_regular: Default::default() };

    let result = match cmd.as_str() {
        "checkout" => claim_then(&mut sc, Step::Checkout).await,
        "barrier" => claim_then(&mut sc, Step::Barrier).await,
        "sync" => claim_then(&mut sc, Step::Sync).await,
        "recover-staged" => claim_then(&mut sc, Step::RecoverStaged).await,
        "run" => run_loop(&mut sc).await,
        other => {
            eprintln!(
                "flint-sync: unknown subcommand {other:?} \
                 (checkout|barrier|sync|status|ctl|recover-staged|run)"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("flint-sync: {e}");
        // A fence is a clean shutdown order, not a crash loop. A
        // refusal is final: EXIT_REFUSED is the code the CSI plugin
        // reads as "tear the worker down and name the reason on the
        // tenant" — under OnFailure any other code is relaunched in
        // place forever, and a refusal that shared it looked to the
        // tenant like a checkout that never finished (leg S22).
        std::process::exit(match e {
            LeanError::Fenced(_) => 0,
            LeanError::Refused(_) => flint_lean::EXIT_REFUSED,
            _ => 1,
        });
    }
}

/// One request over the control socket. Deliberately hand-rolled: the
/// door is a bounded, pod-internal, one-request-per-connection surface,
/// and a client that pulls in an HTTP stack to say twelve bytes would
/// be the tail wagging the dog.
async fn ctl_call(
    sock: &std::path::Path,
    method: &str,
    path: &str,
) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::UnixStream::connect(sock).await?;
    s.write_all(format!("{method} {path} HTTP/1.1\r\nhost: flint\r\n\r\n").as_bytes())
        .await?;
    s.flush().await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf).to_string();
    Ok(match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_string(),
        None => text,
    })
}

enum Step {
    Checkout,
    Barrier,
    Sync,
    RecoverStaged,
}

async fn claim(sc: &mut Sidecar) -> Result<(), LeanError> {
    // Before the first claim step: is this prefix ours to claim at all?
    lease::verify_claim(sc).await?;
    let mut answered_owed = false;
    loop {
        match lease::claim_step(sc).await? {
            ClaimOutcome::Claimed(lease) => {
                eprintln!("flint-sync: holding epoch {}", lease.epoch);
                return Ok(());
            }
            ClaimOutcome::Waiting { quiet_polls } => {
                if !answered_owed {
                    answered_owed = true;
                    match sc.refuse_what_this_incarnation_can_never_honor().await {
                        Ok(true) => eprintln!(
                            "flint-sync: a foreign holder stands and this incarnation owes an \
                             ack it can never honor — refused-fenced written, marker fenced"
                        ),
                        Ok(false) => {}
                        Err(e) => {
                            // Never let this block the claim: a fresh
                            // pod must still take over.
                            answered_owed = false;
                            eprintln!("flint-sync: could not settle owed acks while waiting: {e}");
                        }
                    }
                }
                eprintln!("flint-sync: waiting on the standing lease (quiet {quiet_polls}/6)");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn claim_then(sc: &mut Sidecar, step: Step) -> Result<(), LeanError> {
    claim(sc).await?;
    let out = async {
        match step {
            Step::Checkout => {
                let r = sc.checkout().await?;
                eprintln!(
                    "flint-sync: checkout — {} materialized, {} present, live-tree={}",
                    r.materialized, r.skipped_present, r.resumed_live_tree
                );
                eprintln!(
                    "flint-sync: phase manifest={:.3}s fetch={:.3}s commit={:.3}s",
                    r.manifest_secs, r.fetch_secs, r.commit_secs
                );
            }
            Step::Barrier => {
                let r = sc.run_barrier().await?;
                eprintln!(
                    "flint-sync: barrier seq={:?} up={} del={} parked={} consumed={}",
                    r.seq,
                    r.uploaded.len(),
                    r.deleted.len(),
                    r.parked.len(),
                    r.consumed
                );
            }
            Step::Sync => {
                let r = sc.sync().await?;
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            }
            Step::RecoverStaged => {
                let r = sc.recover_staged().await?;
                eprintln!(
                    "flint-sync: recover-staged seq={:?} recited={} dangling={} unrecoverable={}",
                    r.seq,
                    r.recited.len(),
                    r.dangling.len(),
                    r.unrecoverable.len()
                );
                for p in &r.recited {
                    eprintln!("flint-sync:   recited {p}");
                }
                // Named loudly: no verb can fix these — the retention
                // backstop reaped the cited version and no newer
                // generation survives.
                for p in &r.unrecoverable {
                    eprintln!("flint-sync:   UNRECOVERABLE {p}");
                }
                if !r.unrecoverable.is_empty() {
                    return Err(LeanError::State(format!(
                        "{} path(s) have no surviving version to cite",
                        r.unrecoverable.len()
                    )));
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = lease::release(sc).await;
    out
}

async fn run_loop(sc: &mut Sidecar) -> Result<(), LeanError> {
    claim(sc).await?;
    // This incarnation owes its own drain attestation; one left by an
    // earlier life of this tree must not vouch for it.
    if let Err(e) = sc.state.clear_drained() {
        eprintln!("flint-sync: could not clear a stale drain attestation: {e}");
    }
    // The drain's retry budget: the grace the delivery derived for it
    // (the CSI node plugin stamps the tenant's grace), else the shipped
    // three attempts.
    let drain_budget = Duration::from_secs(env_u64("FLINT_SYNC_DRAIN_BUDGET_SECS", 6));
    // D11: the capability marker is written at EVERY run startup —
    // after claim, before the first poll — not inside checkout. The
    // live-tree restart row returns at `marker_present()` without
    // reaching checkout's body, so pinning the write there would
    // upgrade a fleet whose live workspaces never get the marker:
    // sentinels dead on exactly the pods the upgrade targeted.
    // D8: gated mode is REFUSED over a backend that cannot express the
    // version surface — before a single byte is staged. Degrading into
    // etag semantics on a key whose current version is uncited is
    // precisely the torn view the mode exists to prevent, so this is a
    // startup failure, not a warning.
    sc.gated_startup_check().await?;
    let posture = sc.sentinel_preflight()?;
    sc.write_capabilities(&posture, false)?;
    if !posture.enabled {
        eprintln!(
            "flint-sync: sentinel verbs DISABLED ({}) — the poll arm will not arm",
            posture.reason.as_deref().unwrap_or("unknown")
        );
    }
    sc.checkout().await?;
    // RE-RUN the preflight rather than republishing the pre-checkout
    // snapshot (review: U25). Two of the preflight's inputs are written
    // BY checkout — `baseline.inst_base` wholesale, and the posture file
    // itself, from checkout's own fresher verdict — so passing the
    // `posture` computed above clobbered a newer answer with an older
    // one, and D0.4's fleet-visible verdict could advertise verbs as
    // live on the pod-replacement path. The preflight is sticky
    // (disabled stays disabled unless mode is `force`), so re-running it
    // can only narrow, never spuriously re-enable.
    let posture = sc.sentinel_preflight()?;
    sc.write_capabilities(&posture, false)?;
    eprintln!("flint-sync: checkout complete — agent may start");

    // The uniform crash rule (D2): a surviving pending sentinel is
    // honored, acked and retired BEFORE the poll arm may consume a
    // fresh one.
    if posture.enabled {
        if let Err(e) = sc.settle_pending_at_startup().await {
            if matches!(e, LeanError::Fenced(_)) {
                return Err(e);
            }
            eprintln!("flint-sync: startup settle failed (retrying at the floor): {e}");
        }
    }

    // D15's exposition, opt-in and DEGRADING. The agent container is
    // the likely occupant of any well-known port, so a collision must
    // leave the workspace fully operable — gauges.json, the heartbeat
    // echo and `flint-sync status` remain the authority for every
    // operational decision, and /metrics is additive.
    {
        let enabled = std::env::var("FLINT_SYNC_METRICS").ok().as_deref() == Some("true");
        let port: u16 = std::env::var("FLINT_SYNC_METRICS_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(9847);
        let mut posture =
            flint_lean::metrics::MetricsPosture { enabled, port, bound: false, error: None };
        if enabled {
            let labels = flint_lean::metrics::Labels {
                workspace: std::env::var("FLINT_SYNC_WORKSPACE").unwrap_or_else(|_| "unknown".into()),
                namespace: std::env::var("FLINT_SYNC_NAMESPACE").unwrap_or_else(|_| "unknown".into()),
            };
            let state_dir = sc.cfg.state_dir();
            let route = warp::get().and(warp::path("metrics")).map(move || {
                // Read the file the tick already wrote. No store, no
                // stage, no clock: a scrape costs zero bucket requests.
                let g: flint_lean::Gauges =
                    std::fs::read(state_dir.join("gauges.json"))
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok())
                        .unwrap_or_default();
                warp::reply::with_header(
                    flint_lean::metrics::render(&g, &labels),
                    "content-type",
                    "text/plain; version=0.0.4",
                )
            });
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
            match warp::serve(route).try_bind_ephemeral(addr) {
                Ok((bound, fut)) => {
                    eprintln!("flint-sync: /metrics on {bound}");
                    posture.bound = true;
                    tokio::spawn(fut);
                }
                Err(e) => {
                    eprintln!(
                        "flint-sync: /metrics NOT exposed on {addr} ({e}) — the workspace is \
                         unaffected; gauges.json and the heartbeat echo remain authoritative"
                    );
                    posture.error = Some(e.to_string());
                }
            }
        }
        if let Err(e) = sc.save_metrics_posture(&posture) {
            eprintln!("flint-sync: could not record the metrics posture: {e}");
        }
    }

    // §2.5's UDS door, opt-in. Bind failure DEGRADES: a workspace
    // whose control socket cannot be created is fully operable through
    // the file protocol, and killing the sidecar over a missing
    // convenience would be a worse outcome than not having it.
    let mut ctl_rx = if std::env::var("FLINT_SYNC_UDS_DOOR").ok().as_deref() == Some("true") {
        let path = flint_lean::uds::socket_path(&sc.cfg.state_dir());
        match flint_lean::uds::bind(&path) {
            Ok(listener) => {
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(flint_lean::uds::serve(listener, tx));
                eprintln!("flint-sync: control socket at {}", path.display());
                Some(rx)
            }
            Err(e) => {
                eprintln!("flint-sync: control socket NOT available ({e}) — the file protocol \
                           is unaffected");
                None
            }
        }
    } else {
        None
    };

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");

    // D3/D12: three INDEPENDENT, non-resettable interval timers.
    //
    // The shipped loop recreated `sleep(floor)` inside `select!` on
    // every iteration and renewed the lease only from that arm — so a
    // third arm completing every second would win every iteration,
    // perpetually reset the floor sleep, and the lease would NEVER
    // renew: the sidecar would depose itself into the straggler class
    // by construction. Independent intervals make no arm's readiness
    // able to starve another.
    let floor = Duration::from_secs(sc.cfg.floor_secs.max(1));
    // Decoupled from publish cadence entirely: at the default floor the
    // shipped renew cadence EQUALS the takeover threshold (6 quiet
    // polls × 10 s), which is already racy.
    let renew_every = Duration::from_secs(sc.cfg.floor_secs.min(30).max(1));
    let poll_every = Duration::from_secs(sc.cfg.sentinel_poll_secs.max(1));

    let mut floor_iv = tokio::time::interval(floor);
    let mut renew_iv = tokio::time::interval(renew_every);
    let mut poll_iv = tokio::time::interval(poll_every);
    for iv in [&mut floor_iv, &mut renew_iv, &mut poll_iv] {
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        iv.reset(); // consume the immediate first tick
    }

    loop {
        tokio::select! {
            _ = renew_iv.tick() => {
                // Liveness signaling, independent of publish cadence.
                // The tick settles owed acks on a fence itself — see
                // Sidecar::heartbeat_tick for why that cannot live here.
                if let Err(e) = sc.heartbeat_tick().await {
                    if matches!(e, LeanError::Fenced(_)) { return Err(e); }
                    log_retry(&sc, &e, "renew failed (retrying)");
                }
            }
            _ = floor_iv.tick() => {
                match sc.floor_tick().await {
                    Ok(o) if !o.no_change || !o.acks.is_empty() => eprintln!(
                        "flint-sync: {} seq={:?} up={} del={} consumed={} acks={}{}",
                        if sc.is_gated() { "lane" } else { "barrier" },
                        o.seq, o.uploaded, o.deleted, o.consumed, o.acks.len(),
                        // A gated tick that staged and did not cite is
                        // the mode working; say so rather than leaving
                        // it indistinguishable from a wedged loop.
                        // Structured and greppable: until Phase 6 this
                        // line is the only signal surface there is.
                        match (&o.citation_source, &o.withheld_reason) {
                            (Some(src), _) => format!(" cited={} source={src}", o.cited),
                            (None, Some(why)) => format!(" cited=0 withheld_reason={why}"),
                            (None, None) => String::new(),
                        }
                    ),
                    Ok(_) => {}
                    Err(e @ LeanError::Fenced(_)) => return Err(e),
                    Err(e) => log_retry(&sc, &e, "barrier failed (retrying next floor)"),
                }
            }
            _ = poll_iv.tick() => {
                if !posture.enabled { continue; }
                match sc.sentinel_tick().await {
                    Ok(acks) => for a in acks {
                        eprintln!(
                            "flint-sync: sentinel ack status={} boundary={} seq={:?} nonces={}",
                            a.status, a.boundary, a.seq, a.nonces.len()
                        );
                    },
                    Err(e @ LeanError::Fenced(_)) => return Err(e),
                    Err(e) => log_retry(&sc, &e, "sentinel honor failed (retrying)"),
                }
            }
            // The socket's requests are served by the ONE task that
            // holds the lease and the state directory. That is what
            // makes the door sugar rather than a second writer.
            Some(req) = async { match ctl_rx.as_mut() { Some(rx) => rx.recv().await, None => None } } => {
                match req {
                    flint_lean::uds::CtlRequest::Boundary { note, reply } => {
                        let out = async {
                            sc.request_boundary(&format!("uds:{}", flint_lean_now()), note)?;
                            sc.sentinel_tick().await
                        }
                        .await;
                        let v = match out {
                            Ok(acks) => serde_json::json!({
                                "status": "ok",
                                "acks": acks.iter().map(|a| serde_json::json!({
                                    "status": a.status, "boundary": a.boundary,
                                    "seq": a.seq, "nonces": a.nonces,
                                })).collect::<Vec<_>>(),
                            }),
                            Err(e) => serde_json::json!({
                                "status": "error", "message": e.to_string(),
                            }),
                        };
                        let _ = reply.send(v);
                    }
                    flint_lean::uds::CtlRequest::Sync { reply } => {
                        let v = match sc.sync().await {
                            Ok(r) => serde_json::to_value(&r).unwrap_or_default(),
                            Err(e) => serde_json::json!({
                                "status": "error", "message": e.to_string(),
                            }),
                        };
                        let _ = reply.send(v);
                    }
                    flint_lean::uds::CtlRequest::Status { reply } => {
                        let v = match flint_lean::status_report(&sc.cfg) {
                            Ok(r) => serde_json::to_value(&r).unwrap_or_default(),
                            Err(e) => serde_json::json!({
                                "status": "error", "message": e.to_string(),
                            }),
                        };
                        let _ = reply.send(v);
                    }
                }
            }
            _ = term.recv() => {
                eprintln!("flint-sync: SIGTERM — final drain barrier");
                // D10 rule 2: bounded retry. The shipped arm made ONE
                // attempt and released the lease even on failure, so a
                // transient store error silently forfeited everything
                // since the last boundary. Now: retried for at least
                // three attempts and for as long as the budget allows;
                // and the OUTCOME is attested rather than implied
                // (audit 2026-09-03, finding 3). On success the marker
                // is written and the lease released. On failure neither
                // happens: the cell stays unreleased so a successor waits
                // it out instead of reading a clean handoff, and the
                // absent marker is what makes the node plugin PRESERVE
                // the tree instead of removing it with the pod. A fence
                // is the same — a deposed straggler drained nothing.
                let started = std::time::Instant::now();
                let mut attempt = 0u32;
                let mut last = Ok(vec![]);
                loop {
                    attempt += 1;
                    last = sc.drain().await;
                    match &last {
                        Ok(_) => break,
                        Err(LeanError::Fenced(_)) => break,
                        Err(e) => {
                            eprintln!("flint-sync: drain attempt {attempt} failed: {e}");
                            let again = attempt < 3
                                || started.elapsed() + Duration::from_secs(2) < drain_budget;
                            if !again { break; }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
                match last {
                    Ok(acks) => {
                        let seq = sc.state.load_baseline().ok().map(|b| b.seq);
                        if let Err(e) = sc.state.write_drained(seq, acks.len()) {
                            eprintln!("flint-sync: drain published but its attestation could not be written: {e}");
                        }
                        let _ = lease::release(sc).await;
                        return Ok(());
                    }
                    Err(e @ LeanError::Fenced(_)) => return Err(e),
                    Err(e) => {
                        eprintln!(
                            "flint-sync: drain FAILED after {attempt} attempts over {}s — lease left \
                             UNRELEASED, no drain attestation written; the tree keeps everything \
                             since the last boundary",
                            started.elapsed().as_secs()
                        );
                        return Err(e);
                    }
                }
            }
        }
    }
}
