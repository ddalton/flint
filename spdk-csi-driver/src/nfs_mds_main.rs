//! Flint pNFS Metadata Server - Binary Entry Point
//!
//! This is the pNFS Metadata Server that handles all NFS control plane operations
//! and serves layout information to clients.
//!
//! Usage:
//!   flint-pnfs-mds --config /etc/flint/pnfs.yaml
//!
//! Or with environment variables:
//!   PNFS_MODE=mds flint-pnfs-mds

use clap::Parser;
use spdk_csi_driver::pnfs::{PnfsConfig, PnfsMode};
use std::path::PathBuf;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "flint-pnfs-mds")]
#[command(about = "Flint pNFS Metadata Server - NFSv4.1+ parallel NFS")]
#[command(version)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

/// Async worker count: FLINT_MDS_WORKER_THREADS, else every core on the
/// node. The old hardcoded 4 capped a 16-core node at 4-core capacity.
/// Floor for the runtime worker pool. See [`worker_threads`].
const MIN_WORKER_THREADS: usize = 8;

/// Runtime worker threads.
///
/// **Floored at [`MIN_WORKER_THREADS`], not just `available_parallelism()`.**
/// The READ path runs its blocking body with `block_in_place`, which
/// occupies a WORKER for the duration rather than a blocking-pool thread,
/// so sustaining C concurrent reads needs about C workers. Sizing the
/// pool to the CPU count under-provisions it badly, and
/// `available_parallelism()` honours the cgroup quota — a 1-CPU pod would
/// otherwise get ONE worker and serialise every read through it, which is
/// exactly the shape flint-lite runs in.
///
/// Measured on a 2-vCPU VM, 4 readers, O_DIRECT, warm, paired per rep
/// with the arm order rotating and a page-cache guard:
///
/// | workers | cpu-ms/GiB | MiB/s | vs 2 |
/// |---|---|---|---|
/// | 2 (was the default here) | 350 | 4686 | 1.000 |
/// | 4 | 325 | 5278 | 1.126 |
/// | 8 | 280 | 6169 | **1.318** |
///
/// A separate run put 16 workers 1.135x the CPU-count default and took
/// flint from 0.78 to 0.87 of knfsd. These threads BLOCK rather than
/// spin, so over-provisioning costs stacks, not CPU. The floor only
/// raises small hosts; a big one keeps its CPU count.
fn worker_threads() -> usize {
    std::env::var("FLINT_MDS_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .max(MIN_WORKER_THREADS)
        })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let threads = worker_threads();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?
        .block_on(async_main(threads))
}

