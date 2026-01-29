//! MDS Server Implementation
//!
//! The Metadata Server extends the standard NFSv4.2 server with pNFS operations.
//! It manages data server registration, layout generation, and client state.

use crate::pnfs::config::MdsConfig;
use crate::pnfs::mds::device::{DeviceInfo, DeviceRegistry};
use crate::pnfs::mds::layout::LayoutManager;
use crate::pnfs::mds::operations::PnfsOperationHandler;
use crate::pnfs::grpc::{MdsControlService, MdsControlServer};
use crate::pnfs::Result;
use crate::nfs::rpc::{CallMessage, ReplyBuilder, AuthFlavor};
use crate::nfs::rpcsec_gss::{RpcSecGssManager, RpcGssCred, procedure as gss_proc};
use crate::nfs::xdr::{XdrEncoder, XdrDecoder};
use crate::nfs::v4::protocol::{procedure, NFS4_PROGRAM};
use crate::nfs::v4::dispatcher::CompoundDispatcher;
use crate::nfs::v4::filehandle::FileHandleManager;
use crate::nfs::v4::state::StateManager;
use crate::nfs::v4::operations::lockops::LockManager;
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// Metadata Server
pub struct MetadataServer {
    config: MdsConfig,
    device_registry: Arc<DeviceRegistry>,
    layout_manager: Arc<LayoutManager>,
    operation_handler: Arc<PnfsOperationHandler>,
    base_dispatcher: Arc<CompoundDispatcher>,
    gss_manager: Arc<RpcSecGssManager>,
}

impl MetadataServer {
    /// Create a new metadata server
    pub fn new(config: MdsConfig, exports: Vec<crate::pnfs::config::ExportConfig>) -> Result<Self> {
        info!("Initializing Metadata Server");

        // Get export path from first export, default to /data if not specified
        let export_path = exports.first()
            .map(|e| std::path::PathBuf::from(&e.path))
            .unwrap_or_else(|| std::path::PathBuf::from("/data"));
        
        info!("📂 MDS export path: {:?}", export_path);

        // Initialize file handle manager with configured export path
        let fh_manager = Arc::new(FileHandleManager::new(export_path));

        // Initialize state manager (for NFSv4 sessions, stateids)
        let state_mgr = Arc::new(StateManager::new());
        
        // Initialize lock manager
        let lock_mgr = Arc::new(LockManager::new());

        // Initialize device registry
        let device_registry = Arc::new(DeviceRegistry::new());

        // Initialize layout manager
        let layout_manager = Arc::new(LayoutManager::new(
            Arc::clone(&device_registry),
            config.layout.policy,
            config.layout.stripe_size,
        ));

        // Initialize pNFS operation handler
        let operation_handler = Arc::new(PnfsOperationHandler::new(
            Arc::clone(&layout_manager),
            Arc::clone(&device_registry),
        ));

        // Initialize NFSv4 dispatcher WITH pNFS support
        // This handles ALL NFS and pNFS operations (LAYOUTGET, GETDEVICEINFO, etc.)
        let base_dispatcher = Arc::new(CompoundDispatcher::new_with_pnfs(
            Arc::clone(&fh_manager),
            state_mgr,
            lock_mgr,
            Some(operation_handler.clone() as Arc<dyn crate::pnfs::PnfsOperations>),
        ));

        // Register initial data servers from config
        for ds in &config.data_servers {
            let mut device_info = DeviceInfo::new(
                ds.device_id.clone(),
                ds.endpoint.clone(),
                ds.bdevs.clone(),
            );

            // Add multipath endpoints
            device_info.endpoints = ds.multipath.clone();

            if let Err(e) = device_registry.register(device_info) {
                warn!("Failed to register data server {}: {}", ds.device_id, e);
            }
        }

        info!(
            "Device registry initialized with {} data servers",
            device_registry.count()
        );

        // Initialize RPCSEC_GSS manager with keytab from environment
        let keytab_path = std::env::var("KRB5_KTNAME").ok();
        let gss_manager = Arc::new(RpcSecGssManager::new(keytab_path));

        Ok(Self {
            config,
            device_registry,
            layout_manager,
            operation_handler,
            base_dispatcher,
            gss_manager,
        })
    }

