//! NFS Server - TCP/UDP Transport
//!
//! Handles network I/O for the NFSv3 server.
//! Listens on TCP and UDP ports, receives RPC calls, dispatches to handlers,
//! and sends replies.

use super::handlers;
use super::portmap;
use super::protocol::Procedure;
use super::rpc::{CallMessage, ReplyBuilder, NFS_PROGRAM, NFS_VERSION, MOUNT_PROGRAM, MOUNT_VERSION};
use super::vfs::LocalFilesystem;
use super::xdr::XdrDecoder;
use bytes::Bytes;
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
}

impl NfsServer {
    /// Create a new NFS server
    pub fn new(config: NfsConfig, fs: Arc<LocalFilesystem>) -> std::io::Result<Self> {
        Ok(Self { config, fs })
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
            let addr = addr.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_tcp(&addr, fs).await {
                    error!("TCP server error: {}", e);
                }
            })
        };
        
        let udp_handle = {
            let fs = self.fs.clone();
            let addr = addr.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_udp(&addr, fs).await {
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
async fn serve_tcp(addr: &str, fs: Arc<LocalFilesystem>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("NFS TCP server listening on {}", addr);
    
    loop {
        let (stream, peer) = listener.accept().await?;
        debug!("TCP connection from {}", peer);
        
        let fs = fs.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_connection(stream, fs).await {
                warn!("TCP connection error: {}", e);
            }
        });
    }
}

/// Handle a TCP connection
async fn handle_tcp_connection(mut stream: TcpStream, fs: Arc<LocalFilesystem>) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65536];
    
    loop {
        // Read RPC record marker (4 bytes: 1 bit last-fragment + 31 bits length)
        let mut marker_buf = [0u8; 4];
        match stream.read_exact(&mut marker_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Connection closed
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        
        let marker = u32::from_be_bytes(marker_buf);
        let _is_last = (marker & 0x80000000) != 0;
        let length = (marker & 0x7FFFFFFF) as usize;
        
        if length > buf.len() {
            buf.resize(length, 0);
        }
        
        // Read RPC message
        stream.read_exact(&mut buf[..length]).await?;
        
        // Process the RPC call
        let request = Bytes::copy_from_slice(&buf[..length]);
        let reply = dispatch(request, fs.clone()).await;
        
        // Send reply with record marker
        let reply_len = reply.len() as u32;
        let reply_marker = 0x80000000 | reply_len; // Last fragment + length
        stream.write_all(&reply_marker.to_be_bytes()).await?;
        stream.write_all(&reply).await?;
        stream.flush().await?;
    }
}

/// Serve NFS over UDP
async fn serve_udp(addr: &str, fs: Arc<LocalFilesystem>) -> std::io::Result<()> {
    let socket = Arc::new(UdpSocket::bind(addr).await?);
    info!("NFS UDP server listening on {}", addr);
    
    let mut buf = vec![0u8; 65536];
    
    loop {
        let (len, peer) = socket.recv_from(&mut buf).await?;
        debug!("UDP request from {}, {} bytes", peer, len);
        
        let request = Bytes::copy_from_slice(&buf[..len]);
        let fs = fs.clone();
        let socket = socket.clone();
        
        // Handle request in separate task
        tokio::spawn(async move {
            let reply = dispatch(request, fs).await;
            
            if let Err(e) = socket.send_to(&reply, peer).await {
                warn!("Failed to send UDP reply: {}", e);
            }
        });
    }
}

/// Dispatch an RPC call to the appropriate handler
async fn dispatch(request: Bytes, fs: Arc<LocalFilesystem>) -> Bytes {
    // Parse RPC call message
    let call = match CallMessage::decode(request.clone()) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to parse RPC call: {}", e);
            return ReplyBuilder::garbage_args(0).into();
        }
    };
    
    debug!(
        "RPC: program={}, version={}, procedure={}",
        call.program, call.version, call.procedure
    );
    
    // Check program and version
    if call.program == NFS_PROGRAM && call.version == NFS_VERSION {
        return dispatch_nfs(call, request, fs).await;
    }
    
    if call.program == MOUNT_PROGRAM && call.version == MOUNT_VERSION {
        return dispatch_mount(call, request, fs).await;
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
    
    match Procedure::from_u32(call.procedure) {
        Some(Procedure::Null) => handlers::handle_null(&call),
        Some(Procedure::GetAttr) => handlers::handle_getattr(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Lookup) => handlers::handle_lookup(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Access) => handlers::handle_access(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Read) => handlers::handle_read(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Write) => handlers::handle_write(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Create) => handlers::handle_create(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Mkdir) => handlers::handle_mkdir(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Remove) => handlers::handle_remove(fs.clone(), &call, &mut dec).await,
        Some(Procedure::Rmdir) => handlers::handle_rmdir(fs.clone(), &call, &mut dec).await,
        Some(Procedure::ReadDir) => handlers::handle_readdir(fs.clone(), &call, &mut dec).await,
        Some(Procedure::FsStat) => handlers::handle_fsstat(fs.clone(), &call, &mut dec).await,
        Some(Procedure::FsInfo) => handlers::handle_fsinfo(fs.clone(), &call, &mut dec).await,
        Some(Procedure::PathConf) => handlers::handle_pathconf(fs, &call, &mut dec).await,
        // Procedures we don't implement yet
        Some(_) | None => {
            warn!("Unsupported NFS procedure: {}", call.procedure);
            ReplyBuilder::proc_unavail(call.xid)
        }
    }
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

