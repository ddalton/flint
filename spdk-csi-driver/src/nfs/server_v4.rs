//! NFSv4.2 Server - TCP Transport
//!
//! Handles network I/O for the NFSv4.2 server.
//! Listens on TCP port, receives RPC COMPOUND calls, dispatches to NFSv4.2 handlers,
//! and sends replies.

use super::rpc::{CallMessage, ReplyBuilder, AuthFlavor};
use super::rpcsec_gss::{RpcSecGssManager, RpcGssCred, procedure as gss_proc};
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
use tracing::{debug, info, warn};

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

    /// Export as read-only (for ROX volumes)
    pub read_only: bool,
}

impl Default for NfsConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".to_string(),
            bind_port: 2049,
            volume_id: String::new(),
            export_path: PathBuf::new(),
            read_only: false,
        }
    }
}

/// NFSv4.2 Server
pub struct NfsServer {
    config: NfsConfig,
    dispatcher: Arc<CompoundDispatcher>,
    gss_manager: Arc<RpcSecGssManager>,
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

        // Initialize RPCSEC_GSS manager
        let keytab_path = std::env::var("KRB5_KTNAME").ok();
        let gss_manager = Arc::new(RpcSecGssManager::new(keytab_path));

        Ok(Self { config, dispatcher, gss_manager })
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
        serve_tcp(&addr, self.dispatcher.clone(), self.gss_manager.clone()).await
    }
}

/// Serve NFSv4.2 over TCP
async fn serve_tcp(addr: &str, dispatcher: Arc<CompoundDispatcher>, gss_manager: Arc<RpcSecGssManager>) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    
    // Track active connections for debugging concurrent mount issues
    static ACTIVE_CONNECTIONS: AtomicU64 = AtomicU64::new(0);
    
    let listener = TcpListener::bind(addr).await?;
    info!("✅ NFSv4.2 TCP server listening on {}", addr);
    info!("");
    
    let mut connection_count = 0u64;

    loop {
        let (stream, peer) = listener.accept().await?;
        
        connection_count += 1;
        let active = ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst) + 1;
        info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        info!("📡 [NFS_SERVER] Connection #{} from {} (Active connections: {})", connection_count, peer, active);
        info!("   Timestamp: {:?}", std::time::SystemTime::now());
        info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        // Log TCP socket info
        if let Ok(addr) = stream.local_addr() {
            debug!("   Local addr: {}", addr);
        }
        
        let dispatcher = dispatcher.clone();
        let gss_manager = gss_manager.clone();
        let conn_id = connection_count;
        tokio::spawn(async move {
            info!("🚀 [NFS_SERVER] Spawned handler task for connection #{} from {}", conn_id, peer);
            if let Err(e) = handle_tcp_connection(stream, dispatcher, gss_manager, peer, conn_id).await {
                warn!("❌ [NFS_SERVER] Connection #{} from {} error: {}", conn_id, peer, e);
                let active = ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst) - 1;
                info!("   Active connections remaining: {}", active);
            } else {
                let active = ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst) - 1;
                info!("✓ [NFS_SERVER] Connection #{} from {} closed cleanly (Active: {})", conn_id, peer, active);
            }
        });
    }
}

