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

        // Initialize client manager for EXCHANGE_ID (server trunking)
        // MUST use the same server_owner and server_scope as MDS for trunking to work!
        let lease_mgr = Arc::new(LeaseManager::new());
        let client_mgr = Arc::new(ClientManager::new(lease_mgr));
        info!("✓ Client manager initialized (same server_owner/scope as MDS)");

        Ok(Self { config, io_handler, registration_client, session_mgr, client_mgr })
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
        let mut writer = BufWriter::with_capacity(128 * 1024, writer);

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

            // Process RPC (minimal NFS)
            let reply = Self::dispatch_minimal_nfs(
                request,
                Arc::clone(&io_handler),
                Arc::clone(&session_mgr),
                Arc::clone(&client_mgr),
            ).await;
            rpc_count += 1;

            // Write reply
            let reply_len = reply.len() as u32;
            let reply_marker = 0x80000000 | reply_len;
            
            writer.write_all(&reply_marker.to_be_bytes()).await?;
            writer.write_all(&reply).await?;
            writer.flush().await?;
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
                    
                    warn!("📥 DS: EXCHANGE_ID REQUEST from client:");
                    warn!("   client_owner={:?}", String::from_utf8_lossy(&client_owner));
                    warn!("   verifier=0x{:016x}", verifier);
                    warn!("   client_flags=0x{:08x}", client_flags);
                    warn!("   state_protect=0x{:08x}", state_protect);
                    
                    // Use ClientManager to get consistent clientid
                    // CRITICAL: This ensures DS returns same clientid as MDS for same client_owner!
                    let (clientid, sequenceid, is_new) = client_mgr.exchange_id(
                        client_owner.clone(),
                        verifier,
                        client_flags,
                    );
                    
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
                    
                    warn!("🔍 DS EXCHANGE_ID response encoding:");
                    warn!("   clientid={} (0x{:016x})", clientid, clientid);
                    warn!("   sequenceid={}", sequenceid);
                    warn!("   flags=0x{:08x}", response_flags);
                    warn!("   server_owner={:?}", server_owner);
                    warn!("   server_scope={:?}", String::from_utf8_lossy(server_scope));
                    warn!("   Total bytes: {}", result_bytes.len());
                    warn!("   First 80 bytes: {:02x?}", &result_bytes[..result_bytes.len().min(80)]);
                    
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

                        match io_handler.write(fh, offset, &data, stable_level).await {
                            Ok(write_result) => {
                                let mut encoder = XdrEncoder::new();
                                encoder.encode_u32(write_result.count);
                                encoder.encode_u32(stable);
                                encoder.encode_fixed_opaque(&write_result.verifier);
                                (Nfs4Status::Ok, encoder.finish())
                            }
                            Err(_) => (Nfs4Status::Io, Bytes::new()),
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
                            Err(_) => (Nfs4Status::Io, Bytes::new()),
                        }
                    } else {
                        (Nfs4Status::NoFileHandle, Bytes::new())
                    }
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

    /// Register with the MDS via gRPC
    async fn register_with_mds(&self) -> Result<()> {
        let mut client = self.registration_client.lock().await;
        
        // Use POD_IP if available (for Kubernetes), otherwise use bind address
        // In K8s, bind.address is 0.0.0.0 which clients can't reach
        let advertise_address = std::env::var("POD_IP")
            .unwrap_or_else(|_| self.config.bind.address.clone());
        
        let endpoint = format!("{}:{}", advertise_address, self.config.bind.port);
        info!("📡 Registering with endpoint: {} (POD_IP={:?})", 
              endpoint, std::env::var("POD_IP").ok());
        
        let mount_points: Vec<String> = self.config.bdevs
            .iter()
            .map(|b| b.mount_point.clone())
            .collect();
        
        // Calculate total capacity (simplified - sum all mount points)
        // TODO: Get actual filesystem capacity
        let capacity = 1_000_000_000_000u64;  // 1 TB placeholder
        let used = 0u64;

        match client.register(
            self.config.device_id.clone(),
            endpoint,
            mount_points,
            capacity,
            used,
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

    /// Start heartbeat sender in the background
    fn start_heartbeat_sender(&self) {
        let registration_client = Arc::clone(&self.registration_client);
        let heartbeat_interval_secs = self.config.mds.heartbeat_interval;
        
        // Capture config data needed for re-registration
        let device_id = self.config.device_id.clone();
        let bind_port = self.config.bind.port;
        
        // Use POD_IP if available (same as initial registration)
        let advertise_address = std::env::var("POD_IP")
            .unwrap_or_else(|_| self.config.bind.address.clone());
        
        let mount_points: Vec<String> = self.config.bdevs
            .iter()
            .map(|b| b.mount_point.clone())
            .collect();

        tokio::spawn(async move {
            let mut heartbeat_interval = interval(Duration::from_secs(heartbeat_interval_secs));
            let mut failure_count = 0u32;

            loop {
                heartbeat_interval.tick().await;

                // Send heartbeat via gRPC
                let mut client = registration_client.lock().await;
                
                // TODO: Get actual capacity/usage from filesystem
                let capacity = 1_000_000_000_000u64;
                let used = 0u64;
                let active_connections = 0u32;

                match client.heartbeat(capacity, used, active_connections).await {
                    Ok(true) => {
                        debug!("✅ Heartbeat acknowledged");
                        failure_count = 0;
                    }
                    Ok(false) => {
                        warn!("⚠️ Heartbeat not acknowledged");
                        failure_count += 1;
                    }
                    Err(e) => {
                        error!("❌ Heartbeat failed: {}", e);
                        failure_count += 1;
                    }
                }

                if failure_count >= 3 {
                    error!(
                        "Lost connection to MDS after {} failures, attempting re-registration",
                        failure_count
                    );
                    
                    // Attempt re-registration (use advertise_address, not bind.address!)
                    let endpoint = format!("{}:{}", advertise_address, bind_port);
                    match client.register(
                        device_id.clone(),
                        endpoint.clone(),
                        mount_points.clone(),
                        capacity,
                        used,
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

        info!(
            "Heartbeat sender started (interval: {} seconds)",
            heartbeat_interval_secs
        );
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
        
        let result = self.io_handler.write(filehandle, offset, data, stable_level).await?;
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


