//! Data Server Implementation
//!
//! The Data Server is a lightweight NFS server that handles only data I/O
//! operations (READ, WRITE, COMMIT) for high-throughput parallel access.
//!
//! # Design
//! - Minimal state (no OPEN/CLOSE tracking)
//! - Direct I/O to SPDK bdevs
//! - Registers with MDS at startup
//! - Sends periodic heartbeats to MDS

use crate::pnfs::config::DsConfig;
use crate::pnfs::ds::io::{IoOperationHandler, WriteStable};
use crate::pnfs::ds::registration::RegistrationClient;
use crate::pnfs::ds::session::DsSessionManager;
use crate::pnfs::Result;
use crate::nfs::rpc::{CallMessage, ReplyBuilder};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use crate::nfs::v4::protocol::{procedure, NFS4_PROGRAM, opcode, Nfs4Status, exchgid_flags};
use crate::nfs::v4::xdr::Nfs4XdrDecoder;
use crate::nfs::v4::state::{ClientManager, LeaseManager};
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// Data Server
pub struct DataServer {
    config: DsConfig,
    io_handler: Arc<IoOperationHandler>,
    registration_client: Arc<tokio::sync::Mutex<RegistrationClient>>,
    session_mgr: Arc<DsSessionManager>,
    client_mgr: Arc<ClientManager>,
    /// Creation stamp of the on-volume identity marker (Phase 2);
    /// reported in RegisterRequest so the MDS can spot volume swaps.
    identity_created_at: u64,
}

impl DataServer {
    /// Create a new data server
    pub fn new(config: DsConfig) -> Result<Self> {
        info!("Initializing Data Server: {}", config.device_id);

        // Verify bdevs are mounted
        for bdev in &config.bdevs {
            let mount_point = std::path::Path::new(&bdev.mount_point);
            if !mount_point.exists() {
                warn!(
                    "Mount point does not exist: {}. Ensure SPDK volume is mounted via ublk.",
                    bdev.mount_point
                );
            } else {
                info!("✓ Mount point verified: {}", bdev.mount_point);
            }
        }

        // Initialize I/O handler with the first bdev's mount point
        // TODO: Support multiple bdevs
        let data_path = config.bdevs.first()
            .ok_or_else(|| crate::pnfs::Error::Config(
                "No bdevs configured".to_string()
            ))?
            .mount_point.clone();

        let io_handler = Arc::new(IoOperationHandler::new(&data_path)?);
        info!("✓ I/O handler initialized with data path: {}", data_path);

        // Identity ↔ volume binding guard (durable-DS plan Phase 2):
        // refuse to serve a data volume stamped for another device_id —
        // stripe maps address devices, so serving DS-B's volume as DS-A
        // corrupts client reads silently. First boot stamps the marker;
        // the creation stamp rides in RegisterRequest so the MDS can
        // WARN when a device comes back with a different volume.
        let identity = super::identity::verify_or_stamp(
            std::path::Path::new(&data_path),
            &config.device_id,
        ).map_err(|e| crate::pnfs::Error::Config(format!(
            "DS identity guard on {}: {}", data_path, e
        )))?;
        info!(
            "✓ Identity marker verified: device '{}' (volume first stamped at unix {})",
            identity.device_id, identity.created_at
        );
        let identity_created_at = identity.created_at;

        // Initialize registration client
        let registration_client = Arc::new(tokio::sync::Mutex::new(
            RegistrationClient::new(
                config.device_id.clone(),
                config.mds.endpoint.clone(),
                Duration::from_secs(config.mds.heartbeat_interval),
            )
        ));

        // Initialize session manager (for NFSv4.1 SEQUENCE operations)
        let session_mgr = Arc::new(DsSessionManager::new());
        info!("✓ Session manager initialized (NFSv4.1 support)");

        // Initialize client manager for EXCHANGE_ID. The DS presents
        // its OWN per-device server identity (flint-pnfs-ds-<id>) —
        // NOT the MDS's: this DS keeps an independent client table, so
        // sharing the MDS's server_owner made kernel trunking
        // detection demand clientid parity with the MDS that only held
        // by counter-coincidence (restarted DSes churned EXCHANGE_ID
        // forever). With a unique identity, clients keep a separate
        // clean lease per DS. DS state is ephemeral by design — the
        // in-memory backend is correct regardless of MDS persistence.
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = Arc::new(ClientManager::new(
            lease_mgr,
            &config.device_id,
            crate::state_backend::memory_backend(),
        ));
        info!("✓ Client manager initialized (per-DS server identity)");

        Ok(Self { config, io_handler, registration_client, session_mgr, client_mgr, identity_created_at })
    }

    /// Start the data server
    pub async fn serve(&self) -> Result<()> {
        info!("╔════════════════════════════════════════════════════╗");
        info!("║   Flint pNFS Data Server (DS) - RUNNING           ║");
        info!("╚════════════════════════════════════════════════════╝");
        info!("");
        info!("Device ID: {}", self.config.device_id);
        info!("Listening on: {}:{}", self.config.bind.address, self.config.bind.port);
        info!("MDS Endpoint: {}", self.config.mds.endpoint);
        info!("Block Devices: {}", self.config.bdevs.len());
        for bdev in &self.config.bdevs {
            info!("  - {} mounted at {}", bdev.name, bdev.mount_point);
            if let Some(ref spdk_vol) = bdev.spdk_volume {
                info!("    SPDK volume: {}", spdk_vol);
            }
        }
        info!("");

        // Register with MDS
        info!("Registering with MDS at {}...", self.config.mds.endpoint);
        if let Err(e) = self.register_with_mds().await {
            error!("Failed to register with MDS: {}", e);
            error!("Continuing anyway, will retry...");
        }

        // Start heartbeat sender
        self.start_heartbeat_sender();

        // Start status reporter
        self.start_status_reporter();

        // Start the DsControl gRPC listener (MDS → DS commands). Not
        // optional in production: without it, a client truncate of a
        // striped file parks that file dirty on the MDS until this
        // listener appears (stale stripe bytes must be cut before the
        // MDS lets I/O resume).
        self.start_control_listener();

        info!("✅ Data Server is ready to serve I/O requests");
        info!("");

        // Start TCP server
        let addr = format!("{}:{}", self.config.bind.address, self.config.bind.port);
        self.serve_tcp(&addr).await
    }

