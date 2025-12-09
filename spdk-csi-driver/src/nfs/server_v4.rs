//! NFSv4.2 Server - TCP Transport
//!
//! Handles network I/O for the NFSv4.2 server.
//! Listens on TCP port, receives RPC COMPOUND calls, dispatches to NFSv4.2 handlers,
//! and sends replies.

use super::rpc::{CallMessage, ReplyBuilder};
use super::v4::{CompoundDispatcher, CompoundRequest};
use super::v4::filehandle::FileHandleManager;
use super::v4::operations::lockops::LockManager;
use super::v4::protocol::{NFS4_PROGRAM, procedure};
use super::v4::state::StateManager;
// LocalFilesystem removed - NFSv4 uses direct filesystem access via filehandle manager
use super::xdr::{XdrDecoder, XdrEncoder};
use bytes::{Bytes, BytesMut};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

/// NFS server configuration
#[derive(Debug, Clone)]
pub struct NfsConfig {
    /// Bind address (e.g., "0.0.0.0" or "127.0.0.1")
    pub bind_addr: String,

    /// Bind port (default: 2049)
    pub bind_port: u16,

    /// Volume ID being exported
    pub volume_id: String,

    /// Export path (directory to serve)
    pub export_path: PathBuf,
}

impl Default for NfsConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".to_string(),
            bind_port: 2049,
            volume_id: String::new(),
            export_path: PathBuf::new(),
        }
    }
}

/// NFSv4.2 Server
pub struct NfsServer {
    config: NfsConfig,
    dispatcher: Arc<CompoundDispatcher>,
}

impl NfsServer {
    /// Create a new NFSv4.2 server
    pub fn new(config: NfsConfig) -> std::io::Result<Self> {
        // Initialize NFSv4.2 components
        let fh_mgr = Arc::new(FileHandleManager::new(config.export_path.clone()));
        let state_mgr = Arc::new(StateManager::new());
        let lock_mgr = Arc::new(LockManager::new());

        // Create COMPOUND dispatcher (creates handlers internally)
        let dispatcher = Arc::new(CompoundDispatcher::new(
            fh_mgr,
            state_mgr,
            lock_mgr,
        ));

        Ok(Self { config, dispatcher })
    }

    /// Start the NFSv4.2 server (TCP only - NFSv4 doesn't use UDP)
    pub async fn serve(&self) -> std::io::Result<()> {
        let addr = format!("{}:{}", self.config.bind_addr, self.config.bind_port);

        info!("🚀 Starting NFSv4.2 server on {}", addr);
        info!("📂 Exporting: {:?}", self.config.export_path);
        info!("💾 Volume ID: {}", self.config.volume_id);
        info!("");
        info!("🔧 Mount command (from client):");
        info!("   mount -t nfs -o vers=4.2,tcp <server-ip>:/ /mnt/point");
        info!("");

        // NFSv4 doesn't need portmapper registration (uses well-known port 2049)
        // and doesn't need separate MOUNT protocol

        // Start TCP server
        serve_tcp(&addr, self.dispatcher.clone()).await
    }
}

/// Serve NFSv4.2 over TCP
async fn serve_tcp(addr: &str, dispatcher: Arc<CompoundDispatcher>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("✅ NFSv4.2 TCP server listening on {}", addr);
    info!("");

    loop {
        let (stream, peer) = listener.accept().await?;
        info!("📡 New TCP connection from {}", peer);

        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_connection(stream, dispatcher, peer).await {
                warn!("TCP connection from {} error: {}", peer, e);
            } else {
                info!("✓ TCP connection from {} closed cleanly", peer);
            }
        });
    }
}

/// Handle a TCP connection
async fn handle_tcp_connection(
    stream: TcpStream,
    dispatcher: Arc<CompoundDispatcher>,
    peer: std::net::SocketAddr
) -> std::io::Result<()> {
    use tokio::io::BufWriter;

    // Set TCP_NODELAY for low latency
    stream.set_nodelay(true)?;

    // Split stream for independent reading and buffered writing
    let (reader, writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::with_capacity(128 * 1024, reader);
    let mut writer = BufWriter::with_capacity(128 * 1024, writer);

    // Reusable buffer
    let mut buf = BytesMut::with_capacity(128 * 1024);

    loop {
        // Read RPC record marker (4 bytes)
        let mut marker_buf = [0u8; 4];
        match reader.read_exact(&mut marker_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Connection closed gracefully
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        let marker = u32::from_be_bytes(marker_buf);
        let _is_last = (marker & 0x80000000) != 0;
        let length = (marker & 0x7FFFFFFF) as usize;

        // Prevent oversized allocations
        if length > 4 * 1024 * 1024 {
            warn!("Rejecting oversized RPC message: {} bytes", length);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RPC message too large",
            ));
        }

        // Read message
        buf.clear();
        buf.reserve(length);
        unsafe { buf.set_len(length); }
        reader.read_exact(&mut buf[..length]).await?;

        let request = buf.split().freeze();

        // Process the RPC call
        debug!(">>> Processing NFSv4 request from {}, length={} bytes", peer, length);
        let reply = dispatch_nfsv4(request, dispatcher.clone()).await;
        debug!("<<< Reply ready for {}, length={} bytes", peer, reply.len());

        // Write reply with record marker
        let reply_len = reply.len() as u32;
        let reply_marker = 0x80000000 | reply_len;
        writer.write_all(&reply_marker.to_be_bytes()).await?;
        writer.write_all(&reply).await?;
        writer.flush().await?;
    }
}

