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
//!   FLINT_SYNC_BOUNDARY_MODE      cadence|hybrid|gated (default hybrid)
//!   FLINT_SYNC_SENTINELS          auto|off|force (default auto)
//!   FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS  (default 5)
//!   FLINT_SYNC_SENTINEL_HOURLY_BUDGET      work units/hour (default 60)
//!   FLINT_SYNC_SENTINEL_POLL_SECS          (default 1; env-only)

use std::sync::Arc;
use std::time::Duration;

use flint_lean::lease::{self, ClaimOutcome};
use flint_lean::state::SidecarState;
use flint_lean::{BoundaryMode, LeanConfig, LeanError, Sidecar, SentinelMode};
use flint_store::s3::S3Store;
use flint_store::ObjectStore;

fn env_req(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("flint-sync: {name} is required");
        std::process::exit(2);
    })
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
    cfg.fanout = env_u64("FLINT_SYNC_FANOUT", 16).max(1) as usize;
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
    let mut sc = Sidecar { store, cfg, state, lease: None };

    let result = match cmd.as_str() {
        "checkout" => claim_then(&mut sc, Step::Checkout).await,
        "barrier" => claim_then(&mut sc, Step::Barrier).await,
        "sync" => claim_then(&mut sc, Step::Sync).await,
        "recover-staged" => claim_then(&mut sc, Step::RecoverStaged).await,
        "run" => run_loop(&mut sc).await,
        other => {
            eprintln!(
                "flint-sync: unknown subcommand {other:?} \
                 (checkout|barrier|sync|status|recover-staged|run)"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("flint-sync: {e}");
        // A fence is a clean shutdown order, not a crash loop.
        std::process::exit(if matches!(e, LeanError::Fenced(_)) { 0 } else { 1 });
    }
}

enum Step {
    Checkout,
    Barrier,
    Sync,
    RecoverStaged,
}

async fn claim(sc: &mut Sidecar) -> Result<(), LeanError> {
    loop {
        match lease::claim_step(sc).await? {
            ClaimOutcome::Claimed(lease) => {
                eprintln!("flint-sync: holding epoch {}", lease.epoch);
                return Ok(());
            }
            ClaimOutcome::Waiting { quiet_polls } => {
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
                if let Err(e) = lease::renew(sc).await {
                    if matches!(e, LeanError::Fenced(_)) { return Err(e); }
                    eprintln!("flint-sync: renew failed (retrying): {e}");
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
                    Err(e) => eprintln!("flint-sync: barrier failed (retrying next floor): {e}"),
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
                    Err(e) => eprintln!("flint-sync: sentinel honor failed (retrying): {e}"),
                }
            }
            _ = term.recv() => {
                eprintln!("flint-sync: SIGTERM — final drain barrier");
                // D10 rule 2: bounded retry. The shipped arm made ONE
                // attempt and released the lease even on failure, so a
                // transient store error silently forfeited everything
                // since the last boundary.
                let mut last = Ok(vec![]);
                for attempt in 0..3u32 {
                    last = sc.drain().await;
                    match &last {
                        Ok(_) => break,
                        Err(LeanError::Fenced(_)) => break,
                        Err(e) => {
                            eprintln!("flint-sync: drain attempt {} failed: {e}", attempt + 1);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
                let _ = lease::release(sc).await;
                last?;
                return Ok(());
            }
        }
    }
}
