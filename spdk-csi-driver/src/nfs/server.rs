//! NFS Server - TCP/UDP Transport
//!
//! Handles network I/O for the NFSv3 server.
//! Listens on TCP and UDP ports, receives RPC calls, dispatches to handlers,
//! and sends replies.

use super::handlers;
use super::nlm::{NlmService, NLM_PROGRAM, NLM_VERSION};
use super::portmap;
use super::protocol::Procedure;
use super::rpc::{CallMessage, ReplyBuilder, NFS_PROGRAM, NFS_VERSION, MOUNT_PROGRAM, MOUNT_VERSION};
use super::setattr;
use super::vfs::LocalFilesystem;
use super::xdr::XdrDecoder;
use bytes::{Bytes, BytesMut};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
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

/// NFS Server
pub struct NfsServer {
    config: NfsConfig,
    fs: Arc<LocalFilesystem>,
    nlm: Arc<NlmService>,
}

impl NfsServer {
    /// Create a new NFS server
    pub fn new(config: NfsConfig, fs: Arc<LocalFilesystem>) -> std::io::Result<Self> {
        let nlm = Arc::new(NlmService::new());
        Ok(Self { config, fs, nlm })
    }
    
    /// Start the NFS server
    /// Listens on both TCP and UDP
    pub async fn serve(&self) -> std::io::Result<()> {
        let addr = format!("{}:{}", self.config.bind_addr, self.config.bind_port);
        
        info!("🚀 Starting NFS server on {}", addr);
        info!("📂 Exporting: {:?}", self.config.export_path);
        info!("💾 Volume ID: {}", self.config.volume_id);
        
        // Register with portmapper
        portmap::register_with_portmapper(self.config.bind_port).await?;
        
        // Start TCP and UDP servers concurrently
        let tcp_handle = {
            let fs = self.fs.clone();
            let nlm = self.nlm.clone();
            let addr = addr.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_tcp(&addr, fs, nlm).await {
                    error!("TCP server error: {}", e);
                }
            })
        };

        let udp_handle = {
            let fs = self.fs.clone();
            let nlm = self.nlm.clone();
            let addr = addr.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_udp(&addr, fs, nlm).await {
                    error!("UDP server error: {}", e);
                }
            })
        };
        
        // Wait for both servers
        let _ = tokio::join!(tcp_handle, udp_handle);
        
        Ok(())
    }
}

/// Serve NFS over TCP
async fn serve_tcp(addr: &str, fs: Arc<LocalFilesystem>, nlm: Arc<NlmService>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("NFS TCP server listening on {}", addr);
    
    loop {
        let (stream, peer) = listener.accept().await?;
        info!("📡 New TCP connection from {}", peer);
        
        let fs = fs.clone();
        let nlm = nlm.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_connection(stream, fs, nlm, peer).await {
                warn!("TCP connection from {} error: {}", peer, e);
            } else {
                info!("✓ TCP connection from {} closed cleanly", peer);
            }
        });
    }
}

/// Handle a TCP connection
///
/// Performance optimizations:
/// - Uses BufWriter to batch writes and reduce syscalls
/// - Uses BytesMut for zero-copy buffer reuse
/// - Avoids unnecessary flushes (let BufWriter and OS handle batching)
async fn handle_tcp_connection(stream: TcpStream, fs: Arc<LocalFilesystem>, nlm: Arc<NlmService>, peer: std::net::SocketAddr) -> std::io::Result<()> {
    use bytes::BytesMut;
    use tokio::io::BufWriter;
    
    // Set TCP_NODELAY to reduce latency for small messages
    // This is important for NFS since many operations are small
    stream.set_nodelay(true)?;
    
    // Split stream for independent reading and buffered writing
    let (reader, writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::with_capacity(128 * 1024, reader);
    let mut writer = BufWriter::with_capacity(128 * 1024, writer);
    
    // Reusable buffer for requests - avoids allocations
    let mut buf = BytesMut::with_capacity(128 * 1024);
    
    loop {
        // Read RPC record marker (4 bytes: 1 bit last-fragment + 31 bits length)
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
        
        // Prevent oversized allocations (DoS protection)
        if length > 4 * 1024 * 1024 {
            warn!("Rejecting oversized RPC message: {} bytes", length);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RPC message too large",
            ));
        }
        
        // Ensure capacity and read message
        buf.clear();
        buf.reserve(length);
        unsafe { buf.set_len(length); }
        reader.read_exact(&mut buf[..length]).await?;
        
        // Zero-copy: split off the request bytes
        let request = buf.split().freeze();
        
        // Process the RPC call (async, may take time)
        debug!(">>> Processing request from {}, length={} bytes", peer, length);
        let reply = dispatch(request, fs.clone(), nlm.clone()).await;
        debug!("<<< Reply ready for {}, length={} bytes", peer, reply.len());
        
        // Write reply with record marker
        let reply_len = reply.len() as u32;
        let reply_marker = 0x80000000 | reply_len; // Last fragment + length
        writer.write_all(&reply_marker.to_be_bytes()).await?;
        writer.write_all(&reply).await?;
        
        // Flush to ensure client receives the reply
        // Note: This reduces throughput slightly but is necessary for correctness.
        // Without flush, BufWriter batches replies and clients timeout waiting.
        // Future optimization: Selective flushing for critical operations only.
        writer.flush().await?;
    }
}

