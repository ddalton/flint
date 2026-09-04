//! flint-forge-syncer: the one process that owns a repository's writes
//! (design of record: `docs/plans/flint-forge-design.md` §4).
//!
//! It is the pod's MAIN container: its exit restarts the pod, which is
//! what makes a fence recoverable — a deposed server stops answering
//! fetches, restarts, and restores from the snapshot rather than
//! serving refs it can no longer prove.
//!
//! Environment:
//!   FLINT_FORGE_BUCKET     (required) bucket name
//!   FLINT_FORGE_PREFIX     (required) this repository's key prefix
//!   FLINT_FORGE_REPO       (required) the bare repository on local disk
//!   FLINT_FORGE_ENDPOINT   S3 endpoint override (MinIO/proxy rigs)
//!   FLINT_FORGE_PROJECT_ID refuse a prefix another project claims
//!   FLINT_FORGE_SOCKET     hook socket (default <repo>/flint-forge/syncer.sock)
//!   FLINT_FORGE_STATUS_ADDR   status listener (default 127.0.0.1:9848)
//!   FLINT_FORGE_HEARTBEAT_SECS   lease renewal period (default 10)
//!   FLINT_FORGE_BATCH_WINDOW_MS  how long a batch stays open (default 400)
//!   FLINT_FORGE_BATCH_MAX        pushes per batch (default 64)
//!   FLINT_FORGE_REPACK_THRESHOLD packs before a repack (default 24)
//!   FLINT_FORGE_ORPHAN_GRACE_SECS  sweep grace (default 3600)
//!   FLINT_FORGE_DEFAULT_BRANCH   HEAD for an empty repository (main)
//!   FLINT_FORGE_PROTECTED        comma-separated globs, no direct push
//!   FLINT_FORGE_ALLOW_NON_FF     comma-separated globs, force allowed

use std::path::PathBuf;
use std::sync::Arc;

use flint_forge::policy::Policy;
use flint_forge::server::{self, ServerOpts};
use flint_forge::{ForgeConfig, ForgeError, Syncer, EXIT_REFUSED};
use flint_store::s3::S3Store;
use flint_store::ObjectStore;

fn env_req(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("flint-forge-syncer: {name} is required");
        std::process::exit(2);
    })
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_globs(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|v| {
            v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        })
        .unwrap_or_default()
}

fn rendered_policy(cfg: &ForgeConfig) -> Option<Policy> {
    match Policy::load(&cfg.state_dir) {
        Ok(p) => p,
        Err(e) => {
            // A policy the enforcers cannot read must never read as
            // "no policy": that is how a rendering bug becomes an open
            // repository.
            eprintln!("flint-forge-syncer: {e}");
            std::process::exit(EXIT_REFUSED);
        }
    }
}

#[tokio::main]
async fn main() {
    let bucket = env_req("FLINT_FORGE_BUCKET");
    let prefix = env_req("FLINT_FORGE_PREFIX");
    let repo = PathBuf::from(env_req("FLINT_FORGE_REPO"));
    let endpoint = std::env::var("FLINT_FORGE_ENDPOINT").ok();

    let store = match S3Store::connect(bucket, endpoint).await {
        Ok(s) => Arc::new(s) as Arc<dyn ObjectStore>,
        Err(e) => {
            eprintln!("flint-forge-syncer: store connect: {e}");
            std::process::exit(1);
        }
    };

    let mut cfg = ForgeConfig::new(&prefix, &repo);
    cfg.heartbeat_secs = env_u64("FLINT_FORGE_HEARTBEAT_SECS", 10).max(1);
    cfg.batch_window_ms = env_u64("FLINT_FORGE_BATCH_WINDOW_MS", 400);
    cfg.batch_max = env_u64("FLINT_FORGE_BATCH_MAX", 64).max(1) as usize;
    cfg.repack_threshold = env_u64("FLINT_FORGE_REPACK_THRESHOLD", 24) as usize;
    cfg.orphan_grace_secs = env_u64("FLINT_FORGE_ORPHAN_GRACE_SECS", 3600);
    cfg.project_id = std::env::var("FLINT_FORGE_PROJECT_ID").ok().filter(|p| !p.is_empty());
    cfg.default_branch =
        std::env::var("FLINT_FORGE_DEFAULT_BRANCH").unwrap_or_else(|_| "main".into());

    let socket = std::env::var("FLINT_FORGE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| cfg.state_dir.join(flint_forge::uds::SOCKET_NAME));
    let status_addr = std::env::var("FLINT_FORGE_STATUS_ADDR")
        .ok()
        .filter(|a| !a.is_empty() && a != "off")
        .or_else(|| Some("127.0.0.1:9848".to_string()));

    let opts = ServerOpts {
        socket,
        status_addr,
        // The rendered document beside the repository is the operator's
        // surface; the env knobs are the pre-operator posture and the
        // rigs'. A file, when present, wins outright rather than
        // merging — a policy assembled from two sources is a policy
        // nobody can read off one screen.
        policy: rendered_policy(&cfg).unwrap_or(Policy {
            protected: env_globs("FLINT_FORGE_PROTECTED"),
            allow_non_fast_forward: env_globs("FLINT_FORGE_ALLOW_NON_FF"),
            ..Policy::default()
        }),
    };

    let holder_id = format!("forge-{}", uuid::Uuid::new_v4());
    let sc = Syncer::new(store, cfg, holder_id);

    match server::run(sc, opts).await {
        Ok(()) => {}
        // A refusal is final: the delivery must tear this repository
        // down and name the reason rather than restart into the same
        // wall, which a tenant reads as "starting" for as long as it
        // lives (lean's leg S22).
        Err(e @ ForgeError::Refused(_)) => {
            eprintln!("flint-forge-syncer: {e}");
            std::process::exit(EXIT_REFUSED);
        }
        // A fence IS restartable, and restarting is the right answer:
        // the successor restores from the snapshot it was deposed by.
        Err(e) => {
            eprintln!("flint-forge-syncer: {e}");
            std::process::exit(1);
        }
    }
}