    /// Serve minimal NFS (READ/WRITE/COMMIT only) over TCP
    async fn serve_tcp(&self, addr: &str) -> Result<()> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| crate::pnfs::Error::Io(e))?;
        
        info!("🚀 pNFS DS TCP server listening on {}", addr);
        info!("   Serving: EXCHANGE_ID, CREATE_SESSION, SEQUENCE, READ, WRITE, COMMIT operations");
        info!("");
        
        let mut connection_count = 0u64;

        loop {
            let (stream, peer) = listener.accept()
                .await
                .map_err(|e| crate::pnfs::Error::Io(e))?;
            
            connection_count += 1;
            info!("📡 New TCP connection #{} from {}", connection_count, peer);
            
            let io_handler = Arc::clone(&self.io_handler);
            let session_mgr = Arc::clone(&self.session_mgr);
            let client_mgr = Arc::clone(&self.client_mgr);
            let conn_id = connection_count;
            
            tokio::spawn(async move {
                debug!("🚀 Spawned handler task for connection #{} from {}", conn_id, peer);
                if let Err(e) = Self::handle_tcp_connection(stream, io_handler, session_mgr, client_mgr, peer).await {
                    warn!("❌ Connection #{} from {} error: {}", conn_id, peer, e);
                } else {
                    info!("✓ TCP connection #{} from {} closed cleanly", conn_id, peer);
                }
            });
        }
    }

    /// Handle a single TCP connection (minimal NFS server)
    async fn handle_tcp_connection(
        stream: TcpStream,
        io_handler: Arc<IoOperationHandler>,
        session_mgr: Arc<DsSessionManager>,
        client_mgr: Arc<ClientManager>,
        peer: std::net::SocketAddr,
    ) -> std::io::Result<()> {
        use tokio::time::Instant;

        let connect_time = Instant::now();
        debug!("🔌 DS connection handler started for {}", peer);

        stream.set_nodelay(true)?;

        let (reader, writer) = stream.into_split();
        let mut reader = tokio::io::BufReader::with_capacity(128 * 1024, reader);
        // Mutex-serialized shared writer: pipelined replies are
        // written from concurrent tasks and must not interleave
        // frames on the wire (pipeline invariant I1).
        let writer = Arc::new(tokio::sync::Mutex::new(
            BufWriter::with_capacity(128 * 1024, writer),
        ));

        // Per-connection RPC pipelining (RFC 8881 §2.10.6): DS READ/
        // WRITE/COMMIT dispatch concurrently up to
        // FLINT_NFS_MAX_INFLIGHT (default 64, 0 = sequential).
        let pipeline = crate::nfs::pipeline::ConnectionPipeline::from_env();

        let mut buf = BytesMut::with_capacity(128 * 1024);
        let mut rpc_count = 0;

        loop {
            // Read RPC record marker
            let mut marker_buf = [0u8; 4];
            match reader.read_exact(&mut marker_buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    let duration = connect_time.elapsed();
                    info!("🔌 DS connection from {} closed after {:?} ({} RPCs)", 
                          peer, duration, rpc_count);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }

            let marker = u32::from_be_bytes(marker_buf);
            let length = (marker & 0x7FFFFFFF) as usize;

            if length > 4 * 1024 * 1024 {
                warn!("❌ Rejecting oversized RPC: {} bytes", length);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "RPC too large",
                ));
            }

            // Read message
            buf.clear();
            buf.reserve(length);
            unsafe { buf.set_len(length); }
            reader.read_exact(&mut buf[..length]).await?;

            let request = buf.split().freeze();

            // Dispatch through the pipeline; the reply is framed and
            // flushed under the shared writer mutex.
            let io_c = Arc::clone(&io_handler);
            let sess_c = Arc::clone(&session_mgr);
            let client_c = Arc::clone(&client_mgr);
            let writer_c = Arc::clone(&writer);
            // Backlog hint: bytes already buffered mean the client is
            // pipelining, so concurrent dispatch pays for its overhead.
            let more_queued = !reader.buffer().is_empty();
            pipeline.submit(
                request,
                more_queued,
                move |req| Self::dispatch_minimal_nfs(req, io_c, sess_c, client_c),
                move |reply| async move {
                    let reply_marker = 0x80000000 | reply.len() as u32;
                    let mut w = writer_c.lock().await;
                    w.write_all(&reply_marker.to_be_bytes()).await?;
                    w.write_all(&reply).await?;
                    w.flush().await
                },
            ).await?;
            rpc_count += 1;
        }
    }

    /// Dispatch minimal NFS RPC (SEQUENCE/READ/WRITE/COMMIT only)
    async fn dispatch_minimal_nfs(
        request: Bytes,
        io_handler: Arc<IoOperationHandler>,
        session_mgr: Arc<DsSessionManager>,
        client_mgr: Arc<ClientManager>,
    ) -> Bytes {
        // Parse RPC call
        let (call, args) = match CallMessage::decode_with_args(request) {
            Ok(result) => result,
            Err(e) => {
                warn!("❌ Failed to parse RPC: {}", e);
                return ReplyBuilder::garbage_args(0).into();
            }
        };

        debug!("DS RPC: xid={}, procedure={}", call.xid, call.procedure);

        // Validate program/version
        if call.program != NFS4_PROGRAM || call.version != 4 {
            return ReplyBuilder::prog_unavail(call.xid);
        }

        // Handle procedure
        match call.procedure {
            procedure::NULL => {
                ReplyBuilder::success(call.xid).finish()
            }

            procedure::COMPOUND => {
                Self::handle_minimal_compound(call, args, io_handler, session_mgr, client_mgr).await
            }

            _ => ReplyBuilder::proc_unavail(call.xid),
        }
    }

    /// Handle COMPOUND with SEQUENCE/READ/WRITE/COMMIT
    async fn handle_minimal_compound(
        call: CallMessage,
        args: Bytes,
        io_handler: Arc<IoOperationHandler>,
        session_mgr: Arc<DsSessionManager>,
        client_mgr: Arc<ClientManager>,
    ) -> Bytes {
        let mut decoder = XdrDecoder::new(args);

        // Decode COMPOUND header
        let tag_len = match decoder.decode_u32() {
            Ok(len) => len,
            Err(_) => return ReplyBuilder::garbage_args(call.xid),
        };
        
        // Skip tag
        for _ in 0..((tag_len + 3) / 4) {
            let _ = decoder.decode_u32();
        }

        let minor_version = match decoder.decode_u32() {
            Ok(v) => v,
            Err(_) => return ReplyBuilder::garbage_args(call.xid),
        };

        let op_count = match decoder.decode_u32() {
            Ok(c) => c,
            Err(_) => return ReplyBuilder::garbage_args(call.xid),
        };

        debug!("DS COMPOUND: minor_version={}, {} operations", minor_version, op_count);

        // Process operations (only READ/WRITE/COMMIT supported)
        let mut results: Vec<(u32, Nfs4Status, Bytes)> = Vec::new();  // (opcode, status, data)
        let mut current_fh: Option<Vec<u8>> = None;

        for _ in 0..op_count {
            let opcode = match decoder.decode_u32() {
                Ok(op) => op,
                Err(e) => {
                    warn!("Failed to decode opcode: {}", e);
                    break;
                }
            };

            debug!("DS: Processing opcode={}", opcode);
            let (status, result_data) = match opcode {
                opcode::EXCHANGE_ID => {
                    // Decode EXCHANGE_ID arguments properly for server trunking
                    // Per RFC 8881 Section 18.35 - client_owner structure has verifier FIRST
                    
                    // Decode verifier (8 bytes)
                    let verifier = match decoder.decode_u64() {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("DS: Failed to decode EXCHANGE_ID verifier: {}", e);
                            results.push((opcode, Nfs4Status::BadXdr, Bytes::new()));
                            continue;
                        }
                    };
                    
                    // Decode client owner (opaque)
                    let client_owner = match decoder.decode_opaque() {
                        Ok(bytes) => bytes.to_vec(),
                        Err(e) => {
                            warn!("DS: Failed to decode EXCHANGE_ID client_owner: {}", e);
                            results.push((opcode, Nfs4Status::BadXdr, Bytes::new()));
                            continue;
                        }
                    };
                    
                    // Decode flags
                    let client_flags = decoder.decode_u32().unwrap_or(0);
                    
                    // Decode state_protect (we use SP4_NONE)
                    let state_protect = decoder.decode_u32().unwrap_or(0);
                    
                    // Skip optional impl_id
                    let has_impl_id = decoder.decode_bool().unwrap_or(false);
                    if has_impl_id {
                        let _ = decoder.decode_opaque();
                    }
                    
                    info!("📥 DS: EXCHANGE_ID REQUEST from client:");
                    info!("   client_owner={:?}", String::from_utf8_lossy(&client_owner));
                    info!("   verifier=0x{:016x}", verifier);
                    info!("   client_flags=0x{:08x}", client_flags);
                    info!("   state_protect=0x{:08x}", state_protect);
                    
                    // Use ClientManager to get consistent clientid
                    // CRITICAL: This ensures DS returns same clientid as MDS for same client_owner!
                    use crate::nfs::v4::state::client::ExchangeIdOutcome;
                    let outcome = client_mgr.exchange_id(
                        client_owner.clone(),
                        verifier,
                        client_flags,
                        Vec::new(), // DS doesn't currently track principal
                    );
                    let (clientid, sequenceid, is_new) = match outcome {
                        ExchangeIdOutcome::NewUnconfirmed { client_id, sequence_id } =>
                            (client_id, sequence_id, true),
                        ExchangeIdOutcome::ExistingConfirmed { client_id, sequence_id } =>
                            (client_id, sequence_id, false),
                        // The DS path was historically tuple-returning and never
                        // exercised the UPD/error branches; map them to a fresh
                        // record for now and warn so we can revisit when the DS
                        // grows real client identity tracking.
                        other => {
                            warn!("DS: EXCHANGE_ID outcome {:?}, treating as new", other);
                            (0u64, 0u32, true)
                        }
                    };
                    info!("DS: EXCHANGE_ID - client_owner={:?}, verifier={}, clientid={}, is_new={}",
                          String::from_utf8_lossy(&client_owner), verifier, clientid, is_new);
                    
                    // Build response
                    let mut encoder = XdrEncoder::new();
                    
                    // clientid (8 bytes) - from ClientManager, matches MDS for same client
                    encoder.encode_u64(clientid);
                    
                    // sequenceid (4 bytes)
                    encoder.encode_u32(sequenceid);
                    
                    // Build response flags
                    let mut response_flags = exchgid_flags::USE_PNFS_DS;  // 0x00040000
                    
                    // Set CONFIRMED_R if this is an existing client
                    if !is_new {
                        response_flags |= exchgid_flags::CONFIRMED_R;
                    }
                    
                    // Echo back ALL client capability flags
                    if client_flags & exchgid_flags::SUPP_MOVED_REFER != 0 {
                        response_flags |= exchgid_flags::SUPP_MOVED_REFER;
                    }
                    if client_flags & exchgid_flags::SUPP_MOVED_MIGR != 0 {
                        response_flags |= exchgid_flags::SUPP_MOVED_MIGR;
                    }
                    if client_flags & exchgid_flags::BIND_PRINC_STATEID != 0 {
                        response_flags |= exchgid_flags::BIND_PRINC_STATEID;
                    }
                    
                    encoder.encode_u32(response_flags);
                    
                    // state_protect (4 bytes) - SP4_NONE
                    encoder.encode_u32(0);
                    
                    // server_owner (so_minor_id + so_major_id)
                    // MUST match MDS for server trunking to work!
                    let server_owner = client_mgr.server_owner();
                    encoder.encode_u64(0);  // so_minor_id
                    encoder.encode_string(server_owner);  // so_major_id
                    
                    // server_scope - MUST match MDS for server trunking!
                    let server_scope = client_mgr.server_scope();
                    encoder.encode_opaque(server_scope);
                    
                    // server_impl_id (optional) - empty for simplicity
                    encoder.encode_u32(0);  // impl_id array count = 0
                    
                    let result_bytes = encoder.finish();
                    
                    info!("🔍 DS EXCHANGE_ID response encoding:");
                    info!("   clientid={} (0x{:016x})", clientid, clientid);
                    info!("   sequenceid={}", sequenceid);
                    info!("   flags=0x{:08x}", response_flags);
                    info!("   server_owner={:?}", server_owner);
                    info!("   server_scope={:?}", String::from_utf8_lossy(server_scope));
                    info!("   Total bytes: {}", result_bytes.len());
                    info!("   First 80 bytes: {:02x?}", &result_bytes[..result_bytes.len().min(80)]);
                    
                    (Nfs4Status::Ok, result_bytes)
                }

                opcode::CREATE_SESSION => {
                    // Decode CREATE_SESSION arguments
                    let clientid = decoder.decode_u64().unwrap_or(0);
                    let sequence = decoder.decode_u32().unwrap_or(0);
                    
                    // Skip channel attributes (complex structure, not critical for basic functionality)
                    // Just decode enough to not break XDR stream
                    let _ = decoder.decode_u32(); // fore attributes count
                    let _ = decoder.decode_u32(); // back attributes count  
                    let _ = decoder.decode_u32(); // cb_program
                    
                    // Skip security parameters array
                    let sec_count = decoder.decode_u32().unwrap_or(0);
                    for _ in 0..sec_count {
                        let _ = decoder.decode_u32();
                    }
                    
                    info!("DS: CREATE_SESSION - clientid={}, sequence={}", clientid, sequence);
                    
                    // Create session via session manager
                    match session_mgr.create_session(clientid) {
                        Ok(sessionid) => {
                            let mut encoder = XdrEncoder::new();
                            
                            // sessionid (16 bytes)
                            encoder.encode_fixed_opaque(&sessionid);
                            
                            // sequenceid
                            encoder.encode_u32(sequence);
                            
                            // flags (0 for now)
                            encoder.encode_u32(0);
                            
                            // fore_chan_attrs - MUST match RFC 8881 Section 18.36
                            encoder.encode_u32(0);       // header_pad_size (CRITICAL: was missing!)
                            encoder.encode_u32(1048576); // max_rqst_sz (1MB)
                            encoder.encode_u32(1048576); // max_resp_sz (1MB)
                            encoder.encode_u32(4096);    // max_resp_sz_cached
                            encoder.encode_u32(128);     // max_ops
                            encoder.encode_u32(64);      // max_reqs (match MDS)
                            encoder.encode_u32(0);       // rdma_ird count (array)
                            
                            // back_chan_attrs - for callbacks
                            encoder.encode_u32(0);       // header_pad_size (CRITICAL: was missing!)
                            encoder.encode_u32(4096);    // max_rqst_sz (smaller for callbacks)
                            encoder.encode_u32(4096);    // max_resp_sz
                            encoder.encode_u32(0);       // max_resp_sz_cached
                            encoder.encode_u32(2);       // max_ops (callbacks are simple)
                            encoder.encode_u32(16);      // max_reqs (match MDS)
                            encoder.encode_u32(0);       // rdma_ird count (array)
                            
                            info!("DS: CREATE_SESSION successful - sessionid={:02x?}", &sessionid[0..8]);
                            (Nfs4Status::Ok, encoder.finish())
                        }
                        Err(e) => {
                            warn!("DS: CREATE_SESSION failed: {}", e);
                            (Nfs4Status::BadSession, Bytes::new())
                        }
                    }
                }

                opcode::SEQUENCE => {
                    // Decode SEQUENCE arguments
                    let sessionid = match decoder.decode_fixed_opaque(16) {
                        Ok(bytes) => {
                            let mut sid = [0u8; 16];
                            sid.copy_from_slice(&bytes);
                            sid
                        }
                        Err(_) => {
                            results.push((opcode, Nfs4Status::BadXdr, Bytes::new()));
                            continue;
                        }
                    };
                    
                    let sequenceid = decoder.decode_u32().unwrap_or(0);
                    let slotid = decoder.decode_u32().unwrap_or(0);
                    let highest_slotid = decoder.decode_u32().unwrap_or(0);
                    let _cache_this = decoder.decode_bool().unwrap_or(false);
                    
                    // Handle SEQUENCE via session manager
                    match session_mgr.handle_sequence(sessionid, sequenceid, slotid, highest_slotid) {
                        Ok(result) => {
                            let mut encoder = XdrEncoder::new();
                            encoder.encode_fixed_opaque(&result.sessionid);
                            encoder.encode_u32(result.sequenceid);
                            encoder.encode_u32(result.slotid);
                            encoder.encode_u32(result.highest_slotid);
                            encoder.encode_u32(result.target_highest_slotid);
                            encoder.encode_u32(result.status_flags);
                            (Nfs4Status::Ok, encoder.finish())
                        }
                        Err(err_code) => {
                            // Convert NFS4 error code to Nfs4Status
                            // For now, use a generic error
                            warn!("SEQUENCE error: {}", err_code);
                            (Nfs4Status::BadSession, Bytes::new())
                        }
                    }
                }

                opcode::PUTFH => {
                    // Store current filehandle
                    match decoder.decode_filehandle() {
                        Ok(fh) => {
                            current_fh = Some(fh.data);
                            (Nfs4Status::Ok, Bytes::new())
                        }
                        Err(_) => (Nfs4Status::BadHandle, Bytes::new()),
                    }
                }

                opcode::READ => {
                    let _stateid = match decoder.decode_stateid() {
                        Ok(s) => s,
                        Err(_) => {
                            results.push((opcode, Nfs4Status::BadXdr, Bytes::new()));
                            continue;
                        }
                    };
                    let offset = decoder.decode_u64().unwrap_or(0);
                    let count = decoder.decode_u32().unwrap_or(0);

                    if let Some(ref fh) = current_fh {
                        match io_handler.read(fh, offset, count).await {
                            Ok(read_result) => {
                                let mut encoder = XdrEncoder::new();
                                encoder.encode_bool(read_result.eof);
                                encoder.encode_opaque(&read_result.data);
                                (Nfs4Status::Ok, encoder.finish())
                            }
                            Err(_) => (Nfs4Status::Io, Bytes::new()),
                        }
                    } else {
                        (Nfs4Status::NoFileHandle, Bytes::new())
                    }
                }

                opcode::WRITE => {
                    let _stateid = decoder.decode_stateid();
                    let offset = decoder.decode_u64().unwrap_or(0);
                    let stable = decoder.decode_u32().unwrap_or(2);
                    let data = decoder.decode_opaque().unwrap_or_else(|_| Bytes::new());

                    if let Some(ref fh) = current_fh {
                        let stable_level = match stable {
                            0 => WriteStable::Unstable,
                            1 => WriteStable::DataSync,
                            _ => WriteStable::FileSync,
                        };

                        match io_handler.write(fh, offset, data, stable_level).await {
                            Ok(write_result) => {
                                let mut encoder = XdrEncoder::new();
                                encoder.encode_u32(write_result.count);
                                // committed: NEVER claim FILE_SYNC. FILE_SYNC
                                // means data AND metadata are durable — but a
                                // split pNFS DS can't make the file's SIZE
                                // durable (it lives on the MDS, updated by the
                                // client's LAYOUTCOMMIT). The Linux files-
                                // layout client SKIPS layoutcommit bookkeeping
                                // entirely when a DS write returns FILE_SYNC
                                // (filelayout_set_layoutcommit), so echoing
                                // FILE_SYNC made size-extending stable writes
                                // vanish: stat returned the stale stub size.
                                // Found by fsx (both buffered flush and
                                // O_DIRECT) at the FIRST hole-extending write.
                                // DATA_SYNC is the honest answer: data
                                // durable, metadata not — the client then
                                // tracks lwb and sends LAYOUTCOMMIT.
                                let committed = if stable == 0 { 0 } else { 1 };
                                encoder.encode_u32(committed);
                                encoder.encode_fixed_opaque(&write_result.verifier);
                                (Nfs4Status::Ok, encoder.finish())
                            }
                            Err(e) => {
                                warn!("DS WRITE failed at offset {}: {}", offset, e);
                                (Self::io_error_to_nfs_status(&e), Bytes::new())
                            }
                        }
                    } else {
                        (Nfs4Status::NoFileHandle, Bytes::new())
                    }
                }

                opcode::SECINFO_NO_NAME => {
                    // Advertise supported auth flavors: AUTH_NULL, AUTH_SYS, and RPCSEC_GSS
                    let mut encoder = XdrEncoder::new();
                    encoder.encode_u32(3);  // secinfo count (3 flavors)

                    // AUTH_NULL (flavor 0)
                    encoder.encode_u32(0);

                    // AUTH_SYS (flavor 1)
                    encoder.encode_u32(1);

                    // RPCSEC_GSS (flavor 6) - Kerberos
                    encoder.encode_u32(6);
                    // OID for Kerberos V5 (1.2.840.113554.1.2.2)
                    let krb5_oid = vec![0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
                    encoder.encode_opaque(&krb5_oid);  // GSS mechanism OID
                    encoder.encode_u32(0);  // QOP (quality of protection)
                    encoder.encode_u32(1);  // Service: rpc_gss_svc_none (authentication only)

                    debug!("DS: Advertised AUTH_NULL, AUTH_SYS, and RPCSEC_GSS (Kerberos)");
                    (Nfs4Status::Ok, encoder.finish())
                }

                opcode::COMMIT => {
                    let offset = decoder.decode_u64().unwrap_or(0);
                    let count = decoder.decode_u32().unwrap_or(0);

                    if let Some(ref fh) = current_fh {
                        match io_handler.commit(fh, offset, count).await {
                            Ok(commit_result) => {
                                let mut encoder = XdrEncoder::new();
                                encoder.encode_fixed_opaque(&commit_result.verifier);
                                (Nfs4Status::Ok, encoder.finish())
                            }
                            Err(e) => {
                                warn!("DS COMMIT failed at offset {}: {}", offset, e);
                                (Self::io_error_to_nfs_status(&e), Bytes::new())
                            }
                        }
                    } else {
                        (Nfs4Status::NoFileHandle, Bytes::new())
                    }
                }

                opcode::RECLAIM_COMPLETE => {
                    // Standard NFSv4.1 client startup posts RECLAIM_COMPLETE
                    // (RFC 8881 §18.51) on every minor-version-1 server it
                    // talks to — including data servers reached via pNFS.
                    // A DS holds no reclaimable open/lock state, so we
                    // accept the op (consume the rca_one_fs bool) and
                    // return OK. Without this, the kernel sees the DS as
                    // unhealthy and silently falls back to MDS-direct I/O.
                    let _rca_one_fs = decoder.decode_bool().unwrap_or(false);
                    (Nfs4Status::Ok, Bytes::new())
                }

                opcode::DESTROY_CLIENTID => {
                    // Client is cleaning up - just acknowledge it
                    let _clientid = decoder.decode_u64().unwrap_or(0);
                    info!("DS: DESTROY_CLIENTID - clientid={}", _clientid);
                    (Nfs4Status::Ok, Bytes::new())
                }
                
                opcode::DESTROY_SESSION => {
                    // Client is destroying session - acknowledge it
                    let _sessionid = decoder.decode_fixed_opaque(16).unwrap_or_default();
                    info!("DS: DESTROY_SESSION");
                    (Nfs4Status::Ok, Bytes::new())
                }

                _ => {
                    warn!("DS received unsupported operation: {}", opcode);
                    (Nfs4Status::NotSupp, Bytes::new())
                }
            };

            results.push((opcode, status, result_data));
        }

        // Encode COMPOUND response
        let mut encoder = XdrEncoder::new();
        
        // COMPOUND response header
        encoder.encode_u32(0);  // tag length
        encoder.encode_u32(Nfs4Status::Ok as u32);  // Overall status
        encoder.encode_u32(results.len() as u32);  // Result count

        // Encode each result - MUST include opcode per RFC 8881 Section 18.2
        for (opcode, status, data) in results {
            encoder.encode_u32(opcode);  // Operation opcode (CRITICAL!)
            encoder.encode_u32(status as u32);  // Operation status
            encoder.append_raw(&data);  // Operation-specific data
        }

        let compound_data = encoder.finish();

        // Build RPC reply
        let mut reply_encoder = XdrEncoder::new();
        reply_encoder.encode_u32(call.xid);
        reply_encoder.encode_u32(1);  // REPLY
        reply_encoder.encode_u32(0);  // MSG_ACCEPTED
        reply_encoder.encode_u32(0);  // AUTH_NONE
        reply_encoder.encode_u32(0);  // Auth length
        reply_encoder.encode_u32(0);  // SUCCESS
        reply_encoder.append_raw(&compound_data);

        reply_encoder.finish()
    }

    /// The client-reachable address this DS advertises to the MDS.
    ///
    /// Precedence:
    /// 1. `FLINT_DS_ADVERTISE_ADDR` — a stable address in front of the
    ///    pod, e.g. a per-pod Service DNS name
    ///    (`flint-pnfs-ds-0.ns.svc.cluster.local`). The MDS resolves
    ///    names to IPv4 at GETDEVICEINFO-encode time
    ///    (`endpoint_to_uaddr`), and a ClusterIP behind the name stays
    ///    stable across pod reschedules — so kernel clients' cached
    ///    device info never goes stale.
    /// 2. `POD_IP` — direct pod address (stale after a reschedule
    ///    until clients re-fetch device info; fine for scratch tiers).
    /// 3. The bind address (host-process deployments).
    fn advertise_address(&self) -> String {
        std::env::var("FLINT_DS_ADVERTISE_ADDR")
            .or_else(|_| std::env::var("POD_IP"))
            .unwrap_or_else(|_| self.config.bind.address.clone())
    }

    /// Register with the MDS via gRPC
    async fn register_with_mds(&self) -> Result<()> {
        let mut client = self.registration_client.lock().await;

        let endpoint = format!("{}:{}", self.advertise_address(), self.config.bind.port);
        info!("📡 Registering with endpoint: {} (FLINT_DS_ADVERTISE_ADDR={:?}, POD_IP={:?})",
              endpoint,
              std::env::var("FLINT_DS_ADVERTISE_ADDR").ok(),
              std::env::var("POD_IP").ok());
        
        let mount_points: Vec<String> = self.config.bdevs
            .iter()
            .map(|b| b.mount_point.clone())
            .collect();
        
        // Real capacity truth from the export filesystem (was a 1 TB
        // placeholder — the registry is only as honest as this number).
        let (capacity, used) = mount_points
            .first()
            .and_then(|mp| nix::sys::statvfs::statvfs(std::path::Path::new(mp)).ok())
            .map(|vfs| {
                let total = vfs.blocks() as u64 * vfs.fragment_size() as u64;
                let avail = vfs.blocks_available() as u64 * vfs.fragment_size() as u64;
                (total, total.saturating_sub(avail))
            })
            .unwrap_or((0, 0));

        match client.register(
            self.config.device_id.clone(),
            endpoint,
            mount_points,
            capacity,
            used,
            self.identity_created_at,
            self.config.bind.control_port.unwrap_or(0) as u32,
        ).await {
            Ok(true) => {
                info!("✅ Successfully registered with MDS");
                Ok(())
            }
            Ok(false) => {
                warn!("⚠️ Registration was rejected by MDS");
                Ok(())  // Don't fail, will retry
            }
            Err(e) => {
                error!("❌ Registration failed: {}", e);
                Ok(())  // Don't fail, will retry
            }
        }
    }

    /// Start heartbeat sender in the background.
    ///
    /// Runs on a DEDICATED OS thread with its own current-thread
    /// runtime, with its own RegistrationClient (own gRPC channel).
    /// Phase 3 finding (mds-restart-load drill): the data path's
    /// block_in_place I/O tiering can occupy every worker of the
    /// 4-thread main runtime for tens of seconds under sustained
    /// write load, silently starving a tokio::spawn'd heartbeat —
    /// the MDS then marks a HEALTHY, busy DS stale and recalls its
    /// layouts. Liveness signalling must never share a scheduler
    /// with the data path. (A shared client would not be enough:
    /// its channel's I/O driver lives on the runtime that created
    /// it — the main one.)
    fn start_heartbeat_sender(&self) {
        let heartbeat_interval_secs = self.config.mds.heartbeat_interval;
        let mds_endpoint = self.config.mds.endpoint.clone();

        // Capture config data needed for re-registration
        let device_id = self.config.device_id.clone();
        let bind_port = self.config.bind.port;
        let control_port = self.config.bind.control_port.unwrap_or(0) as u32;
        let identity_created_at = self.identity_created_at;

        // Same precedence as initial registration — see advertise_address().
        let advertise_address = self.advertise_address();

        let mount_points: Vec<String> = self.config.bdevs
            .iter()
            .map(|b| b.mount_point.clone())
            .collect();

        // For DELETE_STRIPE_FILE fd-cache eviction — unlinking alone
        // frees nothing while the data path still holds the fd open.
        let io_handler = Arc::clone(&self.io_handler);

        std::thread::Builder::new()
            .name("ds-heartbeat".into())
            .spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("heartbeat runtime");
        rt.block_on(async move {
            let mut client = RegistrationClient::new(
                device_id.clone(),
                mds_endpoint,
                Duration::from_secs(heartbeat_interval_secs),
            );
            let mut heartbeat_interval = interval(Duration::from_secs(heartbeat_interval_secs));
            let mut failure_count = 0u32;

            let data_dir = std::path::PathBuf::from(
                mount_points.first().cloned().unwrap_or_else(|| "/data".to_string()),
            );

            loop {
                heartbeat_interval.tick().await;

                // Real capacity truth from the export filesystem —
                // the registry (and any future capacity-aware
                // placement) is only as honest as this number.
                let (capacity, used) = match nix::sys::statvfs::statvfs(&data_dir) {
                    Ok(vfs) => {
                        let total = vfs.blocks() as u64 * vfs.fragment_size() as u64;
                        let avail = vfs.blocks_available() as u64 * vfs.fragment_size() as u64;
                        (total, total.saturating_sub(avail))
                    }
                    Err(e) => {
                        warn!("statvfs({:?}) failed: {} — reporting zero capacity", data_dir, e);
                        (0, 0)
                    }
                };
                let active_connections = 0u32;

                let mut reregister_now = false;
                match client.heartbeat(capacity, used, active_connections).await {
                    Ok((true, instructions)) => {
                        debug!("✅ Heartbeat acknowledged");
                        failure_count = 0;
                        Self::apply_mds_instructions(&data_dir, &io_handler, instructions);
                    }
                    Ok((false, _)) => {
                        // A NACK is not a transport blip: the MDS answered
                        // and said "unknown device" — it restarted and lost
                        // its (deliberately in-memory) registry. Waiting out
                        // the 3-failure threshold here just extends the
                        // window where LAYOUTGETs refuse because this
                        // device's placement isn't Active yet. Re-register
                        // on THIS tick (Phase 3: within one heartbeat).
                        warn!("⚠️ Heartbeat not acknowledged — MDS doesn't know us; re-registering now");
                        reregister_now = true;
                    }
                    Err(e) => {
                        // Transport errors stay on the 3-strike path: the
                        // MDS may just be mid-restart, and hammering
                        // register() at a dead endpoint adds nothing over
                        // the next heartbeat's retry.
                        error!("❌ Heartbeat failed: {}", e);
                        failure_count += 1;
                    }
                }

                if reregister_now || failure_count >= 3 {
                    error!(
                        "MDS lost this device ({}), attempting re-registration",
                        if reregister_now { "heartbeat NACK".to_string() } else { format!("{} transport failures", failure_count) }
                    );
                    
                    // Attempt re-registration (use advertise_address, not bind.address!)
                    let endpoint = format!("{}:{}", advertise_address, bind_port);
                    match client.register(
                        device_id.clone(),
                        endpoint.clone(),
                        mount_points.clone(),
                        capacity,
                        used,
                        identity_created_at,
                        control_port,
                    ).await {
                        Ok(true) => {
                            info!("✅ Re-registration successful");
                            failure_count = 0;
                        }
                        Ok(false) => {
                            warn!("⚠️ Re-registration rejected by MDS, will retry");
                            failure_count = 0;  // Reset to try again later
                        }
                        Err(e) => {
                            error!("❌ Re-registration failed: {}, will retry", e);
                            failure_count = 0;  // Reset to try again later
                        }
                    }
                }
            }
        });
            })
            .expect("spawn ds-heartbeat thread");

        info!(
            "Heartbeat sender started on dedicated thread (interval: {} seconds)",
            heartbeat_interval_secs
        );
    }

    /// Map a DS I/O failure to the honest NFS status. ENOSPC/EDQUOT
    /// must surface as NOSPC/DQUOT so the client (and its
    /// application) sees "No space left on device" instead of a
    /// generic EIO — found by the ENOSPC drill: a full DS export
    /// reported EIO, which reads as data-path breakage rather than
    /// capacity exhaustion.
    fn io_error_to_nfs_status(e: &crate::pnfs::Error) -> Nfs4Status {
        if let crate::pnfs::Error::Io(io_err) = e {
            // nix::errno wraps the raw OS errno portably (libc isn't a
            // direct dep; nix already is).
            match io_err.raw_os_error().map(nix::errno::Errno::from_i32) {
                Some(nix::errno::Errno::ENOSPC) => return Nfs4Status::NoSpc,
                Some(nix::errno::Errno::EDQUOT) => return Nfs4Status::DQuot,
                Some(nix::errno::Errno::EROFS) => return Nfs4Status::RoFs,
                _ => {}
            }
        }
        Nfs4Status::Io
    }

    /// Apply MDS-piggybacked heartbeat instructions. Today that's
    /// DELETE_STRIPE_FILE: best-effort unlink of an orphaned stripe
    /// file (its MDS-side file was REMOVEd or renamed-over). `details`
    /// is a path relative to the DS data dir; traversal components are
    /// rejected — the MDS never mints them, but this arrives over the
    /// control channel and costs nothing to check.
    ///
    /// The unlink MUST be paired with an fd-cache eviction: the data
    /// path caches an open fd per stripe file, and an unlinked-but-open
    /// file keeps its blocks allocated — without the eviction a
    /// "removed" stripe file frees no space until the DS restarts
    /// (ENOSPC-drill finding: the export stayed full after cleanup and
    /// the next small write died with ENOSPC).
    fn apply_mds_instructions(
        data_dir: &std::path::Path,
        io_handler: &IoOperationHandler,
        instructions: Vec<crate::pnfs::grpc::Instruction>,
    ) {
        use crate::pnfs::grpc::InstructionType;
        for ins in instructions {
            if ins.r#type != InstructionType::DeleteStripeFile as i32 {
                continue;
            }
            let rel = std::path::Path::new(&ins.details);
            if rel.is_absolute()
                || rel.components().any(|c| matches!(c, std::path::Component::ParentDir))
            {
                warn!("🧹 refusing suspicious cleanup path {:?}", ins.details);
                continue;
            }
            let target = data_dir.join(rel);
            match std::fs::remove_file(&target) {
                Ok(()) => info!("🧹 stripe file removed: {:?}", target),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!("🧹 stripe file already absent: {:?}", target);
                }
                Err(e) => warn!("🧹 stripe cleanup failed for {:?}: {}", target, e),
            }
            // Evict regardless of unlink outcome — an entry may
            // survive from an earlier unlink that raced or predates
            // this code, and a stale fd only ever pins dead blocks.
            let evicted = io_handler.evict_path(&target);
            if evicted > 0 {
                info!("🧹 evicted {} cached fd(s) for {:?} — blocks now freeable", evicted, target);
            }
        }
    }

    /// Start the DsControl gRPC listener (MDS → DS synchronous
    /// commands, today TruncateStripeFile). Token-gated with the same
    /// FLINT_PNFS_CONTROL_TOKEN as the MDS's control plane. No
    /// configured control port = no listener (dev-only; the MDS parks
    /// truncated striped files dirty until it can reach one).
    fn start_control_listener(&self) {
        let Some(port) = self.config.bind.control_port else {
            warn!(
                "⚠️ bind.controlPort unset — no DsControl listener; truncates of striped \
                 files will park them dirty on the MDS (set controlPort in production)"
            );
            return;
        };
        if std::env::var("FLINT_PNFS_CONTROL_TOKEN").is_err() {
            warn!(
                "⚠️ FLINT_PNFS_CONTROL_TOKEN unset — DsControl listener is UNAUTHENTICATED; \
                 anyone who can reach port {} can truncate stripe files", port
            );
        }
        let addr: std::net::SocketAddr =
            match format!("{}:{}", self.config.bind.address, port).parse() {
                Ok(a) => a,
                Err(e) => {
                    error!("❌ bad DsControl bind address: {}", e);
                    return;
                }
            };
        let svc = DsControlService {
            device_id: self.config.device_id.clone(),
            data_dir: std::path::PathBuf::from(
                self.config.bdevs.first().map(|b| b.mount_point.clone())
                    .unwrap_or_else(|| "/data".to_string()),
            ),
        };
        info!("🎛️ DsControl gRPC listener on {}", addr);
        tokio::spawn(async move {
            let server = tonic::transport::Server::builder()
                .add_service(tonic::service::interceptor::InterceptedService::new(
                    crate::pnfs::grpc::DsControlServer::new(svc),
                    crate::pnfs::grpc::check_control_token,
                ))
                .serve(addr);
            if let Err(e) = server.await {
                error!("❌ DsControl listener died: {}", e);
            }
        });
    }

    /// Start status reporter in background
    fn start_status_reporter(&self) {
        let device_id = self.config.device_id.clone();
        let bdev_count = self.config.bdevs.len();
        let mds_endpoint = self.config.mds.endpoint.clone();

        tokio::spawn(async move {
            let mut status_interval = interval(Duration::from_secs(60));

            loop {
                status_interval.tick().await;

                info!("─────────────────────────────────────────────────────");
                info!("DS Status Report:");
                info!("  Device ID: {}", device_id);
                info!("  Block Devices: {}", bdev_count);
                info!("  MDS: {}", mds_endpoint);
                // TODO: Add I/O statistics
                info!("─────────────────────────────────────────────────────");
            }
        });

        info!("Status reporter started (interval: 60 seconds)");
    }

    /// Handle READ operation (opcode 25)
    /// 
    /// Uses filesystem I/O - this is the correct approach for pNFS FILE layout
    /// per RFC 8881 Chapter 13.
    pub async fn handle_read(
        &self,
        filehandle: &[u8],
        _stateid: &[u8; 16],  // Minimal validation - trust MDS
        offset: u64,
        count: u32,
    ) -> Result<Vec<u8>> {
        let result = self.io_handler.read(filehandle, offset, count).await?;
        Ok(result.data)
    }

    /// Handle WRITE operation (opcode 38)
    /// 
    /// Uses filesystem I/O - this is the correct approach for pNFS FILE layout
    /// per RFC 8881 Chapter 13.
    pub async fn handle_write(
        &self,
        filehandle: &[u8],
        _stateid: &[u8; 16],  // Minimal validation - trust MDS
        offset: u64,
        data: &[u8],
        stable: u32,
    ) -> Result<u32> {
        use crate::pnfs::ds::io::WriteStable;
        
        let stable_level = match stable {
            0 => WriteStable::Unstable,
            1 => WriteStable::DataSync,
            2 => WriteStable::FileSync,
            _ => WriteStable::FileSync,
        };
        
        let result = self.io_handler
            .write(filehandle, offset, Bytes::copy_from_slice(data), stable_level)
            .await?;
        Ok(result.count)
    }

    /// Handle COMMIT operation (opcode 5)
    /// 
    /// Uses filesystem sync - this is the correct approach for pNFS FILE layout
    /// per RFC 8881 Chapter 13.
    pub async fn handle_commit(
        &self,
        filehandle: &[u8],
        offset: u64,
        count: u32,
    ) -> Result<[u8; 8]> {
        let result = self.io_handler.commit(filehandle, offset, count).await?;
        Ok(result.verifier)
    }
    
    /// Get the I/O handler (for integration with NFS dispatcher)
    pub fn io_handler(&self) -> Arc<IoOperationHandler> {
        Arc::clone(&self.io_handler)
    }
}



