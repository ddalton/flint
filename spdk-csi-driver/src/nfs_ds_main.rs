//! Flint pNFS Data Server - Binary Entry Point
//!
//! This is the pNFS Data Server that handles only data I/O operations
//! (READ, WRITE, COMMIT) for high-throughput parallel access.
//!
//! Usage:
//!   flint-pnfs-ds --config /etc/flint/pnfs.yaml
//!
//! Or with environment variables:
//!   PNFS_MODE=ds flint-pnfs-ds

use clap::Parser;
use spdk_csi_driver::pnfs::{PnfsConfig, PnfsMode};
use std::path::PathBuf;
use tracing::{debug, error, info};

#[derive(Parser, Debug)]
#[command(name = "flint-pnfs-ds")]
#[command(about = "Flint pNFS Data Server - NFSv4.1+ parallel NFS data plane")]
#[command(version)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

/// Async worker count: FLINT_DS_WORKER_THREADS, else every core on the
/// node. The old hardcoded 4 capped a 16-core node at 4-core capacity.
fn worker_threads() -> usize {
    std::env::var("FLINT_DS_WORKER_THREADS")
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
    info!("║      Flint pNFS Data Server (DS)                         ║");
    info!("║      NFSv4.1+ Parallel NFS - Data Plane                  ║");
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

    // Validate mode
    if config.mode != PnfsMode::DataServer {
        error!("❌ Configuration error: mode must be 'ds' for data server");
        error!("   Current mode: {:?}", config.mode);
        return Err("Invalid configuration mode".into());
    }

    // Validate configuration
    if let Err(e) = config.validate() {
        error!("❌ Configuration validation failed: {}", e);
        return Err(e.into());
    }

    let ds_config = config.ds.expect("DS configuration is required");

    info!("📊 Configuration:");
    info!("   • Worker Threads: {} (FLINT_DS_WORKER_THREADS)", worker_threads);
    debug!("   • NODE_NAME env var: {:?}", std::env::var("NODE_NAME"));
    info!("   • Device ID: {} (after env var expansion)", ds_config.device_id);
    info!("   • Bind: {}:{}", ds_config.bind.address, ds_config.bind.port);
    info!("   • MDS Endpoint: {}", ds_config.mds.endpoint);
    info!("   • Heartbeat Interval: {} seconds", ds_config.mds.heartbeat_interval);
    info!("   • Block Devices: {}", ds_config.bdevs.len());
    for bdev in &ds_config.bdevs {
        info!("     - {} mounted at {}", bdev.name, bdev.mount_point);
    }
    info!("   • Max Connections: {}", ds_config.resources.max_connections);
    info!("   • I/O Queue Depth: {}", ds_config.resources.io_queue_depth);
    info!("");

    // Create and start DS
    info!("⚙️  Initializing Data Server...");
    let ds = spdk_csi_driver::pnfs::ds::DataServer::new(ds_config)?;

    info!("🚀 Starting Data Server...");
    info!("");
    
    // Serve (blocks forever)
    if let Err(e) = ds.serve().await {
        error!("❌ Server error: {}", e);
        return Err(e.into());
    }

    Ok(())
}


