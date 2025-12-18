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
use tracing::{error, info};

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

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Initialize logging
    let log_level = if args.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
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
    info!("   • Device ID: {}", ds_config.device_id);
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


