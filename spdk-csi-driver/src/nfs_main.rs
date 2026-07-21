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
use std::path::PathBuf;
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
    #[arg(long)]
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

    /// Export as read-only (for ROX volumes)
    #[arg(short, long)]
    read_only: bool,
}

/// Async worker count: FLINT_NFS_WORKER_THREADS, else every core on the
/// node. The old hardcoded 4 capped a 16-core node at 4-core capacity.
fn worker_threads() -> usize {
    std::env::var("FLINT_NFS_WORKER_THREADS")
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
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
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
    if args.read_only {
        info!("║        Flint NFSv4.2 Server - ROX Volume Export          ║");
        info!("║          RFC 7862 - Read-Only Multi-Pod Access           ║");
    } else {
        info!("║        Flint NFSv4.2 Server - RWX Volume Export          ║");
        info!("║          RFC 7862 - Concurrent I/O Support               ║");
    }
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

    // NFSv4.2 uses direct filesystem access via file handle manager
    // No separate filesystem backend needed

    // F30: the export must PROVE it is the configured volume before one
    // byte is served. The incident: a bare mountpoint dir got exported
    // and the server silently minted fresh identity over emptiness.
    match spdk_csi_driver::nfs::volume_marker::verify_and_adopt(&args.export_path, &args.volume_id)
    {
        Ok(spdk_csi_driver::nfs::volume_marker::MarkerVerdict::Serve) => {}
        Ok(spdk_csi_driver::nfs::volume_marker::MarkerVerdict::AdoptLegacy) => {
            info!("F30: legacy volume adopted — identity marker stamped for {}", args.volume_id);
        }
        Ok(spdk_csi_driver::nfs::volume_marker::MarkerVerdict::RefuseMismatch { found }) => {
            error!(
                "F30 REFUSAL: export carries volume-id {:?} but this server is configured \
                 for {:?} — the wrong volume is mounted at {:?}; refusing to serve",
                found, args.volume_id, args.export_path
            );
            std::process::exit(57);
        }
        Ok(spdk_csi_driver::nfs::volume_marker::MarkerVerdict::RefuseEmpty) => {
            error!(
                "F30 REFUSAL: export {:?} has neither an identity marker nor flint state — \
                 this is an EMPTY/foreign directory, not volume {} (blind export is how the \
                 fresh-fh.key incident happened); refusing to serve",
                args.export_path, args.volume_id
            );
            std::process::exit(57);
        }
        Err(e) => {
            error!("F30: identity marker check failed on {:?}: {}", args.export_path, e);
            std::process::exit(57);
        }
    }

    // Create NFS server configuration
    let config = NfsConfig {
        bind_addr: args.bind_addr.clone(),
        bind_port: args.port,
        volume_id: args.volume_id.clone(),
        export_path: args.export_path.clone(),
        read_only: args.read_only,
    };

    // Create and start NFS server
    let server = match NfsServer::new(config).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create NFS server: {}", e);
            return Err(e.into());
        }
    };

    // F33: backing-store self-fencing. When this node is isolated (or
    // the backing leg is fenced by a resurrect elsewhere), this process
    // used to stay alive with wedged I/O while clients hung on their
    // established TCP flows — process exit is what lets them RST and
    // fail over. Deadline is generous (default 90s; env
    // FLINT_FENCE_DEADLINE_SECS, 0 disables) so a slow-but-live store
    // under load never trips it.
    if let Some(deadline) = spdk_csi_driver::nfs::fence::deadline_from_env() {
        let interval = std::cmp::min(deadline / 6, std::time::Duration::from_secs(10));
        spdk_csi_driver::nfs::fence::spawn_with_probe(
            spdk_csi_driver::nfs::fence::heartbeat_probe(&args.export_path),
            deadline,
            interval,
            |_stale| std::process::exit(58),
        );
    } else {
        info!("F33 self-fencing DISABLED (FLINT_FENCE_DEADLINE_SECS=0)");
    }

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

    // Serve until SIGTERM/SIGINT. Exiting promptly on SIGTERM is
    // load-bearing for data durability, not just hygiene: if the process
    // rides out kubelet's grace period holding the export mounted, the
    // following NodeUnstage's plain umount hits EBUSY and falls back to a
    // LAZY umount — after which the raid is torn down underneath the
    // still-flushing filesystem and every unflushed page (server-side
    // files, the NFSv4 state DB) is lost. A prompt exit lets the unstage
    // unmount cleanly, which flushes everything (observed live,
    // 2026-06-12: 30 s SIGKILL → lazy umount → empty state DB after the
    // bounce).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = server.serve() => {
            if let Err(e) = r {
                error!("Server error: {}", e);
                return Err(e.into());
            }
        }
        _ = sigterm.recv() => {
            info!("SIGTERM — shutting down (open TCP connections dropped; clients recover via persisted state)");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT — shutting down");
        }
    }

    Ok(())
}