    /// Start the metadata server
    pub async fn serve(&self) -> Result<()> {
        eprintln!("🔥🔥🔥🔥🔥 FLINT-PNFS-MDS STARTING WITH DEBUG LOGGING 🔥🔥🔥🔥🔥");
        warn!("🔥🔥🔥🔥🔥 MDS SERVER BINARY VERSION: DEBUG BUILD 🔥🔥🔥🔥🔥");
        info!("╔════════════════════════════════════════════════════╗");
        info!("║   Flint pNFS Metadata Server (MDS) - RUNNING      ║");
        info!("╚════════════════════════════════════════════════════╝");
        info!("");
        info!("Listening on: {}:{}", self.config.bind.address, self.config.bind.port);
        info!("Layout Type: {:?}", self.config.layout.layout_type);
        info!("Stripe Size: {} bytes", self.config.layout.stripe_size);
        info!("Layout Policy: {:?}", self.config.layout.policy);
        info!("Registered Data Servers: {}", self.device_registry.count());
        info!("");

        // Start heartbeat monitor in the background
        let heartbeat_timeout = Duration::from_secs(self.config.failover.heartbeat_timeout);
        self.start_heartbeat_monitor(heartbeat_timeout);

        // Start status reporter in background
        self.start_status_reporter();

        // Start metrics/monitoring if enabled
        if self.config.ha.enabled {
            info!("HA enabled with {} replicas", self.config.ha.replicas);
            // TODO: Implement leader election
        }

        info!("✅ Metadata Server is ready to accept connections");
        info!("");

        // Start gRPC control server in background (for DS registration)
        self.start_grpc_server();

        // Start TCP server (for NFS client connections)
        let addr = format!("{}:{}", self.config.bind.address, self.config.bind.port);
        self.serve_tcp(&addr).await
    }

    /// Start gRPC control server for DS registration
    fn start_grpc_server(&self) {
        let device_registry = Arc::clone(&self.device_registry);
        let bind_addr = self.config.bind.address.clone();
        
        tokio::spawn(async move {
            // gRPC server on port 50051 (standard gRPC port)
            let grpc_addr = format!("{}:50051", bind_addr)
                .parse()
                .expect("Invalid gRPC address");

            let control_service = MdsControlService::new(device_registry);
            let svc = MdsControlServer::new(control_service);

            info!("🔧 Starting MDS gRPC control server on {}", grpc_addr);

            match tonic::transport::Server::builder()
                .add_service(svc)
                .serve(grpc_addr)
                .await
            {
                Ok(_) => {
                    info!("gRPC control server stopped");
                }
                Err(e) => {
                    error!("gRPC control server error: {}", e);
                }
            }
        });

        info!("gRPC control server started on port 50051 (for DS registration)");
    }

    /// Serve pNFS over TCP
    async fn serve_tcp(&self, addr: &str) -> Result<()> {
        info!("🔧 Attempting to bind TCP server on {}", addr);
        
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => {
                info!("✅ TCP listener bound successfully on {}", addr);
                l
            }
            Err(e) => {
                error!("❌ Failed to bind TCP listener on {}: {}", addr, e);
                return Err(crate::pnfs::Error::Io(e));
            }
        };
        
        info!("🚀 pNFS MDS TCP server listening on {}", addr);
        info!("🔄 Entering accept loop to handle client connections...");
        info!("");
        
        let mut connection_count = 0u64;