/// Serve NFS over UDP
async fn serve_udp(addr: &str, fs: Arc<LocalFilesystem>, nlm: Arc<NlmService>) -> std::io::Result<()> {
    let socket = Arc::new(UdpSocket::bind(addr).await?);
    info!("NFS UDP server listening on {}", addr);

    // Use BytesMut for zero-copy split (same pattern as TCP path)
    let mut buf = BytesMut::with_capacity(65536);

    loop {
        // Prepare buffer for receive
        buf.clear();
        buf.reserve(65536);
        unsafe { buf.set_len(65536); }

        let (len, peer) = socket.recv_from(&mut buf[..]).await?;
        debug!("UDP request from {}, {} bytes", peer, len);

        // Zero-copy split (same as TCP)
        buf.truncate(len);
        let request = buf.split_to(len).freeze();
        let fs = fs.clone();
        let nlm = nlm.clone();
        let socket = socket.clone();

        // Handle request in separate task
        tokio::spawn(async move {
            let reply = dispatch(request, fs, nlm).await;
            
            if let Err(e) = socket.send_to(&reply, peer).await {
                warn!("Failed to send UDP reply: {}", e);
            }
        });
    }
}

/// Dispatch an RPC call to the appropriate handler
async fn dispatch(request: Bytes, fs: Arc<LocalFilesystem>, nlm: Arc<NlmService>) -> Bytes {
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
    
    // Add program name for better debugging
    let prog_name = match call.program {
        100003 => "NFS",
        100005 => "MOUNT",
        100021 => "NLM", // Network Lock Manager
        _ => "UNKNOWN",
    };
    
    if prog_name == "UNKNOWN" || prog_name == "NLM" {
        debug!("RPC call for program {} ({})", prog_name, call.program);
    }
    
    // Check program and version
    if call.program == NFS_PROGRAM && call.version == NFS_VERSION {
        info!(">>> Dispatching to NFS handler");
        let result = dispatch_nfs(call, request, fs).await;
        info!("<<< NFS handler returned");
        return result;
    }

    if call.program == MOUNT_PROGRAM && call.version == MOUNT_VERSION {
        info!(">>> Dispatching to MOUNT handler");
        let result = dispatch_mount(call, request, fs).await;
        info!("<<< MOUNT handler returned");
        return result;
    }

    if call.program == NLM_PROGRAM && call.version == NLM_VERSION {
        info!(">>> Dispatching to NLM handler");
        let result = dispatch_nlm(call, request, nlm).await;
        info!("<<< NLM handler returned");
        return result;
    }

    // Program not available
    warn!("Unknown program: {}", call.program);
    ReplyBuilder::prog_unavail(call.xid)
}

/// Dispatch NFS procedure
async fn dispatch_nfs(call: CallMessage, request: Bytes, fs: Arc<LocalFilesystem>) -> Bytes {
    // Create decoder positioned at procedure arguments
    // We need to skip the RPC header to get to the procedure-specific args
    // The CallMessage::decode already parsed the header, so we need to 
    // calculate where the args start
    
    let mut dec = XdrDecoder::new(request);
    
    // Skip RPC call header fields that were already parsed
    let _ = dec.decode_u32(); // xid
    let _ = dec.decode_u32(); // msg_type
    let _ = dec.decode_u32(); // rpc_version
    let _ = dec.decode_u32(); // program
    let _ = dec.decode_u32(); // version
    let _ = dec.decode_u32(); // procedure
    
    // Skip credentials (variable length)
    let _ = dec.decode_u32(); // cred_flavor
    let cred_len = dec.decode_u32().unwrap_or(0);
    for _ in 0..((cred_len + 3) / 4) {
        let _ = dec.decode_u32(); // skip cred body (in 4-byte chunks)
    }
    
    // Skip verifier (variable length)
    let _ = dec.decode_u32(); // verf_flavor
    let verf_len = dec.decode_u32().unwrap_or(0);
    for _ in 0..((verf_len + 3) / 4) {
        let _ = dec.decode_u32(); // skip verf body (in 4-byte chunks)
    }
    
    // Now dec is positioned at procedure arguments
    
    let proc_name = Procedure::from_u32(call.procedure)
        .map(|p| format!("{:?}", p))
        .unwrap_or_else(|| format!("Unknown({})", call.procedure));
    info!(">>> Calling NFS procedure: {}", proc_name);

    let result = match Procedure::from_u32(call.procedure) {
        Some(Procedure::Null) => handlers::handle_null(&call),
        Some(Procedure::GetAttr) => handlers::handle_getattr(fs.clone(), &call, &mut dec).await,
        Some(Procedure::SetAttr) => setattr::handle_setattr(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Lookup) => handlers::handle_lookup(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Access) => handlers::handle_access(fs.clone(), &call, &mut dec).await,
        Some(Procedure::ReadLink) => handlers::handle_readlink(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Read) => handlers::handle_read(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Write) => handlers::handle_write(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Create) => handlers::handle_create(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Mkdir) => handlers::handle_mkdir(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Symlink) => handlers::handle_symlink(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Mknod) => handlers::handle_mknod(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Remove) => handlers::handle_remove(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Rmdir) => handlers::handle_rmdir(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Rename) => handlers::handle_rename(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Link) => handlers::handle_link(fs.clone(), &call, &mut dec).await,
        Some(Procedure::ReadDir) => handlers::handle_readdir(fs.clone(), &call, &mut dec).await,
        Some(Procedure::ReadDirPlus) => handlers::handle_readdirplus(fs.clone(), &call, &mut dec).await,
        Some(Procedure::FsStat) => handlers::handle_fsstat(fs.clone(), &call, &mut dec).await,
        Some(Procedure::FsInfo) => handlers::handle_fsinfo(fs.clone(), &call, &mut dec).await,
        Some(Procedure::PathConf) => handlers::handle_pathconf(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Commit) => handlers::handle_commit(fs, &call, &mut dec).await,
        // Unknown procedure number
        None => {
            warn!("Unsupported NFS procedure: {}", call.procedure);
            ReplyBuilder::proc_unavail(call.xid)
        }
    };

    info!("<<< NFS procedure {} completed", proc_name);
    result
}

