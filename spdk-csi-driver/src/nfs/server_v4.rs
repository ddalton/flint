//! NFSv4.2 Server - TCP Transport
//!
//! Handles network I/O for the NFSv4.2 server.
//! Listens on TCP port, receives RPC COMPOUND calls, dispatches to NFSv4.2 handlers,
//! and sends replies.

use super::rpc::{CallMessage, ReplyBuilder, AuthFlavor, AuthStat};
use super::rpcsec_gss::{RpcSecGssManager, RpcGssCred, procedure as gss_proc};
use super::v4::{CompoundDispatcher, CompoundRequest};
use super::v4::filehandle::FileHandleManager;
use super::v4::operations::lockops::LockManager;
use super::v4::protocol::{NFS4_PROGRAM, procedure};
use super::v4::state::StateManager;
// LocalFilesystem removed - NFSv4 uses direct filesystem access via filehandle manager
use super::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

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
    /// A prior state DB existed and could not be used, so this
    /// incarnation starts blind. `serve()` hands it to
    /// `load_from_backend`, which must NOT shortcut the grace period in
    /// that case — see the comment there.
    state_lost: bool,
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
    use crate::state_backend::memory_backend;

    if setting.eq_ignore_ascii_case("memory") {
        info!("💾 NFSv4 state: in-memory (FLINT_NFS_STATE=memory) — no restart survival");
        return (memory_backend(), false);
    }
    let db_path = if setting.is_empty() {
        export_path.join(".flint-nfs").join("state.db")
    } else {
        PathBuf::from(setting)
    };
    // This front-end's DB is always pure bookkeeping — `flint-nfs-server`
    // serves a plain export with no layouts, no placements and no tier —
    // so the quarantine policy applies unconditionally here. The
    // mechanism is shared with the hub's standalone arm; see
    // `open_durable_or_quarantine` for when it must NOT be used.
    crate::state_backend::open_durable_or_quarantine(&db_path)
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
        // silently gone. Bind + restore go through the SHARED
        // `bring_up` so this front-end and `MetadataServer` cannot drift.
        let lock_mgr = LockManager::bring_up(state_mgr.backend(), state_lost).await;

        // Create COMPOUND dispatcher (creates handlers internally)
        let dispatcher = Arc::new(CompoundDispatcher::new(
            fh_mgr,
            state_mgr.clone(),
            lock_mgr.clone(),
        ));

        // Validate the security floor BEFORE the listener exists.
        //
        // A typo in a security knob must not resolve to "no floor": an
        // operator who set FLINT_NFS_MIN_SEC=krb5pp asked for
        // enforcement, and starting anyway would serve every sec=sys
        // client while they believed otherwise. Refuse to come up.
        let sec_policy = crate::nfs::sec_policy::SecPolicy::validate_env()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        if sec_policy.floor() == crate::nfs::sec_policy::SecLevel::None {
            info!(
                "🔓 NFS minimum security flavor: none — every flavor accepted \
                 (set {} to raise it)",
                crate::nfs::sec_policy::SecPolicy::ENV
            );
        } else {
            info!(
                "🔒 NFS minimum security flavor: {} — weaker calls get AUTH_TOOWEAK",
                sec_policy.floor().name()
            );
        }

        // Initialize RPCSEC_GSS manager
        let keytab_path = std::env::var("KRB5_KTNAME").ok();
        let gss_manager = Arc::new(RpcSecGssManager::new(keytab_path));

        Ok(Self { config, dispatcher, gss_manager, state_mgr, lock_mgr, state_lost })
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
        if let Err(e) = self.state_mgr.load_from_backend(self.state_lost).await {
            tracing::error!("NFSv4 state restore failed ({}) — starting with empty state", e);
            // Lock state is lost with the rest: refuse NEW locks during
            // grace so a second client can't take a range whose
            // pre-restart holder we no longer know about.
            self.lock_mgr.mark_restore_failed();
        }
        // The lock table itself was bound and restored in `new()` via
        // the shared `LockManager::bring_up` — still pre-listener, since
        // `new()` precedes `serve()`. Only the `load_from_backend`
        // failure trigger above remains here, because that call is what
        // establishes whether the REST of the state survived.

        // NFSv4 doesn't need portmapper registration (uses well-known port 2049)
        // and doesn't need separate MOUNT protocol

        // Start TCP server
        serve_tcp(&addr, self.dispatcher.clone(), self.gss_manager.clone()).await
    }
}

/// Serve NFSv4.2 over TCP
/// Serve NFSv4 over TCP until the listener dies.
///
/// `pub(crate)` because the pNFS MDS serves through this same function.
/// It used to carry its own fork of this whole layer (`serve_tcp` ..
/// `handle_gss_continue_init`, ~500 lines copied in the original pNFS
/// commit). The fork silently missed every later fix to this file — the
/// SEQUENCE reply cache (`1a543b5`) and the F55 drain gate (`a4902ef`)
/// among them — so the two are now one path. Anything MDS-specific
/// belongs in `CompoundDispatcher`, which already knows whether it is
/// a pNFS server.
use std::sync::atomic::{AtomicU64, Ordering};

/// Connections currently being served, across every front-end that
/// goes through [`serve_tcp`] (the standalone server AND the hub).
pub(crate) static ACTIVE_CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// Default concurrent-connection cap (blocker 6).
///
/// Deliberately generous. This is a backstop against runaway fan-out,
/// not a QoS mechanism: a cap that fires during normal operation would
/// be a worse bug than the unbounded accept it replaces. At this value
/// the per-connection buffers are bounded at ~256 MiB and the
/// per-connection dispatch permits (64 each) at 65,536.
pub(crate) const DEFAULT_MAX_CONNECTIONS: u64 = 1024;

/// Resolve the cap. Unset → [`DEFAULT_MAX_CONNECTIONS`]; `0` → disabled
/// (unbounded, the pre-fix behaviour); unparseable → the default, since
/// a typo in an env var must not silently remove the bound.
pub(crate) fn max_connections_from_env() -> u64 {
    match std::env::var("FLINT_NFS_MAX_CONNECTIONS") {
        Ok(v) => v.trim().parse::<u64>().unwrap_or_else(|_| {
            warn!(
                "FLINT_NFS_MAX_CONNECTIONS={:?} is not a number — using the default {}. \
                 A typo must not silently unbound the server.",
                v, DEFAULT_MAX_CONNECTIONS
            );
            DEFAULT_MAX_CONNECTIONS
        }),
        Err(_) => DEFAULT_MAX_CONNECTIONS,
    }
}

