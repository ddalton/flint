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

    // Validate mode
    if config.mode != PnfsMode::MetadataServer {
        error!("❌ Configuration error: mode must be 'mds' for metadata server");
        error!("   Current mode: {:?}", config.mode);
        return Err("Invalid configuration mode".into());
    }

    // Validate configuration
    if let Err(e) = config.validate() {
        error!("❌ Configuration validation failed: {}", e);
        return Err(e.into());
    }

    let mds_config = config.mds.expect("MDS configuration is required");
    let exports = config.exports;

    info!("📊 Configuration:");
    info!("   • Bind: {}:{}", mds_config.bind.address, mds_config.bind.port);
    info!("   • Layout Type: {:?}", mds_config.layout.layout_type);
    info!("   • Stripe Size: {} bytes", mds_config.layout.stripe_size);
    info!("   • Layout Policy: {:?}", mds_config.layout.policy);
    info!("   • Data Servers: {}", mds_config.data_servers.len());
    for ds in &mds_config.data_servers {
        info!("     - {} @ {}", ds.device_id, ds.endpoint);
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
    let mds = spdk_csi_driver::pnfs::mds::MetadataServer::new(mds_config, exports).await?;

    info!("🚀 Starting Metadata Server...");
    info!("");
    
    // Serve (blocks forever)
    if let Err(e) = mds.serve().await {
        error!("❌ Server error: {}", e);
        return Err(e.into());
    }

    Ok(())
}


