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
use crate::nfs::rpcsec_gss::RpcSecGssManager;
use crate::nfs::v4::dispatcher::CompoundDispatcher;
use crate::nfs::v4::filehandle::FileHandleManager;
use crate::nfs::v4::state::StateManager;
use crate::nfs::v4::operations::lockops::LockManager;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{error, info, warn};

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
    /// State manager — held here so `load_persisted_state` can call
    /// `load_from_backend` at startup before accepting any TCP
    /// connections. The dispatcher holds its own `Arc` clone for the
    /// hot path; this is just the same `Arc`.
    state_mgr: Arc<StateManager>,
    /// Shared backend — held so `load_persisted_state` can read
    /// `LayoutRecord`s and bump the instance counter. Same `Arc` the
    /// state managers are using.
    backend: Arc<dyn crate::state_backend::StateBackend>,
}

impl MetadataServer {
    /// Create a new metadata server. Async because the persistent
    /// per-deployment server id (used by `FileHandleManager` so cached
    /// FHs survive MDS restart) is read from the backend on the same
    /// runtime tick as construction. `nfs_mds_main` already runs
    /// under `#[tokio::main]` so this is a single `.await`.
    pub async fn new(config: MdsConfig, exports: Vec<crate::pnfs::config::ExportConfig>) -> Result<Self> {
        info!("Initializing Metadata Server");

        // Get export path from first export, default to /data if not specified
        let export_path = exports.first()
            .map(|e| std::path::PathBuf::from(&e.path))
            .unwrap_or_else(|| std::path::PathBuf::from("/data"));

        info!("📂 MDS export path: {:?}", export_path);

        // Create the export root if absent — on a fresh PVC (the k8s
        // deployment shape) the directory doesn't exist yet. Mirrors
        // build_state_backend's create_dir_all for the state.db parent.
        std::fs::create_dir_all(&export_path).map_err(|e| {
            crate::pnfs::Error::Config(format!(
                "create export dir {:?}: {}",
                export_path, e
            ))
        })?;

        // Flint-lite standalone: a coherence authority with no fleet.
        // The posture must be unambiguous — a config that says
        // standalone AND lists data servers (or a block export) is two
        // different servers at once, and whichever the operator meant,
        // half their config is being ignored. Refuse it.
        let standalone = config.standalone;
        if standalone {
            if !config.data_servers.is_empty() {
                return Err(crate::pnfs::Error::Config(format!(
                    "standalone refuses dataServers ({} configured) — drop them or drop \
                     standalone; layouts are off in this posture and the fleet would \
                     never be used",
                    config.data_servers.len()
                )));
            }
            if config.block_export.is_some() {
                return Err(crate::pnfs::Error::Config(
                    "standalone refuses blockExport — the block tier needs spdk-tgt and \
                     layouts, neither of which exists in this posture"
                        .to_string(),
                ));
            }
            info!(
                "🪶 STANDALONE posture (flint-lite): layouts OFF — every byte serves \
                 through the MDS lane; no DS fleet, no block export"
            );
        }

        // Initialize state manager. The backend kind comes from
        // `config.state.backend` — `memory` for tests / dev work (no
        // restart survival), `sqlite` for production. The shared
        // `Arc<dyn StateBackend>` is also used by `LayoutManager`
        // below so all four record kinds (client / session / stateid
        // / layout) round-trip through the same store.
        let backend = crate::pnfs::config::PnfsConfig::build_state_backend(&config.state)
            .map_err(|e| crate::pnfs::Error::Config(format!("state backend: {}", e)))?;

        // Pull the persistent server id BEFORE constructing the
        // FileHandleManager so its `instance_id` survives MDS
        // restart. `memory` backend generates a fresh random on
        // first call (so MemoryBackend has no restart survival,
        // by design); `sqlite` returns the same value on every
        // open of the same `state.db`. This is what closes the
        // last Phase B gap: `NFS4ERR_BADHANDLE` no longer fires
        // when a kernel client uses a pre-restart cached FH.
        let server_id = backend.get_or_init_server_id().await
            .map_err(|e| crate::pnfs::Error::Config(format!("server_id init: {}", e)))?;
        info!("🔑 MDS server id (persistent): {} — FHs stamped with this survive restart", server_id);

        // Initialize file handle manager with the persisted server id.
        let fh_manager = Arc::new(FileHandleManager::new_with_instance_id(
            export_path.clone(),
            "volume".to_string(),
            server_id,
        ));
        // v2 (id-based) filehandles — minted for paths too long to
        // embed — resolve through a table persisted alongside the rest
        // of the NFS state, so they survive MDS restart.
        fh_manager.attach_backend(Arc::clone(&backend)).await;

        let state_mgr = Arc::new(StateManager::new("", Arc::clone(&backend)));
        
        // Initialize lock manager
        let lock_mgr = Arc::new(LockManager::new());

        // Initialize device registry
        let device_registry = Arc::new(DeviceRegistry::new());

        // F67: the durable file_id↔path binding rides on the stub as a
        // user xattr, in the SAME failure domain as the namespace. Probe
        // support once on the export root; an xattr-less filesystem
        // under a memory state backend is exactly the "restart = silent
        // zeros" configuration and must not boot quietly.
        let xattr_ok =
            crate::pnfs::mds::stub_binding::XattrStubBinding::probe(&export_path);
        if !xattr_ok && standalone {
            // No layouts ⇒ no striped placements ⇒ no bindings to
            // mirror. The F67 failure shape cannot occur here, so an
            // xattr-less export filesystem is not a boot refusal.
            info!(
                "export filesystem at {:?} lacks user xattrs — irrelevant in standalone: \
                 no striped placements exist to bind (F67 machinery idle)",
                export_path
            );
        } else if !xattr_ok {
            let memory_backend = matches!(
                config.state.backend,
                crate::pnfs::config::StateBackend::Memory
            );
            if memory_backend {
                return Err(crate::pnfs::Error::Config(format!(
                    "F67: export filesystem at {:?} does not support user xattrs and \
                     state.backend is 'memory' — every restart would silently zero all \
                     striped reads. Use a persistent backend or an xattr-capable export \
                     filesystem.",
                    export_path
                )));
            }
            tracing::error!(
                "F67: export filesystem at {:?} does not support user xattrs — placement \
                 bindings cannot be mirrored onto stubs. Striped data will not survive \
                 the loss of the state backend. DEGRADED.",
                export_path
            );
        }

        // Initialize layout manager. Shares the StateManager's
        // backend so layout records persist alongside client /
        // session / stateid records.
        // Per-volume stripe geometry persists in the same state backend
        // as placements (`load_persisted_state` seeds it below).
        // `config.layout.stripe_size` stays the fleet-wide default for
        // volumes that ask for nothing.
        let layout_manager = Arc::new(LayoutManager::new_with_binding(
            Arc::clone(&device_registry),
            config.layout.policy,
            config.layout.stripe_size,
            state_mgr.backend(),
            Arc::new(crate::pnfs::mds::stub_binding::XattrStubBinding::new(
                export_path.clone(),
            )),
        ));

        // Initialize pNFS operation handler
        let operation_handler = Arc::new(PnfsOperationHandler::new(
            Arc::clone(&layout_manager),
            Arc::clone(&device_registry),
            export_path.to_string_lossy().into_owned(),
        ));

        // Initialize NFSv4 dispatcher. With a pNFS handler it serves
        // the full op set (LAYOUTGET, GETDEVICEINFO, ...). Standalone
        // passes None — the DS-proven no-handler path: EXCHANGE_ID
        // advertises a non-pNFS server, LAYOUTGET answers NotSupp, the
        // F68a meter never arms (all-MDS I/O is the norm here, not the
        // F68 pathology), and every READ/WRITE serves from the export
        // tree through the MDS lane.
        let pnfs_ops: Option<Arc<dyn crate::pnfs::PnfsOperations>> = if standalone {
            None
        } else {
            Some(operation_handler.clone() as Arc<dyn crate::pnfs::PnfsOperations>)
        };
        let base_dispatcher = Arc::new(CompoundDispatcher::new_with_pnfs(
            Arc::clone(&fh_manager),
            Arc::clone(&state_mgr),
            lock_mgr,
            pnfs_ops,
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

        // Close the wiring cycle: the handler could not receive this at
        // construction (the dispatcher it feeds had to exist first), and
        // note_truncate needs it to recall layouts before cutting the
        // DSes (F65). Without this line the truncate gate still holds
        // but the held-layout window is wide open.
        operation_handler.attach_callback_manager(Arc::clone(&callback_manager));
        // The layout manager needs it too, for a callback the operation
        // handler never sees: CB_NOTIFY_DEVICEID on block expand is
        // driven from the CONTROL plane (ExpandVolume), which reaches
        // the layout manager and not the NFS operation handler.
        layout_manager.attach_callback_manager(Arc::clone(&callback_manager));
        // ...and the lease verdict, for the roller's initiator report: a
        // client-earned admission outlives the client that earned it (only
        // fence and DeleteVolume remove those rows), so without this the
        // report would name a long-gone client forever and the roller
        // would refuse that node's roll for good.
        {
            let leases = Arc::clone(&state_mgr.leases);
            layout_manager
                .attach_lease_oracle(Arc::new(move |c| leases.is_valid(c)));
        }

        // pnfs-block: the NVMe export reconciler, when configured. Same
        // late-attach shape as the callback manager. Without it this MDS
        // refuses block-class CreateVolume — a block volume with no
        // reconciler is a volume no client can ever reach.
        if let Some(be) = &config.block_export {
            let rpc = Arc::new(crate::minimal_disk_service::MinimalDiskService::new(
                "mds".to_string(),
                be.spdk_socket.clone(),
            ));
            let mut reconciler = crate::pnfs::mds::block_export::BlockExportReconciler::new(
                rpc,
                state_mgr.backend(),
                be.lvstore.clone(),
                be.traddr.clone(),
                be.trsvcid,
                be.ptpl_dir.clone(),
            );
            // THE WITNESS. Unset, arbitration runs in this shard's own
            // record — correct for a single-copy volume, where "both
            // targets" is one target, and bit-identical to what shipped
            // before the witness existed. Set, it points at a store BOTH
            // of a volume's targets can reach independently of each
            // other's health, which is the only thing that makes a
            // failover decidable (see `witness.rs`).
            //
            // A path here is the SHARED-SQLITE witness: real CAS from
            // sqlite's own transactions, correct for every shard on one
            // host, and what lets the two-target lima rig fail over at
            // zero cluster spend. It is NOT a production answer for a
            // multi-node cluster — nothing replicates a file between
            // nodes — and the startup line says so, because an operator
            // who points this at a per-node path has a witness that
            // witnesses nothing.
            // PRODUCTION FIRST: a namespace here puts arbitration in
            // Kubernetes, one ConfigMap per volume under resourceVersion
            // CAS — the only witness that is a third party to two
            // targets on two nodes. The sqlite path below stays for the
            // rig and for a single host.
            let kube_ns = std::env::var("FLINT_PNFS_WITNESS_NAMESPACE")
                .ok()
                .filter(|n| !n.trim().is_empty());
            if let Some(ns) = kube_ns {
                match crate::pnfs::mds::witness_kube::ConfigMapStore::new(&ns).await {
                    Ok(store) => {
                        reconciler = reconciler.with_witness(Arc::new(
                            crate::pnfs::mds::witness_kube::KubeWitness::new(Arc::new(store)),
                        ));
                        info!(
                            "⚖️  composition witness: Kubernetes ConfigMaps in namespace {} — \
                             one rv-CAS'd object per volume holds the seat, the leg marks, the \
                             serving lease and the allow-list identities. An API outage longer \
                             than the lease TTL suspends REPLICATED volumes (writes stall, \
                             nothing corrupts); single-copy volumes never touch it",
                            ns
                        );
                    }
                    Err(e) => {
                        // Keep the local record and say so: a witness
                        // that silently is not one is how a failover
                        // gets decided twice.
                        error!(
                            "composition witness for namespace {} could not be reached ({}) — \
                             falling back to this shard's OWN record, which means a replicated \
                             volume here cannot fail over. Single-copy volumes are unaffected",
                            ns, e
                        );
                    }
                }
            } else if let Some(path) = std::env::var("FLINT_PNFS_WITNESS_SQLITE")
                .ok()
                .filter(|p| !p.trim().is_empty())
            {
                match crate::state_backend::SqliteBackend::open(&path) {
                    Ok(w) => {
                        let w: Arc<dyn crate::state_backend::StateBackend> = Arc::new(w);
                        reconciler = reconciler.with_witness(Arc::new(
                            crate::pnfs::mds::witness::BackendWitness::new(w),
                        ));
                        info!(
                            "⚖️  composition witness: shared sqlite at {} — arbitration (seat, \
                             legs, leases, target registry) is read and CAS'd THERE, not in this \
                             shard's own record. Correct only if every shard that can compose \
                             these volumes opens the SAME file",
                            path
                        );
                    }
                    Err(e) => {
                        // Fail LOUD and keep the local record: a witness
                        // that silently is not one is how a failover gets
                        // decided twice.
                        error!(
                            "composition witness at {} could not be opened ({}) — falling back to \
                             this shard's OWN record, which means a replicated volume here cannot \
                             fail over. Single-copy volumes are unaffected",
                            path, e
                        );
                    }
                }
            }
            layout_manager.attach_block_export(Arc::new(reconciler));
            info!(
                "🧱 block-export reconciler attached: socket={} lvstore={} listener={}:{}",
                be.spdk_socket, be.lvstore, be.traddr, be.trsvcid
            );
        }

        // Register initial data servers from config
        for ds in &config.data_servers {
            // The endpoint field may be a comma-separated multipath
            // list — the raw string must never become a primary
            // endpoint (it encodes to an unparseable netaddr4).
            let (primary, mut extras) =
                crate::pnfs::config::split_endpoint_list(&ds.endpoint);
            let mut device_info = DeviceInfo::new(
                ds.device_id.clone(),
                primary,
                ds.bdevs.clone(),
            );

            // Add multipath endpoints (comma extras + explicit list)
            extras.extend(ds.multipath.iter().cloned());
            device_info.endpoints = extras;

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
            state_mgr,
            backend,
        })
    }

    /// Phase B.4 startup hook: pull every persisted record (clients,
    /// sessions, stateids, layouts) into the in-memory caches and
    /// bump the persisted instance counter.
    ///
    /// Called from `serve()` once, before the TCP listener accepts
    /// any connections — by the time a client reconnects, its
    /// pre-restart state is back. Errors are surfaced as
    /// `pnfs::Error::Config` because they're typically operator-
    /// visible (a corrupt or schema-mismatched DB file).
    async fn load_persisted_state(&self) -> Result<()> {
        // Bump the instance counter first so any record persisted
        // during this run is associated with a fresh value. The
        // counter is exposed on the wire only via device-id prefix
        // mixing (a follow-up; B.4 just persists + logs it). Even so,
        // observing it monotonically increasing across restarts is
        // the operator's primary signal that durable state is
        // working.
        let instance = self
            .backend
            .increment_instance_counter()
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("instance counter: {}", e)))?;
        info!(
            "📈 MDS instance counter: {} (incremented at startup; persisted across restart)",
            instance,
        );