/// Default idle-read deadline, seconds (blocker 7 — the cap's other half).
///
/// Both `read_exact`s in [`handle_tcp_connection`] used to wait forever,
/// so a peer that connected and sent nothing — or sent a record marker
/// and then trickled — pinned its buffers and its blocker-6 connection
/// slot indefinitely. With the cap in force that converts slowloris from
/// memory pressure into a clean denial of NEW mounts: fill the cap with
/// idle sockets and every legitimate client is refused at accept.
///
/// The value cannot cut a live mount: an NFSv4.1 client with state
/// renews its lease via SEQUENCE at a fraction of the 90s lease period
/// ([`crate::nfs::v4::state::lease::DEFAULT_LEASE_TIME`]), and an idle
/// nconnect trunk that IS cut reconnects transparently on next use —
/// ordinary NFS client behaviour, whose replay path B3/B5 pinned.
/// Precedent: knfsd ages out idle client sockets after ~6 minutes
/// (sunrpc `svc_age_temp_sockets`); this default matches it.
// Record caps, the pooled-ingress threshold, and its test counter now
// live with the mechanism in `nfs::ingress`; re-exported so existing
// references keep reading naturally.
// The pooled-ingress threshold and its test counter live with the
// mechanism in `nfs::ingress`; the tests below import them from there.
#[cfg(test)]
pub(crate) use crate::nfs::ingress::POOLED_RECORD_MIN;
#[cfg(test)]
pub(crate) use crate::nfs::ingress::POOLED_RECORDS_FOR_TEST;

pub(crate) const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 360;