/// MDS → DS command surface (DsControl). Deliberately tiny and
/// paranoid: identity-guarded (a request naming another device_id is
/// refused — mutating a foreign device's volume corrupts silently),
/// path-guarded (no absolute or parent-dir components), idempotent
/// (truncating an absent stripe file is success — nothing written
/// means nothing stale to cut).
pub struct DsControlService {
    device_id: String,
    data_dir: std::path::PathBuf,
}

#[tonic::async_trait]
impl crate::pnfs::grpc::DsControl for DsControlService {
    async fn truncate_stripe_file(
        &self,
        request: tonic::Request<crate::pnfs::grpc::TruncateStripeFileRequest>,
    ) -> std::result::Result<
        tonic::Response<crate::pnfs::grpc::TruncateStripeFileResponse>,
        tonic::Status,
    > {
        let req = request.into_inner();
        let refuse = |message: String| {
            warn!("🎛️ TruncateStripeFile refused: {}", message);
            Ok(tonic::Response::new(
                crate::pnfs::grpc::TruncateStripeFileResponse { ok: false, message },
            ))
        };
        if req.device_id != self.device_id {
            return refuse(format!(
                "identity mismatch: request is for '{}', this DS is '{}'",
                req.device_id, self.device_id
            ));
        }
        let rel = std::path::Path::new(&req.rel_path);
        if req.rel_path.is_empty()
            || rel.is_absolute()
            || rel.components().any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return refuse(format!("suspicious stripe path {:?}", req.rel_path));
        }
        let target = self.data_dir.join(rel);
        match std::fs::OpenOptions::new().write(true).open(&target) {
            Ok(f) => match f.set_len(req.new_length) {
                Ok(()) => {
                    info!("✂️ stripe file {:?} set_len({})", target, req.new_length);
                    Ok(tonic::Response::new(
                        crate::pnfs::grpc::TruncateStripeFileResponse {
                            ok: true,
                            message: String::new(),
                        },
                    ))
                }
                Err(e) => refuse(format!("set_len({}) on {:?}: {}", req.new_length, target, e)),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("✂️ stripe file {:?} absent — nothing to truncate", target);
                Ok(tonic::Response::new(
                    crate::pnfs::grpc::TruncateStripeFileResponse {
                        ok: true,
                        message: String::new(),
                    },
                ))
            }
            Err(e) => refuse(format!("open {:?}: {}", target, e)),
        }
    }
}