/// Handle a TCP connection
async fn handle_tcp_connection(
    stream: TcpStream,
    dispatcher: Arc<CompoundDispatcher>,
    gss_manager: Arc<RpcSecGssManager>,
    peer: std::net::SocketAddr,
    conn_id: u64,
) -> std::io::Result<()> {
    use tokio::io::BufWriter;
    use tokio::time::Instant;

    let connect_time = Instant::now();
    info!("🔌 [NFS_SERVER] Connection #{} handler started for {}", conn_id, peer);
    info!("   Start time: {:?}", std::time::SystemTime::now());

    // Set TCP_NODELAY for low latency
    stream.set_nodelay(true)?;

    // Split stream for independent reading and buffered writing
    let (reader, writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::with_capacity(128 * 1024, reader);
    let mut writer = BufWriter::with_capacity(128 * 1024, writer);

    // Reusable buffer
    let mut buf = BytesMut::with_capacity(128 * 1024);

    let mut rpc_count = 0;

    loop {
        debug!("📥 [NFS_SERVER] Connection #{}: Waiting for RPC message #{} from {}", conn_id, rpc_count + 1, peer);
        
        // Read RPC record marker (4 bytes)
        let mut marker_buf = [0u8; 4];
        match reader.read_exact(&mut marker_buf).await {
            Ok(_) => {
                debug!("✅ [NFS_SERVER] Connection #{}: Received RPC marker from {}: {:02x?}", conn_id, peer, marker_buf);
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Connection closed gracefully
                let duration = connect_time.elapsed();
                info!("🔌 [NFS_SERVER] Connection #{} from {} closed after {:?} ({} RPCs processed)", 
                      conn_id, peer, duration, rpc_count);
                if rpc_count == 0 {
                    warn!("⚠️  [NFS_SERVER] Client {} connected (conn #{}) but sent NO RPC messages!", peer, conn_id);
                }
                return Ok(());
            }
            Err(e) => {
                warn!("❌ [NFS_SERVER] Connection #{}: Error reading RPC marker from {}: {}", conn_id, peer, e);
                return Err(e);
            }
        }

        let marker = u32::from_be_bytes(marker_buf);
        let is_last = (marker & 0x80000000) != 0;
        let length = (marker & 0x7FFFFFFF) as usize;

        debug!("📊 RPC marker decoded: is_last={}, length={} bytes", is_last, length);

        // Prevent oversized allocations
        if length > 4 * 1024 * 1024 {
            warn!("❌ Rejecting oversized RPC message from {}: {} bytes (max 4MB)", peer, length);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RPC message too large",
            ));
        }

        if length == 0 {
            warn!("⚠️  Zero-length RPC message from {}, ignoring", peer);
            continue;
        }

        // Read message
        buf.clear();
        buf.reserve(length);
        unsafe { buf.set_len(length); }
        
        debug!("📥 Reading RPC payload: {} bytes from {}", length, peer);
        reader.read_exact(&mut buf[..length]).await?;
        
        debug!("✅ Received complete RPC message ({} bytes), first 32 bytes: {:02x?}", 
               length, &buf[..std::cmp::min(32, length)]);

        let request = buf.split().freeze();

        // Process the RPC call with timing
        let rpc_start = Instant::now();
        debug!(">>> [NFS_SERVER] Connection #{}: Processing NFSv4 RPC #{} from {}, length={} bytes", 
               conn_id, rpc_count + 1, peer, length);
        let reply = dispatch_nfsv4(request, dispatcher.clone(), gss_manager.clone(), conn_id, rpc_count + 1).await;
        let rpc_duration = rpc_start.elapsed();
        info!("📨 [NFS_SERVER] Connection #{}: RPC #{} processed in {:?} (reply: {} bytes)", 
              conn_id, rpc_count + 1, rpc_duration, reply.len());
        
        rpc_count += 1;

        // Write reply with record marker
        let reply_len = reply.len() as u32;
        let reply_marker = 0x80000000 | reply_len;
        
        debug!("📤 Sending reply to {}: {} bytes (marker: {:08x})", peer, reply_len, reply_marker);
        debug!("   Reply first 32 bytes: {:02x?}", &reply[..std::cmp::min(32, reply.len())]);
        
        writer.write_all(&reply_marker.to_be_bytes()).await?;
        writer.write_all(&reply).await?;
        writer.flush().await?;
        
        debug!("✅ Reply sent and flushed to {}", peer);
    }
}

