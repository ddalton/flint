//! flint-sync: the lean checkout/publish sidecar (plan of record:
//! docs/plans/flint-lean-plan.md). Runs beside an agent container as a
//! native sidecar: checkout gates the agent start; the barrier loop
//! publishes on the flush floor; preStop drains.
//!
//! Subcommands:
//!   checkout   materialize the workspace (restart-matrix aware), exit
//!   barrier    one publish barrier, exit
//!   sync       the HITL sync verb (scan-first), exit
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

use std::sync::Arc;
use std::time::Duration;

use flint_lean::lease::{self, ClaimOutcome};
use flint_lean::state::SidecarState;
use flint_lean::{LeanConfig, LeanError, Sidecar};
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
        "run" => run_loop(&mut sc).await,
        other => {
            eprintln!("flint-sync: unknown subcommand {other:?} (checkout|barrier|sync|run)");
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
        }
        Ok(())
    }
    .await;
    let _ = lease::release(sc).await;
    out
}

async fn run_loop(sc: &mut Sidecar) -> Result<(), LeanError> {
    claim(sc).await?;
    sc.checkout().await?;
    eprintln!("flint-sync: checkout complete — agent may start");

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    let floor = Duration::from_secs(sc.cfg.floor_secs.max(1));
    loop {
        tokio::select! {
            _ = tokio::time::sleep(floor) => {
                lease::renew(sc).await?;
                match sc.run_barrier().await {
                    Ok(r) if !r.no_change => eprintln!(
                        "flint-sync: barrier seq={:?} up={} del={} parked={} consumed={}",
                        r.seq, r.uploaded.len(), r.deleted.len(), r.parked.len(), r.consumed
                    ),
                    Ok(_) => {}
                    Err(e @ LeanError::Fenced(_)) => return Err(e),
                    Err(e) => eprintln!("flint-sync: barrier failed (retrying next floor): {e}"),
                }
            }
            _ = term.recv() => {
                eprintln!("flint-sync: SIGTERM — final drain barrier");
                let out = sc.run_barrier().await;
                let _ = lease::release(sc).await;
                out?;
                return Ok(());
            }
        }
    }
}