        // Clients / sessions / stateids — `StateManager` knows how to
        // route the records into the right sub-managers and bump the
        // monotonic counters past the highest observed ids.
        self.state_mgr
            .load_from_backend()
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("load state: {}", e)))?;

        // Layouts live outside `StateManager` (pNFS-specific), so
        // pull them separately. Same backend, same records that B.2
        // proved survive `open()` round-trips.
        let layouts = self
            .backend
            .list_layouts()
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("list layouts: {}", e)))?;
        let n = layouts.len();
        // DELIBERATELY LAZY (Phase 3): reloaded layouts and placements
        // reference devices the (in-memory, empty-at-boot) registry
        // hasn't seen yet. Do NOT validate or recall here — the DSes
        // re-introduce themselves within one heartbeat (NACK →
        // immediate re-register), the stale-device sweep holds for the
        // boot grace, and generate_layout refuses per-file on ACTUAL
        // staleness at grant time. Eager validation at this point would
        // recall every layout in the cluster on every MDS restart.
        self.layout_manager.load_records(layouts);
        info!("📦 MDS reloaded {} persisted layouts from backend", n);

        // Pinned per-file placements (Phase 0). Must be back before
        // the first LAYOUTGET: a post-restart grant for a pre-restart
        // file has to reuse its pin, not mint a fresh one from
        // whichever DSes re-registered first.
        let placements = self
            .backend
            .list_placements()
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("list placements: {}", e)))?;
        let n = placements.len();
        self.layout_manager.load_placement_records(placements);

        // R4: a gate restored above with no retry behind it is a wedge —
        // LAYOUTGET answers TRYLATER forever. Re-arm before the listener
        // binds.
        let resumed = self.operation_handler.resume_parked_truncates();
        if resumed > 0 {
            warn!(
                "⏳ {} file(s) came back with an unconfirmed truncate — layouts are \
                 refused for them until a pinned DS confirms the cut",
                resumed,
            );
        }
        info!("📦 MDS reloaded {} persisted placements from backend", n);

        // F67 backfill: mirror restored placements onto stubs that
        // predate the binding xattr, converging pre-F67 fleets to full
        // coverage. Idempotent; skips stubs already bound or deleted.
        let (bound, bind_failed) = self.layout_manager.backfill_stub_bindings();
        if bound > 0 || bind_failed > 0 {
            info!(
                "🩹 F67 backfill: wrote {} stub binding(s), {} failed",
                bound, bind_failed
            );
        }

        // Per-volume stripe geometry, seeded EAGERLY: the cache is the
        // only reader on the layout path, so a LAYOUTGET arriving before
        // the load would pin a file at the fleet default and never
        // re-stripe it. This runs before `serve()` opens the listener.
        //
        // The volume list comes from the export directory, so a volume
        // whose geometry record is missing gets one WARN. That is the
        // only signal separating "created before geometry existed"
        // (routine) from "acked at provision, then lost" (not).
        let volumes: Vec<String> = std::fs::read_dir(&self.export_path)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        self.layout_manager.load_volume_geometry(&volumes).await;

        Ok(())
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
        if self.config.standalone {
            info!("Posture: STANDALONE (flint-lite) — layouts off, all I/O MDS-lane");
        } else {
            info!("Layout Type: {:?}", self.config.layout.layout_type);
            info!("Stripe Size: {} bytes", self.config.layout.stripe_size);
            info!("Layout Policy: {:?}", self.config.layout.policy);
            info!("Registered Data Servers: {}", self.device_registry.count());
        }
        info!("");

        // S3 tier (L2 step 7): the config-driven master switch, flipped
        // BEFORE the state load so the dirty-bit restore below runs.
        // v1 scope is the standalone (flint-lite) posture only — a pNFS
        // MDS's flusher would publish its sparse size-only stubs
        // (design review, the tier-inversion finding), so the
        // combination is refused, not warned about.
        if let Some(t) = self.config.tier.as_ref().filter(|t| t.enabled) {
            if !self.config.standalone {
                return Err(crate::pnfs::Error::Config(
                    "tier: v1 requires the standalone (flint-lite) posture — a pNFS \
                     MDS's flusher would publish sparse stubs; remove `tier` or set \
                     `standalone: true`"
                        .into(),
                ));
            }
            crate::tier::capture::enable();
            info!(
                "🪣 S3 tier ENABLED — bucket {}, prefix {:?}",
                t.bucket, t.key_prefix
            );
        }

        // Phase B.4: pull persisted state out of the backend before
        // accepting any TCP connections. Once this returns, a
        // reconnecting client whose clientid / sessionid / stateid
        // existed pre-restart finds it back in the in-memory cache —
        // no `STALE_CLIENTID` / `BAD_STATEID`. Layout records are
        // loaded into `LayoutManager` separately because it lives in
        // the pNFS layer, outside `state::StateManager`.
        self.load_persisted_state().await?;

        // S3 tier (L2 step 2, A3): restore the durable dirty bits —
        // after a crash, exactly the bit-set files are whole-dirty in
        // memory, which is the "pessimal upload, never wrong data"
        // fallback. Before any TCP accept, like the state load above:
        // a client's first WRITE must find the durable-marked set
        // primed, not race it.
        if crate::tier::capture::enabled() {
            match crate::tier::durable::restore_from_backend(&self.state_mgr.backend()).await
            {
                Ok(_) => {}
                Err(e) => tracing::error!(
                    "tier: dirty-bit restore failed: {} — bits remain set in the \
                     backend (eviction stays blocked); files re-mark on their next \
                     mutation",
                    e
                ),
            }
        }

        // S3 tier (L2 step 7, A8): FENCING BEFORE DAEMON. Claim the
        // volume epoch — waiting out a dead holder's lease if one is
        // recorded — sweep every in-flight assembly, and only then
        // start the flush loop. A failure here refuses startup: the
        // operator asked for a tier, and loud beats silently untiered
        // (the A9 posture).
        if let Some(t) = self.config.tier.clone().filter(|t| t.enabled) {
            self.start_tier(&t).await?;
        }

        // pnfs-block startup replay: converge every known scsi volume's
        // export chain (the geometry cache was just seeded above) and
        // re-establish active fences from the durable record — the
        // together-restart repair (MDS pod restart takes the colocated
        // tgt with it; sqlite is the only durable record of what the
        // tgt should serve) and the PTPL-loss recovery path. Spawned +
        // loud-on-failure: a down tgt must not stop the MDS from
        // serving its file-class volumes. The SAME pass then repeats on
        // a timer (start_export_reconcile below) — that is what repairs
        // a tgt-ONLY restart, where no MDS startup ever runs.
        if self.layout_manager.block_export().is_some() {
            let volumes = self.layout_manager.scsi_volumes();
            if !volumes.is_empty() {
                info!(
                    "🧱 block-export startup replay: {} scsi volume(s)",
                    volumes.len()
                );
                let lm = Arc::clone(&self.layout_manager);
                tokio::spawn(async move {
                    crate::pnfs::mds::operations::export_reconcile_pass(&lm, "startup").await;
                });
            }
        }
        self.start_export_reconcile();

        // Start heartbeat monitor in the background. Standalone has no
        // fleet to watch — the monitor would only ever report an empty
        // registry, so it does not start at all.
        if self.config.standalone {
            info!("🪶 standalone: heartbeat monitor not started (no DS fleet)");
        } else {
            let heartbeat_timeout =
                Duration::from_secs(self.config.failover.heartbeat_timeout);
            self.start_heartbeat_monitor(heartbeat_timeout);
        }

        // The lease sweep: the reaper for clients that die holding
        // layouts/grant rows. Client expiry is otherwise LAZY (checked
        // at the top of incoming COMPOUNDs), so a volume whose ONLY
        // client died is never reaped — its grant rows would block
        // every successor forever.
        self.start_lease_sweep();

        // Start status reporter in background
        self.start_status_reporter();

        // F68a: data-path reporter — one line per interval when client
        // data crossed the MDS, WARN when it did so on a healthy fleet.
        self.start_f68a_reporter();

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

    /// S3 tier startup (L2 step 7): store connect → bucket bootstrap →
    /// epoch claim (A8: self-recognition makes a routine restart resume
    /// instantly; a foreign holder is judged dead only by the store's
    /// own evidence; every claim sweeps in-flight MPUs so a deposed
    /// hub's Complete fails NoSuchUpload) → heartbeat → flush loop.
    /// Runs BEFORE the TCP listener binds — an unfenced hub never
    /// serves.
    async fn start_tier(&self, t: &crate::pnfs::config::TierConfig) -> Result<()> {
        use crate::tier::epoch;
        use crate::tier::flush::{FlushConfig, FlushOrchestrator};
        use crate::tier::store::ObjectStore;

        // A10 first — the space model is local and cheap, and admission
        // + truthful SPACE_* should hold even while the epoch claim
        // below waits. Ballast wants a db file to sit next to; the
        // memory backend has none to protect.
        let ballast_path = match self.config.state.backend {
            crate::pnfs::config::StateBackend::Sqlite => {
                let db = self
                    .config
                    .state
                    .config
                    .get("path")
                    .cloned()
                    .unwrap_or_else(|| "/var/lib/flint-pnfs/state.db".to_string());
                std::path::Path::new(&db)
                    .parent()
                    .map(|p| p.join("flint-ballast.bin"))
            }
            _ => {
                if t.ballast_bytes > 0 {
                    info!("🪣 tier space: memory state backend — ballast disabled");
                }
                None
            }
        };
        let space = crate::tier::space::configure(crate::tier::space::SpaceConfig {
            root: self.export_path.clone(),
            reserve_bytes: t.reserve_bytes,
            watermark_pct: t.watermark_pct,
            ballast_path,
            ballast_bytes: t.ballast_bytes,
        })
        .map_err(|e| crate::pnfs::Error::Config(format!("tier: space model: {}", e)))?;
        tokio::spawn(async move {
            let mut iv = interval(Duration::from_secs(crate::tier::space::REFRESH_SECS));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                let s = Arc::clone(&space);
                // statvfs + possible ballast I/O: off the executor.
                let _ = tokio::task::spawn_blocking(move || s.refresh()).await;
            }
        });
        info!(
            "🪣 tier space: reserve {} MiB, watermark {}%, ballast {} MiB",
            t.reserve_bytes / (1024 * 1024),
            t.watermark_pct,
            t.ballast_bytes / (1024 * 1024)
        );

        let store: Arc<dyn ObjectStore> = Arc::new(
            crate::tier::store::s3::S3Store::connect(t.bucket.clone(), t.endpoint.clone())
                .await
                .map_err(|e| crate::pnfs::Error::Config(format!("tier: store connect: {}", e)))?,
        );

        // A9: verify/create the bucket posture; errors refuse startup.
        let report = store
            .bootstrap(&t.key_prefix)
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("tier: bucket bootstrap: {}", e)))?;
        for n in &report.notes {
            info!("🪣 tier bootstrap: {}", n);
        }
        for w in &report.warnings {
            warn!("🪣 tier bootstrap: {}", w);
        }
        if !report.ok() {
            return Err(crate::pnfs::Error::Config(format!(
                "tier: bucket posture refused: {}",
                report.errors.join("; ")
            )));
        }

        // Holder identity = the persisted server_id (the same one that
        // keeps filehandles stable across restart) — that is what
        // self-recognition recognizes after a pod restart.
        let server_id = self
            .backend
            .get_or_init_server_id()
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("tier: server_id: {}", e)))?;
        let mut ecfg = epoch::EpochConfig::new(&t.key_prefix, format!("hub-{:016x}", server_id));
        ecfg.heartbeat = Duration::from_secs(t.epoch_heartbeat_secs.max(1));
        ecfg.lease_misses = t.epoch_lease_misses.max(1);
        info!(
            "🔐 tier: claiming the volume epoch as {} (a live foreign holder is \
             waited out: {} × {}s)",
            ecfg.holder_id, ecfg.lease_misses, t.epoch_heartbeat_secs
        );
        let lease = epoch::claim(&store, &ecfg, &t.key_prefix)
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("tier: epoch claim: {}", e)))?;
        info!("🔐 tier: epoch {} held", lease.epoch);

        let guard = epoch::EpochGuard::held(lease.epoch);
        // Deposed ⇒ fence + exit. The restart re-claims instantly by
        // self-recognition if the deposition was a renewal blip — or,
        // while a foreign holder is genuinely live, refuses to serve:
        // the split-brain protection A8 demands ("stops serving"). Two
        // hubs on one prefix is a misconfiguration, and runbr proved
        // what serving on moved authority does to pinned clients.
        epoch::spawn_heartbeat(
            Arc::clone(&store),
            ecfg,
            lease,
            Arc::clone(&guard),
            Box::new(|| {
                error!("tier: DEPOSED — exiting so the restart re-judges the epoch");
                std::process::exit(70);
            }),
        );

        let mut fcfg = FlushConfig::new(self.export_path.clone(), t.key_prefix.clone());
        fcfg.floor = Duration::from_secs(t.flush_floor_secs);
        fcfg.quiesce = Duration::from_secs(t.quiesce_secs);
        fcfg.whole_put_max = t.whole_put_max_bytes;
        fcfg.part_floor = t.part_floor_bytes.max(store.min_part_size());
        let floor_s = t.flush_floor_secs;
        let quiesce_s = t.quiesce_secs;
        // The hydrator and the watermark pass share the store.
        let orch_store = Arc::clone(&store);
        let orch = Arc::new(FlushOrchestrator::new(
            store,
            Arc::clone(&self.backend),
            fcfg,
            guard,
        ));
        // Rebuild the durable registry and arbitrate any crash-torn
        // intents BEFORE the first tick (and before clients reconnect
        // and mutate on top).
        orch.startup().await;

        // Step 10 (C2): finish or roll back half-evictions and load
        // the marker consult map — BEFORE the listener binds, so the
        // first READ of an evicted file finds its marker, never a
        // bare 0-byte stub. Step 11 extends it: crashed hydrations are
        // truncated back to the stub (partials never serve) or, if
        // provably complete, committed.
        let er = crate::tier::evict::reconcile(&self.backend).await;
        if er.finished > 0 || er.rolled_back > 0 || er.hydrations_reset > 0 {
            warn!(
                "🪣 tier evict reconcile: {} half-eviction(s) finished, {} rolled back, \
                 {} crashed hydration(s) reset, {} completed",
                er.finished, er.rolled_back, er.hydrations_reset, er.hydrations_finished
            );
        }

        // Step 11: the hydrator — evicted files restore in place on
        // first touch; WRITE-triggered restores hold a reserved
        // concurrency slot (step-9 hung-task finding).
        crate::tier::hydrate::install(
            Arc::clone(&self.backend),
            Arc::clone(&orch_store),
            crate::tier::hydrate::HydrateConfig {
                hold: Duration::from_secs(t.hydrate_hold_secs.max(1)),
                concurrency: t.hydrate_concurrency.max(1),
            },
        );

        // Step 11: the watermark-driven eviction pass goes LIVE —
        // hydration exists, so evicted files can come back. Runs on
        // the flush cadence; each freed file re-checks the gauge via a
        // forced statvfs so the pass stops promptly.
        {
            let backend = Arc::clone(&self.backend);
            let store = Arc::clone(&orch_store);
            let export_root = self.export_path.clone();
            let key_prefix = t.key_prefix.clone();
            let probe_view = self.base_dispatcher.open_file_view();
            let tick = Duration::from_secs(t.tick_secs.max(1));
            tokio::spawn(async move {
                let probe = move |_dev: u64, ino: u64| probe_view.has_writable_ino(ino);
                let mut iv = interval(tick);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    iv.tick().await;
                    if !crate::tier::space::above_watermark() {
                        continue;
                    }
                    let n = crate::tier::evict::evict_pass(
                        &backend,
                        &store,
                        &export_root,
                        &key_prefix,
                        &probe,
                        &|| {
                            crate::tier::space::refresh_now();
                            crate::tier::space::above_watermark()
                        },
                    )
                    .await;
                    if n > 0 {
                        info!("🪣 tier: watermark pass evicted {} file(s)", n);
                    }
                }
            });
        }

        let tick = Duration::from_secs(t.tick_secs.max(1));
        tokio::spawn(async move {
            let mut iv = interval(tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                let r = orch.tick().await;
                if r.published > 0 || r.failed > 0 {
                    info!(
                        "🪣 tier flush: {} published, {} clean, {} failed ({} examined)",
                        r.published, r.clean, r.failed, r.examined
                    );
                }
            }
        });
        info!(
            "🪣 tier: flush loop every {:?} (floor {}s, quiesce {}s)",
            tick, floor_s, quiesce_s
        );
        Ok(())
    }

    /// Start gRPC control server for DS registration
    fn start_grpc_server(&self) {
        let device_registry = Arc::clone(&self.device_registry);
        let bind_addr = self.config.bind.address.clone();
        let nfs_port = self.config.bind.port;
        let export_path = self.export_path.clone();
        // Whether per-volume geometry can be kept across a restart.
        let durable_state = !matches!(
            self.config.state.backend,
            crate::pnfs::config::StateBackend::Memory
        );
        let layout_manager = self.layout_manager.as_ref().clone();
        // F68b: only enabled where the MDS shares the clients' network
        // path to the DSes (k8s chart). See MdsConfig for the trap.
        let verify_ds_reachability = self.config.verify_ds_reachability;
        // Build the operator's `device_id → reachable endpoint` map from
        // the static config. The gRPC service uses this to override the
        // bind-address that registering DSes report (a DS only knows its
        // own bind, often 0.0.0.0; the client needs the externally
        // routable endpoint).
        let configured_endpoints: std::collections::HashMap<String, String> =
            self.config.data_servers.iter()
                .map(|ds| (ds.device_id.clone(), ds.endpoint.clone()))
                .collect();
        // Same idea for the MDS→DS control direction: an explicit
        // override wins over deriving the control host from the
        // client-reachable endpoint (they differ on the lima rig,
        // where clients reach DSes at host.lima.internal but the MDS
        // reaches them at 127.0.0.1).
        let configured_control_endpoints: std::collections::HashMap<String, String> =
            self.config.data_servers.iter()
                .filter_map(|ds| {
                    ds.control_endpoint
                        .as_ref()
                        .map(|ce| (ds.device_id.clone(), ce.clone()))
                })
                .collect();

        tokio::spawn(async move {
            // gRPC control port: 50051 unless FLINT_MDS_GRPC_PORT
            // overrides. The override exists for the multi-shard lima
            // rig, where N MDS processes share one host IP; k8s shards
            // each have their own pod IP and stay on 50051.
            let grpc_port = std::env::var("FLINT_MDS_GRPC_PORT")
                .ok()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(50051);
            let grpc_addr = format!("{}:{}", bind_addr, grpc_port)
                .parse()
                .expect("Invalid gRPC address");

            let control_service = MdsControlService::new(
                device_registry,
                configured_endpoints,
                configured_control_endpoints,
                export_path,
                layout_manager,
                nfs_port,
                durable_state,
                verify_ds_reachability,
            );

            // Control-plane auth: when FLINT_PNFS_CONTROL_TOKEN is set,
            // every MdsControl RPC must carry it as a Bearer token.
            // This is what stops an arbitrary pod from registering
            // itself as a DS (and attracting stripe writes for new
            // files) or calling CreateVolume/DeleteVolume. Unset =
            // open (backward compatible) with a loud boot warning.
            let token = std::env::var("FLINT_PNFS_CONTROL_TOKEN").ok();
            match &token {
                Some(_) => info!("🔐 MDS control plane requires a bearer token"),
                None => warn!(
                    "⚠️ FLINT_PNFS_CONTROL_TOKEN unset — MDS control plane is UNAUTHENTICATED \
                     (any pod that can reach 50051 can register as a DS)"
                ),
            }
            let expected = token.map(|t| format!("Bearer {}", t));
            let svc = tonic::service::interceptor::InterceptedService::new(
                MdsControlServer::new(control_service),
                move |req: tonic::Request<()>| match &expected {
                    None => Ok(req),
                    Some(want) => match req.metadata().get("authorization") {
                        Some(got) if got.to_str().map(|s| s == want).unwrap_or(false) => Ok(req),
                        _ => Err(tonic::Status::unauthenticated(
                            "missing or invalid control-plane token",
                        )),
                    },
                },
            );

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

        info!(
            "gRPC control server started on port {} (for DS registration)",
            std::env::var("FLINT_MDS_GRPC_PORT").unwrap_or_else(|_| "50051".into())
        );
    }

    /// Serve pNFS over TCP.
    ///
    /// Delegates to the standalone server's RPC layer
    /// (`nfs::server_v4::serve_tcp`). The MDS used to carry its own fork of
    /// that whole layer — accept loop, connection handler, RPC dispatch,
    /// COMPOUND handler and the three RPCSEC_GSS entry points. The two were
    /// identical apart from log strings and one EXCHANGE_ID tweak (now in
    /// `CompoundDispatcher`, which knows it is a pNFS server), but the copy
    /// silently missed every fix made to the original after the fork:
    /// the SEQUENCE reply cache (`1a543b5`, pynfs SEQ9a-f) and the F55
    /// drain gate (`a4902ef`) among them.
    async fn serve_tcp(&self, addr: &str) -> Result<()> {
        info!("🚀 pNFS MDS serving NFSv4 on {}", addr);
        crate::nfs::server_v4::serve_tcp(
            addr,
            Arc::clone(&self.base_dispatcher),
            Arc::clone(&self.gss_manager),
        )
        .await
        .map_err(crate::pnfs::Error::Io)
    }


    /// Start heartbeat monitoring in the background
    /// The client-lease sweep (`operations::lease_sweep_pass` is the
    /// pass; this is the pacing). One env knob, house style:
    /// FLINT_PNFS_LEASE_SWEEP_SECS — interval in seconds, default 30
    /// (the cadence StateManager::cleanup_expired's own doc always
    /// asked for), 0 disables the sweep outright (the kill switch).
    ///
    /// Boot hold: a full lease time + grace period. Restored clients
    /// get fresh leases at state reload, so nothing can be HONESTLY
    /// expired before one whole lease window has passed — sweeping
    /// earlier would fence clients that are mid-reclaim. The in-grace
    /// check stays in the loop as the belt.
    /// The periodic export reconcile loop — the repair for a tgt-ONLY
    /// restart. An MDS restart replays exports at startup; a tgt that
    /// restarts UNDER a running MDS came back with empty memory and,
    /// until this loop existed, stayed empty until an operator rolled
    /// the MDS (the runbook rule this retires). Every tick re-runs the
    /// same `export_reconcile_pass` startup uses: level-triggered, so a
    /// converged tgt sees probes and zero mutations; a wiped one gets
    /// its subsystems, namespaces (with ptpl_file — which restores the
    /// reservations ptpl carried), listeners, allow-lists, and any
    /// ptpl-lost fences rebuilt within one interval.
    ///
    /// `FLINT_PNFS_EXPORT_RECONCILE_SECS` (default 30; 0 disables with
    /// a loud warning). The first tick is skipped — the startup replay
    /// just ran.
    fn start_export_reconcile(&self) {
        if self.layout_manager.block_export().is_none() {
            return;
        }
        let secs = std::env::var("FLINT_PNFS_EXPORT_RECONCILE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        if secs == 0 {
            warn!(
                "export reconcile loop DISABLED (FLINT_PNFS_EXPORT_RECONCILE_SECS=0) — \
                 a tgt-only restart will serve NOTHING until the MDS is rolled"
            );
            return;
        }
        let layout_manager = Arc::clone(&self.layout_manager);
        tokio::spawn(async move {
            info!(
                "🧱 export reconcile loop: every {}s (a tgt-only restart repairs \
                 without an MDS roll)",
                secs
            );
            let mut tick = interval(Duration::from_secs(secs));
            tick.tick().await; // consume the immediate tick — startup replay just ran
            loop {
                tick.tick().await;
                let (vols, refenced) = crate::pnfs::mds::operations::export_reconcile_pass(
                    &layout_manager,
                    "reconcile",
                )
                .await;
                tracing::debug!(
                    "export reconcile pass: {} volume(s), {} fence(s) re-established",
                    vols,
                    refenced
                );
            }
        });
    }

    fn start_lease_sweep(&self) {
        let secs = std::env::var("FLINT_PNFS_LEASE_SWEEP_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        if secs == 0 {
            warn!("lease sweep DISABLED (FLINT_PNFS_LEASE_SWEEP_SECS=0) — dead clients' \
                   layouts and grant rows will leak until re-enabled");
            return;
        }
        let layout_manager = Arc::clone(&self.layout_manager);
        let leases = Arc::clone(&self.state_mgr.leases);
        tokio::spawn(async move {
            let hold = Duration::from_secs(u64::from(leases.lease_time()))
                + leases.grace_remaining();
            info!(
                "Lease sweep holds for {}s (one lease window past grace), then every {}s",
                hold.as_secs(),
                secs
            );
            tokio::time::sleep(hold).await;
            let mut tick = interval(Duration::from_secs(secs));
            loop {
                tick.tick().await;
                if leases.in_grace_period() {
                    continue;
                }
                let swept = crate::pnfs::mds::operations::lease_sweep_pass(
                    &layout_manager,
                    &|c| leases.is_valid(c),
                )
                .await;
                if swept > 0 {
                    info!("💀 lease sweep: {} (volume, client) pair(s) fully swept", swept);
                }
            }
        });
    }

    fn start_heartbeat_monitor(&self, timeout: Duration) {
        let device_registry = Arc::clone(&self.device_registry);
        let layout_manager = Arc::clone(&self.layout_manager);
        let callback_manager = Arc::clone(&self.callback_manager);
        let failover_policy = self.config.failover.policy;

        tokio::spawn(async move {
            // Boot grace (Phase 3): a freshly (re)started MDS pre-registers
            // config-listed DSes with last_heartbeat = boot, and dynamic
            // DSes only discover the restart via their next heartbeat NACK.
            // Sweeping before they've had a full timeout window to
            // re-introduce themselves would mark HEALTHY devices Offline
            // and fan out CB_LAYOUTRECALL for every layout in the cluster.
            // Hold the first check for max(heartbeatTimeout, 30s) — clients
            // are reclaiming state through the 90s NFS grace period during
            // this window anyway, so nothing is lost by waiting.
            let boot_grace = std::cmp::max(timeout, Duration::from_secs(30));
            info!(
                "Stale-device sweep holds for {}s boot grace (then every 10s, timeout {}s)",
                boot_grace.as_secs(), timeout.as_secs()
            );
            tokio::time::sleep(boot_grace).await;

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
                // Refused joins the revoke-now arm deliberately: the client
                // answered and did NOT drop the layout, so it is at least as
                // dangerous as silence — and unlike a timeout we know it.
                RecallOutcome::TimedOut
                | RecallOutcome::NoChannel
                | RecallOutcome::Transport(_)
                | RecallOutcome::Refused(_) => {
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
    /// F68a: turn meter deltas into log lines. Silent while idle; INFO
    /// for modest MDS data traffic; WARN when clients push data through
    /// the MDS while the DS fleet is healthy — the F68 signature that
    /// every prior campaign (runbe, the runbg F68c flip) needed and did
    /// not have. Interval and WARN threshold are env-tunable:
    /// FLINT_F68A_INTERVAL_SECS (default 30),
    /// FLINT_F68A_WARN_MIB (default 64, per interval).
    fn start_f68a_reporter(&self) {
        let meter = self.operation_handler.f68a_meter_arc();
        let registry = Arc::clone(&self.device_registry);
        let interval = std::env::var("FLINT_F68A_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(30);
        let warn_bytes = std::env::var("FLINT_F68A_WARN_MIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(64)
            .saturating_mul(1 << 20);
        tokio::spawn(async move {
            let mut prev = meter.snapshot();
            let mut tick = tokio::time::interval(Duration::from_secs(interval));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it so the first
            // report covers a full interval.
            tick.tick().await;
            loop {
                tick.tick().await;
                let cur = meter.snapshot();
                let delta = cur.delta_since(&prev);
                prev = cur;
                if delta.is_zero() {
                    continue;
                }
                let active = registry.count_by_status(
                    crate::pnfs::mds::device::DeviceStatus::Active,
                );
                if active > 0 && delta.mds_data_bytes() >= warn_bytes {
                    warn!(
                        "🚨 F68a: client DATA is flowing through the MDS with {} Active DS(es) — \
                         a healthy pNFS client does this ~never (F68 signature). Last {}s: {}",
                        active, interval, delta.render()
                    );
                } else {
                    info!("📊 F68a last {}s: {}", interval, delta.render());
                }
            }
        });
    }

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