/// Dispatch an NFSv4 RPC call
async fn dispatch_nfsv4(request: Bytes, dispatcher: Arc<CompoundDispatcher>, gss_manager: Arc<RpcSecGssManager>, conn_id: u64, rpc_num: u64) -> Bytes {
    debug!("🔍 [NFS_SERVER] Connection #{}, RPC #{}: Dispatching RPC: {} total bytes", conn_id, rpc_num, request.len());
    debug!("   First 64 bytes of request: {:02x?}", &request[..std::cmp::min(64, request.len())]);

    // Parse RPC call message and extract procedure arguments
    let (call, args) = match CallMessage::decode_with_args(request.clone()) {
        Ok(result) => {
            debug!("✅ [NFS_SERVER] Connection #{}, RPC #{}: RPC message parsed successfully", conn_id, rpc_num);
            result
        }
        Err(e) => {
            warn!("❌ [NFS_SERVER] Connection #{}, RPC #{}: Failed to parse RPC call: {}", conn_id, rpc_num, e);
            warn!("   Request was {} bytes: {:02x?}", request.len(),
                  &request[..std::cmp::min(128, request.len())]);
            return ReplyBuilder::garbage_args(0).into();
        }
    };

    info!(
        ">>> [NFS_RPC] Connection #{}, RPC #{}: xid={}, program={}, version={}, procedure={}",
        conn_id, rpc_num, call.xid, call.program, call.version, call.procedure
    );
    debug!("   Cred: {:?}, Verf: {:?}", call.cred.flavor, call.verf.flavor);

    // Handle RPCSEC_GSS authentication
    if call.cred.flavor == AuthFlavor::RpcsecGss {
        info!("🔐 [NFS_SERVER] Connection #{}, RPC #{}: RPCSEC_GSS authentication detected", conn_id, rpc_num);
        return handle_rpcsec_gss_call(call, args, gss_manager, dispatcher).await;
    }

    // Check program number
    if call.program != NFS4_PROGRAM {
        warn!("❌ Invalid program number: {} (expected {} for NFS4)", call.program, NFS4_PROGRAM);
        warn!("   This might be a different RPC service trying to connect");
        debug!("   Returning PROG_UNAVAIL to client");
        return ReplyBuilder::prog_unavail(call.xid);
    }

    // Check version (4.0, 4.1, or 4.2)
    if call.version != 4 {
        warn!("❌ Invalid NFSv4 version: {} (expected 4)", call.version);
        warn!("   Client might be trying NFSv3 or other version");
        debug!("   Returning PROC_UNAVAIL to client");
        // NFSv4 doesn't have prog_mismatch, return proc_unavail
        return ReplyBuilder::proc_unavail(call.xid);
    }
    
    debug!("✅ RPC validation passed: program={}, version={}", call.program, call.version);

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
            handle_compound(call, args, dispatcher).await
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
    args: Bytes,
    dispatcher: Arc<CompoundDispatcher>,
) -> Bytes {
    // The args Bytes contains only the COMPOUND procedure arguments (RPC header already stripped)

    eprintln!("DEBUG handle_compound: args.len()={}", args.len());
    eprintln!("DEBUG handle_compound: First 32 bytes (hex): {:02x?}", &args[..args.len().min(32)]);

    // Create a decoder from the procedure arguments
    let decoder = XdrDecoder::new(args);

    // Decode COMPOUND request
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

    // Procedure-specific result (COMPOUND response data appended directly, no length prefix)
    // Per RFC 5531, procedure results are appended directly to RPC reply
    encoder.append_bytes(&compound_data);

    encoder.finish()
}

/// Handle RPCSEC_GSS authenticated RPC call
async fn handle_rpcsec_gss_call(
    call: CallMessage,
    args: Bytes,
    gss_manager: Arc<RpcSecGssManager>,
    dispatcher: Arc<CompoundDispatcher>,
) -> Bytes {
    // Decode RPCSEC_GSS credentials
    let gss_cred = match RpcGssCred::decode(&call.cred.body) {
        Ok(cred) => {
            info!("🔐 GSS Cred: version={}, procedure={}, seq={}, service={:?}",
                  cred.version, cred.procedure, cred.sequence_num, cred.service);
            cred
        }
        Err(e) => {
            warn!("❌ Failed to decode RPCSEC_GSS credentials: {}", e);
            return ReplyBuilder::garbage_args(call.xid);
        }
    };

    // Handle different GSS procedures
    match gss_cred.procedure {
        gss_proc::INIT => {
            info!("🔐 RPCSEC_GSS_INIT");
            handle_gss_init(call.xid, &gss_cred, args, gss_manager).await
        }

        gss_proc::CONTINUE_INIT => {
            info!("🔐 RPCSEC_GSS_CONTINUE_INIT");
            handle_gss_continue_init(call.xid, &gss_cred, args, gss_manager).await
        }

        gss_proc::DATA => {
            info!("🔐 RPCSEC_GSS_DATA");
            // Validate the GSS context
            if let Err(e) = gss_manager.validate_data(&gss_cred).await {
                warn!("❌ GSS DATA validation failed: {}", e);
                // Return SYSTEM_ERR for authentication failure
                return ReplyBuilder::system_err(call.xid);
            }

            // GSS validated, proceed with normal COMPOUND processing
            info!("✅ GSS authentication successful, processing COMPOUND");
            handle_compound(call, args, dispatcher).await
        }

        gss_proc::DESTROY => {
            info!("🔐 RPCSEC_GSS_DESTROY");
            gss_manager.handle_destroy(&gss_cred).await;
            // Return success
            ReplyBuilder::success(call.xid).finish()
        }

        _ => {
            warn!("❌ Unknown RPCSEC_GSS procedure: {}", gss_cred.procedure);
            ReplyBuilder::proc_unavail(call.xid)
        }
    }
}

