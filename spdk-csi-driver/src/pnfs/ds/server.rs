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
use crate::nfs::gss_framing::ValidatedCall;
use crate::nfs::rpc::{AuthFlavor, AuthStat, CallMessage, ReplyBuilder};
use crate::nfs::rpcsec_gss::RpcSecGssManager;
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use crate::nfs::v4::protocol::{procedure, NFS4_PROGRAM, opcode, Nfs4Status, exchgid_flags};
use crate::nfs::v4::xdr::Nfs4XdrDecoder;
use crate::nfs::v4::state::{ClientManager, LeaseManager};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufWriter;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// Data Server
pub struct DataServer {
    config: DsConfig,
    io_handler: Arc<IoOperationHandler>,
    /// (endpoint, client) per MDS shard — the DS registers and
    /// heartbeats with every shard independently (sharding Phase 2).
    /// Single-MDS configs hold exactly one entry.
    registration_clients: Vec<(String, Arc<tokio::sync::Mutex<RegistrationClient>>)>,
    session_mgr: Arc<DsSessionManager>,
    client_mgr: Arc<ClientManager>,
    /// Creation stamp of the on-volume identity marker (Phase 2);
    /// reported in RegisterRequest so the MDS can spot volume swaps.
    identity_created_at: u64,
    /// RPCSEC_GSS (pNFS Kerberos plan, Phase 3): `Some` with a keytab
    /// (`KRB5_KTNAME`). Without one, RPCSEC_GSS calls are refused.
    gss: Option<Arc<RpcSecGssManager>>,
    /// `FLINT_NFS_REQUIRE_MACH_CRED` (Phase 3b): refuse an EXCHANGE_ID
    /// that does not ask for SP4_MACH_CRED, as the MDS does.
    require_mach_cred: bool,
}

/// One DS connection as a compound sees it (pNFS Kerberos plan, Phase
/// 3b): its id, which SP4_MACH_CRED sessions are bound to, and whether
/// SP4_MACH_CRED is required.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DsConn {
    pub id: u64,
    pub require_mach_cred: bool,
}

#[cfg(test)]
impl DsConn {
    /// Connection `id`, SP4_MACH_CRED not required.
    pub(crate) fn test(id: u64) -> Self {
        Self { id, require_mach_cred: false }
    }
}

/// A SEQUENCE op for `sid`, slot 0 (tests).
#[cfg(test)]
pub(crate) fn test_sequence(e: &mut XdrEncoder, sid: [u8; 16], seq: u32) {
    e.encode_u32(opcode::SEQUENCE);
    e.encode_fixed_opaque(&sid);
    e.encode_u32(seq);
    e.encode_u32(0); // slot
    e.encode_u32(0); // highest slot
    e.encode_bool(false);
}

/// Skip a SEQUENCE4resok: sessionid, then five words (tests).
#[cfg(test)]
pub(crate) fn test_skip_sequence_res(d: &mut XdrDecoder) {
    for _ in 0..9 {
        d.decode_u32().unwrap();
    }
}

/// Connection ids, unique for the life of the process across every
/// listener (multipath addresses included).
static NEXT_CONN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Unbinds a closed connection from its sessions, however its handler
/// returns.
struct ConnBindings {
    id: u64,
    sessions: Arc<DsSessionManager>,
}

impl Drop for ConnBindings {
    fn drop(&mut self) {
        self.sessions.forget_conn(self.id);
    }
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

        // One registration client per MDS shard (mds-sharding-plan.md
        // Phase 2): the DS registers with EVERY shard so each can
        // grant layouts against this device. Each client keeps its own
        // channel and failure state — shards share nothing, and a dead
        // shard must not affect registration with the others.
        let registration_clients: Vec<(String, Arc<tokio::sync::Mutex<RegistrationClient>>)> =
            config
                .mds
                .effective_endpoints()
                .into_iter()
                .map(|ep| {
                    let client = Arc::new(tokio::sync::Mutex::new(RegistrationClient::new(
                        config.device_id.clone(),
                        ep.clone(),
                        Duration::from_secs(config.mds.heartbeat_interval),
                    )));
                    (ep, client)
                })
                .collect();

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

        // Kerberos (pNFS Kerberos plan, Phase 3). The floor
        // (FLINT_NFS_MIN_SEC) is validated before anything listens, as on
        // the MDS: a typo must not resolve to "no floor". With a keytab,
        // RPCSEC_GSS calls go through the same front end as the MDS's
        // (`nfs::gss_front`); without one they are refused, where before
        // the DS ignored every credential and answered them anyway.
        let floor = crate::nfs::sec_policy::SecPolicy::validate_env()
            .map_err(|e| crate::pnfs::Error::Config(format!("security settings: {e}")))?;
        let gss = std::env::var("KRB5_KTNAME")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .map(|k| Arc::new(RpcSecGssManager::new(Some(k))));
        // Phase 4: the MDS's signed handles (FLINT_PNFS_FH_KEY), checked
        // here (FLINT_PNFS_DS_FH_AUTH). A bad key or mode stops the DS.
        let fh_policy = crate::pnfs::ds_fh::DsPolicy::from_env()
            .map_err(|e| crate::pnfs::Error::Config(format!("data server filehandles: {e}")))?;
        info!(
            "✓ Filehandles: {}",
            match (&fh_policy.keys, fh_policy.enforce) {
                (Some(_), true) => "signed handles only (FLINT_PNFS_DS_FH_AUTH=enforce)",
                (Some(_), false) => "signed handles verified, unsigned ones still served",
                (None, _) => "unsigned (no FLINT_PNFS_FH_KEY)",
            }
        );
        crate::pnfs::ds_fh::init_ds_policy(fh_policy);
        // Phase 3b: SP4_MACH_CRED required, as on the MDS. A typo stops
        // the DS; requiring it with no keytab would refuse every client
        // (SP4_MACH_CRED is granted only under krb5i or krb5p).
        let require_mach_cred = crate::nfs::v4::operations::session::require_mach_cred_from_env()
            .map_err(crate::pnfs::Error::Config)?;
        if require_mach_cred && gss.is_none() {
            return Err(crate::pnfs::Error::Config(format!(
                "{} is on but there is no keytab (KRB5_KTNAME): SP4_MACH_CRED needs krb5i or krb5p, so every client would be refused",
                crate::nfs::v4::operations::session::REQUIRE_MACH_CRED_ENV
            )));
        }
        info!(
            "✓ Security: floor {}; Kerberos {}",
            floor.floor().name(),
            if gss.is_some() { "on (KRB5_KTNAME)" } else { "off (RPCSEC_GSS calls refused)" }
        );
        if require_mach_cred {
            info!("🔒 SP4_MACH_CRED required: sessions are bound to the machine that made them");
        }