/// Dispatch an NFSv4 RPC call
async fn dispatch_nfsv4(request: Bytes, dispatcher: Arc<CompoundDispatcher>) -> Bytes {
    // Parse RPC call message
    let call = match CallMessage::decode(request.clone()) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to parse RPC call: {}", e);
            return ReplyBuilder::garbage_args(0).into();
        }
    };

    info!(
        ">>> RPC CALL: xid={}, program={}, version={}, procedure={}",
        call.xid, call.program, call.version, call.procedure
    );

    // Check program number
    if call.program != NFS4_PROGRAM {
        warn!("Invalid program number: {} (expected {})", call.program, NFS4_PROGRAM);
        return ReplyBuilder::prog_unavail(call.xid);
    }

    // Check version (4.0, 4.1, or 4.2)
    if call.version != 4 {
        warn!("Invalid NFSv4 version: {} (expected 4)", call.version);
        // NFSv4 doesn't have prog_mismatch, return proc_unavail
        return ReplyBuilder::proc_unavail(call.xid);
    }

    // Handle procedure
    match call.procedure {
        procedure::NULL => {
            // NULL procedure - just return success (empty result)
            info!(">>> NULL procedure");
            ReplyBuilder::success(call.xid).finish()
        }

        procedure::COMPOUND => {
            // COMPOUND procedure - dispatch to NFSv4.2 handler
            info!(">>> COMPOUND procedure");
            handle_compound(call, request, dispatcher).await
        }

        _ => {
            warn!("Invalid NFSv4 procedure: {}", call.procedure);
            ReplyBuilder::proc_unavail(call.xid)
        }
    }
}

/// Handle NFSv4 COMPOUND request
async fn handle_compound(
    call: CallMessage,
    request: Bytes,
    dispatcher: Arc<CompoundDispatcher>,
) -> Bytes {
    // The request Bytes contains the full RPC call including header.
    // We need to skip the RPC header to get to COMPOUND procedure arguments.
    // CallMessage::decode already parsed the header, but we need to know where it ends.

    // For now, let's use a simple approach: parse the call again to find where args start
    // This is not optimal but works. TODO: Make CallMessage return the args offset.

    // Create a decoder from the request
    let decoder = XdrDecoder::new(request);

    // Decode COMPOUND request (this handles skipping the RPC header internally)
    let compound_req = match CompoundRequest::decode(decoder) {
        Ok(req) => req,
        Err(e) => {
            warn!("Failed to decode COMPOUND request: {}", e);
            return ReplyBuilder::garbage_args(call.xid);
        }
    };

    debug!("COMPOUND: tag={}, minor_version={}, {} operations",
           compound_req.tag,
           compound_req.minor_version,
           compound_req.operations.len());

    // Dispatch to COMPOUND handler
    let compound_resp = dispatcher.dispatch_compound(compound_req).await;

    debug!("COMPOUND result: status={:?}, {} results",
           compound_resp.status,
           compound_resp.results.len());

    // Encode COMPOUND response
    let compound_data = compound_resp.encode();

    // Build RPC SUCCESS reply with compound data
    // The compound response is the procedure-specific result data
    let mut encoder = XdrEncoder::new();

    // RPC Reply header
    encoder.encode_u32(call.xid);  // XID
    encoder.encode_u32(1);  // Message type: REPLY
    encoder.encode_u32(0);  // Reply status: MSG_ACCEPTED

    // Auth (null)
    encoder.encode_u32(0);  // Auth flavor: AUTH_NONE
    encoder.encode_u32(0);  // Auth length: 0

    // Accept status: SUCCESS
    encoder.encode_u32(0);  // AcceptStatus::Success

    // Procedure-specific result (COMPOUND response)
    encoder.encode_opaque(&compound_data);

    encoder.finish()
}