/// Resolve the deadline. Unset → [`DEFAULT_IDLE_TIMEOUT_SECS`]; `0` →
/// disabled (the pre-fix wait-forever behaviour); unparseable → the
/// default, since a typo must not silently remove the bound.
pub(crate) fn idle_timeout_from_env() -> Option<std::time::Duration> {
    let secs = match std::env::var("FLINT_NFS_IDLE_TIMEOUT_SECS") {
        Ok(v) => v.trim().parse::<u64>().unwrap_or_else(|_| {
            warn!(
                "FLINT_NFS_IDLE_TIMEOUT_SECS={:?} is not a number — using the default {}. \
                 A typo must not silently unbound the server.",
                v, DEFAULT_IDLE_TIMEOUT_SECS
            );
            DEFAULT_IDLE_TIMEOUT_SECS
        }),
        Err(_) => DEFAULT_IDLE_TIMEOUT_SECS,
    };
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

/// RAII slot in the connection budget.
///
/// The release MUST survive a panic in the handler. A leaked slot is
/// permanent — the counter never comes back down — so a cap built on a
/// decrement that can be skipped would ratchet toward refusing every
/// connection, which is a worse failure than having no cap at all.
/// `Drop` runs while unwinding; a `fetch_sub` at the end of the task
/// body does not.
pub(crate) struct ConnSlot;

impl ConnSlot {
    /// Take a slot, incrementing the live count.
    pub(crate) fn acquire() -> Self {
        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
        ConnSlot
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) async fn serve_tcp(addr: &str, dispatcher: Arc<CompoundDispatcher>, gss_manager: Arc<RpcSecGssManager>) -> std::io::Result<()> {
    
    // Track active connections for debugging concurrent mount issues

    // Blocker 6: an upper bound on concurrently-served connections.
    //
    // Before this, `accept` was unbounded and every accepted connection
    // brought its own 64 dispatch permits (`pipeline::DEFAULT_MAX_INFLIGHT`)
    // and two 128 KiB buffers. The permits are PER CONNECTION, so more
    // connections bought MORE concurrency, not less: 100 pods at the
    // documented `nconnect=4` is 400 connections, ~150 MiB of buffers and
    // 25,600 permitted concurrent dispatches, against a chart that requests
    // 100m/128Mi and sets no limit. There was no configuration under which
    // the hub refused load — it only got slower until it was OOMKilled,
    // which on a single-replica RWO hub is an outage.
    //
    // The cap is deliberately generous: it is a backstop against runaway
    // fan-out, not a QoS mechanism, and a value that fires in normal
    // operation would be a worse bug than the one it fixes. At the default
    // this bounds buffers at ~256 MiB and in-flight dispatches at 65,536.
    let max_connections: u64 = max_connections_from_env();
    if max_connections == 0 {
        info!("FLINT_NFS_MAX_CONNECTIONS=0 — connection cap DISABLED (unbounded accept)");
    } else {
        info!("NFS connection cap: {} concurrent (FLINT_NFS_MAX_CONNECTIONS)", max_connections);
    }

    // Blocker 7: the cap's other half. Without a read deadline, idle
    // sockets occupy capped slots forever — see DEFAULT_IDLE_TIMEOUT_SECS.
    let idle_timeout = idle_timeout_from_env();
    match idle_timeout {
        Some(d) => info!("NFS idle-read deadline: {:?} (FLINT_NFS_IDLE_TIMEOUT_SECS)", d),
        None => info!("FLINT_NFS_IDLE_TIMEOUT_SECS=0 — idle-read deadline DISABLED (wait forever)"),
    }

    
    let listener = TcpListener::bind(addr).await?;
    info!("✅ NFSv4.2 TCP server listening on {}", addr);
    info!("");
    
    let mut connection_count = 0u64;

    loop {
        // NEVER `?` here: accept can fail transiently (EMFILE/ENFILE
        // under fd pressure, ECONNABORTED, resource starvation), and
        // propagating the error exits this loop PERMANENTLY while the
        // rest of the process keeps looking healthy — the NFS lane dies
        // with no probe able to see it (the node-agent partial-death
        // lesson, server edition; a wedged-accept MDS was observed live
        // on the sweep drill under host CPU starvation, 2026-08-10).
        // Log loudly, breathe, retry — the listener itself is intact.
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                error!(
                    "❌ [NFS_SERVER] accept failed: {e} — retrying (transient \
                     fd/resource pressure; the listener is intact)"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };

        // Enforce the cap BEFORE taking a slot. Refusal is a close, not
        // a queue: holding the socket open would consume the very fd the
        // cap exists to protect, and a client that cannot connect retries,
        // whereas a client parked on an accepted-but-unserved connection
        // hangs indefinitely on a hard mount.
        if max_connections > 0 {
            let active_now = ACTIVE_CONNECTIONS.load(Ordering::SeqCst);
            if active_now >= max_connections {
                warn!(
                    "❌ [NFS_SERVER] refusing connection from {} — at the {} connection \
                     cap (FLINT_NFS_MAX_CONNECTIONS). The listener stays healthy and the \
                     client will retry; raise the cap if this is legitimate fan-out",
                    peer, max_connections
                );
                drop(stream);
                continue;
            }
        }

        connection_count += 1;
        let _slot = ConnSlot::acquire();
        let active = ACTIVE_CONNECTIONS.load(Ordering::SeqCst);
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
            // Owned by the task: the slot is released when this future is
            // dropped, whether it returns, errors or panics.
            let _slot = _slot;
            info!("🚀 [NFS_SERVER] Spawned handler task for connection #{} from {}", conn_id, peer);
            if let Err(e) = handle_tcp_connection(stream, dispatcher, gss_manager, peer, conn_id, idle_timeout).await {
                warn!("❌ [NFS_SERVER] Connection #{} from {} error: {}", conn_id, peer, e);
                info!("   Active connections remaining: {}", ACTIVE_CONNECTIONS.load(Ordering::SeqCst) - 1);
            } else {
                info!("✓ [NFS_SERVER] Connection #{} from {} closed cleanly (Active: {})", conn_id, peer, ACTIVE_CONNECTIONS.load(Ordering::SeqCst) - 1);
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
    idle_timeout: Option<std::time::Duration>,
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

    // Record ingress: markers, fragment reassembly, caps, pooling —
    // the shared mechanism in `nfs::ingress`.
    let mut ingress = crate::nfs::ingress::RecordReader::new(format!(
        "[NFS_SERVER] Connection #{} from {}",
        conn_id, peer
    ));

    let mut rpc_count = 0;

    // When the loop exits — clean EOF or any error — release any
    // CB callers still awaiting a reply on this connection so they
    // see `ConnectionClosed` rather than wait out the timeout, AND
    // purge this connection's writer from the dispatcher's back-channel
    // registry. The registry holds a STRONG Arc: leaving it in place
    // after the peer disconnects pins the socket fd open (peer FIN →
    // permanent CLOSE_WAIT) and its HUP readiness spins the async
    // driver — measured as two runtime workers pegged in epoll_pwait
    // at ~60% CPU / 83% sys after a single client migration (F18,
    // drill 3.1). The guard runs cleanup on every return path (early
    // Err, EOF return, panic).
    struct InflightGuard {
        bcw: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
        back_channels: Arc<dashmap::DashMap<
            crate::nfs::v4::protocol::SessionId,
            Vec<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
        >>,
    }
    impl Drop for InflightGuard {
        fn drop(&mut self) {
            self.bcw.drop_all_inflight();
            // Drop every registry entry that is THIS connection's writer —
            // the last Arc going away closes the write half and closes the
            // socket the kernel is holding in CLOSE_WAIT.
            // Drop only THIS connection's writer; a session bound over
            // several transports (nconnect) keeps the rest (audit C5).
            self.back_channels.retain(|_, writers| {
                writers.retain(|w| !Arc::ptr_eq(w, &self.bcw));
                !writers.is_empty()
            });
        }
    }
    let _inflight_guard = InflightGuard {
        bcw: Arc::clone(&bcw),
        back_channels: dispatcher.back_channels(),
    };

    // Per-connection RPC pipelining (RFC 8881 §2.10.6): dispatches run
    // concurrently up to FLINT_NFS_MAX_INFLIGHT (default 64, 0 =
    // sequential); replies are serialized on the wire by the BCW mutex.
    let pipeline = crate::nfs::pipeline::ConnectionPipeline::from_env();

    loop {
        // F55: a draining server closes each connection at its frame
        // boundary — the only point where a FIN cannot truncate a reply.
        if crate::nfs::pipeline::DrainGate::global().is_draining() {
            info!("🔻 [NFS_SERVER] Connection #{} from {} closed at frame boundary (draining for shutdown)", conn_id, peer);
            return Ok(());
        }
        debug!("📥 [NFS_SERVER] Connection #{}: Waiting for RPC message #{} from {}", conn_id, rpc_count + 1, peer);

        // One complete record from the shared ingress (markers,
        // fragments, caps, pooled payload buffers — `nfs::ingress`).
        // Between-records idleness and a clean EOF are ordinary closes;
        // everything else is a connection error.
        let request = match ingress.next(&mut reader, idle_timeout).await {
            Ok(crate::nfs::ingress::NextRecord::Record(r)) => r,
            Ok(crate::nfs::ingress::NextRecord::Closed) => {
                let duration = connect_time.elapsed();
                info!("🔌 [NFS_SERVER] Connection #{} from {} closed after {:?} ({} RPCs processed)",
                      conn_id, peer, duration, rpc_count);
                if rpc_count == 0 {
                    warn!("⚠️  [NFS_SERVER] Client {} connected (conn #{}) but sent NO RPC messages!", peer, conn_id);
                }
                return Ok(());
            }
            Ok(crate::nfs::ingress::NextRecord::IdleClosed) => {
                info!(
                    "⏱️  [NFS_SERVER] Connection #{} from {} idle for {:?} with no \
                     request — closing to free its slot (FLINT_NFS_IDLE_TIMEOUT_SECS; \
                     {} RPCs served)",
                    conn_id, peer, idle_timeout, rpc_count
                );
                return Ok(());
            }
            Err(e) => {
                warn!("❌ [NFS_SERVER] Connection #{}: ingress error from {}: {}", conn_id, peer, e);
                return Err(e);
            }
        };

        debug!("✅ Received complete RPC record ({} bytes), first 32 bytes: {:02x?}",
               request.len(), &request[..std::cmp::min(32, request.len())]);

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
               conn_id, rpc_count + 1, peer, request.len());
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
            // R2: never dispatch inline once this connection is a
            // back-channel — its read loop has to stay free to route CB
            // replies for compounds running on it.
            bcw.is_back_channel(),
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
                debug!("📨 [NFS_SERVER] Connection #{}: RPC #{} processed in {:?} ({} bytes in {} segment(s))",
                       conn_id, rpc_num, rpc_start.elapsed(),
                       crate::nfs::segment::total_len(&reply), reply.len());
                reply
            },
            move |reply| async move { bcw_write.send_record_segments(reply).await },
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
) -> Vec<crate::nfs::segment::Segment> {
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
            return vec![ReplyBuilder::garbage_args(0).into()];
        }
    };

    // Per-RPC lines are debug: at INFO these two lines per RPC dominate
    // the server's own CPU under load (containerd shim writes).
    debug!(
        ">>> [NFS_RPC] Connection #{}, RPC #{}: xid={}, program={}, version={}, procedure={}",
        conn_id, rpc_num, call.xid, call.program, call.version, call.procedure
    );
    debug!("   Cred: {:?}, Verf: {:?}", call.cred.flavor, call.verf.flavor);

    // Check program number
    if call.program != NFS4_PROGRAM {
        warn!("❌ Invalid program number: {} (expected {} for NFS4)", call.program, NFS4_PROGRAM);
        warn!("   This might be a different RPC service trying to connect");
        debug!("   Returning PROG_UNAVAIL to client");
        return vec![ReplyBuilder::prog_unavail(call.xid).into()];
    }

    // Check version (4.0, 4.1, or 4.2)
    if call.version != 4 {
        warn!("❌ Invalid NFSv4 version: {} (expected 4)", call.version);
        warn!("   Client might be trying NFSv3 or other version");
        debug!("   Returning PROC_UNAVAIL to client");
        // NFSv4 doesn't have prog_mismatch, return proc_unavail
        return vec![ReplyBuilder::proc_unavail(call.xid).into()];
    }
    
    debug!("✅ RPC validation passed: program={}, version={}", call.program, call.version);

    // Handle RPCSEC_GSS authentication.
    //
    // AFTER the program/version check, deliberately. The GSS branch
    // mints and looks up context state, and taking it first meant a
    // call addressed to a program this server does not even serve
    // still reached that machinery.
    if call.cred.flavor == AuthFlavor::RpcsecGss {
        info!("🔐 [NFS_SERVER] Connection #{}, RPC #{}: RPCSEC_GSS authentication detected", conn_id, rpc_num);
        return handle_rpcsec_gss_call(call, args, gss_manager, dispatcher, back_channel).await;
    }

    // Enforce the export's minimum security flavor.
    //
    // Everything that reaches here is AUTH_NONE or AUTH_SYS — the GSS
    // branch above returned already. Advertising krb5p through SECINFO
    // and then serving whatever the client actually presented meant the
    // choice of protection belonged to the client, which is no
    // enforcement at all (`nfs::sec_policy`).
    //
    // NULL is exempt: it carries no arguments and returns no data, and
    // it is how clients and monitoring probe liveness before they have
    // any credential to offer. Refusing it would break the probe without
    // protecting anything.
    let arrived = crate::nfs::sec_policy::SecLevel::of_call(call.cred.flavor, None);
    if let crate::nfs::sec_policy::Admission::TooWeak { arrived, floor } = crate::nfs::sec_policy::active()
        .admit(arrived, call.procedure == procedure::NULL)
    {
        warn!(
            "🔒 Refusing sec={} call: this export requires at least sec={} ({})",
            arrived.name(),
            floor.name(),
            crate::nfs::sec_policy::SecPolicy::ENV
        );
        return vec![ReplyBuilder::auth_error(call.xid, AuthStat::TooWeak).into()];
    }

    // Handle procedure
    match call.procedure {
        procedure::NULL => {
            // NULL procedure - just return success (empty result)
            info!(">>> NULL procedure");
            vec![ReplyBuilder::success(call.xid).finish().into()]
        }

        procedure::COMPOUND => {
            // COMPOUND procedure - dispatch to NFSv4.2 handler
            debug!(">>> COMPOUND procedure");
            handle_compound(call, args, dispatcher, back_channel, None).await
        }

        _ => {
            warn!("Invalid NFSv4 procedure: {}", call.procedure);
            vec![ReplyBuilder::proc_unavail(call.xid).into()]
        }
    }
}

/// Handle NFSv4 COMPOUND request
async fn handle_compound(
    call: CallMessage,
    args: Bytes,
    dispatcher: Arc<CompoundDispatcher>,
    back_channel: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
    gss: Option<&crate::nfs::gss_framing::ValidatedCall>,
) -> Vec<crate::nfs::segment::Segment> {
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
            return vec![ReplyBuilder::garbage_args(call.xid).into()];
        }
    };
    compound_req.wire_size = wire_size;

    debug!("COMPOUND: tag={}, minor_version={}, {} operations",
           compound_req.tag,
           compound_req.minor_version,
           compound_req.operations.len());

    // RPC-level principal for the EXCHANGE_ID §18.35.5 state machine.
    // Cheap to compute and an empty Vec for AUTH_NONE.
    // Prefer the AUTHENTICATED Kerberos principal when the context gave
    // us one: it is stable across context re-establishment, where the
    // handle is not, and it is what RFC 8881 §18.35.5 actually means by
    // "principal".
    let principal = match gss.and_then(|v| v.client_principal.as_deref()) {
        Some(name) => {
            let mut p = Vec::with_capacity(name.len() + 4);
            p.extend_from_slice(b"gss:");
            p.extend_from_slice(name.as_bytes());
            p
        }
        None => call.cred.principal(),
    };
    // AUTH_SYS (uid, gid) — file-creating ops stamp it onto the backing
    // object so ownership round-trips (postgres-class workloads check it).
    let unix_cred = call.cred.unix_uid_gid();
    let unix_gids = call.cred.unix_gids();

    // Dispatch to COMPOUND handler
    let compound_resp = dispatcher
        .dispatch_compound_with_cred(
            compound_req,
            principal,
            unix_cred,
            unix_gids,
            Some(Arc::clone(&back_channel)),
            // GSS binds a MIC over / seals the body as one octet stream,
            // so a payload that never enters userspace cannot be framed
            // for it. Plain TCP only, and only when switched on.
            gss.is_none() && crate::nfs::splice::enabled(),
        )
        .await;

    debug!("COMPOUND result: status={:?}, {} results",
           compound_resp.status,
           compound_resp.results.len());

    // Pull the cache hint off before we move the response into encode().
    // RFC 8881 §15.1.10.4 requires the slot reply cache to hold the *exact*
    // bytes the client received, so we capture it after encoding finishes.
    let cache_slot = compound_resp.cache_slot;

    // Encode the COMPOUND body.
    //
    // Segmented unless the slot cache wants it: the cache stores the
    // EXACT bytes a later replay returns verbatim, so that path keeps
    // the flat encoding. Everything else — every READ a client actually
    // streams, which sends cachethis=false — takes the segmented path
    // and never copies its payload. See
    // `CompoundResponse::encode_segments`.
    let compound_segments: Vec<crate::nfs::segment::Segment> = if cache_slot.is_some() {
        vec![compound_resp.encode().into()]
    } else {
        compound_resp.encode_segments()
    };

    // Cache the encoded reply against the SEQUENCE slot for replay matching.
    // Skipped automatically when the COMPOUND short-circuited a replay
    // (cache_slot is None on that path).
    if let Some((session_id, slot_id)) = cache_slot {
        // `cache_slot.is_some()` forced the FLAT encode above, so this is
        // the whole reply in one in-memory segment. That is not incidental:
        // the cache must store the exact octets a replay returns, which a
        // payload that never enters userspace could never supply.
        dispatcher.cache_slot_reply(&session_id, slot_id, compound_segments[0].as_mem().clone());
    }

    // RPC reply framing.
    //
    // For RPCSEC_GSS the verifier is a MIC over the request's seq_num and
    // the body is sealed per the negotiated service (RFC 2203 §5.3.3) —
    // a null verifier over a bare body is what this server used to send
    // for every GSS call, and a conforming client rejects it.
    //
    // NOTE the ordering against the reply cache above: the cache stores
    // the UNSEALED compound data, and sealing happens per request. It has
    // to: the verifier and the sealed body both bind the requesting
    // call's sequence number, so a replay served from the cache needs
    // fresh ones rather than a recording of the first reply's.
    frame_reply(call.xid, gss, compound_segments)
}

