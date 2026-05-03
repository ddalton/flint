//! MDS Server Implementation
//!
//! The Metadata Server extends the standard NFSv4.2 server with pNFS operations.
//! It manages data server registration, layout generation, and client state.

use crate::pnfs::config::MdsConfig;
use crate::pnfs::mds::callback::CallbackManager;
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
    /// Export root the MDS serves; passed to MdsControlService so it
    /// can fulfill CreateVolume/DeleteVolume by manipulating files
    /// under this directory.
    export_path: std::path::PathBuf,
    device_registry: Arc<DeviceRegistry>,
    layout_manager: Arc<LayoutManager>,
    operation_handler: Arc<PnfsOperationHandler>,
    base_dispatcher: Arc<CompoundDispatcher>,
    gss_manager: Arc<RpcSecGssManager>,
    /// CB_LAYOUTRECALL fan-out — wired to the dispatcher's per-
    /// session back-channel writer registry and to `state_mgr` for
    /// `Session.cb_program` lookups. Constructed at server startup
    /// and shared with the heartbeat-monitor task so DS deaths
    /// trigger recalls without needing to reach back through the
    /// dispatcher.
    callback_manager: Arc<CallbackManager>,
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
        let fh_manager = Arc::new(FileHandleManager::new(export_path.clone()));

        // Initialize state manager (for NFSv4 sessions, stateids).
        // Wrapped in Arc so the dispatcher and the CallbackManager
        // (cb_program lookup) can share it.
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        
        // Initialize lock manager
        let lock_mgr = Arc::new(LockManager::new());

        // Initialize device registry
        let device_registry = Arc::new(DeviceRegistry::new());

        // Initialize layout manager. Shares the StateManager's
        // backend so layout records persist alongside client /
        // session / stateid records.
        let layout_manager = Arc::new(LayoutManager::new(
            Arc::clone(&device_registry),
            config.layout.policy,
            config.layout.stripe_size,
            state_mgr.backend(),
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
            Arc::clone(&state_mgr),
            lock_mgr,
            Some(operation_handler.clone() as Arc<dyn crate::pnfs::PnfsOperations>),
        ));

        // Build the callback fan-out manager once we know the
        // dispatcher's back-channel registry exists. CallbackManager
        // borrows the same registry the dispatcher populates from
        // BIND_CONN_TO_SESSION, so newly-bound sessions are
        // immediately reachable from the recall path with no extra
        // wiring.
        let callback_manager = Arc::new(CallbackManager::new(
            base_dispatcher.back_channels(),
            Arc::clone(&state_mgr),
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
            export_path,
            device_registry,
            layout_manager,
            operation_handler,
            base_dispatcher,
            gss_manager,
            callback_manager,
        })
    }

    /// Start the metadata server
    pub async fn serve(&self) -> Result<()> {
        warn!("FLINT-PNFS-MDS STARTING WITH DEBUG LOGGING");
        warn!("MDS SERVER BINARY VERSION: DEBUG BUILD");
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
        let export_path = self.export_path.clone();
        // Build the operator's `device_id → reachable endpoint` map from
        // the static config. The gRPC service uses this to override the
        // bind-address that registering DSes report (a DS only knows its
        // own bind, often 0.0.0.0; the client needs the externally
        // routable endpoint).
        let configured_endpoints: std::collections::HashMap<String, String> =
            self.config.data_servers.iter()
                .map(|ds| (ds.device_id.clone(), ds.endpoint.clone()))
                .collect();

        tokio::spawn(async move {
            // gRPC server on port 50051 (standard gRPC port)
            let grpc_addr = format!("{}:50051", bind_addr)
                .parse()
                .expect("Invalid gRPC address");

            let control_service = MdsControlService::new(
                device_registry, configured_endpoints, export_path,
            );
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
        let writer = BufWriter::with_capacity(128 * 1024, writer);
        // Wrap the writer so the dispatcher can register it as a
        // back-channel when the client sends CREATE_SESSION with
        // CONN_BACK_CHAN (or, later, BIND_CONN_TO_SESSION). The
        // forward-reply path also goes through this writer — same
        // mutex serializes against any concurrent CB_LAYOUTRECALL.
        let bcw = crate::nfs::v4::back_channel::BackChannelWriter::new(writer);

        // Inflight cleanup on every exit path. Without this, awaiting
        // CB callers hang on `Timeout` instead of seeing
        // `ConnectionClosed`. The dispatcher's back-channel registry
        // holds another Arc to the writer, so it outlives this
        // function — explicit cleanup is load-bearing.
        struct InflightGuard {
            bcw: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
        }
        impl Drop for InflightGuard {
            fn drop(&mut self) {
                self.bcw.drop_all_inflight();
            }
        }
        let _inflight_guard = InflightGuard { bcw: Arc::clone(&bcw) };

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

            // RFC 5531 §9 frame layout: [0..4]=xid, [4..8]=msg_type
            // (0=CALL, 1=REPLY). REPLY frames coming inbound are
            // responses to our own CB_LAYOUTRECALL CALLs — route them
            // to the inflight registry instead of trying to parse as
            // a forward NFS CALL.
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
                            "📭 CB reply for unknown xid={} on conn from {} (timed out or never registered)",
                            xid, peer,
                        );
                    }
                    continue;
                }
            }

            // Process the RPC call with pNFS support
            debug!(">>> Processing pNFS/NFSv4 request from {}", peer);
            let reply = Self::dispatch_rpc_with_pnfs(
                request,
                Arc::clone(&base_dispatcher),
                Arc::clone(&gss_manager),
                Arc::clone(&bcw),
            ).await;
            debug!("<<< Reply ready for {}, length={} bytes", peer, reply.len());

            rpc_count += 1;

            // Forward replies go through the same writer the back-
            // channel uses — `send_record` prepends the 4-byte
            // record marker and flushes; the inner mutex serializes
            // against any concurrent CB_LAYOUTRECALL.
            debug!("📤 Sending reply to {}: {} bytes", peer, reply.len());
            bcw.send_record(reply).await?;
            debug!("✅ Reply sent and flushed to {}", peer);
        }
    }

    /// Dispatch RPC call with pNFS support
    async fn dispatch_rpc_with_pnfs(
        request: Bytes,
        base_dispatcher: Arc<CompoundDispatcher>,
        gss_manager: Arc<RpcSecGssManager>,
        back_channel: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
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
            return Self::handle_rpcsec_gss_call(call, args, gss_manager, base_dispatcher, back_channel).await;
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
                    back_channel,
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
        back_channel: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
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

        // Dispatch through base dispatcher (which handles both pNFS and regular ops).
        // Pass the RPC-level principal so EXCHANGE_ID's §18.35.5 state machine
        // can distinguish per-principal client owners. The back-channel writer
        // is plumbed through so CREATE_SESSION (CONN_BACK_CHAN flag) and
        // BIND_CONN_TO_SESSION can register it in the dispatcher's per-session
        // back_channels registry — that's how `CallbackManager` finds the
        // writer for CB_LAYOUTRECALL on DS death.
        let principal = call.cred.principal();
        let mut compound_resp = base_dispatcher
            .dispatch_compound_with_back_channel(compound_req, principal, Some(back_channel))
            .await;

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
                Self::handle_compound_with_pnfs(call, args, dispatcher, back_channel).await
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
        let callback_manager = Arc::clone(&self.callback_manager);
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
                                // "Recall everything" is the same as
                                // "recall affected" for a per-DS
                                // failure: only layouts that touch
                                // the dead device are at risk.
                                // RecallAll exists for the case
                                // where the operator wants to
                                // forcibly drain in-flight layouts
                                // even if multiple DSes failed at
                                // once; we still drive the per-
                                // device fan-out here.
                                warn!("RecallAll policy: recalling for {} failure", device_id);
                                Self::fan_out_recalls(
                                    &device_id,
                                    &layout_manager,
                                    &callback_manager,
                                ).await;
                            }
                            crate::pnfs::config::FailoverPolicy::RecallAffected => {
                                // Default: recall only the layouts
                                // that touch this device.
                                Self::fan_out_recalls(
                                    &device_id,
                                    &layout_manager,
                                    &callback_manager,
                                ).await;
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

    /// Compute the (session, stateid) pairs to recall for a dead
    /// device, then drive `CallbackManager` over them. Pulled out
    /// as an associated function so the heartbeat closure stays
    /// readable and so the recall path is unit-testable in
    /// isolation.
    ///
    /// Revocation policy (RFC 5661 §12.5.5.2 — server MAY revoke):
    ///
    /// * `TimedOut` / `NoChannel` / `Transport` → revoke immediately.
    ///   The client either didn't get the recall or won't reply, so
    ///   leaving the layout live with a dead DS in it would silently
    ///   misroute writes.
    /// * `Acked` → schedule a soft post-recall deadline (10s). If a
    ///   client LAYOUTRETURN doesn't arrive by then, revoke.
    ///   `LayoutManager::revoke_layout` is idempotent, so the race
    ///   between LAYOUTRETURN and the timer is harmless.
    async fn fan_out_recalls(
        device_id: &str,
        layout_manager: &Arc<LayoutManager>,
        callback_manager: &Arc<CallbackManager>,
    ) {
        let recalls = layout_manager.recall_layouts_for_device(device_id);
        if recalls.is_empty() {
            return;
        }
        let pairs: Vec<(crate::nfs::v4::protocol::SessionId, _)> = recalls
            .into_iter()
            .map(|(sid_bytes, stateid)| {
                (crate::nfs::v4::protocol::SessionId(sid_bytes), stateid)
            })
            .collect();
        warn!(
            "Recalling {} layout(s) affected by {} failure",
            pairs.len(),
            device_id,
        );
        let results = callback_manager
            .recall_layouts_for_device(device_id, &pairs)
            .await;

        // Pulled out for clarity — single place where the revocation
        // policy matrix lives. See `RecallOutcome` for the shape.
        const POST_RECALL_DEADLINE: Duration = Duration::from_secs(10);
        let mut acked = 0;
        let mut revoked_now = 0;
        let mut deferred = 0;
        for r in &results {
            use crate::pnfs::mds::callback::RecallOutcome;
            match &r.outcome {
                RecallOutcome::Acked => {
                    acked += 1;
                    deferred += 1;
                    let lm = Arc::clone(layout_manager);
                    let stateid = r.stateid;
                    tokio::spawn(async move {
                        tokio::time::sleep(POST_RECALL_DEADLINE).await;
                        if lm.revoke_layout(&stateid) {
                            warn!(
                                "🚫 Layout {:?} not LAYOUTRETURN'd within {:?} after recall — forcibly revoking",
                                &stateid[0..4], POST_RECALL_DEADLINE,
                            );
                        }
                    });
                }
                RecallOutcome::TimedOut | RecallOutcome::NoChannel | RecallOutcome::Transport(_) => {
                    if layout_manager.revoke_layout(&r.stateid) {
                        warn!(
                            "🚫 Forcibly revoking layout {:?} (recall {:?})",
                            &r.stateid[0..4], r.outcome,
                        );
                        revoked_now += 1;
                    }
                }
            }
        }
        info!(
            "CB_LAYOUTRECALL fan-out for {} complete: {}/{} acked, {} revoked-now, {} deferred",
            device_id,
            acked,
            pairs.len(),
            revoked_now,
            deferred,
        );
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


