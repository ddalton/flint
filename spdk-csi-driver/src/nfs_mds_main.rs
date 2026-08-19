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
use tracing::{error, info};

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
fn worker_threads() -> usize {
    std::env::var("FLINT_MDS_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
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
    let mut mds = spdk_csi_driver::pnfs::mds::MetadataServer::new(mds_config, exports).await?;
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