/// Frame an accepted RPC reply, adding the RPCSEC_GSS verifier and
/// sealing the body per the negotiated service when the call was
/// authenticated.
///
/// Shared by COMPOUND and NULL deliberately. NULL used to be routed into
/// the COMPOUND decoder on the GSS path, which answered a legal
/// `sess.c.null()` with GARBAGE_ARGS (found by pynfs `--security=krb5`,
/// 2026-08-27); giving the two paths one framing function is what stops
/// the fix from drifting back apart.
fn frame_reply(
    xid: u32,
    gss: Option<&crate::nfs::gss_framing::ValidatedCall>,
    results: Vec<crate::nfs::segment::Segment>,
) -> Vec<crate::nfs::segment::Segment> {
    // The unauthenticated path — every ordinary READ — emits the RPC
    // header as its own segment and passes the body segments through
    // untouched. `append_bytes` below would otherwise copy the whole
    // payload a SECOND time, after the compound encoder already had.
    if gss.is_none() {
        // A body that is ONE segment carried no READ payload — every
        // metadata reply is a handful of words. Segmenting it buys
        // nothing (there is no large copy to avoid) and costs an extra
        // encoder allocation, so take the original flattening path and
        // stay byte-for-byte and allocation-for-allocation identical to
        // what shipped before segmentation existed.
        //
        // This is not a micro-optimisation on a hunch: the first
        // measured run after segmentation showed `read` up but `meta`
        // down 10% with the spread widening from 2.7% to 9.4%, which is
        // what paying for segmentation on replies that cannot benefit
        // looks like.
        if results.len() == 1 {
            let mut reply =
                ReplyBuilder::success_with_verf(xid, &crate::nfs::rpc::Auth::null());
            reply.encoder().append_bytes(results[0].as_mem());
            return vec![reply.finish().into()];
        }
        let header = ReplyBuilder::success_with_verf(xid, &crate::nfs::rpc::Auth::null()).finish();
        let mut segs = Vec::with_capacity(results.len() + 1);
        segs.push(header.into());
        segs.extend(results);
        return segs;
    }

    // GSS binds a MIC over, or seals, the body as one octet stream, so
    // from here it has to be contiguous. krb5 is a correctness path,
    // not the throughput path, and flattening it costs what the old
    // code cost everyone.
    let results: Bytes = if results.len() == 1 {
        results.into_iter().next().expect("len checked").into_mem()
    } else {
        let total: usize = crate::nfs::segment::total_len(&results);
        let mut flat = bytes::BytesMut::with_capacity(total);
        for seg in &results {
            flat.extend_from_slice(seg.as_mem());
        }
        flat.freeze()
    };

    let (verf, body) = match gss {
        None => unreachable!("handled above"),
        Some(v) => {
            let verf = match crate::nfs::gss_framing::reply_verifier(v) {
                Ok(a) => a,
                Err(e) => {
                    warn!("❌ GSS reply verifier failed: {}", e.reason());
                    return vec![ReplyBuilder::auth_error(xid, e.auth_stat()).into()];
                }
            };
            match crate::nfs::gss_framing::seal_reply_body(v, &results) {
                Ok(b) => (verf, b),
                Err(e) => {
                    warn!("❌ GSS reply sealing failed: {}", e.reason());
                    return vec![ReplyBuilder::auth_error(xid, e.auth_stat()).into()];
                }
            }
        }
    };

    let mut reply = ReplyBuilder::success_with_verf(xid, &verf);
    // Per RFC 5531 procedure results are appended directly, no length prefix.
    reply.encoder().append_bytes(&body);
    vec![reply.finish().into()]
}