/// Dispatch MOUNT protocol procedure
/// For now, we implement a minimal MOUNT protocol that accepts all mount requests
async fn dispatch_mount(call: CallMessage, request: Bytes, fs: Arc<LocalFilesystem>) -> Bytes {
    debug!("MOUNT procedure: {}", call.procedure);
    
    // Parse request to skip RPC header (same as dispatch_nfs)
    let mut dec = XdrDecoder::new(request);
    let _ = dec.decode_u32(); // xid
    let _ = dec.decode_u32(); // msg_type
    let _ = dec.decode_u32(); // rpc_version
    let _ = dec.decode_u32(); // program
    let _ = dec.decode_u32(); // version
    let _ = dec.decode_u32(); // procedure
    
    // Skip credentials
    let _ = dec.decode_u32(); // cred_flavor
    let cred_len = dec.decode_u32().unwrap_or(0);
    for _ in 0..((cred_len + 3) / 4) {
        let _ = dec.decode_u32();
    }
    
    // Skip verifier
    let _ = dec.decode_u32(); // verf_flavor
    let verf_len = dec.decode_u32().unwrap_or(0);
    for _ in 0..((verf_len + 3) / 4) {
        let _ = dec.decode_u32();
    }
    
    match call.procedure {
        0 => {
            // NULL
            ReplyBuilder::success(call.xid).finish()
        }
        1 => {
            // MNT - Mount a filesystem
            // Decode the directory path being mounted
            let _dirpath = dec.decode_string().unwrap_or_else(|_| "/".to_string());
            debug!("MNT request for path: {}", _dirpath);
            
            // Return success with root file handle
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: MNT3_OK (0)
            enc.encode_u32(0);
            
            // Root file handle
            if let Ok(root_fh) = fs.root_handle() {
                root_fh.encode(enc);
                
                // Auth flavors (empty list = accept any)
                enc.encode_u32(0); // No auth flavors specified
                
                return reply.finish();
            }
            
            // If we can't get root handle, return error
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(5); // MNT3ERR_IO
            reply.finish()
        }
        3 => {
            // UMNT - Unmount
            ReplyBuilder::success(call.xid).finish()
        }
        5 => {
            // EXPORT - Return export list
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Export list (simplified - just export root)
            enc.encode_bool(true); // One export follows
            enc.encode_string("/"); // Export path
            
            // Groups (empty)
            enc.encode_bool(false);
            
            // No more exports
            enc.encode_bool(false);
            
            reply.finish()
        }
        _ => ReplyBuilder::proc_unavail(call.xid),
    }
}

/// Dispatch NLM protocol procedure
async fn dispatch_nlm(call: CallMessage, request: Bytes, nlm: Arc<NlmService>) -> Bytes {
    debug!("NLM procedure: {}", call.procedure);

    // Create decoder positioned at procedure arguments
    // Same pattern as dispatch_nfs and dispatch_mount
    let mut dec = XdrDecoder::new(request);

    // Skip RPC call header
    let _ = dec.decode_u32(); // xid
    let _ = dec.decode_u32(); // msg_type
    let _ = dec.decode_u32(); // rpc_version
    let _ = dec.decode_u32(); // program
    let _ = dec.decode_u32(); // version
    let _ = dec.decode_u32(); // procedure

    // Skip credentials
    let _ = dec.decode_u32(); // cred_flavor
    let cred_len = dec.decode_u32().unwrap_or(0);
    for _ in 0..((cred_len + 3) / 4) {
        let _ = dec.decode_u32();
    }

    // Skip verifier
    let _ = dec.decode_u32(); // verf_flavor
    let verf_len = dec.decode_u32().unwrap_or(0);
    for _ in 0..((verf_len + 3) / 4) {
        let _ = dec.decode_u32();
    }

    // Now dec is positioned at procedure arguments
    nlm.handle_call(&call, &mut dec).await
}