/// Handle RPCSEC_GSS_INIT
async fn handle_gss_init(
    xid: u32,
    gss_cred: &RpcGssCred,
    args: Bytes,
    gss_manager: Arc<RpcSecGssManager>,
) -> Bytes {
    // Extract init token from args
    // In RPCSEC_GSS_INIT, args contains the GSS init token
    let mut decoder = XdrDecoder::new(args);
    let init_token = match decoder.decode_opaque() {
        Ok(token) => token.to_vec(),
        Err(e) => {
            warn!("❌ Failed to decode GSS init token: {}", e);
            return ReplyBuilder::garbage_args(xid);
        }
    };

    info!("🔐 GSS_INIT: service={:?}, token_len={}", gss_cred.service, init_token.len());

    // Handle the initialization
    let init_res = gss_manager.handle_init(gss_cred, &init_token).await;

    // Build RPC reply with GSS init result
    let mut encoder = XdrEncoder::new();

    // RPC Reply header
    encoder.encode_u32(xid);  // XID
    encoder.encode_u32(1);  // Message type: REPLY
    encoder.encode_u32(0);  // Reply status: MSG_ACCEPTED

    // Auth verifier (null for now)
    encoder.encode_u32(0);  // Auth flavor: AUTH_NONE
    encoder.encode_u32(0);  // Auth length: 0

    // Accept status: SUCCESS
    encoder.encode_u32(0);  // AcceptStatus::Success

    // Encode RPCSEC_GSS init result
    let init_result_data = init_res.encode();
    encoder.append_bytes(&init_result_data);

    info!("✅ GSS_INIT complete: handle_len={}, major={}, minor={}",
          init_res.handle.len(), init_res.major_status, init_res.minor_status);

    encoder.finish()
}

/// Handle RPCSEC_GSS_CONTINUE_INIT
async fn handle_gss_continue_init(
    xid: u32,
    gss_cred: &RpcGssCred,
    args: Bytes,
    gss_manager: Arc<RpcSecGssManager>,
) -> Bytes {
    // Extract continuation token from args
    let mut decoder = XdrDecoder::new(args);
    let token = match decoder.decode_opaque() {
        Ok(t) => t.to_vec(),
        Err(e) => {
            warn!("❌ Failed to decode GSS continue token: {}", e);
            return ReplyBuilder::garbage_args(xid);
        }
    };

    info!("🔐 GSS_CONTINUE_INIT: token_len={}", token.len());

    // Handle the continuation
    let init_res = gss_manager.handle_continue_init(gss_cred, &token).await;

    // Build RPC reply
    let mut encoder = XdrEncoder::new();

    encoder.encode_u32(xid);
    encoder.encode_u32(1);  // REPLY
    encoder.encode_u32(0);  // MSG_ACCEPTED
    encoder.encode_u32(0);  // AUTH_NONE
    encoder.encode_u32(0);
    encoder.encode_u32(0);  // SUCCESS

    let init_result_data = init_res.encode();
    encoder.append_bytes(&init_result_data);

    info!("✅ GSS_CONTINUE_INIT complete: major={}, minor={}",
          init_res.major_status, init_res.minor_status);

    encoder.finish()
}