async fn async_main(worker_threads: usize) -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Hot-path per-op chatter lives at debug!; RUST_LOG overrides the
    // default level (e.g. RUST_LOG=debug recovers it without a rebuild).
    // The non-blocking writer keeps a slow stdout consumer from ever
    // backpressuring dispatch; the guard must outlive the server.
    let default_level = if args.verbose { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level));
    let (writer, _log_guard) = tracing_appender::non_blocking(std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(writer)
        .init();

    info!("╔═══════════════════════════════════════════════════════════╗");
    info!("║      Flint pNFS Metadata Server (MDS)                    ║");
    info!("║      NFSv4.1+ Parallel NFS - Control Plane               ║");
    info!("╚═══════════════════════════════════════════════════════════╝");
    info!("");

    // Load configuration
    let config = if let Some(config_path) = args.config {
        info!("📄 Loading configuration from: {:?}", config_path);
        PnfsConfig::from_file(&config_path)?
    } else {
        info!("📄 Loading configuration from environment variables");
        PnfsConfig::from_env()?
    };

    // Validate mode. `mds` runs the full pNFS control plane;
    // `standalone` (flint-lite) runs the SAME server with layouts off —
    // one pod, no DS fleet, every byte through the MDS lane.
    let standalone = config.mode == PnfsMode::Standalone;
    if config.mode != PnfsMode::MetadataServer && !standalone {
        error!("❌ Configuration error: mode must be 'mds' or 'standalone'");
        error!("   Current mode: {:?}", config.mode);
        return Err("Invalid configuration mode".into());
    }

    // Validate configuration
    if let Err(e) = config.validate() {
        error!("❌ Configuration validation failed: {}", e);
        return Err(e.into());
    }

    let mut mds_config = config
        .mds
        .ok_or("standalone/mds mode requires an 'mds' section (bind, state, exports)")?;
    if standalone {
        mds_config.standalone = true;
    }
    let exports = config.exports;

    info!("📊 Configuration:");
    info!("   • Worker Threads: {} (FLINT_MDS_WORKER_THREADS)", worker_threads);
    info!("   • Bind: {}:{}", mds_config.bind.address, mds_config.bind.port);
    info!("   • Layout Type: {:?}", mds_config.layout.layout_type);
    info!("   • Stripe Size: {} bytes", mds_config.layout.stripe_size);
    info!("   • Layout Policy: {:?}", mds_config.layout.policy);
    if mds_config.standalone {
        info!("   • Posture: STANDALONE (flint-lite) — layouts off, all I/O MDS-lane");
    } else {
        info!("   • Data Servers: {}", mds_config.data_servers.len());
        for ds in &mds_config.data_servers {
            info!("     - {} @ {}", ds.device_id, ds.endpoint);
        }
    }
    info!("   • Exports: {}", exports.len());
    for export in &exports {
        info!("     - {} (fsid={})", export.path, export.fsid);
    }
    info!("   • State Backend: {:?}", mds_config.state.backend);
    if mds_config.ha.enabled {
        info!("   • HA Enabled: {} replicas", mds_config.ha.replicas);
    }
    info!("");

    // Create and start MDS
    info!("⚙️  Initializing Metadata Server...");
    let monitoring = config.monitoring.clone();
    // F33 probes the backing store through the export root, so capture
    // it before `exports` moves into the server. Multi-export MDS
    // deployments fence on the FIRST export: the failure this guards
    // against is the node's backing store going away underneath the
    // process, which takes every export with it. A hub (standalone) has
    // exactly one.
    let fence_root: Option<std::path::PathBuf> =
        exports.first().map(|e| std::path::PathBuf::from(&e.path));
    let mut mds = spdk_csi_driver::pnfs::mds::MetadataServer::new(mds_config, exports).await?;
    // F33: backing-store self-fencing, armed through the same shared
    // path `flint-nfs-server` uses. Without this the hub stays alive
    // with wedged I/O on an isolated node while every hard mount hangs
    // — the 93-minute orphan recorded in `nfs::fence`'s module docs —
    // because process exit is what lets clients RST and fail over.
    // Exit code 59 (vs 58 in nfs_main) identifies which front-end fenced.
    match &fence_root {
        Some(root) => spdk_csi_driver::nfs::fence::arm_from_env(root, 59),
        None => warn!("F33 self-fencing NOT armed: no exports configured"),
    }
    // Finding 7 — see the matching call in `nfs_main.rs`. Both
    // front-ends report, because the whole point of §1.1 is that a
    // mechanism present on one and absent from the other is invisible
    // until it costs something.
    if let Some(root) = &fence_root {
        spdk_csi_driver::nfs::privilege::report_at_startup(root);
    }
    // The status surface is off unless the deployment asks for it. It
    // binds before the tier starts, so the epoch-claim and import
    // phases — the long, pre-listener part of startup — are visible to
    // whatever is waiting for this hub to come up.
    if monitoring.health.enabled {
        info!(
            "   • Status endpoint: :{}{} (+ /status)",
            monitoring.health.port, monitoring.health.path
        );
    }
    mds.set_monitoring(monitoring);

    info!("🚀 Starting Metadata Server...");
    info!("");
    
    // Serve until told to stop. SIGTERM is the normal way a hub ends:
    // a lifecycle suspend, a rolling update, a node drain, a spot
    // reclaim. Without a handler the process is SIGKILLed at the grace
    // deadline with its last writes unflushed and its epoch cell still
    // claimed — the next hub then waits out a lease nobody holds, and
    // anything dirtied since the last barrier exists only on the PVC.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = mds.serve() => {
            if let Err(e) = r {
                error!("❌ Server error: {}", e);
                return Err(e.into());
            }
        }
        _ = sigterm.recv() => {
            info!("🛑 SIGTERM — draining and flushing before exit");
            mds.graceful_shutdown().await;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("🛑 SIGINT — draining and flushing before exit");
            mds.graceful_shutdown().await;
        }
    }

    Ok(())
}