        loop {
            debug!("💤 Waiting for TCP connection...");
            let (stream, peer) = listener.accept()
                .await
                .map_err(|e| crate::pnfs::Error::Io(e))?;
            
            connection_count += 1;
            info!("📡 New TCP connection #{} from {}", connection_count, peer);
            
            // Clone refs for this connection
            let base_dispatcher = Arc::clone(&self.base_dispatcher);
            let gss_manager = Arc::clone(&self.gss_manager);
            let conn_id = connection_count;
            
            tokio::spawn(async move {
                debug!("🚀 Spawned handler task for connection #{} from {}", conn_id, peer);
                if let Err(e) = Self::handle_tcp_connection(
                    stream,
                    base_dispatcher,
                    gss_manager,
                    peer,
                ).await {
                    warn!("❌ Connection #{} from {} error: {}", conn_id, peer, e);
                } else {
                    info!("✓ TCP connection #{} from {} closed cleanly", conn_id, peer);
                }
            });
        }
    }

    /// Handle a single TCP connection
    async fn handle_tcp_connection(
        stream: TcpStream,
        base_dispatcher: Arc<CompoundDispatcher>,
        gss_manager: Arc<RpcSecGssManager>,
        peer: std::net::SocketAddr,
    ) -> std::io::Result<()> {
        use tokio::time::Instant;

        let connect_time = Instant::now();
        debug!("🔌 TCP connection handler started for {}", peer);

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
            debug!("📥 Waiting for RPC message #{} from {}", rpc_count + 1, peer);
            
            // Read RPC record marker (4 bytes)
            let mut marker_buf = [0u8; 4];
            match reader.read_exact(&mut marker_buf).await {
                Ok(_) => {
                    debug!("✅ Received RPC marker from {}", peer);
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Connection closed gracefully
                    let duration = connect_time.elapsed();
                    info!("🔌 Connection from {} closed after {:?} ({} RPCs processed)", 
                          peer, duration, rpc_count);
                    return Ok(());
                }
                Err(e) => {
                    warn!("❌ Error reading RPC marker from {}: {}", peer, e);
                    return Err(e);
                }
            }

            let marker = u32::from_be_bytes(marker_buf);
            let _is_last = (marker & 0x80000000) != 0;
            let length = (marker & 0x7FFFFFFF) as usize;

            debug!("📊 RPC message size: {} bytes", length);

            // Prevent oversized allocations
            if length > 4 * 1024 * 1024 {
                warn!("❌ Rejecting oversized RPC message from {}: {} bytes", peer, length);
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

            let request = buf.split().freeze();

            // Process the RPC call with pNFS support
            debug!(">>> Processing pNFS/NFSv4 request from {}", peer);
            let reply = Self::dispatch_rpc_with_pnfs(
                request,
                Arc::clone(&base_dispatcher),
                Arc::clone(&gss_manager),
            ).await;
            debug!("<<< Reply ready for {}, length={} bytes", peer, reply.len());
            
            rpc_count += 1;

            // Write reply with record marker
            let reply_len = reply.len() as u32;
            let reply_marker = 0x80000000 | reply_len;
            
            debug!("📤 Sending reply to {}: {} bytes", peer, reply_len);
            
            writer.write_all(&reply_marker.to_be_bytes()).await?;
            writer.write_all(&reply).await?;
            writer.flush().await?;
            
            debug!("✅ Reply sent and flushed to {}", peer);
        }
    }

    /// Dispatch RPC call with pNFS support
    async fn dispatch_rpc_with_pnfs(
        request: Bytes,
        base_dispatcher: Arc<CompoundDispatcher>,
        gss_manager: Arc<RpcSecGssManager>,
    ) -> Bytes {
        // Parse RPC call message
        let (call, args) = match CallMessage::decode_with_args(request) {
            Ok(result) => result,
            Err(e) => {
                warn!("❌ Failed to parse RPC call: {}", e);
                return ReplyBuilder::garbage_args(0).into();
            }
        };

        info!(
            ">>> RPC CALL: xid={}, program={}, procedure={}, cred={:?}",
            call.xid, call.program, call.procedure, call.cred.flavor
        );

        // Handle RPCSEC_GSS authentication
        if call.cred.flavor == AuthFlavor::RpcsecGss {
            info!("🔐 RPCSEC_GSS authentication detected on MDS");
            return Self::handle_rpcsec_gss_call(call, args, gss_manager, base_dispatcher).await;
        }

        // Check program number
        if call.program != NFS4_PROGRAM {
            warn!("❌ Invalid program number: {}", call.program);
            return ReplyBuilder::prog_unavail(call.xid);
        }

        // Check version
        if call.version != 4 {
            warn!("❌ Invalid NFSv4 version: {}", call.version);
            return ReplyBuilder::proc_unavail(call.xid);
        }

        // Handle procedure
        match call.procedure {
            procedure::NULL => {
                info!(">>> NULL procedure");
                ReplyBuilder::success(call.xid).finish()
            }

            procedure::COMPOUND => {
                info!(">>> COMPOUND procedure");
                // Handle COMPOUND with pNFS support
                Self::handle_compound_with_pnfs(
                    call,
                    args,
                    base_dispatcher,
                ).await
            }

            _ => {
                warn!("Invalid NFSv4 procedure: {}", call.procedure);
                ReplyBuilder::proc_unavail(call.xid)
            }
        }
    }

    /// Handle COMPOUND request with pNFS operation support
    async fn handle_compound_with_pnfs(
        call: CallMessage,
        args: Bytes,
        base_dispatcher: Arc<CompoundDispatcher>,
    ) -> Bytes {
        use crate::nfs::v4::compound::CompoundRequest;
        use crate::nfs::xdr::XdrDecoder;

        // Decode COMPOUND request
        let decoder = XdrDecoder::new(args);
        let compound_req = match CompoundRequest::decode(decoder) {
            Ok(req) => req,
            Err(e) => {
                warn!("Failed to decode COMPOUND request: {}", e);
                return ReplyBuilder::garbage_args(call.xid);
            }
        };

        debug!(
            "COMPOUND: tag={}, minor_version={}, {} operations",
            compound_req.tag,
            compound_req.minor_version,
            compound_req.operations.len()
        );

        // Dispatch through base dispatcher (which handles both pNFS and regular ops)
        let mut compound_resp = base_dispatcher.dispatch_compound(compound_req).await;

        // Post-process EXCHANGE_ID responses to set pNFS MDS flags
        // This tells clients that we're a pNFS server capable of providing layouts
        use crate::pnfs::exchange_id::set_pnfs_mds_flags;
        use crate::nfs::v4::compound::OperationResult;
        
        for result in &mut compound_resp.results {
            if let OperationResult::ExchangeId(status, Some(ref mut res)) = result {
                if *status == crate::nfs::v4::protocol::Nfs4Status::Ok {
                    let old_flags = res.flags;
                    // Modify flags to advertise pNFS MDS role
                    res.flags = set_pnfs_mds_flags(res.flags);
                    info!("🎯 EXCHANGE_ID: Modified flags for pNFS MDS");
                    info!("   Before: 0x{:08x} (USE_NON_PNFS)", old_flags);
                    info!("   After:  0x{:08x} (USE_PNFS_MDS)", res.flags);
                    info!("   ✅ Client will now request layouts and use pNFS!");
                }
            }
        }

        debug!(
            "COMPOUND result: status={:?}, {} results",
            compound_resp.status,
            compound_resp.results.len()
        );

        // Encode COMPOUND response
        let compound_data = compound_resp.encode();

        // Build RPC SUCCESS reply
        let mut encoder = XdrEncoder::new();
        encoder.encode_u32(call.xid);
        encoder.encode_u32(1);  // REPLY
        encoder.encode_u32(0);  // MSG_ACCEPTED
        encoder.encode_u32(0);  // AUTH_NONE
        encoder.encode_u32(0);  // Auth length
        encoder.encode_u32(0);  // SUCCESS
        encoder.append_raw(&compound_data);

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
                info!("🔐 RPCSEC_GSS_INIT on MDS");
                Self::handle_gss_init(call.xid, &gss_cred, args, gss_manager).await
            }

            gss_proc::CONTINUE_INIT => {
                info!("🔐 RPCSEC_GSS_CONTINUE_INIT on MDS");
                Self::handle_gss_continue_init(call.xid, &gss_cred, args, gss_manager).await
            }

            gss_proc::DATA => {
                info!("🔐 RPCSEC_GSS_DATA on MDS");
                // Validate the GSS context
                if let Err(e) = gss_manager.validate_data(&gss_cred).await {
                    warn!("❌ GSS DATA validation failed: {}", e);
                    return ReplyBuilder::system_err(call.xid);
                }

                // GSS validated, proceed with normal COMPOUND processing
                info!("✅ GSS authentication successful on MDS, processing COMPOUND");
                Self::handle_compound_with_pnfs(call, args, dispatcher).await
            }

            gss_proc::DESTROY => {
                info!("🔐 RPCSEC_GSS_DESTROY on MDS");
                gss_manager.handle_destroy(&gss_cred).await;
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
        encoder.encode_u32(xid);
        encoder.encode_u32(1);  // Message type: REPLY
        encoder.encode_u32(0);  // Reply status: MSG_ACCEPTED
        encoder.encode_u32(0);  // Auth flavor: AUTH_NONE
        encoder.encode_u32(0);  // Auth length: 0
        encoder.encode_u32(0);  // AcceptStatus::Success

        // Encode RPCSEC_GSS init result
        let init_result_data = init_res.encode();
        encoder.append_bytes(&init_result_data);

        info!("✅ GSS_INIT complete on MDS: handle_len={}, major={}, minor={}",
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

        info!("✅ GSS_CONTINUE_INIT complete on MDS: major={}, minor={}",
              init_res.major_status, init_res.minor_status);

        encoder.finish()
    }

    /// Start heartbeat monitoring in the background
    fn start_heartbeat_monitor(&self, timeout: Duration) {
        let device_registry = Arc::clone(&self.device_registry);
        let layout_manager = Arc::clone(&self.layout_manager);
        let failover_policy = self.config.failover.policy;

        tokio::spawn(async move {
            let mut check_interval = interval(Duration::from_secs(10));

            loop {
                check_interval.tick().await;

                // Check for stale devices
                let stale_devices = device_registry.check_stale_devices(timeout);

                if !stale_devices.is_empty() {
                    error!("Detected {} stale data servers", stale_devices.len());

                    // Handle failover based on policy
                    for device_id in stale_devices {
                        match failover_policy {
                            crate::pnfs::config::FailoverPolicy::RecallAll => {
                                // Recall all layouts (disruptive)
                                warn!("Recalling all layouts due to {} failure", device_id);
                                // TODO: Implement layout recall to all clients
                            }
                            crate::pnfs::config::FailoverPolicy::RecallAffected => {
                                // Recall only affected layouts
                                let recalled = layout_manager.recall_layouts_for_device(&device_id);
                                if !recalled.is_empty() {
                                    warn!(
                                        "Recalling {} layouts affected by {} failure",
                                        recalled.len(),
                                        device_id
                                    );
                                    // TODO: Send CB_LAYOUTRECALL to clients
                                }
                            }
                            crate::pnfs::config::FailoverPolicy::Lazy => {
                                // Let clients discover failure
                                info!(
                                    "Device {} offline, clients will discover organically",
                                    device_id
                                );
                            }
                        }
                    }
                }
            }
        });

        info!("Heartbeat monitor started (timeout: {} seconds)", timeout.as_secs());
    }

    /// Start status reporter in background
    fn start_status_reporter(&self) {
        let device_registry = Arc::clone(&self.device_registry);
        let layout_manager = Arc::clone(&self.layout_manager);

        tokio::spawn(async move {
            let mut status_interval = interval(Duration::from_secs(60));

            loop {
                status_interval.tick().await;

                let total_devices = device_registry.count();
                let active_devices = device_registry.count_by_status(
                    crate::pnfs::mds::device::DeviceStatus::Active
                );
                let active_layouts = layout_manager.layout_count();
                let total_capacity = device_registry.total_capacity();
                let total_used = device_registry.total_used();

                info!("─────────────────────────────────────────────────────");
                info!("MDS Status Report:");
                info!("  Data Servers: {} active / {} total", active_devices, total_devices);
                info!("  Active Layouts: {}", active_layouts);
                info!("  Capacity: {} bytes total, {} bytes used", total_capacity, total_used);
                info!("─────────────────────────────────────────────────────");
            }
        });

        info!("Status reporter started (interval: 60 seconds)");
    }

    /// Get the operation handler (for integration with NFSv4 dispatcher)
    pub fn operation_handler(&self) -> Arc<PnfsOperationHandler> {
        Arc::clone(&self.operation_handler)
    }

    /// Get the device registry (for DS registration)
    pub fn device_registry(&self) -> Arc<DeviceRegistry> {
        Arc::clone(&self.device_registry)
    }

    /// Get the layout manager
    pub fn layout_manager(&self) -> Arc<LayoutManager> {
        Arc::clone(&self.layout_manager)
    }
}