/// Handle RPCSEC_GSS authenticated RPC call
async fn handle_rpcsec_gss_call(
    call: CallMessage,
    args: Bytes,
    gss_manager: Arc<RpcSecGssManager>,
    dispatcher: Arc<CompoundDispatcher>,
    back_channel: Arc<crate::nfs::v4::back_channel::BackChannelWriter>,
) -> Vec<crate::nfs::segment::Segment> {
    // Decode RPCSEC_GSS credentials
    let gss_cred = match RpcGssCred::decode(&call.cred.body) {
        Ok(cred) => {
            info!("🔐 GSS Cred: version={}, procedure={}, seq={}, service={:?}",
                  cred.version, cred.procedure, cred.sequence_num, cred.service);
            cred
        }
        Err(e) => {
            warn!("❌ Failed to decode RPCSEC_GSS credentials: {}", e);
            return vec![ReplyBuilder::garbage_args(call.xid).into()];
        }
    };

    // Handle different GSS procedures
    match gss_cred.procedure {
        gss_proc::INIT => {
            info!("🔐 RPCSEC_GSS_INIT");
            vec![handle_gss_init(call.xid, &gss_cred, args, gss_manager).await.into()]
        }

        gss_proc::CONTINUE_INIT => {
            info!("🔐 RPCSEC_GSS_CONTINUE_INIT");
            vec![handle_gss_continue_init(call.xid, &gss_cred, args, gss_manager).await.into()]
        }

        gss_proc::DATA => {
            info!("🔐 RPCSEC_GSS_DATA");

            // 1. Context lookup, expiry and the base key. NOT the replay
            //    window -- see step 2b.
            let validated = match gss_manager.validate_data(&gss_cred).await {
                Ok(v) => v,
                Err(e) => {
                    warn!("❌ GSS DATA validation failed: {}", e.reason());
                    // CREDPROBLEM/CTXPROBLEM tell the client to refresh and
                    // retry. SYSTEM_ERR, which is what this used to send,
                    // tells it to give up.
                    return vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()];
                }
            };

            // 1b. The negotiated service against the export's floor.
            //     A context established for svc_none is a real Kerberos
            //     identity and still not privacy; an export that asks
            //     for krb5p must refuse it here, not merely decline to
            //     advertise it.
            {
                let arrived = crate::nfs::sec_policy::SecLevel::of_call(
                    AuthFlavor::RpcsecGss,
                    Some(validated.service),
                );
                if let crate::nfs::sec_policy::Admission::TooWeak { arrived, floor } =
                    crate::nfs::sec_policy::active()
                        .admit(arrived, call.procedure == procedure::NULL)
                {
                    warn!(
                        "🔒 Refusing sec={} call: this export requires at least sec={} ({})",
                        arrived.name(),
                        floor.name(),
                        crate::nfs::sec_policy::SecPolicy::ENV
                    );
                    return vec![ReplyBuilder::auth_error(call.xid, AuthStat::TooWeak).into()];
                }
            }

            // 2. The call verifier: a MIC over the header up to and
            //    including the credential (RFC 2203 §5.3.1). Previously
            //    decoded and discarded, so any verifier was accepted.
            if let Err(e) =
                crate::nfs::gss_framing::verify_call_verifier(&validated, &call.verf, &call.cred_span)
            {
                warn!("❌ GSS call verifier rejected: {}", e.reason());
                return vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()];
            }

            // 2b. ONLY NOW spend the sequence number. RFC 2203 §5.3.3.1
            //     puts the header checksum before the sequence check, and
            //     this used to do the reverse -- inside step 1, where
            //     `verify_sequence` also ADVANCES the window. The wire
            //     drill (tests/krb5/run-gssneg.sh, leg N5) showed what
            //     that bought an attacker: replay a captured record with
            //     its seq_num rewritten to a large number, hold no key at
            //     all, and the MIC check duly rejects the call -- after
            //     the window has already been parked at that number. The
            //     real client's next calls then fall outside it and are
            //     refused as replays. An unauthenticated wedge of a live
            //     mount, costing one packet.
            if let Err(e) = gss_manager.accept_sequence(&gss_cred).await {
                warn!("❌ GSS sequence rejected: {}", e.reason());
                return vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()];
            }

            // 3. Unseal the body BEFORE the COMPOUND is decoded. For
            //    krb5i/krb5p the args are still wrapped at this point.
            let args = match crate::nfs::gss_framing::unseal_call_body(&validated, args) {
                Ok(a) => a,
                Err(e) if e.is_garbage() => {
                    warn!("❌ GSS body framing malformed: {}", e.reason());
                    return vec![ReplyBuilder::garbage_args(call.xid).into()];
                }
                Err(e) => {
                    warn!("❌ GSS body rejected: {}", e.reason());
                    return vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()];
                }
            };

            // 4. Dispatch on the RPC procedure — which this branch never
            //    did. Every authenticated call went to the COMPOUND
            //    decoder, so an RPC NULL over RPCSEC_GSS (a legal call,
            //    and how clients probe a context) met an empty body and
            //    came back GARBAGE_ARGS. The non-GSS path above has
            //    always dispatched correctly; only the GSS path did not,
            //    which is why sec=sys testing could never see it.
            match call.procedure {
                procedure::NULL => {
                    info!(">>> NULL procedure (over RPCSEC_GSS)");
                    // No results, but still a GSS-verified reply.
                    frame_reply(call.xid, Some(&validated), vec![])
                }
                procedure::COMPOUND => {
                    info!("✅ GSS authentication successful, processing COMPOUND");
                    handle_compound(call, args, dispatcher, back_channel, Some(&validated)).await
                }
                other => {
                    warn!("Invalid NFSv4 procedure over RPCSEC_GSS: {}", other);
                    vec![ReplyBuilder::proc_unavail(call.xid).into()]
                }
            }
        }

        gss_proc::DESTROY => {
            info!("🔐 RPCSEC_GSS_DESTROY");
            gss_manager.handle_destroy(&gss_cred).await;
            // Return success
            vec![ReplyBuilder::success(call.xid).finish().into()]
        }

        _ => {
            warn!("❌ Unknown RPCSEC_GSS procedure: {}", gss_cred.procedure);
            vec![ReplyBuilder::proc_unavail(call.xid).into()]
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

    // RFC 2203 §5.2.3.1: on GSS_S_COMPLETE the INIT reply verifier is a
    // MIC over the sequence window. Only on complete — any other major
    // status takes a NULL verifier, because there is no context to sign
    // with. This was unconditionally AUTH_NONE.
    let verf = if init_res.major_status == 0 {
        match gss_manager.tokens_for(&init_res.handle).await {
            Some(t) => match crate::nfs::gss_framing::init_reply_verifier(&t, init_res.sequence_window) {
                Ok(a) => a,
                Err(e) => {
                    warn!("❌ GSS INIT verifier failed: {}", e.reason());
                    return ReplyBuilder::auth_error(xid, e.auth_stat());
                }
            },
            // Placeholder mode: established, but holds no key material.
            None => crate::nfs::rpc::Auth::null(),
        }
    } else {
        crate::nfs::rpc::Auth::null()
    };

    let init_result_data = init_res.encode();
    let mut reply = ReplyBuilder::success_with_verf(xid, &verf);
    let encoder = reply.encoder();
    encoder.append_bytes(&init_result_data);

    info!("✅ GSS_INIT complete: handle_len={}, major={}, minor={}",
          init_res.handle.len(), init_res.major_status, init_res.minor_status);

    reply.finish()
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
            .create_session(client_id, 1, 0, 1 << 20, 1 << 20, 4096, 8, 64, 0x4000_0000, None, 1);
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
        mgr2.load_from_backend(false).await.unwrap();

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
            .create_session(client_id, 1, 0, 1 << 20, 1 << 20, 4096, 8, 64, 0x4000_0000, None, 1);
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

/// Serializes every test that observes or perturbs ACTIVE_CONNECTIONS.
/// The counter is process-global and several tests park REAL served
/// connections (each holding a ConnSlot) for their whole duration — a
/// delta assertion sampled while one of those lives (or dies) mid-test
/// reads someone else's slot. Scheduling luck hid this until the
/// ingress extraction shifted connection-close timing.
#[cfg(test)]
pub(crate) static CONN_SLOT_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod connection_cap_tests {
    use super::*;

    /// The safety property the whole cap rests on: a slot is released
    /// even when the handler panics.
    ///
    /// This is not a hypothetical. The pre-existing code decremented the
    /// counter with an explicit `fetch_sub` at the end of the task body,
    /// on both the Ok and Err arms — neither of which runs if the task
    /// panics. That was harmless while nothing read the counter. The
    /// moment a CAP reads it, a leaked slot becomes permanent: the count
    /// ratchets up, never comes down, and the server ends up refusing
    /// every connection while looking healthy. That is strictly worse
    /// than the unbounded accept the cap replaces.
    #[test]
    fn a_slot_is_released_even_when_the_holder_panics() {
        let _serial = crate::nfs::server_v4::CONN_SLOT_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = ACTIVE_CONNECTIONS.load(Ordering::SeqCst);

        let r = std::panic::catch_unwind(|| {
            let _slot = ConnSlot::acquire();
            assert_eq!(
                ACTIVE_CONNECTIONS.load(Ordering::SeqCst),
                before + 1,
                "acquire must be visible while the slot is held — otherwise the \
                 assertion below passes for the wrong reason"
            );
            panic!("handler blew up mid-connection");
        });

        assert!(r.is_err(), "the test must actually panic, or it proves nothing");
        assert_eq!(
            ACTIVE_CONNECTIONS.load(Ordering::SeqCst),
            before,
            "the slot LEAKED across a panic — with a cap in force this ratchets \
             toward refusing every connection, permanently"
        );
    }

    #[test]
    fn a_slot_is_released_on_the_ordinary_path() {
        let _serial = crate::nfs::server_v4::CONN_SLOT_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = ACTIVE_CONNECTIONS.load(Ordering::SeqCst);
        {
            let _slot = ConnSlot::acquire();
            assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::SeqCst), before + 1);
        }
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::SeqCst), before);
    }

    /// A typo must not silently unbound the server. `0` is the only
    /// value that disables the cap, and it has to be spelled exactly.
    #[test]
    fn an_unparseable_cap_falls_back_to_the_default_rather_than_to_unbounded() {
        // These run in-process with other tests, so scope the env var
        // tightly and restore it. Serialised by the mutex below.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restore = std::env::var("FLINT_NFS_MAX_CONNECTIONS").ok();

        std::env::set_var("FLINT_NFS_MAX_CONNECTIONS", "not-a-number");
        assert_eq!(
            max_connections_from_env(),
            DEFAULT_MAX_CONNECTIONS,
            "a malformed cap must fall back to the DEFAULT, never to 0 — falling \
             back to 0 would turn a typo into an unbounded server"
        );

        std::env::set_var("FLINT_NFS_MAX_CONNECTIONS", "");
        assert_eq!(max_connections_from_env(), DEFAULT_MAX_CONNECTIONS);

        std::env::set_var("FLINT_NFS_MAX_CONNECTIONS", "  32 ");
        assert_eq!(max_connections_from_env(), 32, "surrounding whitespace must be tolerated");

        std::env::set_var("FLINT_NFS_MAX_CONNECTIONS", "0");
        assert_eq!(max_connections_from_env(), 0, "0 is the documented opt-out");

        std::env::remove_var("FLINT_NFS_MAX_CONNECTIONS");
        assert_eq!(max_connections_from_env(), DEFAULT_MAX_CONNECTIONS);

        match restore {
            Some(v) => std::env::set_var("FLINT_NFS_MAX_CONNECTIONS", v),
            None => std::env::remove_var("FLINT_NFS_MAX_CONNECTIONS"),
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

#[cfg(test)]
mod idle_timeout_tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Accept one connection and serve it with the given deadline.
    /// Returns the client half and the handler's join handle.
    async fn one_served_connection(
        idle: Option<Duration>,
    ) -> (TcpStream, tokio::task::JoinHandle<std::io::Result<()>>, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = Arc::new(CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr));
        let gss = Arc::new(RpcSecGssManager::new(None));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (served, peer) = listener.accept().await.unwrap();
        let handle = tokio::spawn(async move {
            handle_tcp_connection(served, dispatcher, gss, peer, 1, idle).await
        });
        (client, handle, temp)
    }

    /// Read until EOF (or error) with a test-side deadline. Ok(()) means
    /// the server closed the connection; Err(()) means it did not within
    /// the window — which is exactly the pre-fix behaviour.
    async fn server_closed_within(client: &mut TcpStream, window: Duration) -> Result<(), ()> {
        let mut sink = [0u8; 64];
        match tokio::time::timeout(window, async {
            loop {
                match client.read(&mut sink).await {
                    Ok(0) => return,        // EOF — server closed
                    Ok(_) => continue,      // drain anything written
                    Err(_) => return,       // RST also counts as closed
                }
            }
        })
        .await
        {
            Ok(()) => Ok(()),
            Err(_) => Err(()),
        }
    }

    /// The idle half of blocker 7: a peer that connects and never sends
    /// a byte must be closed at the deadline, freeing its blocker-6
    /// slot. Against the reverted defect (no deadline on the marker
    /// read) the server holds the socket forever and this test fails on
    /// its bounded wait.
    #[tokio::test]
    async fn an_idle_connection_is_closed_at_the_deadline() {
        let _serial = crate::nfs::server_v4::CONN_SLOT_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (mut client, handle, _t) =
            one_served_connection(Some(Duration::from_millis(200))).await;
        server_closed_within(&mut client, Duration::from_secs(5))
            .await
            .expect("an idle connection must be CLOSED at the deadline — it was still open");
        // The handler treats idle as an ordinary close, not an error:
        // refusal-by-error would spam the log for every aged-out trunk.
        assert!(handle.await.unwrap().is_ok(), "idle close must be the Ok path");
    }

    /// An RPC record may arrive as several fragments; only the last
    /// carries the high bit in its marker.
    ///
    /// `is_last` was decoded and then used in a single `debug!`, so every
    /// fragment was handed to the parser as a whole record: a legal
    /// two-fragment call was decoded as garbage, and its continuation
    /// decoded AGAIN as a second call on the same connection. Nothing
    /// shipped fragments, which is the only reason it was never seen.
    ///
    /// The oracle is equality against the single-fragment reply, not
    /// "a reply came back". A server that mangles the record still
    /// answers something, and answering something is what the defect
    /// did.
    #[tokio::test]
    async fn a_record_split_across_fragments_is_reassembled_before_dispatch() {
        let _serial = crate::nfs::server_v4::CONN_SLOT_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // RFC 5531 CALL: xid, msg_type=0, rpcvers=2, prog=100003 (NFS),
        // vers=4, proc=0 (NULL), then null cred and null verf.
        let call: Vec<u8> = [0x0Bu32, 0, 2, 100_003, 4, 0, 0, 0, 0, 0]
            .iter()
            .flat_map(|w| w.to_be_bytes())
            .collect();
        assert_eq!(call.len(), 40);

        async fn reply_to(frames: &[(&[u8], bool)]) -> Vec<u8> {
            let (mut client, handle, _t) =
                one_served_connection(Some(Duration::from_secs(5))).await;
            for (payload, last) in frames {
                let mut marker = payload.len() as u32;
                if *last {
                    marker |= 0x8000_0000;
                }
                client.write_all(&marker.to_be_bytes()).await.unwrap();
                client.write_all(payload).await.unwrap();
            }
            client.flush().await.unwrap();

            let mut m = [0u8; 4];
            tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut m))
                .await
                .expect("the server must answer within the window")
                .expect("reply marker");
            let len = (u32::from_be_bytes(m) & 0x7FFF_FFFF) as usize;
            let mut body = vec![0u8; len];
            client.read_exact(&mut body).await.expect("reply body");
            drop(client);
            let _ = handle.await;
            body
        }

        // CONTROL first: the same call as ONE fragment. If this is not
        // a working baseline the comparison below proves nothing.
        let whole = reply_to(&[(&call[..], true)]).await;
        assert!(!whole.is_empty(), "the single-fragment control must get a reply");

        // The same 40 bytes, split. Only the second marker sets the bit.
        let split = reply_to(&[(&call[..20], false), (&call[20..], true)]).await;

        assert_eq!(
            split, whole,
            "a fragmented record must produce the SAME reply as the whole one",
        );
    }

    /// A payload-bearing single-fragment record (> POOLED_RECORD_MIN)
    /// reads into a pooled buffer: `split().freeze()` donates the
    /// connection buffer's storage to the request, so the BytesMut path
    /// paid a fresh allocation + kernel page-clear per large record.
    ///
    /// The oracle is reply equality against the same call sent small: a
    /// pooled path that mis-sliced or corrupted the record would decode
    /// garbage and answer differently. The counter is the anti-vacuity
    /// guard — without it, a record that quietly took the BytesMut path
    /// would pass this test for no reason.
    #[tokio::test]
    async fn a_large_single_fragment_record_takes_the_pooled_path_and_decodes() {
        let _serial = crate::nfs::server_v4::CONN_SLOT_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use std::sync::atomic::Ordering;
        // RFC 5531 NULL CALL, as in the fragment test above.
        let call: Vec<u8> = [0x2Cu32, 0, 2, 100_003, 4, 0, 0, 0, 0, 0]
            .iter()
            .flat_map(|w| w.to_be_bytes())
            .collect();

        async fn reply_to(payload: &[u8]) -> Vec<u8> {
            let (mut client, handle, _t) =
                one_served_connection(Some(Duration::from_secs(5))).await;
            let marker = 0x8000_0000u32 | payload.len() as u32;
            client.write_all(&marker.to_be_bytes()).await.unwrap();
            client.write_all(payload).await.unwrap();
            client.flush().await.unwrap();
            let mut m = [0u8; 4];
            tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut m))
                .await
                .expect("the server must answer within the window")
                .expect("reply marker");
            let len = (u32::from_be_bytes(m) & 0x7FFF_FFFF) as usize;
            let mut body = vec![0u8; len];
            client.read_exact(&mut body).await.expect("reply body");
            drop(client);
            let _ = handle.await;
            body
        }

        // CONTROL: the bare call, small enough to take the BytesMut path.
        let whole = reply_to(&call).await;
        assert!(!whole.is_empty(), "the small control must get a reply");

        // The same call padded past the pool threshold. The pad bytes are
        // NULL-proc args, which the server ignores; the reply must match.
        let mut big = call.clone();
        big.resize(POOLED_RECORD_MIN + 4096, 0xAB);
        let before = POOLED_RECORDS_FOR_TEST.load(Ordering::Relaxed);
        let padded = reply_to(&big).await;
        let after = POOLED_RECORDS_FOR_TEST.load(Ordering::Relaxed);

        assert!(
            after > before,
            "the {}-byte record must take the POOLED path (counter {} -> {})",
            big.len(),
            before,
            after
        );
        assert_eq!(
            padded, whole,
            "a pooled record must produce the SAME reply as the small control",
        );
    }

    /// The trickle half: a peer that sends a marker promising 100 bytes
    /// and then stalls is holding the loop mid-message — closed at the
    /// deadline, and as an ERROR, because the peer broke its promise.
    #[tokio::test]
    async fn a_mid_request_stall_is_closed_at_the_deadline() {
        let _serial = crate::nfs::server_v4::CONN_SLOT_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (mut client, handle, _t) =
            one_served_connection(Some(Duration::from_millis(200))).await;
        // Last-fragment marker, length 100 — then silence.
        client.write_all(&(0x8000_0064u32).to_be_bytes()).await.unwrap();
        client.write_all(&[0u8; 10]).await.unwrap();
        server_closed_within(&mut client, Duration::from_secs(5))
            .await
            .expect("a mid-request stall must be CLOSED at the deadline — it was still open");
        let res = handle.await.unwrap();
        assert!(res.is_err(), "a broken promise of bytes is an error, not a quiet close");
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    /// The failing control for both tests above: with the deadline
    /// disabled (the pre-fix behaviour, FLINT_NFS_IDLE_TIMEOUT_SECS=0),
    /// the same idle connection is NOT closed — proving the two tests
    /// pass because of the deadline and not because something else
    /// hangs up idle sockets.
    #[tokio::test]
    async fn with_the_deadline_disabled_an_idle_connection_stays_open() {
        let _serial = crate::nfs::server_v4::CONN_SLOT_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (mut client, handle, _t) = one_served_connection(None).await;
        assert!(
            server_closed_within(&mut client, Duration::from_secs(1)).await.is_err(),
            "deadline disabled, yet the connection was closed — the other tests \
             would then pass without the fix, proving nothing"
        );
        handle.abort();
    }

    /// Env-knob semantics, following the connection-cap contract: unset
    /// → default; 0 → disabled; a typo → the default, never unbounded.
    #[test]
    fn an_unparseable_deadline_falls_back_to_the_default_rather_than_to_unbounded() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restore = std::env::var("FLINT_NFS_IDLE_TIMEOUT_SECS").ok();

        std::env::set_var("FLINT_NFS_IDLE_TIMEOUT_SECS", "not-a-number");
        assert_eq!(
            idle_timeout_from_env(),
            Some(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)),
            "a malformed deadline must fall back to the DEFAULT, never to disabled"
        );

        std::env::set_var("FLINT_NFS_IDLE_TIMEOUT_SECS", "0");
        assert_eq!(idle_timeout_from_env(), None, "0 is the documented opt-out");

        std::env::set_var("FLINT_NFS_IDLE_TIMEOUT_SECS", "  45 ");
        assert_eq!(idle_timeout_from_env(), Some(Duration::from_secs(45)));

        std::env::remove_var("FLINT_NFS_IDLE_TIMEOUT_SECS");
        assert_eq!(
            idle_timeout_from_env(),
            Some(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS))
        );

        match restore {
            Some(v) => std::env::set_var("FLINT_NFS_IDLE_TIMEOUT_SECS", v),
            None => std::env::remove_var("FLINT_NFS_IDLE_TIMEOUT_SECS"),
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
