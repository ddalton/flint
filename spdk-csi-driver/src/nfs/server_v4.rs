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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::AsyncReadExt;
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
    state_mgr: Arc<StateManager>,
    lock_mgr: Arc<LockManager>,
}

/// Pick the NFSv4 state persistence target.
///
/// Default: a SQLite DB on the exported volume
/// (`<export>/.flint-nfs/state.db`) so clientids, sessions, stateids and
/// the reclaim-complete flags survive a server pod replacement AND roam
/// with the PVC to whichever node the next incarnation lands on. Without
/// this, a client holding dirty open state across a cutover bounce
/// resumes writes against open state the new server never heard of — the
/// writes are acked from the client's page cache and silently dropped
/// (RWX round, 2026-06-12).
///
/// `FLINT_NFS_STATE=memory` opts out (tests, throwaway exports);
/// any other value is used as an explicit DB path.
///
/// A DB that fails to open (corrupt file, schema mismatch from a
/// downgrade) is moved aside and recreated — losing state degrades one
/// bounce to today's behavior, while refusing to start would take the
/// volume down entirely.
fn build_state_backend(
    config: &NfsConfig,
) -> (Arc<dyn crate::state_backend::StateBackend>, bool) {
    let setting = std::env::var("FLINT_NFS_STATE").unwrap_or_default();
    select_state_backend(&setting, &config.export_path)
}

/// Returns the backend plus `state_lost: true` when a prior state DB
/// existed but could not be used (quarantined-and-recreated, or the
/// in-memory fallback) — pre-restart state is gone even though the
/// backend itself is healthy. The caller gates NEW byte-range locks
/// during grace in that case: with the lock table lost, conflict
/// detection isn't authoritative until the reclaim window closes.
fn select_state_backend(
    setting: &str,
    export_path: &Path,
) -> (Arc<dyn crate::state_backend::StateBackend>, bool) {
    use crate::state_backend::{memory_backend, SqliteBackend};

    if setting.eq_ignore_ascii_case("memory") {
        info!("💾 NFSv4 state: in-memory (FLINT_NFS_STATE=memory) — no restart survival");
        return (memory_backend(), false);
    }
    let db_path = if setting.is_empty() {
        export_path.join(".flint-nfs").join("state.db")
    } else {
        PathBuf::from(setting)
    };
    // Whether a previous incarnation left state behind — distinguishes
    // "fresh volume, nothing to lose" from "state existed and is gone".
    let had_prior_state = db_path.exists();
    if let Some(dir) = db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::error!("NFSv4 state dir {:?} not creatable ({}) — falling back to in-memory state", dir, e);
            return (memory_backend(), had_prior_state);
        }
    }
    match SqliteBackend::open_durable(&db_path) {
        Ok(b) => {
            info!("💾 NFSv4 state: persistent at {:?} (synchronous=FULL)", db_path);
            (Arc::new(b), false)
        }
        Err(e) => {
            tracing::error!("NFSv4 state DB {:?} unusable ({}) — moving it aside and recreating", db_path, e);
            let quarantine = db_path.with_extension("db.unusable");
            let _ = std::fs::rename(&db_path, &quarantine);
            match SqliteBackend::open_durable(&db_path) {
                Ok(b) => {
                    tracing::warn!("NFSv4 state DB recreated (prior state lost; old file at {:?})", quarantine);
                    (Arc::new(b), had_prior_state)
                }
                Err(e) => {
                    tracing::error!("NFSv4 state DB recreate failed ({}) — falling back to in-memory state", e);
                    (memory_backend(), had_prior_state)
                }
            }
        }
    }
}