#[cfg(test)]
mod ds_control_tests {
    use super::*;
    use crate::pnfs::grpc::{DsControl, TruncateStripeFileRequest};

    fn svc(dir: &std::path::Path) -> DsControlService {
        DsControlService {
            device_id: "ds-test-1".into(),
            data_dir: dir.to_path_buf(),
        }
    }

    fn req(device_id: &str, rel_path: &str, new_length: u64) -> tonic::Request<TruncateStripeFileRequest> {
        tonic::Request::new(TruncateStripeFileRequest {
            device_id: device_id.into(),
            rel_path: rel_path.into(),
            new_length,
        })
    }

    #[tokio::test]
    async fn truncates_existing_stripe_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00000000deadbeef.stripe0");
        std::fs::write(&path, vec![7u8; 4096]).unwrap();
        let r = svc(dir.path())
            .truncate_stripe_file(req("ds-test-1", "00000000deadbeef.stripe0", 1024))
            .await.unwrap().into_inner();
        assert!(r.ok, "{}", r.message);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024);
    }

    #[tokio::test]
    async fn absent_stripe_file_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let r = svc(dir.path())
            .truncate_stripe_file(req("ds-test-1", "no-such.stripe1", 0))
            .await.unwrap().into_inner();
        assert!(r.ok, "absent file must be a no-op success: {}", r.message);
    }

    #[tokio::test]
    async fn refuses_foreign_device() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.stripe0");
        std::fs::write(&path, vec![7u8; 4096]).unwrap();
        let r = svc(dir.path())
            .truncate_stripe_file(req("ds-OTHER", "s.stripe0", 0))
            .await.unwrap().into_inner();
        assert!(!r.ok);
        assert!(r.message.contains("identity mismatch"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096, "file untouched");
    }

    #[tokio::test]
    async fn refuses_traversal_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["../escape", "/etc/passwd", ""] {
            let r = svc(dir.path())
                .truncate_stripe_file(req("ds-test-1", bad, 0))
                .await.unwrap().into_inner();
            assert!(!r.ok, "should refuse {:?}", bad);
        }
    }
}