        Ok(Self { config, io_handler, registration_clients, session_mgr, client_mgr, identity_created_at, gss, require_mach_cred })
    }

    /// Start the data server
    pub async fn serve(&self) -> Result<()> {
        info!("╔════════════════════════════════════════════════════╗");
        info!("║   Flint pNFS Data Server (DS) - RUNNING           ║");
        info!("╚════════════════════════════════════════════════════╝");
        info!("");
        info!("Device ID: {}", self.config.device_id);
        info!("Listening on: {}:{}", self.config.bind.address, self.config.bind.port);
        for (i, (ep, _)) in self.registration_clients.iter().enumerate() {
            info!("MDS Endpoint [shard {}]: {}", i, ep);
        }
        info!("Block Devices: {}", self.config.bdevs.len());
        for bdev in &self.config.bdevs {
            info!("  - {} mounted at {}", bdev.name, bdev.mount_point);
            if let Some(ref spdk_vol) = bdev.spdk_volume {
                info!("    SPDK volume: {}", spdk_vol);
            }
        }
        info!("");

        // F68b: bind the NFS listener BEFORE registering with any MDS
        // shard. Registration makes this deviceid grantable — a client
        // may hold a layout naming this DS moments later, and a connect
        // attempt that fails while the port is still unbound gets the
        // deviceid blacklisted client-side for 120s
        // (NFS4_DEVICE_ID_NEG_ENTRY, re-armed on every failed retry)
        // with all I/O silently rerouted through the MDS fallback path.
        // Listener-first makes "registered" imply "accepting"; the
        // session/compound layer is fully constructed in new() before
        // serve() runs, so "accepting" implies "serving" and a
        // tcpSocket readinessProbe on this port is truthful.
        let addr = crate::netaddr::join(&self.config.bind.address, self.config.bind.port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(crate::pnfs::Error::Io)?;
        info!("🚀 pNFS DS TCP server listening on {}", addr);

        // Register with every MDS shard
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

        self.serve_tcp(listener).await
    }

    /// Serve minimal NFS (READ/WRITE/COMMIT only) over TCP.
    /// The listener is bound by serve() before MDS registration (F68b)
    /// so a granted layout can never point at an unbound port.
    async fn serve_tcp(&self, listener: TcpListener) -> Result<()> {
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
            let gss = self.gss.clone();
            let require_mach_cred = self.require_mach_cred;
            let conn_id = connection_count;
            
            tokio::spawn(async move {
                debug!("🚀 Spawned handler task for connection #{} from {}", conn_id, peer);
                if let Err(e) = Self::handle_tcp_connection(stream, io_handler, session_mgr, client_mgr, gss, require_mach_cred, peer).await {
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
        gss: Option<Arc<RpcSecGssManager>>,
        require_mach_cred: bool,
        peer: std::net::SocketAddr,
    ) -> std::io::Result<()> {
        use tokio::time::Instant;

        let conn = DsConn {
            id: NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            require_mach_cred,
        };
        let _bindings = ConnBindings { id: conn.id, sessions: Arc::clone(&session_mgr) };

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

        // Record ingress: the shared mechanism in `nfs::ingress`. This
        // is where the DS FINALLY gains fragment reassembly — its old
        // hand-rolled loop dispatched every RPC fragment as a whole
        // record (the `85184494` fix had reached only the shared lane)
        // — along with the whole-record cap and the pooled payload
        // buffers. No idle deadline here: the client tears DS
        // connections down itself when the last layout goes away, and
        // the DS holds no per-connection state worth reaping.
        let mut ingress = crate::nfs::ingress::RecordReader::new(format!(
            "DS connection from {}",
            peer
        ));
        let mut rpc_count = 0;

        loop {
            let request = match ingress.next(&mut reader, None).await {
                Ok(crate::nfs::ingress::NextRecord::Record(r)) => r,
                Ok(crate::nfs::ingress::NextRecord::Closed)
                | Ok(crate::nfs::ingress::NextRecord::IdleClosed) => {
                    let duration = connect_time.elapsed();
                    info!("🔌 DS connection from {} closed after {:?} ({} RPCs)",
                          peer, duration, rpc_count);
                    return Ok(());
                }
                Err(e) => return Err(e),
            };

            // Dispatch through the pipeline; the reply is framed and
            // flushed under the shared writer mutex.
            let io_c = Arc::clone(&io_handler);
            let sess_c = Arc::clone(&session_mgr);
            let client_c = Arc::clone(&client_mgr);
            let writer_c = Arc::clone(&writer);
            let gss_c = gss.clone();
            // Backlog hint: bytes already buffered mean the client is
            // pipelining, so concurrent dispatch pays for its overhead.
            let more_queued = !reader.buffer().is_empty();
            pipeline.submit(
                request,
                more_queued,
                // A DS connection never carries a back-channel — the MDS
                // owns the client's callback path — so the inline fast
                // path stays available here.
                false,
                move |req| Self::dispatch_minimal_nfs(req, io_c, sess_c, client_c, gss_c, conn),
                // The reply arrives as wire segments (see [`OpBody`]): for
                // a READ, [rpc+compound+head], [payload], [pad]. The 128 KiB
                // BufWriter coalesces the marker and every small segment
                // into ONE syscall; a >capacity payload flushes them and
                // goes to the socket directly. Net effect per 1 MiB READ:
                // two writes — ~100 B of framing, then the payload — where
                // the old single-buffer path issued a 4-BYTE flush (the
                // marker alone) ahead of every megabyte reply.
                move |reply: Vec<crate::nfs::segment::Segment>| async move {
                    let mut w = writer_c.lock().await;
                    crate::nfs::segment::write_record(&mut w, reply).await
                },
            ).await?;
            rpc_count += 1;
        }
    }

    /// Dispatch minimal NFS RPC (SEQUENCE/READ/WRITE/COMMIT only).
    ///
    /// Returns the reply as wire SEGMENTS whose concatenation is the RPC
    /// reply body (no record marker — the writer frames it). Single-buffer
    /// replies are a one-element vec; a READ reply carries its payload as
    /// its own untouched segment (see [`OpBody`]).
    async fn dispatch_minimal_nfs(
        request: Bytes,
        io_handler: Arc<IoOperationHandler>,
        session_mgr: Arc<DsSessionManager>,
        client_mgr: Arc<ClientManager>,
        gss: Option<Arc<RpcSecGssManager>>,
        conn: DsConn,
    ) -> Vec<crate::nfs::segment::Segment> {
        // Parse RPC call
        let (call, args) = match CallMessage::decode_with_args(request) {
            Ok(result) => result,
            Err(e) => {
                warn!("❌ Failed to parse RPC: {}", e);
                return vec![ReplyBuilder::garbage_args(0).into()];
            }
        };

        debug!("DS RPC: xid={}, procedure={}", call.xid, call.procedure);

        // Validate program/version
        if call.program != NFS4_PROGRAM || call.version != 4 {
            return vec![ReplyBuilder::prog_unavail(call.xid).into()];
        }

        // Authentication. RPCSEC_GSS through the shared front end (its
        // INIT/CONTINUE_INIT/DESTROY answered there; DATA checked and
        // unsealed), every other flavor against the floor.
        let (args, validated) = if call.cred.flavor == AuthFlavor::RpcsecGss {
            let Some(mgr) = gss.as_deref() else {
                warn!("🔒 DS: RPCSEC_GSS call refused: this data server has no keytab (KRB5_KTNAME)");
                return vec![ReplyBuilder::auth_error(call.xid, AuthStat::RejectedCred).into()];
            };
            match crate::nfs::gss_front::gss_front(&call, args, mgr).await {
                crate::nfs::gss_front::GssFront::Reply(reply) => return reply,
                crate::nfs::gss_front::GssFront::Data { args, validated } => (args, Some(validated)),
            }
        } else {
            if let Some(refused) = crate::nfs::gss_front::floor_refusal(&call) {
                return vec![refused];
            }
            (args, None)
        };

        // Handle procedure
        match call.procedure {
            procedure::NULL => match validated.as_ref() {
                None => vec![ReplyBuilder::success(call.xid).finish().into()],
                // Still a GSS-verified reply.
                gss => crate::nfs::gss_front::frame_reply(call.xid, gss, vec![]),
            },

            procedure::COMPOUND => {
                Self::handle_minimal_compound(call, args, io_handler, session_mgr, client_mgr, validated.as_ref(), conn).await
            }

            _ => vec![ReplyBuilder::proc_unavail(call.xid).into()],
        }
    }

    /// Handle COMPOUND with SEQUENCE/READ/WRITE/COMMIT
    async fn handle_minimal_compound(
        call: CallMessage,
        args: Bytes,
        io_handler: Arc<IoOperationHandler>,
        session_mgr: Arc<DsSessionManager>,
        client_mgr: Arc<ClientManager>,
        gss: Option<&ValidatedCall>,
        conn: DsConn,
    ) -> Vec<crate::nfs::segment::Segment> {
        let xid = call.xid;
        let Some(results) = Self::run_compound(call, args, io_handler, session_mgr, client_mgr, gss, conn).await else {
            return vec![ReplyBuilder::garbage_args(xid).into()];
        };
        // One pass, RPC header first, payloads passed through as their
        // own segments — replaces the flatten that copied a READ payload
        // three times (op encoder -> compound encoder -> reply encoder).
        // Under GSS the body is framed by the shared front end: a MIC'd
        // verifier and the body signed or sealed.
        match gss {
            None => assemble_reply(xid, results),
            Some(v) => crate::nfs::gss_front::frame_reply(xid, Some(v), assemble_body(XdrEncoder::new(), results)),
        }
    }

    /// The COMPOUND's ops, each `(opcode, status, body)`; `None` when its
    /// header does not decode (GARBAGE_ARGS).
    async fn run_compound(
        call: CallMessage,
        args: Bytes,
        io_handler: Arc<IoOperationHandler>,
        session_mgr: Arc<DsSessionManager>,
        client_mgr: Arc<ClientManager>,
        gss: Option<&ValidatedCall>,
        conn: DsConn,
    ) -> Option<Vec<(u32, Nfs4Status, OpBody)>> {
        // The caller as RFC 8881 §18.35.5 means it: the authenticated
        // Kerberos principal when there is one.
        let principal = match gss.and_then(|v| v.client_principal.as_deref()) {
            Some(name) => [b"gss:".as_slice(), name.as_bytes()].concat(),
            None => call.cred.principal(),
        };
        let mut decoder = XdrDecoder::new(args);

        // Decode COMPOUND header
        let tag_len = match decoder.decode_u32() {
            Ok(len) => len,
            Err(_) => return None,
        };

        // Skip tag
        for _ in 0..((tag_len + 3) / 4) {
            let _ = decoder.decode_u32();
        }

        let minor_version = match decoder.decode_u32() {
            Ok(v) => v,
            Err(_) => return None,
        };

        let op_count = match decoder.decode_u32() {
            Ok(c) => c,
            Err(_) => return None,
        };

        debug!("DS COMPOUND: minor_version={}, {} operations", minor_version, op_count);

        // Process operations (only READ/WRITE/COMMIT supported)
        let mut results: Vec<(u32, Nfs4Status, OpBody)> = Vec::new();  // (opcode, status, body)
        let mut current_fh: Option<Vec<u8>> = None;
        // The principal that made this compound's session's client, once
        // SEQUENCE has named the session: Phase 4 checks a signed handle
        // against its node.
        let mut session_owner: Option<Vec<u8>> = None;
        // Whether this compound began with a SEQUENCE that succeeded.
        let mut in_session = false;
        // SP4_MACH_CRED (Phase 3b): granted only to a krb5i or krb5p call,
        // as on the MDS; over AUTH_SYS or plain krb5 nothing protects the
        // ops it guards.
        let gss_protected = gss.is_some_and(|v| {
            matches!(v.service, crate::nfs::rpcsec_gss::GssService::Integrity | crate::nfs::rpcsec_gss::GssService::Privacy)
        });
        // An SP4_MACH_CRED client's must_enforce ops (BIND_CONN_TO_SESSION,
        // EXCHANGE_ID, CREATE_SESSION, DESTROY_SESSION, DESTROY_CLIENTID)
        // from any principal but the machine that made it: WRONG_CRED.
        let wrong_cred = |client_id: u64| client_mgr.mach_cred_principal(client_id).is_some_and(|p| p != principal);
        // The client a session id belongs to: the DS's session ids begin
        // with their client id (`DsSessionManager::create_session`).
        let client_of = |sid: &[u8; 16]| u64::from_be_bytes(sid[..8].try_into().expect("16 bytes"));

        for i in 0..op_count {
            let opcode = match decoder.decode_u32() {
                Ok(op) => op,
                Err(e) => {
                    warn!("Failed to decode opcode: {}", e);
                    break;
                }
            };

            // RFC 8881 §2.6.3.1.1: SEQUENCE comes first, and every op but
            // the five that make, bind and tear down clients and sessions
            // runs only after it. Without this a compound with no SEQUENCE
            // (or after a refused one) reached PUTFH, READ and WRITE with
            // no session at all, past the session binding (Phase 3b) and
            // the attached-nodes gate on SEQUENCE.
            let sessionless = matches!(
                opcode,
                opcode::EXCHANGE_ID
                    | opcode::CREATE_SESSION
                    | opcode::DESTROY_SESSION
                    | opcode::DESTROY_CLIENTID
                    | opcode::BIND_CONN_TO_SESSION
            );
            if opcode == opcode::SEQUENCE && i != 0 {
                results.push((opcode, Nfs4Status::SequencePos, OpBody::Inline(Bytes::new())));
                break;
            }
            if !sessionless && opcode != opcode::SEQUENCE && !in_session {
                warn!("DS: opcode {opcode} outside a session → OP_NOT_IN_SESSION");
                results.push((opcode, Nfs4Status::OpNotInSession, OpBody::Inline(Bytes::new())));
                break;
            }

            debug!("DS: Processing opcode={}", opcode);

            // Only the nodes a pNFS volume is attached to
            // (FLINT_NFS_ATTACHED_NODES_ONLY, `nfs::node_authz`), as on the
            // MDS: clients and sessions from an attached node's machine
            // principal, and a session used only while its node is
            // attached. The union over every pNFS volume: which volume a
            // stripe belongs to is not known here (Phase 4).
            if let Some(nodes) = crate::nfs::node_authz::global() {
                if let Some(refused) = Self::unattached_refusal(nodes, opcode, &principal, &decoder, &client_mgr) {
                    let op = match opcode {
                        opcode::EXCHANGE_ID => "EXCHANGE_ID",
                        opcode::CREATE_SESSION => "CREATE_SESSION",
                        opcode::BIND_CONN_TO_SESSION => "BIND_CONN_TO_SESSION",
                        _ => "SEQUENCE",
                    };
                    warn!("DS: {op} refused: {} is not a node a pNFS volume is attached to", String::from_utf8_lossy(&principal));
                    results.push((opcode, refused, OpBody::Inline(Bytes::new())));
                    break;
                }
            }

            let (status, result_data) = match opcode {
                opcode::EXCHANGE_ID => {
                    // Decode EXCHANGE_ID arguments properly for server trunking
                    // Per RFC 8881 Section 18.35 - client_owner structure has verifier FIRST
                    
                    // Decode verifier (8 bytes)
                    let verifier = match decoder.decode_u64() {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("DS: Failed to decode EXCHANGE_ID verifier: {}", e);
                            results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    };
                    
                    // Decode client owner (opaque)
                    let client_owner = match decoder.decode_opaque() {
                        Ok(bytes) => bytes.to_vec(),
                        Err(e) => {
                            warn!("DS: Failed to decode EXCHANGE_ID client_owner: {}", e);
                            results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    };
                    
                    // Decode flags
                    let client_flags = decoder.decode_u32().unwrap_or(0);
                    
                    // state_protect4_a: decoded whole, so impl_id below is
                    // read from where it is. SP4_MACH_CRED carries two
                    // bitmaps (spo_must_enforce, spo_must_allow); the DS
                    // answers SP4_NONE to it (sessions here are not bound
                    // to the machine principal yet: Phase 3b). SP4_SSV is
                    // refused, as the MDS refuses it.
                    let state_protect = decoder.decode_u32().unwrap_or(0);
                    match state_protect {
                        0 => {}
                        1 => {
                            use crate::nfs::v4::xdr::Nfs4XdrDecoder as _;
                            if decoder.decode_bitmap().and_then(|_| decoder.decode_bitmap()).is_err() {
                                results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                                break;
                            }
                        }
                        _ => {
                            results.push((opcode, Nfs4Status::EncrAlgUnsupp, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    }
                    let mach_cred = state_protect == 1 && gss_protected;
                    // FLINT_NFS_REQUIRE_MACH_CRED: only SP4_MACH_CRED
                    // clients, so every session here is bound to its
                    // machine. ACCESS, as the MDS answers.
                    if conn.require_mach_cred && !mach_cred {
                        warn!(
                            "DS: EXCHANGE_ID refused: {} did not get SP4_MACH_CRED (asked {}, {}), and it is required ({})",
                            String::from_utf8_lossy(&principal),
                            state_protect,
                            if gss_protected { "krb5i/krb5p" } else { "no GSS integrity" },
                            crate::nfs::v4::operations::session::REQUIRE_MACH_CRED_ENV,
                        );
                        results.push((opcode, Nfs4Status::Access, OpBody::Inline(Bytes::new())));
                        break;
                    }
                    
                    // Skip the optional impl_id (nfs_impl_id4<1>): domain,
                    // name and nfstime4 date. Only the domain was skipped,
                    // which left the next operation misread.
                    let has_impl_id = decoder.decode_bool().unwrap_or(false);
                    if has_impl_id {
                        let _ = decoder.decode_opaque();
                        let _ = decoder.decode_opaque();
                        let _ = decoder.decode_u64();
                        let _ = decoder.decode_u32();
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
                        principal.clone(),
                    );
                    let (clientid, sequenceid, is_new) = match outcome {
                        ExchangeIdOutcome::NewUnconfirmed { client_id, sequence_id } =>
                            (client_id, sequence_id, true),
                        ExchangeIdOutcome::ExistingConfirmed { client_id, sequence_id } =>
                            (client_id, sequence_id, false),
                        // RFC 8881 §18.35.4 case 3 against an SP4_MACH_CRED
                        // client: another principal under its owner. The
                        // client is its machine's (EXCHANGE_ID is in its
                        // must_enforce set).
                        ExchangeIdOutcome::ClientCollision { incumbent }
                            if client_mgr.mach_cred_principal(incumbent).is_some() =>
                        {
                            warn!(
                                "DS: EXCHANGE_ID: client {incumbent} is SP4_MACH_CRED and {} is not its principal → CLID_INUSE",
                                String::from_utf8_lossy(&principal)
                            );
                            results.push((opcode, Nfs4Status::ClIdInUse, OpBody::Inline(Bytes::new())));
                            break;
                        }
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
                    // Only the record's own principal gets here (another
                    // is case 3), so the protection follows its latest
                    // EXCHANGE_ID, as on the MDS.
                    if clientid != 0 {
                        client_mgr.set_mach_cred(clientid, mach_cred);
                    }
                    if mach_cred {
                        info!("🔐 DS: EXCHANGE_ID: client {clientid} is SP4_MACH_CRED, bound to {}", String::from_utf8_lossy(&principal));
                    }
                    
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
                    
                    // Capability flags are NOT echoed. bc7fed3 fixed this
                    // on the MDS (B10) and this fork kept the original
                    // comment, "Echo back ALL client capability flags",
                    // along with the behaviour: SUPP_MOVED_REFER,
                    // SUPP_MOVED_MIGR and BIND_PRINC_STATEID were handed
                    // back to anyone who asked, advertising referral,
                    // migration and principal-bound stateids that nothing
                    // here implements — the DS cannot answer
                    // NFS4ERR_MOVED and discards the stateid outright.
                    //
                    // A server that claims a recovery path it does not
                    // have sends the client down it exactly once, during
                    // the failure the path was supposed to handle.
                    let _ = client_flags;
                    
                    encoder.encode_u32(response_flags);
                    
                    // state_protect: SP4_MACH_CRED with the MDS's
                    // must_enforce set (the Linux client requires all
                    // five) and an empty must_allow (the DS serves none
                    // of the cleanup ops it may name), else SP4_NONE.
                    if mach_cred {
                        use crate::nfs::v4::xdr::Nfs4XdrEncoder as _;
                        let grant = crate::nfs::v4::operations::session::mach_cred_grant(&[]);
                        encoder.encode_u32(1);
                        encoder.encode_bitmap(&grant.must_enforce);
                        encoder.encode_bitmap(&grant.must_allow);
                    } else {
                        encoder.encode_u32(0);
                    }
                    
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
                    if wrong_cred(clientid) {
                        warn!("DS: CREATE_SESSION: client {clientid} is SP4_MACH_CRED and this is not its principal → WRONG_CRED");
                        results.push((opcode, Nfs4Status::WrongCred, OpBody::Inline(Bytes::new())));
                        break;
                    }
                    let sequence = decoder.decode_u32().unwrap_or(0);
                    let _csa_flags = decoder.decode_u32().unwrap_or(0);

                    // Decode the client's REQUESTED channel attrs with
                    // the shared `channel_attrs4` decode, and NEGOTIATE
                    // with the shared min(request, ceiling) arithmetic —
                    // Linux's nfs4_verify_channel_attrs EINVALs a server
                    // that grants more than the client asked, the client
                    // then destroys the session, marks the deviceid
                    // unavailable, and every read silently detours
                    // through the MDS proxy (measured: the DS-lane rig
                    // collapsed 1650 → 716 MiB/s when this arm
                    // advertised fixed values above the request).
                    let fore = match crate::nfs::v4::compound::ChannelAttrs::decode(&mut decoder) {
                        Ok(a) => a,
                        Err(e) => {
                            warn!("DS: CREATE_SESSION bad fore_chan_attrs: {}", e);
                            results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    };
                    let _back = crate::nfs::v4::compound::ChannelAttrs::decode(&mut decoder);
                    let _cb_program = decoder.decode_u32();
                    // csa_sec_parms left unparsed: nothing in this arm
                    // reads the stream past here, and the DS ignores
                    // callback security (it never issues callbacks).

                    // The DS's ceilings: the same wire maxima as the MDS
                    // (SERVER_MAX_* carry the 3104fc51 headroom), its own
                    // cached/slot policy — no replay cache, 64-slot table.
                    let neg = crate::nfs::v4::operations::session::negotiate_fore_channel(
                        &fore,
                        &crate::nfs::v4::operations::session::ForeChannelCeilings {
                            max_request: crate::nfs::v4::operations::session::SERVER_MAX_REQUEST,
                            max_response: crate::nfs::v4::operations::session::SERVER_MAX_RESPONSE,
                            max_response_cached: 4096,
                            max_ops: 128,
                            max_requests: 64,
                        },
                    );

                    info!("DS: CREATE_SESSION - clientid={}, sequence={}", clientid, sequence);
                    
                    // Create session via session manager
                    match session_mgr.create_session(clientid) {
                        Ok(sessionid) => {
                            // Confirm the client record (RFC 8881 §18.35.5).
                            // Without this every EXCHANGE_ID finds an
                            // UNCONFIRMED record and lawfully replaces it
                            // with a FRESH clientid — which broke session
                            // trunking: the kernel's multipath probe
                            // (EXCHANGE_ID on the extra address) got a
                            // different clientid than the live session and
                            // nfs4_detect_session_trunking refused the
                            // trunk. A case-5 replacement's predecessor
                            // only holds DS sessions — tear those down.
                            if let Some(old_id) = client_mgr.mark_confirmed(clientid) {
                                warn!("DS: CREATE_SESSION case-5 cleanup — discarding old client {}", old_id);
                                session_mgr.destroy_client_sessions(old_id);
                                // The incumbent's RECORD goes too — the
                                // MDS cascade removes it, and this arm
                                // only ever destroyed its sessions,
                                // leaking a record + lease per case-5
                                // handover.
                                client_mgr.remove_client(old_id);
                            }
                            // CREATE_SESSION binds its connection to the
                            // new session (RFC 8881 §18.36.3).
                            session_mgr.bind_conn(&sessionid, conn.id);
                            let mut encoder = XdrEncoder::new();
                            
                            // sessionid (16 bytes)
                            encoder.encode_fixed_opaque(&sessionid);
                            
                            // sequenceid
                            encoder.encode_u32(sequence);
                            
                            // flags (0 for now)
                            encoder.encode_u32(0);
                            
                            // fore_chan_attrs — the negotiated values
                            // from above; never granted above the
                            // request. A client that ASKS for 1 MiB +
                            // overhead now gets it, where the old fixed
                            // 1048576 forced its subtraction below 1 MiB.
                            encoder.encode_u32(0);       // header_pad_size (CRITICAL: was missing!)
                            encoder.encode_u32(neg.max_request);
                            encoder.encode_u32(neg.max_response);
                            encoder.encode_u32(neg.max_response_cached);
                            encoder.encode_u32(neg.max_ops);
                            encoder.encode_u32(neg.max_requests);
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
                            results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    };
                    
                    let sequenceid = decoder.decode_u32().unwrap_or(0);
                    let slotid = decoder.decode_u32().unwrap_or(0);
                    let highest_slotid = decoder.decode_u32().unwrap_or(0);
                    let _cache_this = decoder.decode_bool().unwrap_or(false);

                    // Phase 3b: an SP4_MACH_CRED client's sessions are used
                    // only on connections bound to them (by CREATE_SESSION,
                    // or by BIND_CONN_TO_SESSION, which needs the machine's
                    // credential), as on the MDS: a session id seen on the
                    // wire is no use from another connection. The client
                    // answers by binding the connection. Before the slot,
                    // so a refused call changes nothing.
                    if client_mgr.mach_cred_principal(client_of(&sessionid)).is_some()
                        && !session_mgr.conn_bound(&sessionid, conn.id)
                    {
                        warn!(
                            "DS: SEQUENCE: session {:02x?} is SP4_MACH_CRED and not bound to this connection → CONN_NOT_BOUND_TO_SESSION",
                            &sessionid[..8]
                        );
                        results.push((opcode, Nfs4Status::ConnNotBoundToSession, OpBody::Inline(Bytes::new())));
                        break;
                    }

                    // Handle SEQUENCE via session manager
                    match session_mgr.handle_sequence(sessionid, sequenceid, slotid, highest_slotid) {
                        Ok(result) => {
                            session_owner = client_mgr.get_client(client_of(&sessionid)).map(|c| c.principal);
                            in_session = true;
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
                        // Phase 4 (`pnfs::ds_fh`): a signed handle must
                        // verify for this session's node; with
                        // FLINT_PNFS_DS_FH_AUTH=enforce nothing else is
                        // served. BADHANDLE: the client fetches a fresh
                        // layout (E4).
                        Ok(fh) => match crate::pnfs::ds_fh::ds_policy().admit(&fh.data, session_owner.as_deref()) {
                            Ok(resolved) => {
                                current_fh = Some(resolved);
                                (Nfs4Status::Ok, Bytes::new())
                            }
                            Err(why) => {
                                // The handle's clear part (version, key id,
                                // file id, stripe), never its MAC. The
                                // MDS logs the node it signed a file's
                                // handles for at LAYOUTGET.
                                let clear: String = fh.data.iter().take(17).map(|b| format!("{b:02x}")).collect();
                                let (file, stripe) = match fh.data.get(5..17) {
                                    Some(c) => (
                                        u64::from_be_bytes(c[..8].try_into().expect("8 bytes")).to_string(),
                                        u32::from_be_bytes(c[8..].try_into().expect("4 bytes")).to_string(),
                                    ),
                                    None => ("?".into(), "?".into()),
                                };
                                // Which attached node the handle was signed
                                // for, if any: a handle of one node sent on
                                // another's session (two nodes sharing one
                                // kernel's DS connection, or a stolen
                                // handle) against a bad key or encoding.
                                let hosts = crate::nfs::node_authz::global().and_then(|n| n.hosts()).unwrap_or_default();
                                let signer = crate::pnfs::ds_fh::ds_policy()
                                    .keys
                                    .as_ref()
                                    .and_then(|k| k.signed_for(&fh.data, &hosts))
                                    .map(|h| format!("signed for node {h:?}"))
                                    .unwrap_or_else(|| "signed for no attached node".into());
                                warn!(
                                    "DS PUTFH refused: {why} (session of {}, node {:?}, {signer}, file {file} stripe {stripe}, handle {}…, {} bytes)",
                                    session_owner.as_deref().map(String::from_utf8_lossy).unwrap_or_default(),
                                    String::from_utf8_lossy(&session_owner.as_deref().map(crate::pnfs::ds_fh::node_of).unwrap_or_default()),
                                    clear,
                                    fh.data.len()
                                );
                                current_fh = None;
                                (Nfs4Status::BadHandle, Bytes::new())
                            }
                        },
                        Err(_) => (Nfs4Status::BadHandle, Bytes::new()),
                    }
                }

                opcode::READ => {
                    let _stateid = match decoder.decode_stateid() {
                        Ok(s) => s,
                        Err(_) => {
                            results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    };
                    let offset = decoder.decode_u64().unwrap_or(0);
                    let count = decoder.decode_u32().unwrap_or(0);

                    if let Some(ref fh) = current_fh {
                        // No splice under GSS: the reply body is signed or
                        // sealed as one octet stream, so its payload must
                        // be in memory.
                        match io_handler.read_with(fh, offset, count, gss.is_none() && crate::nfs::splice::enabled()).await {
                            Ok(read_result) => {
                                // The payload is NOT encoded: encode only
                                // the READ4resok head (eof + length) and
                                // hand the data Vec straight through as a
                                // Bytes segment (Vec -> Bytes is an
                                // ownership move, not a copy). The old
                                // encode_opaque here started the payload's
                                // three-copy trip through the encoders —
                                // see [`OpBody`] for the receipts.
                                let mut head = XdrEncoder::new();
                                head.encode_bool(read_result.eof);
                                head.encode_u32(read_result.data.len() as u32);
                                results.push((opcode, Nfs4Status::Ok, OpBody::Payload {
                                    head: head.finish(),
                                    data: read_result.data,
                                }));
                                continue;
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
                    // The flavors this DS accepts, strongest first: the
                    // MDS's list (floor and keytab applied), where this
                    // used to offer AUTH_NONE first and Kerberos it could
                    // not do.
                    let _style = decoder.decode_u32();
                    let mut encoder = XdrEncoder::new();
                    crate::nfs::v4::compound::encode_secinfo_flavors(&mut encoder);
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
                    // REAL, not an ack. When this was a no-op the record
                    // outlived the client that destroyed it, and the NEXT
                    // incarnation's EXCHANGE_ID found a confirmed record —
                    // answered with CONFIRMED_R, which Linux
                    // (nfs4state.c, nfs41_init_clientid) reads as "the
                    // server holds state from a prior instance of me" and
                    // answers by PURGING: EXCHANGE_ID with an all-ones
                    // boot verifier, a forced case-5 discard, a fresh
                    // establishment — every association, forever. Wire-
                    // proven on lima: alternating boot/0xffff… verifiers,
                    // 2-3 clientids minted per reconnect, and the DS
                    // leaking a record, lease and session per cycle.
                    let clientid = match decoder.decode_u64() {
                        Ok(v) => v,
                        Err(_) => {
                            results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    };
                    if wrong_cred(clientid) {
                        warn!("DS: DESTROY_CLIENTID: client {clientid} is SP4_MACH_CRED, not its principal → WRONG_CRED");
                        (Nfs4Status::WrongCred, Bytes::new())
                    } else if session_mgr.client_session_count(clientid) > 0 {
                        // §18.50: live sessions block the destroy. The DS
                        // holds no stateids/locks/delegations, so sessions
                        // are the entire BUSY predicate.
                        warn!("DS: DESTROY_CLIENTID {} refused — sessions still exist", clientid);
                        (Nfs4Status::ClientIdBusy, Bytes::new())
                    } else if client_mgr.get_client(clientid).is_none() {
                        info!("DS: DESTROY_CLIENTID {} — unknown (stale)", clientid);
                        (Nfs4Status::StaleClientId, Bytes::new())
                    } else {
                        client_mgr.remove_client(clientid);
                        info!("DS: DESTROY_CLIENTID {} — record removed", clientid);
                        (Nfs4Status::Ok, Bytes::new())
                    }
                }

                opcode::DESTROY_SESSION => {
                    match decoder.decode_fixed_opaque(16) {
                        Ok(bytes) if bytes.len() == 16 => {
                            let mut sid = [0u8; 16];
                            sid.copy_from_slice(&bytes);
                            let protected = client_mgr.mach_cred_principal(client_of(&sid)).is_some();
                            if wrong_cred(client_of(&sid)) {
                                warn!("DS: DESTROY_SESSION: SP4_MACH_CRED session, not its principal → WRONG_CRED");
                                (Nfs4Status::WrongCred, Bytes::new())
                            } else if protected && session_mgr.has_session(&sid) && !session_mgr.conn_bound(&sid, conn.id) {
                                warn!("DS: DESTROY_SESSION: connection not bound to SP4_MACH_CRED session → CONN_NOT_BOUND_TO_SESSION");
                                (Nfs4Status::ConnNotBoundToSession, Bytes::new())
                            } else if session_mgr.destroy_session(&sid) {
                                info!("DS: DESTROY_SESSION {:02x?} — destroyed", &sid[0..8]);
                                (Nfs4Status::Ok, Bytes::new())
                            } else {
                                // §18.37: the session does not exist.
                                info!("DS: DESTROY_SESSION {:02x?} — unknown", &sid[0..8]);
                                (Nfs4Status::BadSession, Bytes::new())
                            }
                        }
                        _ => (Nfs4Status::BadXdr, Bytes::new()),
                    }
                }

                opcode::BIND_CONN_TO_SESSION => {
                    // RFC 8881 §18.34. The DS has no back channel (the MDS
                    // owns callbacks), so it binds the fore channel only:
                    // CDFS4_FORE for any direction that includes it
                    // (FORE, FORE_OR_BOTH, BACK_OR_BOTH), INVAL for BACK.
                    // Never RDMA.
                    let sid = match decoder.decode_fixed_opaque(16) {
                        Ok(b) if b.len() == 16 => {
                            let mut sid = [0u8; 16];
                            sid.copy_from_slice(&b);
                            sid
                        }
                        _ => {
                            results.push((opcode, Nfs4Status::BadXdr, OpBody::Inline(Bytes::new())));
                            break;
                        }
                    };
                    let dir = decoder.decode_u32().unwrap_or(0);
                    let _rdma = decoder.decode_bool().unwrap_or(false);
                    const CDFC4_FORE: u32 = 0x1;
                    const CDFS4_FORE: u32 = 0x1;
                    if wrong_cred(client_of(&sid)) {
                        warn!("DS: BIND_CONN_TO_SESSION: SP4_MACH_CRED session, not its principal → WRONG_CRED");
                        (Nfs4Status::WrongCred, Bytes::new())
                    } else if dir & CDFC4_FORE == 0 {
                        warn!("DS: BIND_CONN_TO_SESSION: direction {dir} without the fore channel; the DS has no back channel → INVAL");
                        (Nfs4Status::Inval, Bytes::new())
                    } else if session_mgr.bind_conn(&sid, conn.id) {
                        info!("DS: BIND_CONN_TO_SESSION {:02x?} — connection {} bound", &sid[..8], conn.id);
                        let mut encoder = XdrEncoder::new();
                        encoder.encode_fixed_opaque(&sid);
                        encoder.encode_u32(CDFS4_FORE);
                        encoder.encode_bool(false);
                        (Nfs4Status::Ok, encoder.finish())
                    } else {
                        (Nfs4Status::BadSession, Bytes::new())
                    }
                }

                _ => {
                    warn!("DS received unsupported operation: {}", opcode);
                    (Nfs4Status::NotSupp, Bytes::new())
                }
            };

            results.push((opcode, status, OpBody::Inline(result_data)));
            // RFC 8881 §15.2: a COMPOUND stops at its first failed op. It
            // used to run on, so a READ after a refused SEQUENCE was served.
            if status != Nfs4Status::Ok {
                break;
            }
        }
        Some(results)
    }

    /// [`crate::nfs::node_authz`] at the DS, before `opcode` runs (its
    /// arguments still unread in `decoder`): EXCHANGE_ID and
    /// CREATE_SESSION only from an attached node's machine principal
    /// (NFS4ERR_ACCESS); SEQUENCE only on a session of an attached node's
    /// client, by that node or a user (NFS4ERR_BADSESSION). A session the
    /// DS does not know is left to SEQUENCE.
    fn unattached_refusal(
        nodes: &crate::nfs::node_authz::AttachedNodes,
        opcode: u32,
        principal: &[u8],
        decoder: &XdrDecoder,
        client_mgr: &ClientManager,
    ) -> Option<Nfs4Status> {
        match opcode {
            opcode::EXCHANGE_ID | opcode::CREATE_SESSION => {
                (!nodes.may_create(principal)).then_some(Nfs4Status::Access)
            }
            opcode::SEQUENCE | opcode::BIND_CONN_TO_SESSION => {
                // The session id begins with its client id (DsSessionManager).
                let head: [u8; 8] = decoder.peek_bytes(8).try_into().ok()?;
                let client_id = u64::from_be_bytes(head);
                let owner = client_mgr.get_client(client_id)?;
                (!nodes.may_use(principal, &owner.principal)).then_some(Nfs4Status::BadSession)
            }
            _ => None,
        }
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

    /// Additional client-path endpoints this DS advertises for
    /// multipath trunking (`FLINT_DS_EXTRA_ADVERTISE_ADDRS`, comma-
    /// separated `host` or `host:port`; a bare host gets the NFS bind
    /// port). Each extra address lands in GETDEVICEINFO's
    /// multipath_list4 for this DS, and the Linux client opens ONE
    /// trunked transport per extra address (`rpc_clnt_add_xprt`, capped
    /// at 16 total) — on top of nconnect on the primary. The addresses
    /// must resolve to DISTINCT IPs: the kernel dedupes trunk
    /// candidates by address (ignoring port), so K aliases of one IP
    /// add nothing.
    fn extra_advertise_addresses(&self) -> Vec<String> {
        let bind_port = self.config.bind.port;
        std::env::var("FLINT_DS_EXTRA_ADVERTISE_ADDRS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                // A bare hostname, or one whose tail is not a port,
                // gets the bind port appended. The check must be
                // bracket-aware: `rsplit_once(':')` on a bare IPv6
                // literal finds `1` in `fd00::1` and calls it a port,
                // so the address was kept as-is and shipped with no
                // port at all.
                if crate::netaddr::has_port(s) {
                    s.to_string()
                } else {
                    crate::netaddr::join(s, bind_port)
                }
            })
            .collect()
    }

    /// Register with every MDS shard via gRPC. Per-shard outcomes are
    /// independent — a dead shard logs and is left to that shard's
    /// heartbeat thread to repair; it must not block registration with
    /// the healthy ones.
    async fn register_with_mds(&self) -> Result<()> {
        let endpoint = crate::netaddr::join(&self.advertise_address(), self.config.bind.port);
        let extra_endpoints = self.extra_advertise_addresses();
        info!("📡 Registering with endpoint: {} extra(multipath)={:?} (FLINT_DS_ADVERTISE_ADDR={:?}, POD_IP={:?})",
              endpoint,
              extra_endpoints,
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

        for (shard, (mds_ep, client)) in self.registration_clients.iter().enumerate() {
            info!("Registering with MDS at {} [shard {}]...", mds_ep, shard);
            let mut client = client.lock().await;
            match client.register(
                self.config.device_id.clone(),
                endpoint.clone(),
                extra_endpoints.clone(),
                mount_points.clone(),
                capacity,
                used,
                self.identity_created_at,
                self.config.bind.control_port.unwrap_or(0) as u32,
            ).await {
                Ok(true) => {
                    info!("✅ Successfully registered with MDS ({})", mds_ep);
                }
                Ok(false) => {
                    warn!("⚠️ Registration was rejected by MDS ({})", mds_ep);
                    // Don't fail, will retry
                }
                Err(e) => {
                    error!("❌ Registration failed ({}): {}", mds_ep, e);
                    // Don't fail, will retry
                }
            }
        }
        Ok(())
    }

    /// Probe the export storage's DATA path with a real write + fsync.
    ///
    /// `statvfs` (used above for capacity) reads the cached superblock
    /// and succeeds even when the backing store is EIO'd — the exact
    /// gap that let a storage-dead DS keep heart­beating "Healthy" while
    /// every client write to it returned EIO. This forces actual I/O to
    /// the device (create/truncate a hidden probe file, write a few
    /// bytes, `sync_all`) and returns `Unhealthy` on any failure so the
    /// MDS marks the device Offline and stops striping new files onto
    /// it. The probe file is overwritten in place each tick (no
    /// accumulation) and sits alongside `.flint-ds-identity`.
    ///
    /// Cheap (a few bytes + one fsync). The caller bounds it with a
    /// timeout so a D-state hang on a wedged mount degrades to
    /// `Unhealthy` rather than stalling the heartbeat/re-register loop.
    fn probe_storage_health(data_dir: &std::path::Path) -> crate::pnfs::grpc::HealthStatus {
        use crate::pnfs::grpc::HealthStatus;
        use std::io::Write;
        let probe = data_dir.join(".flint-ds-health");
        let result = (|| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&probe)?;
            f.write_all(b"flint-ds-health\n")?;
            f.sync_all()?; // the step that actually reaches the device
            Ok(())
        })();
        match result {
            Ok(()) => HealthStatus::Healthy,
            Err(e) => {
                warn!(
                    "🩺 DS storage health probe failed on {:?}: {} — reporting Unhealthy",
                    data_dir, e
                );
                HealthStatus::Unhealthy
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
        // One dedicated thread PER SHARD (sharding Phase 2): each
        // shard's liveness, NACK/re-register cycle, and failure
        // counter are fully independent — a dead or slow shard must
        // never delay heartbeats to the others. Single-MDS configs
        // spawn exactly one thread, byte-identical behavior to
        // pre-sharding.
        for (shard, (mds_endpoint, _)) in self.registration_clients.iter().enumerate() {
            self.start_heartbeat_thread(shard, mds_endpoint.clone());
        }
    }

    fn start_heartbeat_thread(&self, shard: usize, mds_endpoint: String) {
        let heartbeat_interval_secs = self.config.mds.heartbeat_interval;

        // Capture config data needed for re-registration
        let device_id = self.config.device_id.clone();
        let bind_port = self.config.bind.port;
        let control_port = self.config.bind.control_port.unwrap_or(0) as u32;
        let identity_created_at = self.identity_created_at;

        // Same precedence as initial registration — see advertise_address().
        let advertise_address = self.advertise_address();
        let extra_endpoints = self.extra_advertise_addresses();

        let mount_points: Vec<String> = self.config.bdevs
            .iter()
            .map(|b| b.mount_point.clone())
            .collect();

        // For DELETE_STRIPE_FILE fd-cache eviction — unlinking alone
        // frees nothing while the data path still holds the fd open.
        // Shared across shard threads: each shard only enqueues
        // cleanups for ITS volumes' files (file_id namespaces are
        // disjoint by the shard-bits construction).
        let io_handler = Arc::clone(&self.io_handler);

        std::thread::Builder::new()
            .name(format!("ds-heartbeat-{}", shard))
            .spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("heartbeat runtime");
        rt.block_on(async move {
            let mut client = RegistrationClient::new(
                device_id.clone(),
                mds_endpoint.clone(),
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

                // Data-path readiness (distinct from the gRPC liveness
                // this heartbeat itself proves). Bounded so a wedged
                // mount that hangs in D-state degrades to Unhealthy
                // instead of freezing this loop (which also drives
                // re-registration). Timeout / probe-panic => Unhealthy.
                let health = {
                    let dir = data_dir.clone();
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        tokio::task::spawn_blocking(move || Self::probe_storage_health(&dir)),
                    )
                    .await
                    {
                        Ok(Ok(h)) => h,
                        _ => {
                            warn!("🩺 DS storage health probe timed out/panicked — reporting Unhealthy");
                            crate::pnfs::grpc::HealthStatus::Unhealthy
                        }
                    }
                };

                let mut reregister_now = false;
                match client.heartbeat(capacity, used, active_connections, health).await {
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
                        warn!("⚠️ Heartbeat not acknowledged — MDS doesn't know us; re-registering now ({})", mds_endpoint);
                        reregister_now = true;
                    }
                    Err(e) => {
                        // Transport errors stay on the 3-strike path: the
                        // MDS may just be mid-restart, and hammering
                        // register() at a dead endpoint adds nothing over
                        // the next heartbeat's retry.
                        error!("❌ Heartbeat failed ({}): {}", mds_endpoint, e);
                        failure_count += 1;
                    }
                }

                if reregister_now || failure_count >= 3 {
                    error!(
                        "MDS lost this device ({}), attempting re-registration",
                        if reregister_now { "heartbeat NACK".to_string() } else { format!("{} transport failures", failure_count) }
                    );
                    
                    // Attempt re-registration (use advertise_address, not bind.address!)
                    let endpoint = crate::netaddr::join(&advertise_address, bind_port);
                    match client.register(
                        device_id.clone(),
                        endpoint.clone(),
                        extra_endpoints.clone(),
                        mount_points.clone(),
                        capacity,
                        used,
                        identity_created_at,
                        control_port,
                    ).await {
                        Ok(true) => {
                            info!("✅ Re-registration successful ({})", mds_endpoint);
                            failure_count = 0;
                        }
                        Ok(false) => {
                            warn!("⚠️ Re-registration rejected by MDS ({}), will retry", mds_endpoint);
                            failure_count = 0;  // Reset to try again later
                        }
                        Err(e) => {
                            error!("❌ Re-registration failed ({}): {}, will retry", mds_endpoint, e);
                            failure_count = 0;  // Reset to try again later
                        }
                    }
                }
            }
        });
            })
            .expect("spawn ds-heartbeat thread");

        info!(
            "Heartbeat sender started on dedicated thread ds-heartbeat-{} (interval: {} seconds)",
            shard, heartbeat_interval_secs
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
        if std::env::var("FLINT_PNFS_CONTROL_TOKEN").is_err()
            && !crate::pnfs::control_auth::mode_from_env().unwrap_or(false)
        {
            warn!(
                "⚠️ FLINT_PNFS_CONTROL_TOKEN unset — DsControl listener is UNAUTHENTICATED; \
                 anyone who can reach port {} can truncate stripe files", port
            );
        }
        let addr: std::net::SocketAddr =
            match crate::netaddr::join(&self.config.bind.address, port).parse() {
                Ok(a) => a,
                Err(e) => {
                    error!("❌ bad DsControl bind address: {}", e);
                    return;
                }
            };
        let svc = DsControlService::new(
            self.config.device_id.clone(),
            std::path::PathBuf::from(
                self.config.bdevs.first().map(|b| b.mount_point.clone())
                    .unwrap_or_else(|| "/data".to_string()),
            ),
        );
        info!("🎛️ DsControl gRPC listener on {}", addr);
        tokio::spawn(async move {
            // Phase 5b: the MDS's own ServiceAccount token, reviewed.
            let sa_policy = match crate::pnfs::control_auth::policy_from_env(crate::pnfs::control_auth::DS_AUDIENCE).await {
                Ok(p) => p,
                Err(e) => {
                    error!("❌ DsControl NOT served: {}", e);
                    return;
                }
            };
            if sa_policy.is_some() {
                info!("🔐 DsControl: the MDS's ServiceAccount token, reviewed");
            }
            let router = tonic::transport::Server::builder()
                .layer(tower::util::option_layer(sa_policy.map(crate::pnfs::control_auth::AuthLayer::new)))
                .add_service(tonic::service::interceptor::InterceptedService::new(
                    crate::pnfs::grpc::DsControlServer::new(svc),
                    crate::pnfs::grpc::check_control_token,
                ));
            // TLS when FLINT_PNFS_CONTROL_TLS_DIR is set (Phase 5).
            let served = match crate::pnfs::control_tls::bind_incoming(addr).await {
                Ok(Some(incoming)) => router.serve_with_incoming(incoming).await.map_err(|e| e.to_string()),
                Ok(None) => router.serve(addr).await.map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            if let Err(e) = served {
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

impl DsControlService {
    pub(crate) fn new(device_id: String, data_dir: std::path::PathBuf) -> Self {
        Self { device_id, data_dir }
    }
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
        let target = match self.validate_stripe_target(&req.device_id, &req.rel_path) {
            Ok(t) => t,
            Err(m) => return refuse(m),
        };
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

    /// F66: serve a fallback READ chunk for the MDS proxy
    /// (docs/plans/mds-fallback-proxy-plan.md §3.4).
    ///
    /// Returns the bytes `read_at` produced, short reads included, and
    /// ok=true with EMPTY data for an absent stripe file — the same
    /// explicit-hole semantics the DS's own NFS data path has
    /// (io.rs: an absent stripe file is a hole, not an error). No
    /// zero-fill, no EOF inference: the DS does not know the file size.
    async fn read_stripe(
        &self,
        request: tonic::Request<crate::pnfs::grpc::ReadStripeRequest>,
    ) -> std::result::Result<
        tonic::Response<crate::pnfs::grpc::ReadStripeResponse>,
        tonic::Status,
    > {
        use std::os::unix::fs::FileExt;
        let req = request.into_inner();
        let refuse = |message: String| {
            warn!("🎛️ ReadStripe refused: {}", message);
            Ok(tonic::Response::new(crate::pnfs::grpc::ReadStripeResponse {
                ok: false,
                message,
                data: Vec::new(),
            }))
        };
        let target = match self.validate_stripe_target(&req.device_id, &req.rel_path) {
            Ok(t) => t,
            Err(m) => return refuse(m),
        };
        // Cap defensively: fallback RPCs are client-sized (~1 MiB via
        // SERVER_MAX_REQUEST); anything larger is a confused caller,
        // not a bigger read.
        if req.count as usize > 4 * 1024 * 1024 {
            return refuse(format!("count {} exceeds the 4 MiB ceiling", req.count));
        }
        let ok = |data: Vec<u8>| {
            Ok(tonic::Response::new(crate::pnfs::grpc::ReadStripeResponse {
                ok: true,
                message: String::new(),
                data,
            }))
        };
        match std::fs::File::open(&target) {
            Ok(f) => {
                let mut buf = vec![0u8; req.count as usize];
                match f.read_at(&mut buf, req.offset) {
                    Ok(n) => {
                        buf.truncate(n);
                        ok(buf)
                    }
                    Err(e) => refuse(format!("read_at {:?}: {}", target, e)),
                }
            }
            // Absent stripe file = hole. Explicitly NOT an error: a
            // sparse file's untouched stripes have no file at all.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ok(Vec::new()),
            Err(e) => refuse(format!("open {:?}: {}", target, e)),
        }
    }

    /// F66: apply a fallback WRITE chunk for the MDS proxy
    /// (docs/plans/mds-fallback-proxy-plan.md §3.3).
    ///
    /// Creates the stripe file if absent (a fallback write may precede
    /// any DS-path write), writes at the FILE offset (sparse
    /// addressing), and — non-negotiably — fdatasyncs BEFORE answering
    /// ok: the MDS replies FILE_SYNC to the client on the strength of
    /// this reply, and a buffered-then-ok would put a power-loss window
    /// behind a durability claim.
    async fn write_stripe(
        &self,
        request: tonic::Request<crate::pnfs::grpc::WriteStripeRequest>,
    ) -> std::result::Result<
        tonic::Response<crate::pnfs::grpc::WriteStripeResponse>,
        tonic::Status,
    > {
        use std::os::unix::fs::FileExt;
        let req = request.into_inner();
        let refuse = |message: String| {
            warn!("🎛️ WriteStripe refused: {}", message);
            Ok(tonic::Response::new(crate::pnfs::grpc::WriteStripeResponse {
                ok: false,
                message,
            }))
        };
        let target = match self.validate_stripe_target(&req.device_id, &req.rel_path) {
            Ok(t) => t,
            Err(m) => return refuse(m),
        };
        // The sync write runs on a blocking thread: this is gRPC on the
        // DS's tokio runtime, and an fdatasync on a busy disk is
        // exactly the ms-scale op the io.rs blocking rules exist for.
        let offset = req.offset;
        let data = req.data;
        let res = tokio::task::spawn_blocking(move || -> std::result::Result<(), String> {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(&target)
                .map_err(|e| format!("open {:?}: {}", target, e))?;
            f.write_all_at(&data, offset)
                .map_err(|e| format!("write_all_at {:?}: {}", target, e))?;
            f.sync_data()
                .map_err(|e| format!("fdatasync {:?}: {}", target, e))?;
            Ok(())
        })
        .await
        .map_err(|e| tonic::Status::internal(format!("write task: {}", e)))?;
        match res {
            Ok(()) => Ok(tonic::Response::new(crate::pnfs::grpc::WriteStripeResponse {
                ok: true,
                message: String::new(),
            })),
            Err(m) => refuse(m),
        }
    }
}

impl DsControlService {
    /// The identity + containment guard every DsControl mutation and
    /// read shares, verbatim from TruncateStripeFile (which keeps its
    /// inline copy's behavior through this same function): refuse a
    /// request addressed to another device (serving DS-B's volume as
    /// DS-A corrupts silently — the registration-stamp rationale), and
    /// refuse absolute/traversal/empty paths.
    fn validate_stripe_target(
        &self,
        device_id: &str,
        rel_path: &str,
    ) -> std::result::Result<std::path::PathBuf, String> {
        if device_id != self.device_id {
            return Err(format!(
                "identity mismatch: request is for '{}', this DS is '{}'",
                device_id, self.device_id
            ));
        }
        let rel = std::path::Path::new(rel_path);
        if rel_path.is_empty()
            || rel.is_absolute()
            || rel.components().any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("suspicious stripe path {:?}", rel_path));
        }
        Ok(self.data_dir.join(rel))
    }
}

/// One operation's result body inside a COMPOUND reply.
///
/// Almost every op result is a handful of words and is happily flattened
/// into the running reply buffer (`Inline`). READ is the exception: its
/// result is ~1 MiB of file data, and flattening it cost THREE full
/// payload copies (`encode_opaque` into the op buffer, `append_raw` into
/// the compound buffer, `append_raw` again into the RPC reply), plus the
/// realloc-doubling of each 8 KiB-born encoder on the way past 1 MiB.
/// Measured on runba (2026-08-02): ~11 MiB of memory traffic per 1 MiB
/// served, ~0.6-1.0 Skylake cores at 918 MiB/s — on nodes that share CPUs
/// with tenant workloads. `Payload` keeps the data as the `Bytes` the I/O
/// layer produced; only the tiny XDR head (eof + length) is encoded, and
/// the payload travels to the socket untouched.
///
/// DELIBERATELY NOT A THROUGHPUT FIX. An 8-agent adversarial review
/// (2026-08-02) established the copies run in the parallel dispatch phase
/// at <1 of 24 workers busy, and the identical flattening binary sustained
/// 2432 MiB/s on runaz — the ceiling lives at the per-TCP-flow drain rate,
/// not here. This is a CPU-per-byte fix; expected throughput change ~0%.
pub(crate) enum OpBody {
    Inline(Bytes),
    /// `head` = the op-specific words BEFORE the opaque byte stream (for
    /// READ: eof bool + data length). `data` = the payload, unpadded; XDR
    /// pad-to-4 is emitted by [`assemble_reply`] AFTER the payload, so a
    /// short read whose length is not a multiple of 4 stays wire-correct.
    /// A `Segment` since the splice port: memory (`Mem`) travels exactly
    /// as the `Bytes` did, and a splice-staged payload (`Piped`) reaches
    /// the writer without ever entering userspace.
    Payload { head: Bytes, data: crate::nfs::segment::Segment },
}

/// Assemble the full RPC reply as an ordered list of wire segments whose
/// concatenation is BYTE-IDENTICAL to the old single-buffer encode (the
/// unit tests below assert exactly that against a reference flatten).
///
/// The running encoder is "cut" at each payload boundary: everything up
/// to and including a READ result's head becomes one small segment, the
/// payload `Bytes` is passed through as its own segment (zero copies),
/// and encoding resumes in a fresh segment with the payload's XDR pad.
/// A compound with no payload op degenerates to one segment — the old
/// behavior with one fewer copy.
pub(crate) fn assemble_reply(
    xid: u32,
    results: Vec<(u32, Nfs4Status, OpBody)>,
) -> Vec<crate::nfs::segment::Segment> {
    let mut cur = XdrEncoder::new();

    // RPC reply header — same six words ReplyBuilder::success emits.
    cur.encode_u32(xid);
    cur.encode_u32(1); // REPLY
    cur.encode_u32(0); // MSG_ACCEPTED
    cur.encode_u32(0); // AUTH_NONE
    cur.encode_u32(0); // auth length
    cur.encode_u32(0); // SUCCESS
    assemble_body(cur, results)
}

/// The COMPOUND reply body after whatever `cur` already holds (the RPC
/// header for [`assemble_reply`], nothing for a GSS reply, which
/// `gss_front::frame_reply` frames).
pub(crate) fn assemble_body(
    mut cur: XdrEncoder,
    results: Vec<(u32, Nfs4Status, OpBody)>,
) -> Vec<crate::nfs::segment::Segment> {
    let mut segs: Vec<crate::nfs::segment::Segment> = Vec::with_capacity(3);

    // COMPOUND response header. The overall status has always been
    // encoded as OK here (clients act on per-op statuses); preserved
    // bit-for-bit — this function changes the copy count, not the wire.
    cur.encode_u32(0); // tag length
    cur.encode_u32(Nfs4Status::Ok as u32);
    cur.encode_u32(results.len() as u32);

    for (opcode, status, body) in results {
        cur.encode_u32(opcode); // opcode is REQUIRED per RFC 8881 §18.2
        cur.encode_u32(status as u32);
        match body {
            OpBody::Inline(data) => cur.append_raw(&data),
            OpBody::Payload { head, data } => {
                cur.append_raw(&head);
                if data.is_empty() {
                    // Zero-length opaque: no bytes, no pad — nothing to
                    // cut a segment for.
                    continue;
                }
                let pad = (4 - data.len() % 4) % 4;
                segs.push(cur.finish().into());
                cur = XdrEncoder::new();
                segs.push(data);
                if pad > 0 {
                    cur.append_raw(&[0u8; 3][..pad]);
                }
            }
        }
    }

    let tail = cur.finish();
    if !tail.is_empty() {
        segs.push(tail.into());
    }
    segs
}

#[cfg(test)]
mod reply_segment_tests {
    use super::*;

    /// The reference implementation: the OLD flatten path, kept alive in
    /// the test so drift between the segmented assembly and the wire
    /// format it replaced is caught as a byte diff, not a client hang.
    fn reference_flatten(xid: u32, results: &[(u32, Nfs4Status, Vec<u8>)]) -> Bytes {
        let mut encoder = XdrEncoder::new();
        encoder.encode_u32(0);
        encoder.encode_u32(Nfs4Status::Ok as u32);
        encoder.encode_u32(results.len() as u32);
        for (opcode, status, data) in results {
            encoder.encode_u32(*opcode);
            encoder.encode_u32(*status as u32);
            encoder.append_raw(data);
        }
        let compound = encoder.finish();

        let mut reply = XdrEncoder::new();
        reply.encode_u32(xid);
        reply.encode_u32(1);
        reply.encode_u32(0);
        reply.encode_u32(0);
        reply.encode_u32(0);
        reply.encode_u32(0);
        reply.append_raw(&compound);
        reply.finish()
    }

    fn concat(segs: &[crate::nfs::segment::Segment]) -> Vec<u8> {
        segs.iter().flat_map(|s| s.as_mem().iter().copied()).collect()
    }

    /// A READ result body as the old code built it, for the reference.
    fn read_body_flat(eof: bool, data: &[u8]) -> Vec<u8> {
        let mut e = XdrEncoder::new();
        e.encode_bool(eof);
        e.encode_opaque(data);
        e.finish().to_vec()
    }

    fn read_body_segmented(eof: bool, data: &[u8]) -> OpBody {
        let mut h = XdrEncoder::new();
        h.encode_bool(eof);
        h.encode_u32(data.len() as u32);
        OpBody::Payload { head: h.finish(), data: Bytes::copy_from_slice(data).into() }
    }

    #[test]
    fn segmented_read_reply_is_byte_identical_to_the_flatten_it_replaced() {
        // SEQUENCE + PUTFH + READ, the exact compound the Linux client
        // sends for pNFS data — payload length a multiple of 4.
        let payload = vec![0xAB; 1 << 20];
        let seq = vec![7u8; 40];
        let segs = assemble_reply(0xDEAD, vec![
            (opcode::SEQUENCE, Nfs4Status::Ok, OpBody::Inline(Bytes::from(seq.clone()))),
            (opcode::PUTFH, Nfs4Status::Ok, OpBody::Inline(Bytes::new())),
            (opcode::READ, Nfs4Status::Ok, read_body_segmented(false, &payload)),
        ]);
        let reference = reference_flatten(0xDEAD, &[
            (opcode::SEQUENCE, Nfs4Status::Ok, seq),
            (opcode::PUTFH, Nfs4Status::Ok, Vec::new()),
            (opcode::READ, Nfs4Status::Ok, read_body_flat(false, &payload)),
        ]);
        assert_eq!(concat(&segs), reference.to_vec());
        // And the payload segment is the payload ITSELF, not a copy of it
        // — three segments: [headers+head][payload][none: len%4==0 and no
        // trailing ops, so no tail].
        assert_eq!(segs.len(), 2, "1 MiB payload with no pad and no tail");
        assert_eq!(segs[1].len(), 1 << 20);
    }

    #[test]
    fn short_read_pad_lands_after_the_payload() {
        // 5-byte read at EOF: XDR requires 3 pad bytes after the opaque.
        // The pad must open the TAIL segment, not corrupt the payload.
        for len in [1usize, 2, 3, 5, 1023] {
            let payload = vec![0x5A; len];
            let segs = assemble_reply(1, vec![
                (opcode::READ, Nfs4Status::Ok, read_body_segmented(true, &payload)),
            ]);
            let reference = reference_flatten(1, &[
                (opcode::READ, Nfs4Status::Ok, read_body_flat(true, &payload)),
            ]);
            assert_eq!(concat(&segs), reference.to_vec(), "len={len}");
        }
    }

    #[test]
    fn ops_after_the_payload_survive_the_cut() {
        // READ not last in the compound: the following op's words must
        // land after the payload and its pad, in their own segment.
        let payload = vec![0x11; 7];
        let getattr_ish = vec![9u8; 12];
        let segs = assemble_reply(2, vec![
            (opcode::READ, Nfs4Status::Ok, read_body_segmented(false, &payload)),
            (opcode::PUTFH, Nfs4Status::Ok, OpBody::Inline(Bytes::from(getattr_ish.clone()))),
        ]);
        let reference = reference_flatten(2, &[
            (opcode::READ, Nfs4Status::Ok, read_body_flat(false, &payload)),
            (opcode::PUTFH, Nfs4Status::Ok, getattr_ish),
        ]);
        assert_eq!(concat(&segs), reference.to_vec());
        assert_eq!(segs.len(), 3, "head / payload / pad+next-op tail");
    }

    #[test]
    fn inline_only_compound_is_one_segment() {
        let segs = assemble_reply(3, vec![
            (opcode::SEQUENCE, Nfs4Status::Ok, OpBody::Inline(Bytes::from(vec![1, 2, 3, 4]))),
            (opcode::COMMIT, Nfs4Status::Io, OpBody::Inline(Bytes::new())),
        ]);
        assert_eq!(segs.len(), 1, "no payload op, no cut");
        let reference = reference_flatten(3, &[
            (opcode::SEQUENCE, Nfs4Status::Ok, vec![1, 2, 3, 4]),
            (opcode::COMMIT, Nfs4Status::Io, Vec::new()),
        ]);
        assert_eq!(concat(&segs), reference.to_vec());
    }

    #[test]
    fn empty_payload_read_is_wire_correct() {
        // EOF-at-offset read: zero-length opaque, no pad, no cut.
        let segs = assemble_reply(4, vec![
            (opcode::READ, Nfs4Status::Ok, read_body_segmented(true, &[])),
        ]);
        let reference = reference_flatten(4, &[
            (opcode::READ, Nfs4Status::Ok, read_body_flat(true, &[])),
        ]);
        assert_eq!(concat(&segs), reference.to_vec());
        assert_eq!(segs.len(), 1);
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

    // ── F66 ReadStripe/WriteStripe: the same guards, test for test ──

    fn rreq(device_id: &str, rel: &str, offset: u64, count: u32)
        -> tonic::Request<crate::pnfs::grpc::ReadStripeRequest> {
        tonic::Request::new(crate::pnfs::grpc::ReadStripeRequest {
            device_id: device_id.into(), rel_path: rel.into(), offset, count,
        })
    }
    fn wreq(device_id: &str, rel: &str, offset: u64, data: Vec<u8>)
        -> tonic::Request<crate::pnfs::grpc::WriteStripeRequest> {
        tonic::Request::new(crate::pnfs::grpc::WriteStripeRequest {
            device_id: device_id.into(), rel_path: rel.into(), offset, data,
        })
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_at_file_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        // Sparse addressing: write at a large FILE offset on a file
        // that does not exist yet (create-on-write).
        let w = s.write_stripe(wreq("ds-test-1", "cafe.stripe0", 8 << 20, vec![7u8; 4096]))
            .await.unwrap().into_inner();
        assert!(w.ok, "{}", w.message);
        let r = s.read_stripe(rreq("ds-test-1", "cafe.stripe0", 8 << 20, 4096))
            .await.unwrap().into_inner();
        assert!(r.ok, "{}", r.message);
        assert_eq!(r.data, vec![7u8; 4096]);
    }

    #[tokio::test]
    async fn absent_stripe_file_reads_as_an_explicit_hole() {
        let dir = tempfile::tempdir().unwrap();
        let r = svc(dir.path())
            .read_stripe(rreq("ds-test-1", "never-written.stripe1", 0, 65536))
            .await.unwrap().into_inner();
        assert!(r.ok, "absent stripe file is a HOLE, not an error: {}", r.message);
        assert!(r.data.is_empty(), "the DS must not zero-fill — it does not know the file size");
    }

    #[tokio::test]
    async fn short_read_is_reported_raw() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.stripe0"), vec![9u8; 100]).unwrap();
        let r = svc(dir.path())
            .read_stripe(rreq("ds-test-1", "s.stripe0", 40, 4096))
            .await.unwrap().into_inner();
        assert!(r.ok);
        assert_eq!(r.data.len(), 60, "short read passes through raw — no zero-fill, no EOF guess");
    }

    #[tokio::test]
    async fn proxy_rpcs_refuse_foreign_device_and_bad_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.stripe0"), vec![1u8; 16]).unwrap();
        let s = svc(dir.path());
        let r = s.read_stripe(rreq("ds-OTHER", "s.stripe0", 0, 16)).await.unwrap().into_inner();
        assert!(!r.ok && r.message.contains("identity mismatch"));
        let w = s.write_stripe(wreq("ds-OTHER", "s.stripe0", 0, vec![2u8; 4])).await.unwrap().into_inner();
        assert!(!w.ok && w.message.contains("identity mismatch"));
        assert_eq!(std::fs::read(dir.path().join("s.stripe0")).unwrap(), vec![1u8; 16], "untouched");
        for bad in ["../escape", "/etc/passwd", ""] {
            let r = s.read_stripe(rreq("ds-test-1", bad, 0, 4)).await.unwrap().into_inner();
            assert!(!r.ok, "read should refuse {:?}", bad);
            let w = s.write_stripe(wreq("ds-test-1", bad, 0, vec![0u8; 4])).await.unwrap().into_inner();
            assert!(!w.ok, "write should refuse {:?}", bad);
        }
    }

    #[tokio::test]
    async fn written_bytes_are_durable_before_ok() {
        // Behavioral proxy for the fdatasync contract: the write path
        // must go through sync_data before replying. We can't power-cut
        // a unit test, but we CAN assert the bytes are visible through
        // a fresh descriptor immediately after ok — which a
        // buffered-only implementation also passes, so the real belt is
        // the sync_data call in the handler; this test pins that the
        // data lands where a crash-recovery reader would look.
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        let w = s.write_stripe(wreq("ds-test-1", "d.stripe1", 512, vec![0xAA; 256]))
            .await.unwrap().into_inner();
        assert!(w.ok, "{}", w.message);
        let on_disk = std::fs::read(dir.path().join("d.stripe1")).unwrap();
        assert_eq!(&on_disk[512..768], &[0xAA; 256][..]);
        assert_eq!(&on_disk[..512], &vec![0u8; 512][..], "leading hole reads as zeros");
    }
}

#[cfg(test)]
mod storage_health_probe_tests {
    use super::*;
    use crate::pnfs::grpc::HealthStatus;

    #[test]
    fn healthy_on_writable_export() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            DataServer::probe_storage_health(dir.path()),
            HealthStatus::Healthy
        );
        // Probe file exists and is overwritten in place (no accumulation).
        let probe = dir.path().join(".flint-ds-health");
        assert!(probe.exists());
        DataServer::probe_storage_health(dir.path());
        let entries = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() == std::ffi::OsStr::new(".flint-ds-health"))
            .count();
        assert_eq!(entries, 1, "probe file overwritten, not accumulated");
    }

    #[test]
    fn unhealthy_when_export_unwritable() {
        // Parent directory does not exist → create/write fails → the
        // storage-dead signal the MDS needs to drop this DS from layouts.
        let bogus = std::path::Path::new("/flint-nonexistent-export-xyz/data");
        assert_eq!(
            DataServer::probe_storage_health(bogus),
            HealthStatus::Unhealthy
        );
    }
}

/// THE DS CLIENT-BEHAVIOUR PIN (docs/plans/nfs-delegations-design.md
/// §9, slice 5).
///
/// A client holding a READ delegation plus a READ layout sends its
/// READs to the data servers, and RFC 8881 §13.9.1 lets it present the
/// DELEGATION stateid there. Delegation stateids are minted on the
/// MDS, unpersisted and epoch-mixed; the DS has never heard of them.
/// The path works today for one reason: the DS discards the stateid
/// on READ and WRITE (`let _stateid = ...` in the compound arm) and
/// trusts the MDS's layout grant as its authorization.
///
/// Any "the DS validates the stateid" assertion is therefore
/// vacuously green, and this test deliberately asserts the OPPOSITE:
/// a stateid the DS cannot know is ACCEPTED. If DS-side stateid
/// validation is ever added, this fails first — which is the point:
/// that work must then teach the DS about delegation stateids (or
/// keep trusting the MDS), instead of silently breaking every
/// delegation-holding pNFS reader with BAD_STATEID.
#[cfg(test)]
mod deleg_stateid_pin_tests {
    use super::*;
    use crate::nfs::v4::filehandle_pnfs::generate_pnfs_filehandle_from_id;
    use crate::nfs::rpc::Auth;

    /// One COMPOUND [PUTFH, READ(stateid)] through the DS's real
    /// compound arm, decoded back to (PUTFH status, READ status, data).
    async fn ds_read_with_stateid(
        base: &std::path::Path,
        fh: &[u8],
        stateid_seqid: u32,
        stateid_other: [u8; 12],
    ) -> (Nfs4Status, Nfs4Status, Vec<u8>) {
        let io = Arc::new(IoOperationHandler::new(base).unwrap());
        let sessions = Arc::new(DsSessionManager::new());
        let leases = Arc::new(LeaseManager::new());
        let clients = Arc::new(ClientManager::new(
            leases,
            "ds-pin",
            crate::state_backend::memory_backend(),
        ));

        let mut args = XdrEncoder::new();
        args.encode_u32(0); // tag length
        args.encode_u32(1); // minorversion
        args.encode_u32(3); // ops
        test_sequence(&mut args, [7u8; 16], 1);
        args.encode_u32(opcode::PUTFH);
        args.encode_opaque(fh);
        args.encode_u32(opcode::READ);
        args.encode_u32(stateid_seqid);
        args.append_raw(&stateid_other);
        args.encode_u64(0); // offset
        args.encode_u32(64); // count

        let call = CallMessage {
            xid: 0x5eed,
            program: NFS4_PROGRAM,
            version: 4,
            procedure: procedure::COMPOUND,
            cred: Auth::null(),
            verf: Auth::null(),
            cred_span: Bytes::new(),
        };
        let segs = DataServer::handle_minimal_compound(call, args.finish(), io, sessions, clients, None, DsConn::test(1)).await;
        // `into_test_bytes`, not `as_mem`: on Linux the DS answers a
        // READ through the splice path, so the payload segment is a
        // pipe, not bytes. `as_mem` panics on it by design. (macOS
        // compiles the splice arm out entirely, which is why this only
        // ever showed up on the Linux suite.)
        let mut reply = Vec::new();
        for s in segs {
            reply.extend_from_slice(&s.into_test_bytes());
        }
        // RPC accepted-reply header: xid, REPLY, MSG_ACCEPTED, verf
        // (AUTH_NONE, len 0), SUCCESS = 6 words; then COMPOUND4res.
        let mut dec = XdrDecoder::new(Bytes::from(reply));
        for _ in 0..6 {
            dec.decode_u32().unwrap();
        }
        let _tag_len = dec.decode_u32().unwrap();
        let _status = dec.decode_u32().unwrap();
        let nres = dec.decode_u32().unwrap();
        assert_eq!(dec.decode_u32().unwrap(), opcode::SEQUENCE);
        assert_eq!(dec.decode_u32().unwrap(), Nfs4Status::Ok as u32);
        test_skip_sequence_res(&mut dec);
        assert_eq!(dec.decode_u32().unwrap(), opcode::PUTFH);
        let putfh = Nfs4Status::from_u32(dec.decode_u32().unwrap());
        if putfh != Nfs4Status::Ok {
            assert_eq!(nres, 2, "an error ends the compound");
            return (putfh, Nfs4Status::Ok, Vec::new());
        }
        assert_eq!(nres, 3, "every op answers — a truncated compound is its own bug");
        assert_eq!(dec.decode_u32().unwrap(), opcode::READ);
        let read = Nfs4Status::from_u32(dec.decode_u32().unwrap());
        let data = if read == Nfs4Status::Ok {
            let _eof = dec.decode_bool().unwrap();
            dec.decode_opaque().unwrap().to_vec()
        } else {
            Vec::new()
        };
        (putfh, read, data)
    }

    #[tokio::test]
    async fn a_delegation_shaped_stateid_the_ds_never_saw_is_accepted_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let file_id: u64 = 0x0000_0000_dead_beef;
        std::fs::write(
            dir.path().join(format!("{:016x}.stripe0", file_id)),
            b"bytes the holder reads from the DS",
        )
        .unwrap();
        let fh = generate_pnfs_filehandle_from_id(0, file_id, 0);

        // A delegation stateid as the MDS mints one: seqid 1 for life,
        // an `other` no DS table contains (epoch-mixed on the MDS).
        let mut other = [0xE7u8; 12];
        other[0..4].copy_from_slice(&0xDE1E_6A7Eu32.to_be_bytes());
        let (putfh, read, data) = ds_read_with_stateid(dir.path(), &fh.data, 1, other).await;
        assert_eq!(putfh, Nfs4Status::Ok);
        assert_eq!(
            read,
            Nfs4Status::Ok,
            "the DS must not validate stateids it cannot know: a delegation holder's \
             DS READ carries the MDS-minted delegation stateid, and BAD_STATEID here \
             would break every delegation-holding pNFS reader"
        );
        assert_eq!(&data[..], b"bytes the holder reads from the DS");

        // ANTI-VACUITY: the arm is not accepting because it ignores
        // everything — a stateid it could never resolve AND a bogus
        // filehandle is refused at PUTFH, so the OK above is the READ
        // arm's own verdict on a resolvable fh with an unknown stateid.
        let bogus_fh = [0xFFu8; 4];
        let (_putfh, read2, _) = ds_read_with_stateid(dir.path(), &bogus_fh, 1, other).await;
        assert_ne!(read2, Nfs4Status::Ok, "a garbage fh must not read OK");
    }
}

/// Kerberos on the data server (pNFS Kerberos plan, Phase 3): what the DS
/// answers before and around the GSS front end. The front end itself is
/// `nfs::gss_front`'s, tested with the MDS's.
#[cfg(test)]
mod ds_gss_tests {
    use super::*;
    use crate::nfs::rpc::Auth;
    use crate::nfs::v4::state::LeaseManager;

    fn parts(base: &std::path::Path) -> (Arc<IoOperationHandler>, Arc<DsSessionManager>, Arc<ClientManager>) {
        let io = Arc::new(IoOperationHandler::new(base).unwrap());
        let sessions = Arc::new(DsSessionManager::new());
        let clients = Arc::new(ClientManager::new(
            Arc::new(LeaseManager::new()),
            "ds-gss",
            crate::state_backend::memory_backend(),
        ));
        (io, sessions, clients)
    }

    fn flat(segs: Vec<crate::nfs::segment::Segment>) -> Bytes {
        let mut v = Vec::new();
        for s in segs {
            v.extend_from_slice(&s.into_test_bytes());
        }
        Bytes::from(v)
    }

    /// An RPC CALL record: COMPOUND (or `proc`) with `flavor` and an
    /// opaque credential body, a null verifier, then `args`.
    fn call_record(proc_: u32, flavor: u32, cred: &[u8], args: &[u8]) -> Bytes {
        let mut e = XdrEncoder::new();
        e.encode_u32(0x7a11); // xid
        e.encode_u32(0); // CALL
        e.encode_u32(2); // RPC version
        e.encode_u32(NFS4_PROGRAM);
        e.encode_u32(4);
        e.encode_u32(proc_);
        e.encode_u32(flavor);
        e.encode_opaque(cred);
        e.encode_u32(0); // verifier: AUTH_NONE
        e.encode_u32(0);
        e.append_raw(args);
        e.finish()
    }

    #[tokio::test]
    async fn without_a_keytab_an_rpcsec_gss_call_is_refused_not_answered() {
        let dir = tempfile::tempdir().unwrap();
        let (io, s, c) = parts(dir.path());
        // Any RPCSEC_GSS credential: the DS must not look past the flavor.
        let rec = call_record(procedure::NULL, 6, &[0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0], &[]);
        let reply = flat(DataServer::dispatch_minimal_nfs(rec, io, s, c, None, DsConn::test(1)).await);
        let mut dec = XdrDecoder::new(reply);
        assert_eq!(dec.decode_u32().unwrap(), 0x7a11);
        assert_eq!(dec.decode_u32().unwrap(), 1, "REPLY");
        assert_eq!(dec.decode_u32().unwrap(), 1, "MSG_DENIED: before, the DS answered it as if unauthenticated");
        assert_eq!(dec.decode_u32().unwrap(), 1, "AUTH_ERROR");
        assert_eq!(dec.decode_u32().unwrap(), AuthStat::RejectedCred as u32);
    }

    #[tokio::test]
    async fn secinfo_no_name_reads_its_style_and_puts_auth_sys_before_auth_none() {
        let dir = tempfile::tempdir().unwrap();
        let (io, s, c) = parts(dir.path());
        let mut a = XdrEncoder::new();
        a.encode_u32(0); // tag
        a.encode_u32(1); // minorversion
        a.encode_u32(3);
        test_sequence(&mut a, [7u8; 16], 1);
        a.encode_u32(opcode::PUTFH);
        a.encode_opaque(&[0xAB; 16]);
        a.encode_u32(opcode::SECINFO_NO_NAME);
        a.encode_u32(0); // SECINFO_STYLE4_CURRENT_FH
        let call = CallMessage {
            xid: 1,
            program: NFS4_PROGRAM,
            version: 4,
            procedure: procedure::COMPOUND,
            cred: Auth::null(),
            verf: Auth::null(),
            cred_span: Bytes::new(),
        };
        let reply = flat(DataServer::handle_minimal_compound(call, a.finish(), io, s, c, None, DsConn::test(1)).await);
        let mut dec = XdrDecoder::new(reply);
        for _ in 0..8 {
            dec.decode_u32().unwrap();
        }
        assert_eq!(dec.decode_u32().unwrap(), 3, "every op answered");
        assert_eq!(dec.decode_u32().unwrap(), opcode::SEQUENCE);
        assert_eq!(dec.decode_u32().unwrap(), Nfs4Status::Ok as u32);
        test_skip_sequence_res(&mut dec);
        assert_eq!(dec.decode_u32().unwrap(), opcode::PUTFH);
        assert_eq!(dec.decode_u32().unwrap(), Nfs4Status::Ok as u32);
        assert_eq!(dec.decode_u32().unwrap(), opcode::SECINFO_NO_NAME);
        assert_eq!(dec.decode_u32().unwrap(), Nfs4Status::Ok as u32);
        let n = dec.decode_u32().unwrap();
        assert!(n >= 1);
        // No floor and no keytab here: AUTH_SYS, then AUTH_NONE, and no
        // Kerberos it cannot do (it offered AUTH_NONE first, and krb5).
        assert_eq!(dec.decode_u32().unwrap(), 1, "AUTH_SYS first");
        let rest: Vec<u32> = (1..n).map(|_| dec.decode_u32().unwrap()).collect();
        assert_eq!(rest, vec![0], "then AUTH_NONE, nothing else");
    }

    /// EXCHANGE_ID with `state_protect` (its words), an impl_id, then a
    /// PUTFH: the PUTFH decodes only if EXCHANGE_ID read its arguments
    /// whole.
    fn exchange_id_then_putfh(state_protect: &[u32]) -> Bytes {
        let mut a = XdrEncoder::new();
        a.encode_u32(0); // tag
        a.encode_u32(1);
        a.encode_u32(2);
        a.encode_u32(opcode::EXCHANGE_ID);
        a.encode_u64(42); // verifier
        a.encode_opaque(b"client-owner");
        a.encode_u32(0); // flags
        for w in state_protect {
            a.encode_u32(*w);
        }
        a.encode_bool(true); // impl_id: domain, name, date
        a.encode_opaque(b"kernel.org");
        a.encode_opaque(b"Linux");
        a.encode_u64(0);
        a.encode_u32(0);
        a.encode_u32(opcode::PUTFH);
        a.encode_opaque(&[0xAB; 16]);
        a.finish()
    }

    /// (EXCHANGE_ID's status, then the next result's (opcode, status)
    /// if there is one).
    async fn exchange_id_then_putfh_status(state_protect: &[u32]) -> (Nfs4Status, Option<(u32, Nfs4Status)>) {
        let dir = tempfile::tempdir().unwrap();
        let (io, s, c) = parts(dir.path());
        let call = CallMessage {
            xid: 2,
            program: NFS4_PROGRAM,
            version: 4,
            procedure: procedure::COMPOUND,
            cred: Auth::null(),
            verf: Auth::null(),
            cred_span: Bytes::new(),
        };
        let reply = flat(
            DataServer::handle_minimal_compound(call, exchange_id_then_putfh(state_protect), io, s, c, None, DsConn::test(1)).await,
        );
        let mut dec = XdrDecoder::new(reply);
        for _ in 0..8 {
            dec.decode_u32().unwrap();
        }
        let n = dec.decode_u32().unwrap();
        assert_eq!(dec.decode_u32().unwrap(), opcode::EXCHANGE_ID);
        let st = Nfs4Status::from_u32(dec.decode_u32().unwrap());
        if st != Nfs4Status::Ok {
            assert_eq!(n, 1, "an error ends the compound");
            return (st, None);
        }
        // EXCHANGE_ID4resok: clientid, sequenceid, flags, state_protect
        // (SP4_NONE), server_owner, server_scope, impl_id<>.
        dec.decode_u64().unwrap();
        dec.decode_u32().unwrap();
        dec.decode_u32().unwrap();
        assert_eq!(dec.decode_u32().unwrap(), 0, "the DS answers SP4_NONE");
        dec.decode_u64().unwrap();
        dec.decode_opaque().unwrap();
        dec.decode_opaque().unwrap();
        assert_eq!(dec.decode_u32().unwrap(), 0);
        assert_eq!(n, 2);
        let op = dec.decode_u32().unwrap();
        (st, Some((op, Nfs4Status::from_u32(dec.decode_u32().unwrap()))))
    }

    #[tokio::test]
    async fn exchange_id_decodes_every_state_protection() {
        // The op after EXCHANGE_ID is read as PUTFH only if EXCHANGE_ID
        // read its arguments whole; outside a session it is then refused.
        let putfh_read = Some((opcode::PUTFH, Nfs4Status::OpNotInSession));
        // SP4_NONE.
        assert_eq!(exchange_id_then_putfh_status(&[0]).await, (Nfs4Status::Ok, putfh_read));
        // SP4_MACH_CRED, two one-word bitmaps: read whole, so the PUTFH
        // after it decodes (only the discriminant was read before, and
        // the bitmaps were taken for the impl_id).
        assert_eq!(
            exchange_id_then_putfh_status(&[1, 1, 0x0000_0003, 1, 0x0000_0010]).await,
            (Nfs4Status::Ok, putfh_read)
        );
        // SP4_SSV: refused, as on the MDS.
        assert_eq!(exchange_id_then_putfh_status(&[2, 0, 0, 0, 0, 0]).await, (Nfs4Status::EncrAlgUnsupp, None));
    }

    #[test]
    fn only_attached_nodes_make_and_use_data_server_sessions() {
        use crate::nfs::node_authz::AttachedNodes;
        use crate::nfs::v4::state::client::ExchangeIdOutcome;
        let dir = tempfile::tempdir().unwrap();
        let (_io, _s, clients) = parts(dir.path());
        let nodes = AttachedNodes::default();
        nodes.set(std::collections::HashSet::from(["w1".to_string()]));
        let none = XdrDecoder::new(Bytes::new());
        let w1 = b"gss:nfs/w1@CORP".as_slice();
        let w2 = b"gss:nfs/w2@CORP".as_slice();
        // Making a client or a session: an attached node's key only.
        for op in [opcode::EXCHANGE_ID, opcode::CREATE_SESSION] {
            assert_eq!(DataServer::unattached_refusal(&nodes, op, w1, &none, &clients), None);
            assert_eq!(DataServer::unattached_refusal(&nodes, op, w2, &none, &clients), Some(Nfs4Status::Access));
            assert_eq!(DataServer::unattached_refusal(&nodes, op, b"gss:bob@CORP", &none, &clients), Some(Nfs4Status::Access));
            assert_eq!(DataServer::unattached_refusal(&nodes, op, b"", &none, &clients), Some(Nfs4Status::Access));
        }
        // Using one: the session's client must be an attached node's.
        let id = |principal: &[u8]| match clients.exchange_id(principal.to_vec(), 1, 0, principal.to_vec()) {
            ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("{other:?}"),
        };
        let seq = |client_id: u64| {
            let mut sid = client_id.to_be_bytes().to_vec();
            sid.extend_from_slice(&[0; 8]);
            XdrDecoder::new(Bytes::from(sid))
        };
        let c1 = id(w1);
        let c2 = id(w2);
        assert_eq!(DataServer::unattached_refusal(&nodes, opcode::SEQUENCE, w1, &seq(c1), &clients), None);
        assert_eq!(
            DataServer::unattached_refusal(&nodes, opcode::SEQUENCE, b"gss:bob@CORP", &seq(c1), &clients),
            None,
            "a user rides an attached node's session"
        );
        assert_eq!(
            DataServer::unattached_refusal(&nodes, opcode::SEQUENCE, w2, &seq(c1), &clients),
            Some(Nfs4Status::BadSession),
            "another node's key on w1's session"
        );
        assert_eq!(
            DataServer::unattached_refusal(&nodes, opcode::SEQUENCE, w2, &seq(c2), &clients),
            Some(Nfs4Status::BadSession),
            "w2 is attached to nothing"
        );
        // Detached: w1's session stops.
        nodes.set(std::collections::HashSet::new());
        assert_eq!(DataServer::unattached_refusal(&nodes, opcode::SEQUENCE, w1, &seq(c1), &clients), Some(Nfs4Status::BadSession));
        // Reads and writes are not gated here: they ride the session.
        assert_eq!(DataServer::unattached_refusal(&nodes, opcode::READ, w2, &none, &clients), None);
    }
}


/// pNFS Kerberos plan, Phase 3b: SP4_MACH_CRED on the data servers.
#[cfg(test)]
mod mach_cred_tests {
    use super::*;
    use crate::nfs::rpc::Auth;
    use crate::nfs::rpcsec_gss::GssService;

    struct Ds {
        io: Arc<IoOperationHandler>,
        sessions: Arc<DsSessionManager>,
        clients: Arc<ClientManager>,
        _dir: tempfile::TempDir,
    }

    fn ds() -> Ds {
        let dir = tempfile::tempdir().unwrap();
        Ds {
            io: Arc::new(IoOperationHandler::new(dir.path()).unwrap()),
            sessions: Arc::new(DsSessionManager::new()),
            clients: Arc::new(ClientManager::new(
                Arc::new(LeaseManager::new()),
                "ds-mach",
                crate::state_backend::memory_backend(),
            )),
            _dir: dir,
        }
    }

    /// Who sends a compound: a GSS principal under a service, or AUTH_NONE.
    #[derive(Clone, Copy)]
    enum As {
        Gss(&'static str, GssService),
        Null,
    }

    const W1: As = As::Gss("nfs/w1@CORP", GssService::Integrity);
    const W2: As = As::Gss("nfs/w2@CORP", GssService::Integrity);
    const BOB: As = As::Gss("bob@CORP", GssService::Privacy);

    impl Ds {
        /// Run `ops` (encoded) on connection `conn`, sent as `who`.
        async fn run(&self, who: As, conn: DsConn, nops: u32, ops: impl FnOnce(&mut XdrEncoder)) -> Vec<(u32, Nfs4Status, Bytes)> {
            let mut a = XdrEncoder::new();
            a.encode_u32(0);
            a.encode_u32(1);
            a.encode_u32(nops);
            ops(&mut a);
            let call = CallMessage {
                xid: 9,
                program: NFS4_PROGRAM,
                version: 4,
                procedure: procedure::COMPOUND,
                cred: Auth::null(),
                verf: Auth::null(),
                cred_span: Bytes::new(),
            };
            let v = match who {
                As::Gss(p, service) => Some(ValidatedCall { service, seq_num: 1, client_principal: Some(p.into()), tokens: None }),
                As::Null => None,
            };
            DataServer::run_compound(call, a.finish(), self.io.clone(), self.sessions.clone(), self.clients.clone(), v.as_ref(), conn)
                .await
                .expect("decodes")
                .into_iter()
                .map(|(op, st, body)| match body {
                    OpBody::Inline(b) => (op, st, b),
                    _ => (op, st, Bytes::new()),
                })
                .collect()
        }

        /// EXCHANGE_ID with `state_protect` words; (status, clientid, spr_how).
        async fn exchange_id(&self, who: As, conn: DsConn, owner: &[u8], sp: &[u32]) -> (Nfs4Status, u64, u32) {
            let r = self
                .run(who, conn, 1, |a| {
                    a.encode_u32(opcode::EXCHANGE_ID);
                    a.encode_u64(42);
                    a.encode_opaque(owner);
                    a.encode_u32(0);
                    for w in sp {
                        a.encode_u32(*w);
                    }
                    a.encode_bool(false);
                })
                .await;
            let (_, st, body) = r.into_iter().next().unwrap();
            if st != Nfs4Status::Ok {
                return (st, 0, 0);
            }
            let mut d = XdrDecoder::new(body);
            let id = d.decode_u64().unwrap();
            d.decode_u32().unwrap();
            d.decode_u32().unwrap();
            let how = d.decode_u32().unwrap();
            if how == 1 {
                use crate::nfs::v4::xdr::Nfs4XdrDecoder as _;
                let enforce = d.decode_bitmap().unwrap();
                for op in [
                    opcode::BIND_CONN_TO_SESSION,
                    opcode::EXCHANGE_ID,
                    opcode::CREATE_SESSION,
                    opcode::DESTROY_SESSION,
                    opcode::DESTROY_CLIENTID,
                ] {
                    assert!(enforce[(op / 32) as usize] & (1 << (op % 32)) != 0, "must_enforce lacks op {op}");
                }
                assert!(d.decode_bitmap().unwrap().iter().all(|w| *w == 0), "must_allow empty");
            }
            (st, id, how)
        }

        async fn create_session(&self, who: As, conn: DsConn, clientid: u64) -> (Nfs4Status, [u8; 16]) {
            let r = self
                .run(who, conn, 1, |a| {
                    a.encode_u32(opcode::CREATE_SESSION);
                    a.encode_u64(clientid);
                    a.encode_u32(1);
                    a.encode_u32(0);
                    for _ in 0..2 {
                        for w in [0, 1 << 20, 1 << 20, 4096, 16, 16, 0] {
                            a.encode_u32(w);
                        }
                    }
                    a.encode_u32(0x4000_0000);
                    a.encode_u32(0);
                })
                .await;
            let (_, st, body) = r.into_iter().next().unwrap();
            let mut sid = [0u8; 16];
            if st == Nfs4Status::Ok {
                sid.copy_from_slice(&body[..16]);
            }
            (st, sid)
        }

        async fn one(&self, who: As, conn: DsConn, op: u32, args: impl FnOnce(&mut XdrEncoder)) -> Nfs4Status {
            let r = self
                .run(who, conn, 1, |a| {
                    a.encode_u32(op);
                    args(a);
                })
                .await;
            r[0].1
        }

        async fn sequence(&self, who: As, conn: DsConn, sid: [u8; 16]) -> Nfs4Status {
            self.run(who, conn, 1, |a| test_sequence(a, sid, 1)).await[0].1
        }

        async fn bind(&self, who: As, conn: DsConn, sid: [u8; 16], dir: u32) -> Nfs4Status {
            self.one(who, conn, opcode::BIND_CONN_TO_SESSION, |a| {
                a.encode_fixed_opaque(&sid);
                a.encode_u32(dir);
                a.encode_bool(false);
            })
            .await
        }

        /// An SP4_MACH_CRED client of W1's on connection 1, and its session.
        async fn w1_session(&self) -> (u64, [u8; 16]) {
            let (st, id, how) = self.exchange_id(W1, DsConn::test(1), b"w1-owner", &[1, 0, 0]).await;
            assert_eq!((st, how), (Nfs4Status::Ok, 1), "krb5i asks, SP4_MACH_CRED granted");
            let (st, sid) = self.create_session(W1, DsConn::test(1), id).await;
            assert_eq!(st, Nfs4Status::Ok);
            (id, sid)
        }
    }

    #[tokio::test]
    async fn sp4_mach_cred_is_granted_only_under_gss_integrity() {
        let d = ds();
        let c1 = DsConn::test(1);
        let (st, id, how) = d.exchange_id(W1, c1, b"a", &[1, 0, 0]).await;
        assert_eq!((st, how), (Nfs4Status::Ok, 1));
        assert_eq!(d.clients.mach_cred_principal(id).as_deref(), Some(b"gss:nfs/w1@CORP".as_slice()));
        // Plain krb5 (no integrity) and AUTH_NONE: SP4_NONE, as before.
        let krb5 = As::Gss("nfs/w3@CORP", GssService::None);
        let (st, id, how) = d.exchange_id(krb5, c1, b"b", &[1, 0, 0]).await;
        assert_eq!((st, how), (Nfs4Status::Ok, 0));
        assert_eq!(d.clients.mach_cred_principal(id), None);
        let (st, id, how) = d.exchange_id(As::Null, c1, b"c", &[1, 0, 0]).await;
        assert_eq!((st, how), (Nfs4Status::Ok, 0));
        assert_eq!(d.clients.mach_cred_principal(id), None);
        // A krb5i client that asks SP4_NONE gets SP4_NONE.
        let (st, id, how) = d.exchange_id(W2, c1, b"d", &[0]).await;
        assert_eq!((st, how), (Nfs4Status::Ok, 0));
        assert_eq!(d.clients.mach_cred_principal(id), None);
    }

    #[tokio::test]
    async fn a_mach_cred_session_is_used_only_on_a_connection_its_machine_bound() {
        let d = ds();
        let (_, sid) = d.w1_session().await;
        let (c1, c2) = (DsConn::test(1), DsConn::test(2));
        // CREATE_SESSION bound connection 1; a user's I/O rides it.
        assert_eq!(d.sequence(W1, c1, sid).await, Nfs4Status::Ok);
        assert_eq!(d.sequence(BOB, c1, sid).await, Nfs4Status::Ok);
        // Another connection with the session id seen on the wire: by any
        // principal, the machine's own included, until it is bound.
        for who in [W2, BOB, W1] {
            assert_eq!(d.sequence(who, c2, sid).await, Nfs4Status::ConnNotBoundToSession);
        }
        // Binding needs the machine's credential.
        assert_eq!(d.bind(W2, c2, sid, 3).await, Nfs4Status::WrongCred);
        assert_eq!(d.bind(BOB, c2, sid, 3).await, Nfs4Status::WrongCred);
        assert_eq!(d.sequence(W2, c2, sid).await, Nfs4Status::ConnNotBoundToSession, "a refused bind binds nothing");
        // The DS has no back channel: BACK alone is refused.
        assert_eq!(d.bind(W1, c2, sid, 2).await, Nfs4Status::Inval);
        // FORE_OR_BOTH, as Linux sends it.
        assert_eq!(d.bind(W1, c2, sid, 3).await, Nfs4Status::Ok);
        assert_eq!(d.sequence(BOB, c2, sid).await, Nfs4Status::Ok);
        // A closed connection's binding goes with it.
        d.sessions.forget_conn(2);
        assert_eq!(d.sequence(W1, c2, sid).await, Nfs4Status::ConnNotBoundToSession);
        // Binding a session that does not exist.
        let mut ghost = sid;
        ghost[15] ^= 0xff;
        assert_eq!(d.bind(W1, c2, ghost, 3).await, Nfs4Status::BadSession);
    }

    #[tokio::test]
    async fn the_must_enforce_ops_need_the_machine_that_made_the_client() {
        let d = ds();
        let (id, sid) = d.w1_session().await;
        let c1 = DsConn::test(1);
        for who in [W2, BOB] {
            assert_eq!(d.create_session(who, c1, id).await.0, Nfs4Status::WrongCred);
            assert_eq!(d.one(who, c1, opcode::DESTROY_SESSION, |a| a.encode_fixed_opaque(&sid)).await, Nfs4Status::WrongCred);
            assert_eq!(d.one(who, c1, opcode::DESTROY_CLIENTID, |a| a.encode_u64(id)).await, Nfs4Status::WrongCred);
            // Another principal under the client's owner: CLID_INUSE, and
            // the client stays the machine's.
            assert_eq!(d.exchange_id(who, c1, b"w1-owner", &[1, 0, 0]).await.0, Nfs4Status::ClIdInUse);
            assert_eq!(d.clients.mach_cred_principal(id).as_deref(), Some(b"gss:nfs/w1@CORP".as_slice()));
        }
        assert_eq!(d.sequence(W1, c1, sid).await, Nfs4Status::Ok, "the refusals left the session alone");
        // DESTROY_SESSION from the machine, but not on a bound connection.
        assert_eq!(
            d.one(W1, DsConn::test(2), opcode::DESTROY_SESSION, |a| a.encode_fixed_opaque(&sid)).await,
            Nfs4Status::ConnNotBoundToSession
        );
        // The machine on its connection tears both down.
        assert_eq!(d.one(W1, c1, opcode::DESTROY_SESSION, |a| a.encode_fixed_opaque(&sid)).await, Nfs4Status::Ok);
        assert_eq!(d.one(W1, c1, opcode::DESTROY_CLIENTID, |a| a.encode_u64(id)).await, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn required_mach_cred_refuses_every_client_without_it() {
        let d = ds();
        let req = DsConn { id: 1, require_mach_cred: true };
        assert_eq!(d.exchange_id(W1, req, b"a", &[0]).await.0, Nfs4Status::Access, "asked SP4_NONE");
        let krb5 = As::Gss("nfs/w1@CORP", GssService::None);
        assert_eq!(d.exchange_id(krb5, req, b"b", &[1, 0, 0]).await.0, Nfs4Status::Access, "asked, but no integrity");
        assert_eq!(d.exchange_id(As::Null, req, b"c", &[1, 0, 0]).await.0, Nfs4Status::Access, "asked, AUTH_NONE");
        let (st, _, how) = d.exchange_id(W1, req, b"d", &[1, 0, 0]).await;
        assert_eq!((st, how), (Nfs4Status::Ok, 1));
    }

    #[tokio::test]
    async fn an_sp4_none_session_still_binds_to_whatever_connection_uses_it() {
        // The AUTH_SYS path is unchanged: no binding is checked.
        let d = ds();
        let (st, id, how) = d.exchange_id(As::Null, DsConn::test(1), b"sys", &[0]).await;
        assert_eq!((st, how), (Nfs4Status::Ok, 0));
        let (st, sid) = d.create_session(As::Null, DsConn::test(1), id).await;
        assert_eq!(st, Nfs4Status::Ok);
        assert_eq!(d.sequence(As::Null, DsConn::test(2), sid).await, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn io_runs_only_in_a_session_and_an_error_ends_the_compound() {
        let d = ds();
        let c1 = DsConn::test(1);
        let fh = [0xABu8; 16];
        // PUTFH and READ with no SEQUENCE: refused at the first op.
        let r = d
            .run(As::Null, c1, 2, |a| {
                a.encode_u32(opcode::PUTFH);
                a.encode_opaque(&fh);
                a.encode_u32(opcode::READ);
                a.encode_u32(0);
                a.append_raw(&[0u8; 12]);
                a.encode_u64(0);
                a.encode_u32(64);
            })
            .await;
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].0, r[0].1), (opcode::PUTFH, Nfs4Status::OpNotInSession));
        // A refused SEQUENCE ends the compound: the PUTFH after it is not run.
        let (_, sid) = d.w1_session().await;
        let r = d
            .run(W2, DsConn::test(2), 2, |a| {
                test_sequence(a, sid, 1);
                a.encode_u32(opcode::PUTFH);
                a.encode_opaque(&fh);
            })
            .await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, Nfs4Status::ConnNotBoundToSession);
        // SEQUENCE only first.
        let r = d
            .run(W1, c1, 2, |a| {
                test_sequence(a, sid, 2);
                test_sequence(a, sid, 3);
            })
            .await;
        assert_eq!(r.iter().map(|x| x.1).collect::<Vec<_>>(), vec![Nfs4Status::Ok, Nfs4Status::SequencePos]);
    }
}