impl NfsServer {
    /// Create a new NFSv4.2 server. Async because the filehandle
    /// manager loads its persisted v2 (id-based) handle mappings from
    /// the state backend before the listener accepts.
    pub async fn new(config: NfsConfig) -> std::io::Result<Self> {
        // Initialize NFSv4.2 components
        // Filehandles embed the instance id, so it must be stable across
        // restarts or every client-held handle goes ESTALE on a bounce and
        // persisted lock/stateid records stop matching their files. Prefer
        // the pNFS cluster-shared env id, else derive the same per-volume id
        // the RWX pod spec would have set — never a boot-time id.
        let instance_id = std::env::var("PNFS_INSTANCE_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_else(|| crate::rwx_nfs::stable_nfs_instance_id(&config.volume_id));
        let fh_mgr = Arc::new(FileHandleManager::new_with_instance_id(
            config.export_path.clone(),
            "volume".to_string(),
            instance_id,
        ));
        let (backend, state_lost) = build_state_backend(&config);
        // v2 (id-based) filehandles — minted for paths too long to
        // embed — resolve through a table persisted alongside the rest
        // of the NFS state, so they survive server restart.
        fh_mgr.attach_backend(Arc::clone(&backend)).await;
        let state_mgr = Arc::new(StateManager::new(&config.volume_id, backend));
        // Locks share the state backend: their stateids always survived a
        // restart (StateIdRecord), so the lock table must too — otherwise
        // post-restart the stateid validates while mutual exclusion is
        // silently gone.
        let lock_mgr = Arc::new(LockManager::with_backend(state_mgr.backend()));
        if state_lost {
            // A prior state DB existed but was quarantined/unreadable:
            // the lock table is gone with it. Gate new locks in grace.
            lock_mgr.mark_restore_failed();
        }

        // Create COMPOUND dispatcher (creates handlers internally)
        let dispatcher = Arc::new(CompoundDispatcher::new(
            fh_mgr,
            state_mgr.clone(),
            lock_mgr.clone(),
        ));

        // Initialize RPCSEC_GSS manager
        let keytab_path = std::env::var("KRB5_KTNAME").ok();
        let gss_manager = Arc::new(RpcSecGssManager::new(keytab_path));

        Ok(Self { config, dispatcher, gss_manager, state_mgr, lock_mgr })
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

        // Restore persisted NFSv4 state BEFORE accepting connections: by
        // the time a client's TCP reconnect lands, its clientid, session,
        // stateids and reclaim-complete flag are back — SEQUENCE on the
        // old session simply succeeds and in-flight writes resume instead
        // of dying against unknown state. (Same pre-listener hook the
        // pNFS MDS uses; an unreadable backend degrades to an empty
        // state table, which is exactly the pre-persistence behavior.)
        match self.state_mgr.backend().increment_instance_counter().await {
            Ok(n) => info!("📈 NFSv4 server instance #{} for this volume (persisted counter)", n),
            Err(e) => tracing::warn!("NFSv4 instance counter unavailable: {}", e),
        }
        if let Err(e) = self.state_mgr.load_from_backend().await {
            tracing::error!("NFSv4 state restore failed ({}) — starting with empty state", e);
            // Lock state is lost with the rest: refuse NEW locks during
            // grace so a second client can't take a range whose
            // pre-restart holder we no longer know about.
            self.lock_mgr.mark_restore_failed();
        }
        match self.state_mgr.backend().list_locks().await {
            Ok(records) => self.lock_mgr.load_records(records),
            Err(e) => {
                tracing::error!("NFSv4 lock restore failed ({}) — new locks gated for grace", e);
                self.lock_mgr.mark_restore_failed();
            }
        }

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
    let writer = BufWriter::with_capacity(128 * 1024, writer);
    // Wrap the writer so the same handle can be used by:
    //   1. The main loop below (forward replies).
    //   2. The dispatcher (registered as a back-channel writer once
    //      BIND_CONN_TO_SESSION arrives).
    // The `tokio::sync::Mutex` inside `BackChannelWriter` serializes
    // writes so RPC frames cannot interleave on the wire — required by
    // ONC RPC framing (RFC 1831).
    let bcw = crate::nfs::v4::back_channel::BackChannelWriter::new(writer);

    // Reusable buffer
    let mut buf = BytesMut::with_capacity(128 * 1024);

    let mut rpc_count = 0;

    // When the loop exits — clean EOF or any error — release any
    // CB callers still awaiting a reply on this connection so they
    // see `ConnectionClosed` rather than wait out the timeout. We
    // can't rely on the writer's Drop because the dispatcher's
    // back-channel registry holds another Arc, so the writer
    // outlives this function. The guard runs cleanup on every
    // return path (early Err, EOF return, panic).
    struct InflightGuard {
        bcw: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
    }
    impl Drop for InflightGuard {
        fn drop(&mut self) {
            self.bcw.drop_all_inflight();
        }
    }
    let _inflight_guard = InflightGuard {
        bcw: Arc::clone(&bcw),
    };

    // Per-connection RPC pipelining (RFC 8881 §2.10.6): dispatches run
    // concurrently up to FLINT_NFS_MAX_INFLIGHT (default 64, 0 =
    // sequential); replies are serialized on the wire by the BCW mutex.
    let pipeline = crate::nfs::pipeline::ConnectionPipeline::from_env();

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

        // Read message.
        //
        // We size `buf` to the fragment length and let `read_exact` populate
        // the slice. The previous form used `unsafe { set_len(length) }`,
        // which is undefined behavior if `read_exact` returned an error
        // before fully writing the buffer (the user of `buf` could then
        // observe uninitialized memory). `resize` is implemented as a single
        // `RawVec::reserve` + memset pair and is essentially free relative to
        // the I/O cost of receiving `length` bytes from the socket.
        buf.clear();
        buf.resize(length, 0);

        debug!("📥 Reading RPC payload: {} bytes from {}", length, peer);
        reader.read_exact(&mut buf[..length]).await?;
        
        debug!("✅ Received complete RPC message ({} bytes), first 32 bytes: {:02x?}", 
               length, &buf[..std::cmp::min(32, length)]);

        let request = buf.split().freeze();

        // RFC 5531 §9 frame layout: [0..4]=xid, [4..8]=msg_type
        // (0=CALL, 1=REPLY). The forward channel only ever sees
        // CALLs — but if `BIND_CONN_TO_SESSION` registered this
        // connection as a back-channel, the *server's* CB_COMPOUND
        // CALLs come back as REPLYs on the same socket. Route those
        // to the inflight registry instead of trying to parse them
        // as a forward CALL (which would crash with "expected CALL,
        // got REPLY").
        if request.len() >= 8 {
            let msg_type = u32::from_be_bytes([
                request[4], request[5], request[6], request[7],
            ]);
            if msg_type == 1 {
                let xid = u32::from_be_bytes([
                    request[0], request[1], request[2], request[3],
                ]);
                if !bcw.deliver_reply(xid, request) {
                    warn!(
                        "📭 [NFS_SERVER] Connection #{}: CB reply for unknown xid={} (timed out or never registered)",
                        conn_id, xid,
                    );
                }
                continue;
            }
        }

        // Dispatch through the pipeline: concurrent up to the permit
        // bound, sequential when FLINT_NFS_MAX_INFLIGHT=0. The reply
        // goes out via the same writer the back-channel uses —
        // `send_record` prepends the 4-byte record marker and
        // flushes; its inner Mutex serializes against concurrent
        // replies and CB_LAYOUTRECALL frames so wire framing stays
        // valid.
        debug!(">>> [NFS_SERVER] Connection #{}: Processing NFSv4 RPC #{} from {}, length={} bytes",
               conn_id, rpc_count + 1, peer, length);
        let dispatcher_c = dispatcher.clone();
        let gss_c = gss_manager.clone();
        let bcw_dispatch = Arc::clone(&bcw);
        let bcw_write = Arc::clone(&bcw);
        let rpc_num = rpc_count + 1;
        // Backlog hint: bytes already buffered mean the client is
        // pipelining, so concurrent dispatch pays for its overhead.
        let more_queued = !reader.buffer().is_empty();
        pipeline.submit(
            request,
            more_queued,
            move |req| async move {
                let rpc_start = Instant::now();
                let reply = dispatch_nfsv4(
                    req,
                    dispatcher_c,
                    gss_c,
                    conn_id,
                    rpc_num,
                    bcw_dispatch,
                ).await;
                debug!("📨 [NFS_SERVER] Connection #{}: RPC #{} processed in {:?} (reply: {} bytes)",
                       conn_id, rpc_num, rpc_start.elapsed(), reply.len());
                reply
            },
            move |reply| async move { bcw_write.send_record(reply).await },
        ).await?;

        rpc_count += 1;
    }
}

/// Dispatch an NFSv4 RPC call. `back_channel` is the connection's
/// own writer; passed through so `BIND_CONN_TO_SESSION` can register
/// it for later callback frames.
async fn dispatch_nfsv4(
    request: Bytes,
    dispatcher: Arc<CompoundDispatcher>,
    gss_manager: Arc<RpcSecGssManager>,
    conn_id: u64,
    rpc_num: u64,
    back_channel: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
) -> Bytes {
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
        return handle_rpcsec_gss_call(call, args, gss_manager, dispatcher, back_channel).await;
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
            handle_compound(call, args, dispatcher, back_channel).await
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
    back_channel: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
) -> Bytes {
    // The args Bytes contains only the COMPOUND procedure arguments (RPC header already stripped)

    tracing::trace!("handle_compound: args.len()={}", args.len());
    tracing::trace!("handle_compound: First 32 bytes (hex): {:02x?}", &args[..args.len().min(32)]);

    // Capture the original wire-byte length BEFORE decoding so the
    // dispatcher can compare against the session's negotiated
    // `ca_maxrequestsize` after SEQUENCE binds the session
    // (RFC 8881 §18.46.4 / pynfs SEQ6).
    let wire_size = args.len();

    // Create a decoder from the procedure arguments
    let decoder = XdrDecoder::new(args);

    // Decode COMPOUND request
    let mut compound_req = match CompoundRequest::decode(decoder) {
        Ok(req) => req,
        Err(e) => {
            warn!("Failed to decode COMPOUND request: {}", e);
            return ReplyBuilder::garbage_args(call.xid);
        }
    };
    compound_req.wire_size = wire_size;

    debug!("COMPOUND: tag={}, minor_version={}, {} operations",
           compound_req.tag,
           compound_req.minor_version,
           compound_req.operations.len());

    // RPC-level principal for the EXCHANGE_ID §18.35.5 state machine.
    // Cheap to compute and an empty Vec for AUTH_NONE.
    let principal = call.cred.principal();
    // AUTH_SYS (uid, gid) — file-creating ops stamp it onto the backing
    // object so ownership round-trips (postgres-class workloads check it).
    let unix_cred = call.cred.unix_uid_gid();

    // Dispatch to COMPOUND handler
    let compound_resp = dispatcher
        .dispatch_compound_with_cred(compound_req, principal, unix_cred, Some(Arc::clone(&back_channel)))
        .await;

    debug!("COMPOUND result: status={:?}, {} results",
           compound_resp.status,
           compound_resp.results.len());

    // Pull the cache hint off before we move the response into encode().
    // RFC 8881 §15.1.10.4 requires the slot reply cache to hold the *exact*
    // bytes the client received, so we capture it after encoding finishes.
    let cache_slot = compound_resp.cache_slot;

    // Encode COMPOUND response
    let compound_data = compound_resp.encode();

    // Cache the encoded reply against the SEQUENCE slot for replay matching.
    // Skipped automatically when the COMPOUND short-circuited a replay
    // (cache_slot is None on that path).
    if let Some((session_id, slot_id)) = cache_slot {
        dispatcher.cache_slot_reply(&session_id, slot_id, compound_data.clone());
    }

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
    back_channel: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
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
            handle_compound(call, args, dispatcher, back_channel).await
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

#[cfg(test)]
mod state_persistence_tests {
    use super::*;
    use crate::nfs::v4::state::client::ExchangeIdOutcome;
    use crate::nfs::v4::state::StateType;

    /// The pod-replacement contract behind RWX cutover transparency.
    ///
    /// What must survive the export-volume DB round-trip: the client
    /// record (clientid, confirmed, reclaim-complete) and every stateid.
    /// Sessions are deliberately NOT restored — `SessionManager::
    /// load_records` drops them so the reconnecting client gets
    /// BADSESSION and re-CREATE_SESSIONs against its restored, confirmed
    /// clientid (no EXCHANGE_ID, no STALE_CLIENTID). Its retransmitted
    /// WRITEs then carry the restored stateids and land — instead of
    /// being acked from the client page cache and silently dropped
    /// against a blank-state server (observed live, 2026-06-12).
    #[tokio::test]
    async fn nfsv4_state_survives_server_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let export = dir.path().to_path_buf();

        // ── Incarnation 1 ────────────────────────────────────────────
        let mgr1 = StateManager::new("vol-rt", select_state_backend("", &export).0);
        let outcome = mgr1
            .clients
            .exchange_id(b"client-A".to_vec(), 0xfeed, 0, vec![]);
        let client_id = match outcome {
            ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("unexpected exchange_id outcome: {:?}", other),
        };
        let session = mgr1
            .sessions
            .create_session(client_id, 1, 0, 1 << 20, 1 << 20, 4096, 8, 64, 0x4000_0000);
        mgr1.clients.mark_confirmed(client_id);
        mgr1.clients.mark_reclaim_complete(client_id);
        let open_stateid =
            mgr1.stateids
                .allocate(StateType::Open, client_id, Some(b"/data/log".to_vec()));

        // Persistence is fire-and-forget (spawn_persist); wait for the
        // backend to observe everything before "killing the pod".
        let backend = mgr1.backend();
        for _ in 0..200 {
            let cs = backend.list_clients().await.unwrap();
            let st = backend.list_stateids().await.unwrap();
            if !st.is_empty() && cs.iter().any(|c| c.confirmed && c.reclaim_complete) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let persisted = backend.list_clients().await.unwrap();
        assert!(
            persisted.iter().any(|c| c.confirmed && c.reclaim_complete),
            "client record (confirmed + reclaim_complete) must persist, got {:?}",
            persisted
        );
        drop(mgr1);
        drop(backend);

        // ── Incarnation 2: same DB file, fresh managers ─────────────
        let mgr2 = StateManager::new("vol-rt", select_state_backend("", &export).0);
        mgr2.load_from_backend().await.unwrap();

        // Stateids survive — a retransmitted WRITE with the pre-bounce
        // stateid resolves instead of BAD_STATEID.
        let entry = mgr2.stateids.get_state(&open_stateid);
        assert!(entry.is_some(), "open stateid must survive replacement");
        assert_eq!(entry.unwrap().client_id, client_id);

        // The reclaim-complete flag survives, so the client's NEW opens
        // are not GRACE-blocked during the post-restart window.
        assert!(mgr2.clients.is_reclaim_complete(client_id));

        // Sessions intentionally do not survive: the client's SEQUENCE
        // gets BADSESSION and re-creates. The new session must not
        // collide with the dropped one's id (counter bumped past it).
        assert!(mgr2.sessions.get_session(&session.session_id).is_none());
        let session2 = mgr2
            .sessions
            .create_session(client_id, 1, 0, 1 << 20, 1 << 20, 4096, 8, 64, 0x4000_0000);
        assert_ne!(session2.session_id.0, session.session_id.0);

        // A re-issued EXCHANGE_ID with the same owner finds the
        // confirmed record instead of minting a new clientid.
        match mgr2.clients.exchange_id(b"client-A".to_vec(), 0xfeed, 0, vec![]) {
            ExchangeIdOutcome::ExistingConfirmed { client_id: cid, .. } => {
                assert_eq!(cid, client_id)
            }
            other => panic!("expected ExistingConfirmed, got {:?}", other),
        }
    }

    /// `FLINT_NFS_STATE=memory` opts out of persistence entirely.
    #[tokio::test]
    async fn memory_setting_skips_db_creation() {
        let dir = tempfile::tempdir().unwrap();
        let _ = select_state_backend("memory", dir.path());
        assert!(
            !dir.path().join(".flint-nfs").exists(),
            "memory backend must not create the state dir"
        );
    }

    /// Default placement: the DB lives on the exported volume so state
    /// roams with the PVC to the next server incarnation's node.
    #[tokio::test]
    async fn default_db_lives_on_export_volume() {
        let dir = tempfile::tempdir().unwrap();
        let _ = select_state_backend("", dir.path());
        assert!(dir.path().join(".flint-nfs").join("state.db").exists());
    }
}
