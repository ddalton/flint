//! Flint NFS Server - Binary Entry Point
//!
//! This is a standalone NFSv4.2 server that exports a local filesystem directory
//! over NFS. Used by Flint CSI driver to provide ReadWriteMany (RWX) volumes
//! with concurrent I/O support per RFC 7862.
//!
//! Usage:
//!   flint-nfs-server --export-path /var/lib/flint/exports/vol-123 --volume-id vol-123
//!
//! The server listens on port 2049 (standard NFS port) and serves NFSv4.2 with
//! full support for concurrent reads, writes, CREATE, REMOVE, and all file operations.

use clap::Parser;
use spdk_csi_driver::nfs::{NfsConfig, NfsServer};
use spdk_csi_driver::nfs::vfs::LocalFilesystem;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber;

#[derive(Parser, Debug)]
#[command(name = "flint-nfs-server")]
#[command(about = "Flint NFSv4.2 Server - Exports SPDK volumes with concurrent I/O (RFC 7862)")]
#[command(version)]
struct Args {
    /// Path to the directory to export over NFS
    #[arg(short, long)]
    export_path: PathBuf,

    /// Volume ID (for logging and identification)
    #[arg(short, long)]
    volume_id: String,

    /// Bind address (default: 0.0.0.0 - all interfaces)
    #[arg(short, long, default_value = "0.0.0.0")]
    bind_addr: String,

    /// Port to listen on (default: 2049 - standard NFS port)
    #[arg(short, long, default_value_t = 2049)]
    port: u16,

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
    info!("║        Flint NFSv4.2 Server - RWX Volume Export          ║");
    info!("║          RFC 7862 - Concurrent I/O Support               ║");
    info!("╚═══════════════════════════════════════════════════════════╝");
    info!("");

    // Validate export path exists
    if !args.export_path.exists() {
        error!("Export path does not exist: {:?}", args.export_path);
        error!("Please create the directory before starting the server");
        return Err("Export path does not exist".into());
    }

    if !args.export_path.is_dir() {
        error!("Export path is not a directory: {:?}", args.export_path);
        return Err("Export path must be a directory".into());
    }

    // Create filesystem backend
    let fs = match LocalFilesystem::new(args.export_path.clone()) {
        Ok(fs) => {
            info!("✅ Filesystem backend initialized");
            Arc::new(fs)
        }
        Err(e) => {
            error!("Failed to initialize filesystem: {}", e);
            return Err(e.into());
        }
    };

    // Create NFS server configuration
    let config = NfsConfig {
        bind_addr: args.bind_addr.clone(),
        bind_port: args.port,
        volume_id: args.volume_id.clone(),
        export_path: args.export_path.clone(),
    };

    // Create and start NFS server
    let server = match NfsServer::new(config, fs) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create NFS server: {}", e);
            return Err(e.into());
        }
    };

    info!("");
    info!("📊 Configuration:");
    info!("   • Export Path: {:?}", args.export_path);
    info!("   • Volume ID:   {}", args.volume_id);
    info!("   • Bind Address: {}:{}", args.bind_addr, args.port);
    info!("");
    info!("🔧 Mount command (from client):");
    info!("   mount -t nfs -o vers=4.2,tcp <server-ip>:/ /mnt/point");
    info!("   OR: mount -t nfs -o vers=4.1,tcp <server-ip>:/ /mnt/point");
    info!("");
    info!("⚡ Server starting...");
    info!("");

    // Serve (blocks forever)
    if let Err(e) = server.serve().await {
        error!("Server error: {}", e);
        return Err(e.into());
    }

    Ok(())
}

