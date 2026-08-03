//! pNFS Operations
//!
//! Implements pNFS-specific NFS operations as per RFC 8881:
//! - LAYOUTGET (opcode 50) - Get layout information
//! - LAYOUTRETURN (opcode 51) - Return layout to server
//! - LAYOUTCOMMIT (opcode 52) - Commit layout changes
//! - GETDEVICEINFO (opcode 47) - Get device addressing information
//! - GETDEVICELIST (opcode 48) - List all devices
//!
//! # Protocol References
//! - RFC 8881 Section 18.40 - GETDEVICEINFO
//! - RFC 8881 Section 18.41 - GETDEVICELIST
//! - RFC 8881 Section 18.42 - LAYOUTCOMMIT
//! - RFC 8881 Section 18.43 - LAYOUTGET
//! - RFC 8881 Section 18.44 - LAYOUTRETURN
//! - RFC 8881 Chapter 13 - NFSv4.1 File Layout Type

use crate::pnfs::mds::layout::{
    truncate_gate_key, FilePlacement, IoMode, LayoutManager, LayoutOwner, LayoutSegment,
    LayoutType,
};
use crate::pnfs::mds::device::{DeviceId, DeviceRegistry, DeviceStatus};
use crate::pnfs::handler_trait::FallbackIoDisposition;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Ceiling on how long a fallback READ/WRITE for a pinned file is
/// parked with NFS4ERR_DELAY while a pinned DS is down. Past this the
/// MDS fails the RPC with NFS4ERR_IO instead — an indefinitely-DELAYed
/// fallback is a client livelock (the kernel's fallback loop never
/// re-drives its layout path; see docs/pnfs-operator-runbook.md).
/// 90 s covers the drilled DS-recovery windows (reschedule 49–64 s,
/// node death + taint 64–70 s) with slack.
/// Override: FLINT_PNFS_FALLBACK_DELAY_CEILING_SECS.
const FALLBACK_DELAY_CEILING_DEFAULT: Duration = Duration::from_secs(90);

fn fallback_delay_ceiling() -> Duration {
    static CEILING: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CEILING.get_or_init(|| {
        std::env::var("FLINT_PNFS_FALLBACK_DELAY_CEILING_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(FALLBACK_DELAY_CEILING_DEFAULT)
    })
}

/// pNFS operation handler
pub struct PnfsOperationHandler {
    layout_manager: Arc<LayoutManager>,
    device_registry: Arc<DeviceRegistry>,
    /// When this handler (≈ the MDS process) came up. Anchors the
    /// outage clock for pinned devices that have not (re-)registered
    /// with this MDS incarnation at all — e.g. during the boot grace
    /// or an MDS-node-blackhole re-register window.
    boot_instant: Instant,
    /// The MDS export root's filesystem path (e.g. "/data/exports").
    /// Needed to compute the DS-side rebased relative path of a LEGACY
    /// pin's stripe files for cleanup (DSes store legacy stripes at
    /// <ds-data-dir>/<export-path-minus-leading-slash>/<file_key>).
    export_fs_path: String,

    /// Cached DsControl clients (MDS → DS), keyed by control endpoint.
    /// Entries are evicted on RPC failure so retries re-dial fresh.
    ds_control_clients: Arc<DashMap<String, crate::pnfs::grpc::AuthedDsControlClient>>,

    /// Back-channel fan-out, for recalling layouts on truncate (F65).
    ///
    /// Attached AFTER construction rather than passed to `new`, because
    /// the wiring is a cycle: `CallbackManager` borrows the
    /// dispatcher's back-channel registry, the dispatcher needs this
    /// handler as its `PnfsOperations`, and this handler needs the
    /// CallbackManager. `MdsServer::new` builds them in that order and
    /// calls `attach_callback_manager` last.
    ///
    /// `None` (never attached) is a legitimate state — unit tests build
    /// the handler standalone — and means no recall is sent. That is
    /// the pre-F65 behaviour, so the truncate gate still holds; only
    /// the held-layout window reopens.
    callback_manager: std::sync::OnceLock<Arc<crate::pnfs::mds::callback::CallbackManager>>,

    /// F68a data-path meter. Created here, fed by the dispatcher's
    /// READ/WRITE fallback lanes and layout ops, drained by the MDS's
    /// reporter task.
    f68a: Arc<crate::pnfs::mds::f68a_meter::DataPathMeter>,
}

impl PnfsOperationHandler {
    /// Create a new pNFS operation handler
    pub fn new(
        layout_manager: Arc<LayoutManager>,
        device_registry: Arc<DeviceRegistry>,
        export_fs_path: String,
    ) -> Self {
        Self {
            layout_manager,
            device_registry,
            boot_instant: Instant::now(),
            export_fs_path,
            ds_control_clients: Arc::new(DashMap::new()),
            callback_manager: std::sync::OnceLock::new(),
            f68a: Arc::new(crate::pnfs::mds::f68a_meter::DataPathMeter::default()),
        }
    }

    /// F68a: the meter, for the MDS reporter task.
    pub fn f68a_meter_arc(&self) -> Arc<crate::pnfs::mds::f68a_meter::DataPathMeter> {
        Arc::clone(&self.f68a)
    }

    /// Hand the handler the back-channel fan-out, once the dispatcher
    /// that owns the back-channel registry exists. Idempotent; a second
    /// call is ignored (the first wins) rather than panicking, because
    /// losing this race is harmless — both callers pass the same Arc.
    pub fn attach_callback_manager(
        &self,
        callbacks: Arc<crate::pnfs::mds::callback::CallbackManager>,
    ) {
        if self.callback_manager.set(callbacks).is_err() {
            debug!("PnfsOperationHandler: callback manager already attached");
        }
    }

    /// Recall + revoke every outstanding layout for a file whose size
    /// is changing (F65). No-op when nothing holds a layout, which is
    /// the common case — `return_on_close` is set on every grant, so
    /// layouts rarely outlive the open that created them.
    ///
    /// Silently does nothing when no `CallbackManager` was attached.
    /// That is the standalone-handler case (unit tests); a real MDS
    /// always attaches one in `MdsServer::new`.
    async fn recall_layouts_for_truncate(&self, gate: &str, new_size: u64) {
        let Some(callbacks) = self.callback_manager.get() else {
            debug!(
                "truncate of {}: no callback manager attached, layouts not recalled",
                gate
            );
            return;
        };
        let recalls = self.layout_manager.recall_layouts_for_file(gate);
        if recalls.is_empty() {
            return;
        }
        let pairs: Vec<(crate::nfs::v4::protocol::SessionId, _, _)> = recalls
            .into_iter()
            .map(|(sid, stateid, fh)| (crate::nfs::v4::protocol::SessionId(sid), stateid, fh))
            .collect();
        crate::pnfs::mds::callback::recall_layouts_for_truncate(
            callbacks,
            &self.layout_manager,
            gate,
            new_size,
            &pairs,
        )
        .await;
    }

    /// Re-arm the background retry for every truncate that was still
    /// parked when this MDS last stopped (audit R4).
    ///
    /// Restoring the GATE without the retry would be a wedge: LAYOUTGET
    /// answers TRYLATER forever and nothing ever confirms the cut. Call
    /// once at startup, after placements are loaded.
    ///
    /// Returns how many were re-armed. Deliberately loud even at zero on
    /// the "some were parked" path — an operator seeing this line knows
    /// a file is unreadable-by-layout until a DS comes back, which is
    /// the honest state.
    pub fn resume_parked_truncates(&self) -> usize {
        let parked = self.layout_manager.parked_truncates();
        for (gate, file_key, placement, pending) in &parked {
            warn!(
                "⏳ resuming parked truncate for '{}' (gate {}, deepest cut {}) — \
                 a pinned DS never confirmed before the last restart",
                file_key, gate, pending,
            );
            let registry = Arc::clone(&self.device_registry);
            let clients = Arc::clone(&self.ds_control_clients);
            let manager = Arc::clone(&self.layout_manager);
            let export = self.export_fs_path.clone();
            let key = file_key.clone();
            let gate = gate.clone();
            let placement = placement.clone();
            tokio::spawn(async move {
                // Same shape as note_truncate's retry: bounded backoff,
                // unbounded duration. The first attempts will very likely
                // fail — at startup the DSes have not re-registered yet —
                // which is exactly what the backoff is for.
                let mut delay = Duration::from_millis(500);
                loop {
                    tokio::time::sleep(delay).await;
                    let Some((_, min_size)) = manager.truncate_dirty_state(&gate) else {
                        return;
                    };
                    if truncate_fanout(&registry, &clients, &export, &key, &placement, min_size)
                        .await
                    {
                        manager.clear_truncate_dirty_if(&gate, min_size);
                        info!(
                            "✂️ resumed truncate for '{}' (set_len {}) confirmed on all pinned DSes",
                            key, min_size,
                        );
                        return;
                    }
                    delay = (delay * 2).min(Duration::from_secs(10));
                }
            });
        }
        parked.len()
    }

    /// DS-relative path of a legacy (path-keyed) pin's stripe file.
    fn legacy_stripe_rel_path(&self, file_key: &str) -> String {
        let export_rel = self.export_fs_path.trim_start_matches('/');
        format!("{}/{}", export_rel, file_key)
    }

    /// Bounded-DELAY escalation for MDS-fallback I/O on a pinned file
    /// (see `FallbackIoDisposition`). Policy:
    /// - not pinned → Serve (the MDS holds the real bytes);
    /// - every pinned DS Active/Degraded → FailFast: a fallback RPC
    ///   arriving while the fleet is healthy means the CLIENT is stuck
    ///   in its MDS-fallback trap, and only a fatal error springs it;
    /// - a pinned DS is down (Offline or never registered with this
    ///   MDS incarnation) → Delay while the longest such outage is
    ///   under the ceiling, FailFast after.
    fn fallback_io_disposition_impl(&self, file_key: &str) -> FallbackIoDisposition {
        self.fallback_io_disposition_bounded(file_key, fallback_delay_ceiling())
    }

    /// Ceiling-parameterized core of the policy (tests pass explicit
    /// ceilings; the env-derived one is process-wide via OnceLock).
    fn fallback_io_disposition_bounded(
        &self,
        file_key: &str,
        ceiling: Duration,
    ) -> FallbackIoDisposition {
        self.fallback_io_disposition_core(file_key, ceiling, fallback_proxy_enabled())
    }

    /// Fully-parameterized core: tests pass `proxy_enabled` explicitly
    /// because the env-derived value is process-wide via OnceLock and
    /// cannot be flipped per-test.
    fn fallback_io_disposition_core(
        &self,
        file_key: &str,
        ceiling: Duration,
        proxy_enabled: bool,
    ) -> FallbackIoDisposition {
        // F67: a map miss must consult the stub binding before deciding
        // this file is MDS-native — a records-lost MDS still recognizes
        // striped files by their xattr.
        let Some(placement) = self.layout_manager.placement_or_recovered(file_key) else {
            // No binding anywhere. Dense stub (blocks > 0) = an
            // MDS-native file whose data IS the stub: Serve, the
            // pre-F67 behavior. Sparse-with-size = either an all-holes
            // native file (serving zeros would be correct) or a striped
            // file whose binding was destroyed (serving zeros is silent
            // corruption). Indistinguishable — so fail LOUD on the
            // ambiguity rather than quiet on the corruption. (F67; an
            // all-sparse native file hitting this is rare, and EIO is
            // recoverable where zeros-for-data is not.)
            if let Some(meta) = self.layout_manager.stub_meta(file_key) {
                if meta.len > 0 && meta.blocks == 0 {
                    tracing::error!(
                        "🛑 F67: MDS fallback I/O for '{}' — {} bytes claimed, zero \
                         blocks allocated, no placement binding. Refusing rather than \
                         serving zeros.",
                        file_key, meta.len
                    );
                    return FallbackIoDisposition::FailFast;
                }
            }
            return FallbackIoDisposition::Serve;
        };
        // Truncate-dirty overrides the healthy-fleet trap check: the
        // client is (correctly) being refused layouts right now, so its
        // MDS-fallback I/O is expected, not a trap symptom. Park it
        // while the confirmation retry runs; ceiling still applies so a
        // permanently unreachable DS can't livelock the client.
        // `truncate_dirty_age`, NOT `truncate_dirty_since`: the age has
        // to span MDS incarnations. Measuring from a process-local
        // Instant re-armed this ceiling at every restart, so a client on
        // the fallback path during a long park could be DELAYed without
        // bound as long as the MDS bounced periodically — the exact
        // livelock the ceiling exists to prevent.
        let gate = truncate_gate_key(&placement, file_key);
        if let Some(age) = self.layout_manager.truncate_dirty_age(&gate) {
            return if age < ceiling {
                FallbackIoDisposition::Delay
            } else {
                FallbackIoDisposition::FailFast
            };
        }
        let now = Instant::now();
        // Longest current outage among the file's pinned DSes — and,
        // for F66, whether the whole pinned set is proxy-reachable
        // (registered, not Offline, DsControl listener advertised, and
        // a v2 pin — legacy rotation is FH-derived and unproxyable).
        let mut worst_outage: Option<Duration> = None;
        let mut proxy_ready = placement.file_id != 0;
        for device_id in &placement.device_ids {
            let outage = match self.device_registry.get(device_id) {
                // Degraded still serves I/O — not an outage.
                Some(d) if d.status != DeviceStatus::Offline => {
                    if d.control_endpoint.is_none() {
                        // Healthy but unproxyable: without a DsControl
                        // listener the Proxy arm would Delay-loop
                        // forever on a config gap. Fall back to the
                        // pre-F66 answer for this file.
                        proxy_ready = false;
                    }
                    continue;
                }
                // (checked before the loop: legacy pins are never
                // proxy_ready — their rotation is FH-derived)
                // Offline: down since its last heartbeat.
                Some(d) => now.saturating_duration_since(d.last_heartbeat),
                // Unknown to this MDS incarnation: anchor at boot.
                None => now.saturating_duration_since(self.boot_instant),
            };
            worst_outage = Some(worst_outage.map_or(outage, |w| w.max(outage)));
        }
        match worst_outage {
            // F66: a healthy fleet no longer means FailFast — it means
            // the MDS can apply the I/O to the stripes itself. The
            // client this arm used to "spring" with NFS4ERR_IO was, in
            // the fsx repro, a HEALTHY client whose straggler write
            // (queued during the truncate-time no-layout window) got
            // EIO surfaced to msync. FailFast remains for proxy-off
            // and unproxyable configurations.
            None if proxy_ready && proxy_enabled => FallbackIoDisposition::Proxy,
            None => FallbackIoDisposition::FailFast,
            Some(outage) if outage < ceiling => FallbackIoDisposition::Delay,
            Some(_) => FallbackIoDisposition::FailFast,
        }
    }

    /// Handle LAYOUTGET operation (opcode 50)
    /// 
    /// Returns layout information telling the client which data servers
    /// to use for I/O on a specific byte range.
    pub fn layoutget(
        &self,
        args: LayoutGetArgs,
    ) -> Result<LayoutGetResult, LayoutGetError> {
        debug!("🔥🔥🔥 PnfsOperationHandler::layoutget() CALLED 🔥🔥🔥");
        debug!(
            "📥 LAYOUTGET: offset={}, length={}, iomode={:?}, layout_type={:?}",
            args.offset, args.length, args.iomode, args.layout_type
        );

        // Truncate-dirty gate: while a size change is unconfirmed on
        // any pinned DS, a fresh layout would let the client read
        // stale stripe bytes beyond the new EOF. TRYLATER regardless
        // of how long it has been dirty — layouts must NEVER expose
        // stale bytes; the fallback path's ceiling keeps clients from
        // parking forever.
        if let Some(placement) = self.layout_manager.placement_for(&args.file_key) {
            let gate = truncate_gate_key(&placement, &args.file_key);
            if self.layout_manager.truncate_dirty_since(&gate).is_some() {
                warn!(
                    "⏳ LAYOUTGET for truncate-dirty file '{}' → TRYLATER (stripe truncation unconfirmed)",
                    args.file_key
                );
                return Err(LayoutGetError::TryLater);
            }
        }

        // Check available devices
        let active_devices = self.device_registry.count_by_status(
            crate::pnfs::mds::device::DeviceStatus::Active
        );
        debug!("   Available data servers: {}", active_devices);

        // Validate layout type (support FILE and FFLv4)
        match args.layout_type {
            LayoutType::NfsV4_1Files | LayoutType::FlexFiles => {
                // Supported
            }
            _ => {
                warn!("❌ Unsupported layout type: {:?}", args.layout_type);
                return Err(LayoutGetError::UnknownLayoutType);
            }
        }

        // Generate layout (grants go through the file's pinned
        // placement; a pinned-but-missing DS is a refusal, not a
        // re-map).
        let layout = self.layout_manager
            .generate_layout(
                args.owner,
                args.filehandle.clone(),
                &args.file_key,
                args.offset,
                args.length,
                args.iomode,
            )
            .map_err(|e| {
                // A grant that raced an arming truncate is TRYLATER, not
                // "unavailable": the file is fine, the client just has to
                // ask again once the stripes are cut. Same answer the
                // up-front gate check gives, so a racing client and a
                // late client are indistinguishable to the client.
                if e == crate::pnfs::mds::layout::GRANT_RACED_TRUNCATE {
                    LayoutGetError::TryLater
                } else {
                    warn!("❌ Layout generation failed: {}", e);
                    LayoutGetError::LayoutUnavailable
                }
            })?;

        // The grant above pinned (or reused) the placement; surface
        // its stripe unit + composite deviceid so the encoder
        // advertises exactly the pinned group.
        let placement = self
            .layout_manager
            .placement_for(&args.file_key)
            .ok_or(LayoutGetError::LayoutUnavailable)?;
        let device_id_bin =
            crate::pnfs::mds::layout::composite_device_id(&placement.device_ids);

        debug!("✅ LAYOUTGET successful: {} segments returned", layout.segments.len());

        Ok(LayoutGetResult {
            return_on_close: layout.return_on_close,
            stateid: layout.stateid,
            layouts: vec![Layout {
                offset: args.offset,
                length: args.length,
                iomode: args.iomode,
                layout_type: args.layout_type,
                segments: layout.segments,
                stripe_unit: placement.stripe_size,
                device_id_bin,
                file_id: placement.file_id,
            }],
        })
    }

    /// Handle GETDEVICEINFO operation (opcode 47)
    /// 
    /// Returns network addressing information for a specific data server
    /// device ID.
    pub fn getdeviceinfo(
        &self,
        args: GetDeviceInfoArgs,
    ) -> Result<GetDeviceInfoResult, GetDeviceInfoError> {
        debug!(
            "🔥 GETDEVICEINFO: device_id={:02x?}, layout_type={:?}",
            &args.device_id[0..8],
            args.layout_type
        );

        // Validate layout type
        match args.layout_type {
            LayoutType::NfsV4_1Files | LayoutType::FlexFiles => {
                // Supported
            }
            _ => {
                warn!("❌ Unsupported layout type: {:?}", args.layout_type);
                return Err(GetDeviceInfoError::UnknownLayoutType);
            }
        }

        // Try to look up device as single DS
        let device_addr = if let Some(device_info) = self.device_registry.get_by_binary_id(&args.device_id) {
            // Single DS device found
            debug!("✅ Found single device: id={}, primary_endpoint={}", 
                  device_info.device_id, device_info.primary_endpoint);

            DeviceAddr4 {
                netid: "tcp".to_string(),
                addr: device_info.primary_endpoint.clone(),
                multipath: device_info.endpoints.clone(),
            }
        } else if let Some(group) = self.layout_manager.stripe_group_devices(&args.device_id) {
            // Composite (striped) deviceid: resolve the placement's
            // ordered device list — the ORDER here is the stripe map
            // clients apply, so it must come from the pinned group,
            // never from the registry's current membership/iteration
            // order. Endpoints stay live (a re-registered DS serves
            // its new address); a missing group member is NoEnt, not
            // a silently shuffled stripe pattern.
            debug!(
                "🔧 Composite stripe deviceid: {} pinned DSes {:?}",
                group.len(),
                group
            );

            let mut endpoints = Vec::with_capacity(group.len());
            for id in &group {
                match self.device_registry.get(id) {
                    Some(d) => endpoints.push(d.primary_endpoint.clone()),
                    None => {
                        warn!(
                            "❌ Stripe-group DS '{}' not registered — refusing GETDEVICEINFO",
                            id
                        );
                        return Err(GetDeviceInfoError::NoEnt);
                    }
                }
            }

            DeviceAddr4 {
                netid: "tcp".to_string(),
                addr: endpoints[0].clone(),
                multipath: endpoints[1..].to_vec(),
            }
        } else {
            warn!(
                "❌ Unknown deviceid {:02x?} — no registered DS and no stripe group",
                &args.device_id[0..8]
            );
            return Err(GetDeviceInfoError::NoEnt);
        };

        debug!("📤 Returning device address with {} total DSes", 
              1 + device_addr.multipath.len());

        Ok(GetDeviceInfoResult {
            device_addr,
            notification: Vec::new(),
        })
    }

    /// Handle LAYOUTRETURN operation (opcode 51)
    ///
    /// Client returns a layout to the server, indicating it no longer
    /// needs it.
    pub fn layoutreturn(
        &self,
        args: LayoutReturnArgs,
    ) -> Result<LayoutReturnResult, LayoutReturnError> {
        debug!(
            "LAYOUTRETURN: layout_type={:?}, iomode={:?}, return_type={:?}",
            args.layout_type, args.iomode, args.return_type
        );

        // Validate layout type (support both FILE and FlexFiles)
        match args.layout_type {
            LayoutType::NfsV4_1Files | LayoutType::FlexFiles => {
                // Supported
            }
            _ => {
                warn!("Unsupported layout type: {:?}", args.layout_type);
                return Err(LayoutReturnError::UnknownLayoutType);
            }
        }

        match args.return_type {
            LayoutReturnType::File { stateid, layout_body, .. } => {
                // Process FFLv4 layout return body if present
                if args.layout_type == LayoutType::FlexFiles && !layout_body.is_empty() {
                    let body_bytes = bytes::Bytes::from(layout_body);
                    self.process_fflv4_layout_return(&body_bytes, &stateid)?;
                }

                // Return specific layout
                self.layout_manager
                    .return_layout(&stateid)
                    .map_err(|e| {
                        warn!("Layout return failed: {}", e);
                        LayoutReturnError::BadStateId
                    })?;
            }
            LayoutReturnType::Fsid => {
                // Drop every layout this client holds in `fsid`. The
                // by-client/by-fsid index lives on `LayoutOwner` so the
                // manager filters internally; we just hand it the keys.
                let dropped = self.layout_manager
                    .return_fsid_for_client(args.client_id, args.fsid);
                debug!(
                    "LAYOUTRETURN FSID: released {} layout(s) for client_id={} fsid={}",
                    dropped.len(), args.client_id, args.fsid,
                );
            }
            LayoutReturnType::All => {
                // Linux issues this during unmount. Drop every layout
                // owned by this client across all filesystems.
                let dropped = self.layout_manager
                    .return_all_for_client(args.client_id);
                debug!(
                    "LAYOUTRETURN ALL: released {} layout(s) for client_id={}",
                    dropped.len(), args.client_id,
                );
            }
        }

        Ok(LayoutReturnResult {
            new_stateid: None,
        })
    }

    /// Process FFLv4 layout return body (errors and statistics)
    fn process_fflv4_layout_return(
        &self,
        layout_body: &bytes::Bytes,
        stateid: &[u8; 16],
    ) -> Result<(), LayoutReturnError> {
        use crate::nfs::xdr::XdrDecoder;
        use crate::pnfs::protocol::FfLayoutReturn4;

        let mut decoder = XdrDecoder::new(layout_body.clone());
        let ff_return = FfLayoutReturn4::decode(&mut decoder)
            .map_err(|e| {
                warn!("Failed to decode FFLv4 layout return: {}", e);
                LayoutReturnError::Inval
            })?;

        // Process error reports
        if !ff_return.ioerr_report.is_empty() {
            debug!(
                "📋 LAYOUTRETURN received {} error reports for layout {:?}",
                ff_return.ioerr_report.len(),
                &stateid[0..4]
            );

            for (i, err_report) in ff_return.ioerr_report.iter().enumerate() {
                debug!(
                    "   Error report {}: offset={}, length={}, {} device errors",
                    i, err_report.offset, err_report.length, err_report.errors.len()
                );

                for (j, dev_err) in err_report.errors.iter().enumerate() {
                    warn!(
                        "      Device error {}: device_id={:02x?}, status=0x{:x}, opnum={}",
                        j,
                        &dev_err.device_id[0..4],
                        dev_err.status,
                        dev_err.opnum
                    );

                    // Mark device as degraded if errors are persistent
                    // TODO: Implement error threshold and device health tracking
                    if dev_err.status != 0 {
                        warn!("      ⚠️ Device {:02x?} experienced I/O error - may need recovery",
                              &dev_err.device_id[0..4]);
                    }
                }
            }
        }

        // Process statistics reports
        if !ff_return.iostats_report.is_empty() {
            debug!(
                "📊 LAYOUTRETURN received {} statistics reports for layout {:?}",
                ff_return.iostats_report.len(),
                &stateid[0..4]
            );

            for (i, stats) in ff_return.iostats_report.iter().enumerate() {
                debug!(
                    "   Stats report {}: offset={}, length={}, device={:02x?}",
                    i, stats.offset, stats.length, &stats.device_id[0..4]
                );
                debug!(
                    "      Read: {} bytes, {} ops",
                    stats.read.bytes, stats.read.ops
                );
                debug!(
                    "      Write: {} bytes, {} ops",
                    stats.write.bytes, stats.write.ops
                );

                // TODO: Store statistics for performance monitoring and optimization
                // This data can be used to:
                // - Identify hot files/ranges
                // - Optimize layout policies
                // - Detect performance bottlenecks
                // - Trigger data migration
            }
        }

        Ok(())
    }

    /// Handle LAYOUTCOMMIT operation (opcode 52)
    ///
    /// Client commits changes made through a layout (e.g., updates file size
    /// after writes to data servers).
    ///
    /// Per RFC 8435 Section 7, the MDS must ensure data stability before
    /// processing LAYOUTCOMMIT and updating metadata.
    pub fn layoutcommit(
        &self,
        args: LayoutCommitArgs,
    ) -> Result<LayoutCommitResult, LayoutCommitError> {
        debug!(
            "📝 LAYOUTCOMMIT: offset={}, length={}, stateid={:?}",
            args.offset,
            args.length,
            &args.stateid[0..4]
        );

        // Verify layout exists
        let layout = self.layout_manager
            .get_layout(&args.stateid)
            .ok_or_else(|| {
                warn!("Layout not found for commit: {:?}", &args.stateid[0..4]);
                LayoutCommitError::BadStateId
            })?;

        // Extract file information from layout
        debug!(
            "   Layout has {} segments for filehandle length={}",
            layout.segments.len(),
            layout.filehandle.len()
        );

        // Update file metadata if new offset is provided
        let new_size = if let Some(new_offset) = args.new_offset {
            debug!("   Updating file size to {} bytes", new_offset);

            // Try to update file size via filehandle
            if let Err(e) = self.update_file_size(&layout.filehandle, new_offset) {
                warn!("   Failed to update file size: {}", e);
                // Don't fail the operation - the metadata update is best-effort
            }

            Some(new_offset)
        } else {
            debug!("   No size update requested");
            None
        };

        // Update modification time
        let new_time = if args.new_time.is_some() {
            args.new_time
        } else {
            // Use current time if not specified
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos() as u64);

            if let Some(time) = now {
                debug!("   Setting mtime to current time: {}", time);
                if let Err(e) = self.update_file_mtime(&layout.filehandle, time) {
                    warn!("   Failed to update mtime: {}", e);
                }
            }

            now
        };

        debug!("   ✅ LAYOUTCOMMIT completed successfully");

        Ok(LayoutCommitResult {
            new_size,
            new_time,
        })
    }

    /// Update file size based on filehandle
    fn update_file_size(&self, filehandle: &[u8], new_size: u64) -> Result<(), String> {
        use std::fs;
        
        use crate::nfs::v4::filehandle_pnfs;

        // Parse filehandle to get file path
        let path = if filehandle.len() >= 21 && filehandle[0] == 2 {
            // pNFS filehandle - extract file_id
            let fh = crate::nfs::v4::protocol::Nfs4FileHandle {
                data: filehandle.to_vec(),
            };

            match filehandle_pnfs::parse_pnfs_filehandle(&fh) {
                Ok((_, file_id, _stripe_index)) => {
                    // For MDS, we need to map file_id back to original file
                    // Since we don't have a persistent mapping yet, use a simple approach
                    // TODO: Implement persistent file_id -> path mapping
                    let base_path = std::path::Path::new("/data");
                    base_path.join(format!("{:016x}", file_id))
                }
                Err(e) => {
                    return Err(format!("Failed to parse pNFS filehandle: {}", e));
                }
            }
        } else {
            // Traditional filehandle - we can't easily extract path
            // TODO: Implement filehandle -> path mapping
            return Err("Traditional filehandle path resolution not implemented".to_string());
        };

        // Truncate or extend file to new size
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(file) => {
                if let Err(e) = file.set_len(new_size) {
                    return Err(format!("Failed to set file size: {}", e));
                }
                Ok(())
            }
            Err(e) => {
                // File might not exist on MDS if it's on DS
                Err(format!("File not found on MDS: {}", e))
            }
        }
    }

    /// Update file modification time
    fn update_file_mtime(&self, filehandle: &[u8], mtime_nanos: u64) -> Result<(), String> {
        
        use filetime::{FileTime, set_file_mtime};
        use crate::nfs::v4::filehandle_pnfs;

        // Parse filehandle to get file path (same logic as update_file_size)
        let path = if filehandle.len() >= 21 && filehandle[0] == 2 {
            let fh = crate::nfs::v4::protocol::Nfs4FileHandle {
                data: filehandle.to_vec(),
            };

            match filehandle_pnfs::parse_pnfs_filehandle(&fh) {
                Ok((_, file_id, _)) => {
                    let base_path = std::path::Path::new("/data");
                    base_path.join(format!("{:016x}", file_id))
                }
                Err(e) => {
                    return Err(format!("Failed to parse pNFS filehandle: {}", e));
                }
            }
        } else {
            return Err("Traditional filehandle path resolution not implemented".to_string());
        };

        // Convert nanos to seconds for filetime
        let secs = (mtime_nanos / 1_000_000_000) as i64;
        let nsecs = (mtime_nanos % 1_000_000_000) as u32;
        let mtime = FileTime::from_unix_time(secs, nsecs);

        set_file_mtime(&path, mtime)
            .map_err(|e| format!("Failed to set mtime: {}", e))
    }

    /// Handle GETDEVICELIST operation (opcode 48)
    /// 
    /// Returns a list of all available device IDs.
    pub fn getdevicelist(
        &self,
        args: GetDeviceListArgs,
    ) -> Result<GetDeviceListResult, GetDeviceListError> {
        debug!(
            "GETDEVICELIST: layout_type={:?}, maxdevices={}",
            args.layout_type, args.maxdevices
        );

        // Validate layout type
        if args.layout_type != LayoutType::NfsV4_1Files {
            warn!("Unsupported layout type: {:?}", args.layout_type);
            return Err(GetDeviceListError::UnknownLayoutType);
        }

        // Get all active devices
        let devices = self.device_registry.list_active();
        let device_ids: Vec<DeviceId> = devices
            .iter()
            .take(args.maxdevices as usize)
            .map(|d| d.binary_device_id)
            .collect();

        Ok(GetDeviceListResult {
            cookie: 0,
            cookieverf: [0u8; 8],
            device_ids,
            eof: true,
        })
    }
}

// ============================================================================
// Operation Arguments and Results
// ============================================================================

/// LAYOUTGET arguments (RFC 8881 Section 18.43.1)
#[derive(Debug, Clone)]
pub struct LayoutGetArgs {
    pub signal_layout_avail: bool,
    pub layout_type: LayoutType,
    pub iomode: IoMode,
    pub offset: u64,
    pub length: u64,
    pub minlength: u64,
    pub stateid: [u8; 16],
    pub maxcount: u32,
    pub filehandle: Vec<u8>,
    /// Export-relative path of the file (resolved from the CFH by the
    /// dispatcher). Keys the pinned per-file placement — the same
    /// identity the DSes use for path-nested local storage.
    pub file_key: String,
    /// Identity of the issuing client / session / fsid (set by the
    /// COMPOUND dispatcher from `CompoundContext`). Tracked on the
    /// resulting layout so CB_LAYOUTRECALL can find its session and
    /// LAYOUTRETURN with `return_type=ALL`/`FSID` can filter by client.
    pub owner: LayoutOwner,
}

/// LAYOUTGET result (RFC 8881 Section 18.43.2)
#[derive(Debug, Clone)]
pub struct LayoutGetResult {
    pub return_on_close: bool,
    pub stateid: [u8; 16],
    pub layouts: Vec<Layout>,
}

/// Layout structure
#[derive(Debug, Clone)]
pub struct Layout {
    pub offset: u64,
    pub length: u64,
    pub iomode: IoMode,
    pub layout_type: LayoutType,
    pub segments: Vec<LayoutSegment>,
    /// Stripe unit (`nfl_util`) from the file's pinned placement —
    /// NOT the live config, which may have changed since the file was
    /// first striped.
    pub stripe_unit: u64,
    /// The composite deviceid advertising this file's stripe group.
    /// Derived from the placement's ordered device list; the encoder
    /// must use this verbatim so GETDEVICEINFO resolves to the same
    /// group.
    pub device_id_bin: [u8; 16],
    /// The placement's immutable file identity. Nonzero ⇒ the encoder
    /// emits per-DS v2 file-ID filehandles in nfl_fh_list (DS storage
    /// keyed by identity, rename-safe); 0 ⇒ legacy empty fh list (DSes
    /// rebase the MDS path filehandle).
    pub file_id: u64,
}

/// LAYOUTGET errors
#[derive(Debug, Clone, Copy)]
pub enum LayoutGetError {
    LayoutUnavailable,
    UnknownLayoutType,
    BadStateId,
    Io,
    /// Transient refusal (NFS4ERR_LAYOUTTRYLATER): the file is
    /// truncate-dirty — its new size reached the MDS stub but not yet
    /// every pinned DS's stripe file, so a fresh layout would expose
    /// stale bytes beyond the new EOF.
    TryLater,
}

/// GETDEVICEINFO arguments (RFC 8881 Section 18.40.1)
#[derive(Debug, Clone)]
pub struct GetDeviceInfoArgs {
    pub device_id: DeviceId,
    pub layout_type: LayoutType,
    pub maxcount: u32,
    pub notify_types: Vec<u32>,
}

/// GETDEVICEINFO result (RFC 8881 Section 18.40.2)
#[derive(Debug, Clone)]
pub struct GetDeviceInfoResult {
    pub device_addr: DeviceAddr4,
    pub notification: Vec<u32>,
}

/// Device address structure (RFC 8881 Section 3.3.14)
#[derive(Debug, Clone)]
pub struct DeviceAddr4 {
    pub netid: String,
    pub addr: String,
    pub multipath: Vec<String>,
}

/// GETDEVICEINFO errors
#[derive(Debug, Clone, Copy)]
pub enum GetDeviceInfoError {
    NoEnt,
    UnknownLayoutType,
    TooSmall,
}

/// LAYOUTRETURN arguments (RFC 8881 Section 18.44.1)
///
/// `client_id` and `fsid` are *not* on the wire — they're resolved by the
/// dispatcher from the SEQUENCE-bound session and the CFH respectively.
/// We need them here because FSID/ALL filter `LayoutManager.by_owner` and
/// `LayoutOwner.fsid`.
#[derive(Debug, Clone)]
pub struct LayoutReturnArgs {
    pub reclaim: bool,
    pub layout_type: LayoutType,
    pub iomode: IoMode,
    pub return_type: LayoutReturnType,
    pub client_id: u64,
    pub fsid: u64,
}

/// Layout return type
#[derive(Debug, Clone)]
pub enum LayoutReturnType {
    File {
        offset: u64,
        length: u64,
        stateid: [u8; 16],
        layout_body: Vec<u8>,
    },
    Fsid,
    All,
}

/// LAYOUTRETURN result (RFC 8881 Section 18.44.2)
#[derive(Debug, Clone)]
pub struct LayoutReturnResult {
    pub new_stateid: Option<[u8; 16]>,
}

/// LAYOUTRETURN errors
#[derive(Debug, Clone, Copy)]
pub enum LayoutReturnError {
    BadStateId,
    UnknownLayoutType,
    Inval,
}

/// LAYOUTCOMMIT arguments (RFC 8881 Section 18.42.1)
#[derive(Debug, Clone)]
pub struct LayoutCommitArgs {
    pub offset: u64,
    pub length: u64,
    pub reclaim: bool,
    pub stateid: [u8; 16],
    pub new_offset: Option<u64>,
    pub new_time: Option<u64>,
    pub layout_body: Vec<u8>,
}

/// LAYOUTCOMMIT result (RFC 8881 Section 18.42.2)
#[derive(Debug, Clone)]
pub struct LayoutCommitResult {
    pub new_size: Option<u64>,
    pub new_time: Option<u64>,
}

/// LAYOUTCOMMIT errors
#[derive(Debug, Clone, Copy)]
pub enum LayoutCommitError {
    BadStateId,
    Inval,
    Io,
}

/// GETDEVICELIST arguments (RFC 8881 Section 18.41.1)
#[derive(Debug, Clone)]
pub struct GetDeviceListArgs {
    pub layout_type: LayoutType,
    pub maxdevices: u32,
    pub cookie: u64,
    pub cookieverf: [u8; 8],
}

/// GETDEVICELIST result (RFC 8881 Section 18.41.2)
#[derive(Debug, Clone)]
pub struct GetDeviceListResult {
    pub cookie: u64,
    pub cookieverf: [u8; 8],
    pub device_ids: Vec<DeviceId>,
    pub eof: bool,
}

/// GETDEVICELIST errors
#[derive(Debug, Clone, Copy)]
pub enum GetDeviceListError {
    UnknownLayoutType,
    TooSmall,
}



/// One TruncateStripeFile RPC to one DS, through the shared client
/// cache. Transport failures evict the cached client so the next
/// attempt re-dials.
async fn ds_truncate_one(
    clients: &DashMap<String, crate::pnfs::grpc::AuthedDsControlClient>,
    endpoint: &str,
    device_id: &str,
    rel_path: &str,
    new_length: u64,
) -> Result<(), String> {
    const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
    const RPC_TIMEOUT: Duration = Duration::from_secs(3);

    let mut client = match clients.get(endpoint).map(|c| c.clone()) {
        Some(c) => c,
        None => {
            let uri = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
                endpoint.to_string()
            } else {
                format!("http://{}", endpoint)
            };
            let ep = tonic::transport::Channel::from_shared(uri)
                .map_err(|e| format!("bad DS control endpoint '{}': {}", endpoint, e))?;
            let channel = tokio::time::timeout(DIAL_TIMEOUT, ep.connect())
                .await
                .map_err(|_| format!("dial {} timed out", endpoint))?
                .map_err(|e| format!("dial {}: {}", endpoint, e))?;
            let c = crate::pnfs::grpc::authed_ds_control_client(channel);
            clients.insert(endpoint.to_string(), c.clone());
            c
        }
    };

    let req = crate::pnfs::grpc::TruncateStripeFileRequest {
        device_id: device_id.to_string(),
        rel_path: rel_path.to_string(),
        new_length,
    };
    match tokio::time::timeout(RPC_TIMEOUT, client.truncate_stripe_file(tonic::Request::new(req)))
        .await
    {
        Ok(Ok(resp)) => {
            let r = resp.into_inner();
            if r.ok {
                Ok(())
            } else {
                // The DS answered and refused — not a channel problem.
                Err(format!("DS {} refused: {}", device_id, r.message))
            }
        }
        Ok(Err(status)) => {
            clients.remove(endpoint);
            Err(format!("DS {} rpc failed: {}", device_id, status))
        }
        Err(_) => {
            clients.remove(endpoint);
            Err(format!("DS {} rpc timed out", device_id))
        }
    }
}

/// Push `new_size` to every pinned DS's stripe file for one file.
/// Returns true only when EVERY DS confirmed — anything less leaves
/// the truncate-dirty gate in place.
async fn truncate_fanout(
    device_registry: &DeviceRegistry,
    clients: &DashMap<String, crate::pnfs::grpc::AuthedDsControlClient>,
    export_fs_path: &str,
    file_key: &str,
    placement: &FilePlacement,
    new_size: u64,
) -> bool {
    let legacy_rel = format!("{}/{}", export_fs_path.trim_start_matches('/'), file_key);
    let mut all_ok = true;
    for (slot, device_id) in placement.device_ids.iter().enumerate() {
        let rel = if placement.file_id != 0 {
            placement.stripe_rel_path(slot)
        } else {
            legacy_rel.clone()
        };
        let Some(info) = device_registry.get(device_id) else {
            warn!(
                "✂️ truncate('{}'): DS {} not registered with this MDS incarnation",
                file_key, device_id
            );
            all_ok = false;
            continue;
        };
        let Some(endpoint) = info.control_endpoint else {
            warn!(
                "✂️ truncate('{}'): DS {} advertises no DsControl listener (set bind.controlPort)",
                file_key, device_id
            );
            all_ok = false;
            continue;
        };
        match ds_truncate_one(clients, &endpoint, device_id, &rel, new_size).await {
            Ok(()) => debug!("✂️ {}: {} set_len({}) confirmed", device_id, rel, new_size),
            Err(e) => {
                warn!("✂️ truncate('{}') on {}: {}", file_key, device_id, e);
                all_ok = false;
            }
        }
    }
    all_ok
}

// ── F66: the MDS fallback-I/O proxy ─────────────────────────────────────
// docs/plans/mds-fallback-proxy-plan.md. A layout-less client's I/O
// through the MDS is applied to the stripes over the same DsControl
// channel truncate_fanout proved, instead of being refused with
// NFS4ERR_IO — which fsx showed surfacing as msync EIO on a HEALTHY
// client (the straggler write queued during the truncate-time no-layout
// window; LAYOUTGET succeeded 200 µs before the refused WRITE).

/// Kill switch: FLINT_MDS_FALLBACK_PROXY, default ON. OFF restores the
/// pre-F66 FailFast verbatim — the bug, but an operator diagnosing
/// corruption must be able to remove the proxy's write channel from the
/// suspect list in one restart. Read once; a flip requires the restart
/// it implies anyway.
fn fallback_proxy_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("FLINT_MDS_FALLBACK_PROXY").as_deref(),
            Ok("0") | Ok("off") | Ok("false")
        )
    })
}

/// Dial-once client cache access, the exact pattern `ds_truncate_one`
/// proved: transport failures evict so the next attempt re-dials.
async fn dial_control_client(
    clients: &DashMap<String, crate::pnfs::grpc::AuthedDsControlClient>,
    endpoint: &str,
) -> Result<crate::pnfs::grpc::AuthedDsControlClient, String> {
    const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
    if let Some(c) = clients.get(endpoint).map(|c| c.clone()) {
        return Ok(c);
    }
    let uri = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("http://{}", endpoint)
    };
    let ep = tonic::transport::Channel::from_shared(uri)
        .map_err(|e| format!("bad DS control endpoint '{}': {}", endpoint, e))?;
    let channel = tokio::time::timeout(DIAL_TIMEOUT, ep.connect())
        .await
        .map_err(|_| format!("dial {} timed out", endpoint))?
        .map_err(|e| format!("dial {}: {}", endpoint, e))?;
    let c = crate::pnfs::grpc::authed_ds_control_client(channel);
    clients.insert(endpoint.to_string(), c.clone());
    Ok(c)
}

/// Zero-fill-and-truncate assembly for a proxied READ: pure, so the
/// hole semantics are unit-testable without a DS. `chunks` are
/// `(file_offset, bytes_the_DS_returned)` — SHORT and EMPTY entries are
/// holes the DS reported raw (it does not know the file size; only the
/// stub does, and that authority is exercised HERE and nowhere else).
fn assemble_fallback_read(
    chunks: &[(u64, Vec<u8>)],
    offset: u64,
    count: u32,
    stub_size: u64,
) -> (Vec<u8>, bool) {
    let want = (count as u64).min(stub_size.saturating_sub(offset)) as usize;
    let mut out = vec![0u8; want];
    for (co, data) in chunks {
        if data.is_empty() {
            continue; // hole — stays zero
        }
        let start = (co - offset) as usize;
        if start >= want {
            continue; // chunk entirely past EOF — stub size rules
        }
        let n = data.len().min(want - start);
        out[start..start + n].copy_from_slice(&data[..n]);
    }
    let eof = offset + want as u64 >= stub_size;
    (out, eof)
}

impl PnfsOperationHandler {
    /// Resolve one chunk's (device_id, control endpoint, stripe path).
    fn proxy_target(
        &self,
        placement: &FilePlacement,
        file_key: &str,
        chunk_offset: u64,
    ) -> Result<(String, String, String), String> {
        if placement.file_id == 0 {
            // Legacy pins rotate their stripe pattern by a hash of the
            // FILEHANDLE (see the dispatcher's layout encode), which
            // this path does not carry — an unrotated guess writes the
            // wrong stripe file. The disposition never answers Proxy
            // for legacy pins; this is the belt behind that gate.
            return Err(format!(
                "legacy path-keyed pin '{}' — proxy unsupported (FH-derived rotation)",
                file_key
            ));
        }
        let slot = placement.slot_for_offset(chunk_offset);
        let device_id = placement.device_ids[slot].clone();
        let info = self
            .device_registry
            .get(&device_id)
            .ok_or_else(|| format!("DS {} not registered", device_id))?;
        let endpoint = info
            .control_endpoint
            .ok_or_else(|| format!("DS {} advertises no DsControl listener", device_id))?;
        let rel = if placement.file_id != 0 {
            placement.stripe_rel_path(slot)
        } else {
            self.legacy_stripe_rel_path(file_key)
        };
        Ok((device_id, endpoint, rel))
    }

    pub(crate) async fn proxy_fallback_read_impl(
        &self,
        file_key: &str,
        offset: u64,
        count: u32,
        stub_size: u64,
    ) -> Result<(Vec<u8>, bool), String> {
        const RPC_TIMEOUT: Duration = Duration::from_secs(3);
        let placement = self
            .layout_manager
            .placement_for(file_key)
            .ok_or_else(|| format!("no placement for '{}'", file_key))?;
        // Reads past EOF need no DS at all — the stub decides.
        let effective = (count as u64).min(stub_size.saturating_sub(offset));
        let mut chunks: Vec<(u64, Vec<u8>)> = Vec::with_capacity(2);
        for (co, cl) in placement.split_at_stripe_bounds(offset, effective) {
            let (device_id, endpoint, rel) = self.proxy_target(&placement, file_key, co)?;
            let mut client = dial_control_client(&self.ds_control_clients, &endpoint).await?;
            let req = crate::pnfs::grpc::ReadStripeRequest {
                device_id: device_id.clone(),
                rel_path: rel,
                offset: co,
                count: cl as u32,
            };
            let resp = tokio::time::timeout(
                RPC_TIMEOUT,
                client.read_stripe(tonic::Request::new(req)),
            )
            .await
            .map_err(|_| {
                self.ds_control_clients.remove(&endpoint);
                format!("ReadStripe to {} timed out", device_id)
            })?
            .map_err(|e| {
                self.ds_control_clients.remove(&endpoint);
                format!("ReadStripe to {}: {}", device_id, e)
            })?
            .into_inner();
            if !resp.ok {
                return Err(format!("DS {} refused ReadStripe: {}", device_id, resp.message));
            }
            chunks.push((co, resp.data));
        }
        Ok(assemble_fallback_read(&chunks, offset, count, stub_size))
    }

    pub(crate) async fn proxy_fallback_write_impl(
        &self,
        file_key: &str,
        offset: u64,
        data: bytes::Bytes,
    ) -> Result<(), String> {
        const RPC_TIMEOUT: Duration = Duration::from_secs(5);
        let placement = self
            .layout_manager
            .placement_for(file_key)
            .ok_or_else(|| format!("no placement for '{}'", file_key))?;
        for (co, cl) in placement.split_at_stripe_bounds(offset, data.len() as u64) {
            let (device_id, endpoint, rel) = self.proxy_target(&placement, file_key, co)?;
            let mut client = dial_control_client(&self.ds_control_clients, &endpoint).await?;
            let start = (co - offset) as usize;
            let req = crate::pnfs::grpc::WriteStripeRequest {
                device_id: device_id.clone(),
                rel_path: rel,
                offset: co,
                data: data[start..start + cl as usize].to_vec(),
            };
            let resp = tokio::time::timeout(
                RPC_TIMEOUT,
                client.write_stripe(tonic::Request::new(req)),
            )
            .await
            .map_err(|_| {
                self.ds_control_clients.remove(&endpoint);
                format!("WriteStripe to {} timed out", device_id)
            })?
            .map_err(|e| {
                self.ds_control_clients.remove(&endpoint);
                format!("WriteStripe to {}: {}", device_id, e)
            })?
            .into_inner();
            if !resp.ok {
                return Err(format!("DS {} refused WriteStripe: {}", device_id, resp.message));
            }
            info!(
                "🔁 fallback WRITE proxied: '{}' [{}, +{}) → {} (durable)",
                file_key, co, cl, device_id
            );
        }
        Ok(())
    }
}

// Implement PnfsOperations trait for PnfsOperationHandler
#[tonic::async_trait]
impl crate::pnfs::PnfsOperations for PnfsOperationHandler {
    fn f68a_meter(&self) -> Option<Arc<crate::pnfs::mds::f68a_meter::DataPathMeter>> {
        Some(Arc::clone(&self.f68a))
    }

    fn layoutget(&self, args: LayoutGetArgs) -> Result<LayoutGetResult, LayoutGetError> {
        self.layoutget(args)
    }

    fn getdeviceinfo(&self, args: GetDeviceInfoArgs) -> Result<GetDeviceInfoResult, GetDeviceInfoError> {
        self.getdeviceinfo(args)
    }
    
    fn layoutreturn(&self, args: LayoutReturnArgs) -> Result<(), LayoutReturnError> {
        self.layoutreturn(args).map(|_| ())
    }

    fn is_pnfs_managed(&self, file_key: &str) -> bool {
        self.layout_manager.has_placement(file_key)
    }

    fn fallback_io_disposition(&self, file_key: &str) -> FallbackIoDisposition {
        self.fallback_io_disposition_impl(file_key)
    }

    async fn proxy_fallback_read(
        &self,
        file_key: &str,
        offset: u64,
        count: u32,
        stub_size: u64,
    ) -> Result<(Vec<u8>, bool), String> {
        self.proxy_fallback_read_impl(file_key, offset, count, stub_size).await
    }

    async fn proxy_fallback_write(
        &self,
        file_key: &str,
        offset: u64,
        data: bytes::Bytes,
    ) -> Result<(), String> {
        self.proxy_fallback_write_impl(file_key, offset, data).await
    }

    fn truncate_gate_ceiling(&self, file_key: &str) -> Option<u64> {
        let placement = self.layout_manager.placement_for(file_key)?;
        let gate = truncate_gate_key(&placement, file_key);
        self.layout_manager.truncate_dirty_state(&gate).map(|(_, min)| min)
    }

    fn note_remove(&self, file_key: &str) {
        if let Some(placement) = self.layout_manager.forget_placement(file_key) {
            if placement.file_id != 0 {
                self.layout_manager.enqueue_stripe_cleanup(&placement, file_key);
            } else {
                let rel = self.legacy_stripe_rel_path(file_key);
                self.layout_manager.enqueue_legacy_cleanup(&placement, &rel);
            }
        }
    }

    async fn note_truncate(&self, file_key: &str, new_size: u64) {
        let Some(placement) = self.layout_manager.placement_for(file_key) else {
            // Not striped — the MDS stub IS the file; nothing to push.
            return;
        };
        // Gate before fanning out: from here until every pinned DS
        // confirms, no fresh layout may expose the file.
        let gate = truncate_gate_key(&placement, file_key);
        self.layout_manager.mark_truncate_dirty(&gate, new_size);

        // F65: the gate above stops FRESH layouts, and that is all it
        // can do — a client that already holds one reads its stripes
        // straight from the DSes without the MDS ever being consulted.
        // So recall and revoke the outstanding ones, and do it HERE:
        // before the fanout, for the same reason the mark comes first.
        // A recall issued after the DSes are cut is decoration.
        //
        // This blocks the SETATTR/OPEN compound for up to one CB
        // round-trip per outstanding layout. That is deliberate — the
        // alternative is returning success to the client while its
        // peers can still read the bytes we just promised are gone —
        // and it is bounded by CallbackManager's per-call timeout.
        // The client issuing the truncate is recalled along with the
        // rest: excluding it would rest on the assumption that a
        // client never reads past a size it set itself, which is a
        // claim about client behaviour, not about this server.
        self.recall_layouts_for_truncate(&gate, new_size).await;

        let ok = truncate_fanout(
            &self.device_registry,
            &self.ds_control_clients,
            &self.export_fs_path,
            file_key,
            &placement,
            new_size,
        )
        .await;
        if ok {
            // Lifts the gate unless a DEEPER cut is still unconfirmed
            // (that one's retry task owns the gate then).
            self.layout_manager.clear_truncate_dirty_if(&gate, new_size);
            return;
        }

        warn!(
            "⏳ '{}' parked truncate-dirty — a pinned DS has not confirmed set_len({}); background retry armed",
            file_key, new_size
        );
        let registry = Arc::clone(&self.device_registry);
        let clients = Arc::clone(&self.ds_control_clients);
        let manager = Arc::clone(&self.layout_manager);
        let export = self.export_fs_path.clone();
        let key = file_key.to_string();
        tokio::spawn(async move {
            // Bounded backoff, unbounded duration: a DS that comes back
            // hours later still gets the cut; the gate keeps the file
            // safe (and its I/O eventually FailFast) meanwhile. The
            // placement is captured by value — it is immutable per
            // identity, so a concurrent RENAME can't stale it.
            let mut delay = Duration::from_millis(500);
            loop {
                tokio::time::sleep(delay).await;
                // Re-read the deepest pending size each round; the mark
                // may also have been lifted (file removed, or a deeper
                // concurrent truncate confirmed everywhere).
                let Some((_, min_size)) = manager.truncate_dirty_state(&gate) else {
                    return;
                };
                if truncate_fanout(&registry, &clients, &export, &key, &placement, min_size).await
                {
                    manager.clear_truncate_dirty_if(&gate, min_size);
                    info!(
                        "✂️ deferred stripe truncation for '{}' (set_len {}) confirmed on all pinned DSes",
                        key, min_size
                    );
                    return;
                }
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        });
    }

    fn rename_preserves_data(&self, old_key: &str) -> bool {
        let self_ok = match self.layout_manager.placement_for(old_key) {
            // Legacy path-keyed pin: DS stripes live at the old path;
            // renaming would strand them (fresh readers get nothing).
            Some(p) => p.file_id != 0,
            // Unpinned: plain MDS-local file or a directory.
            None => true,
        };
        // A directory rename moves every child's path too — refuse if
        // any child is a legacy pin (identity-keyed children follow
        // via the note_rename prefix sweep).
        self_ok && !self.layout_manager.has_legacy_placements_under(old_key)
    }

    fn note_rename(&self, old_key: &str, new_key: &str) {
        match self.layout_manager.rename_placement(old_key, new_key) {
            Ok(Some(overwritten)) => {
                // Rename-over: the target's old pin is gone; reclaim
                // its stripes.
                if overwritten.file_id != 0 {
                    self.layout_manager.enqueue_stripe_cleanup(&overwritten, new_key);
                } else {
                    let rel = self.legacy_stripe_rel_path(new_key);
                    self.layout_manager.enqueue_legacy_cleanup(&overwritten, &rel);
                }
            }
            Ok(None) => {}
            Err(e) => {
                // rename_preserves_data() gates the op before the fs
                // rename, so this arm firing means a race or a bug —
                // loud, because the file's data is now stranded.
                warn!("💥 note_rename('{}' → '{}') failed AFTER fs rename: {}", old_key, new_key, e);
            }
        }
        // Directory rename: every child placement's path key moved
        // with it. No-op for file renames.
        let moved = self
            .layout_manager
            .rename_placements_under(old_key, new_key);
        if moved > 0 {
            info!(
                "Directory rename '{}' → '{}': re-keyed {} child placement(s)",
                old_key, new_key, moved
            );
        }
    }
}


#[cfg(test)]
mod fallback_tests {
    use super::*;
    use crate::pnfs::mds::device::DeviceInfo;
    use crate::pnfs::config::LayoutPolicy;

    const CEILING: Duration = Duration::from_secs(90);

    fn ds(id: &str) -> DeviceInfo {
        DeviceInfo::new(id.to_string(), format!("{}:2049", id), vec![])
    }

    fn owner() -> LayoutOwner {
        LayoutOwner { client_id: 1, session_id: [0u8; 16], fsid: 1 }
    }

    /// Registry with `ids` registered + a handler whose layout manager
    /// has `file` pinned across all of them.
    fn pinned_handler(ids: &[&str], file: &str) -> (Arc<DeviceRegistry>, PnfsOperationHandler) {
        let registry = Arc::new(DeviceRegistry::new());
        for id in ids {
            registry.register(ds(id)).unwrap();
        }
        let mgr = Arc::new(LayoutManager::new(
            Arc::clone(&registry),
            LayoutPolicy::Stripe,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        ));
        mgr.generate_layout(owner(), vec![1], file, 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        let handler = PnfsOperationHandler::new(mgr, Arc::clone(&registry), "/data/exports".into());
        (registry, handler)
    }

    #[test]
    fn unpinned_file_is_served() {
        let (_registry, handler) = pinned_handler(&["ds-1"], "pinned.bin");
        assert_eq!(
            handler.fallback_io_disposition_bounded("never-layouted.bin", CEILING),
            FallbackIoDisposition::Serve,
            "files without a placement are MDS-local and must be served"
        );
    }

    #[test]
    fn healthy_but_unproxyable_fleet_fails_fast() {
        // F66: the healthy-fleet arm now PROXIES — but these devices
        // advertise no DsControl listener, and a Proxy answer against
        // an unproxyable fleet would Delay-loop the client forever on a
        // config gap. Pre-F66 behavior (FailFast) is the honest floor.
        let (_registry, handler) = pinned_handler(&["ds-1", "ds-2"], "f.bin");
        assert_eq!(
            handler.fallback_io_disposition_core("f.bin", CEILING, true),
            FallbackIoDisposition::FailFast
        );
    }

    /// F67 disposition tests: handler over a manager with a scripted
    /// stub binding and NO placement for the file.
    fn f67_handler(
        metas: &[(&str, u64, u64)],
    ) -> PnfsOperationHandler {
        use crate::pnfs::mds::stub_binding::{test_support::MemoryStubBinding, StubMeta};
        let registry = Arc::new(DeviceRegistry::new());
        let binding = Arc::new(MemoryStubBinding::default());
        for (key, len, blocks) in metas {
            binding
                .metas
                .lock()
                .unwrap()
                .insert(key.to_string(), StubMeta { len: *len, blocks: *blocks });
        }
        let mgr = Arc::new(LayoutManager::new_with_binding(
            Arc::clone(&registry),
            LayoutPolicy::Stripe,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
            binding as Arc<dyn crate::pnfs::mds::stub_binding::StubBinding>,
        ));
        PnfsOperationHandler::new(mgr, registry, "/data/exports".into())
    }

    #[test]
    fn f67_sparse_bindingless_stub_fails_fast_never_serves_zeros() {
        // Bytes claimed, zero blocks allocated, no binding anywhere:
        // either an all-holes native file (EIO is recoverable) or a
        // striped file whose binding was destroyed (Serve would be
        // silent corruption). Loud beats quiet.
        let handler = f67_handler(&[("orphan.bin", 1 << 30, 0)]);
        assert_eq!(
            handler.fallback_io_disposition_core("orphan.bin", CEILING, true),
            FallbackIoDisposition::FailFast
        );
    }

    #[test]
    fn f67_dense_bindingless_stub_still_serves() {
        // An MDS-native file: its data IS the stub. The pre-F67 Serve
        // answer stays — native files must keep working.
        let handler = f67_handler(&[("native.bin", 4096, 8)]);
        assert_eq!(
            handler.fallback_io_disposition_core("native.bin", CEILING, true),
            FallbackIoDisposition::Serve
        );
    }

    #[test]
    fn f67_absent_stub_serves() {
        // No stub at all (metadata-only ops, races with create):
        // nothing to protect, keep the permissive default.
        let handler = f67_handler(&[]);
        assert_eq!(
            handler.fallback_io_disposition_core("nothing.bin", CEILING, true),
            FallbackIoDisposition::Serve
        );
    }

    /// Registry whose devices DO advertise DsControl listeners — the
    /// proxy-ready shape a real fleet has (registration carries the
    /// control endpoint).
    fn proxyable_handler(ids: &[&str], file: &str) -> (Arc<DeviceRegistry>, PnfsOperationHandler) {
        let registry = Arc::new(DeviceRegistry::new());
        for id in ids {
            let mut d = ds(id);
            d.control_endpoint = Some(format!("{}:21491", id));
            registry.register(d).unwrap();
        }
        let mgr = Arc::new(LayoutManager::new(
            Arc::clone(&registry),
            LayoutPolicy::Stripe,
            8 * 1024 * 1024,
            crate::state_backend::memory_backend(),
        ));
        mgr.generate_layout(owner(), vec![1], file, 0, 16 * 1024 * 1024, IoMode::ReadWrite)
            .unwrap();
        let handler = PnfsOperationHandler::new(mgr, Arc::clone(&registry), "/data/exports".into());
        (registry, handler)
    }

    #[test]
    fn healthy_proxyable_fleet_proxies() {
        // F66's whole point: a healthy fleet answers fallback I/O by
        // APPLYING it, not by refusing it. The client this used to
        // "spring" with NFS4ERR_IO was a healthy one whose straggler
        // write got EIO surfaced to msync (the fsx failure).
        let (_registry, handler) = proxyable_handler(&["ds-1", "ds-2"], "f.bin");
        assert_eq!(
            handler.fallback_io_disposition_core("f.bin", CEILING, true),
            FallbackIoDisposition::Proxy
        );
    }

    #[test]
    fn kill_switch_restores_failfast() {
        let (_registry, handler) = proxyable_handler(&["ds-1", "ds-2"], "f.bin");
        assert_eq!(
            handler.fallback_io_disposition_core("f.bin", CEILING, false),
            FallbackIoDisposition::FailFast,
            "FLINT_MDS_FALLBACK_PROXY=off must restore pre-F66 behavior verbatim"
        );
    }

    #[test]
    fn outage_still_beats_the_proxy() {
        // A pinned DS down ⇒ the proxy CANNOT serve that slot's chunks;
        // the bounded Delay→FailFast ladder owns the file, proxy or no.
        let (registry, handler) = proxyable_handler(&["ds-1", "ds-2"], "f.bin");
        registry.update_status("ds-2", DeviceStatus::Offline).unwrap();
        assert_eq!(
            handler.fallback_io_disposition_core("f.bin", CEILING, true),
            FallbackIoDisposition::Delay
        );
        assert_eq!(
            handler.fallback_io_disposition_core("f.bin", Duration::ZERO, true),
            FallbackIoDisposition::FailFast
        );
    }

    // ── F66 hole resolution: the stub size is the only EOF authority ──

    #[test]
    fn assemble_zero_fills_holes_and_respects_stub_size() {
        // 100-byte file; read [40, 40+80) — only 60 bytes exist.
        // DS returned a 20-byte fragment at 40 and a hole after.
        let (data, eof) = assemble_fallback_read(
            &[(40, vec![7u8; 20])], 40, 80, 100,
        );
        assert_eq!(data.len(), 60, "capped at stub size, not at count");
        assert_eq!(&data[..20], &[7u8; 20][..]);
        assert_eq!(&data[20..], &vec![0u8; 40][..], "hole reads as zeros");
        assert!(eof);
    }

    #[test]
    fn assemble_read_past_eof_is_empty_eof() {
        let (data, eof) = assemble_fallback_read(&[], 200, 50, 100);
        assert!(data.is_empty());
        assert!(eof);
    }

    #[test]
    fn assemble_full_read_inside_file_no_eof() {
        let (data, eof) = assemble_fallback_read(
            &[(0, vec![1u8; 64])], 0, 64, 1000,
        );
        assert_eq!(data, vec![1u8; 64]);
        assert!(!eof, "64 of 1000 bytes is not EOF");
    }

    #[test]
    fn assemble_absent_stripe_is_all_zeros_not_error() {
        // The DS reported the whole chunk as a hole (absent stripe
        // file). The MDS serves zeros up to the stub size — the sparse
        // semantics tar --sparse depends on.
        let (data, eof) = assemble_fallback_read(&[(0, Vec::new())], 0, 100, 100);
        assert_eq!(data, vec![0u8; 100]);
        assert!(eof);
    }

    #[test]
    fn recent_outage_delays_then_ceiling_fails_fast() {
        let (registry, handler) = pinned_handler(&["ds-1", "ds-2"], "f.bin");
        registry.update_status("ds-2", DeviceStatus::Offline).unwrap();
        // Outage just started (last_heartbeat ≈ now) → park the client.
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", CEILING),
            FallbackIoDisposition::Delay
        );
        // Same state past the ceiling (ZERO makes any outage "too long").
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", Duration::ZERO),
            FallbackIoDisposition::FailFast,
            "an outage past the ceiling must fail fast, not hang apps forever"
        );
    }

    /// The ceiling that bounds a fallback client's wait must survive an
    /// MDS restart.
    ///
    /// The gate's age lived in a process-local `Instant`, re-stamped by
    /// `load_placement_records` on every boot. So an MDS that bounced
    /// more often than the ceiling handed the same client `Delay`
    /// forever — the unbounded wait the ceiling exists to prevent, and
    /// invisible because each individual process saw a young gate.
    ///
    /// A restart is not progress. A gate armed before this process
    /// started is as old as it says it is.
    #[test]
    fn a_restart_does_not_re_arm_the_fallback_ceiling() {
        let (_registry, handler) = pinned_handler(&["ds-1", "ds-2"], "f.bin");
        let placement = handler.layout_manager.placement_for("f.bin").unwrap();
        let gate = truncate_gate_key(&placement, "f.bin");

        // Freshly armed in this process: the client waits.
        handler.layout_manager.mark_truncate_dirty(&gate, 0);
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", CEILING),
            FallbackIoDisposition::Delay,
            "a gate armed a moment ago is inside the ceiling",
        );

        // Now the MDS restarts, an hour into the same parked truncate.
        let (_registry2, restarted) = pinned_handler(&["ds-1", "ds-2"], "f.bin");
        let mut rec = restarted
            .layout_manager
            .placement_for("f.bin")
            .unwrap()
            .to_record("f.bin");
        rec.truncate_pending = Some(0);
        rec.truncate_since_unix =
            Some(crate::pnfs::mds::layout::unix_now_for_test() - 3600);
        restarted.layout_manager.load_placement_records(vec![rec]);

        assert_eq!(
            restarted.fallback_io_disposition_bounded("f.bin", CEILING),
            FallbackIoDisposition::FailFast,
            "the gate has been dirty for an hour across the restart — a bounce \
             is not progress, and re-arming the ceiling lets a fallback client \
             be DELAYed without bound",
        );
    }

    #[test]
    fn degraded_device_is_not_an_outage() {
        // Degraded still serves I/O → fleet counts as healthy → the
        // fallback RPC is a trapped client → FailFast.
        let (registry, handler) = pinned_handler(&["ds-1"], "f.bin");
        registry.update_status("ds-1", DeviceStatus::Degraded).unwrap();
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", CEILING),
            FallbackIoDisposition::FailFast
        );
    }

    #[test]
    fn unregistered_device_anchors_outage_at_mds_boot() {
        // A pinned DS unknown to this MDS incarnation (boot grace /
        // blackhole re-register window): outage clock starts at
        // handler boot, so a fresh MDS parks fallbacks (Delay) and
        // escalates only after the ceiling.
        let (registry, handler) = pinned_handler(&["ds-1", "ds-2"], "f.bin");
        registry.unregister("ds-2").unwrap();
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", CEILING),
            FallbackIoDisposition::Delay
        );
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", Duration::ZERO),
            FallbackIoDisposition::FailFast
        );
    }

    /// While a file is truncate-dirty its MDS-fallback I/O parks even
    /// though the fleet is healthy (the client is being refused
    /// layouts by design, not trapped) — and still escalates past the
    /// ceiling so an unreachable DS can't livelock the client.
    #[test]
    fn truncate_dirty_overrides_healthy_failfast_within_ceiling() {
        let (_registry, handler) = pinned_handler(&["ds-1", "ds-2"], "f.bin");
        let p = handler.layout_manager.placement_for("f.bin").unwrap();
        let gate = truncate_gate_key(&p, "f.bin");
        handler.layout_manager.mark_truncate_dirty(&gate, 0);

        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", CEILING),
            FallbackIoDisposition::Delay,
            "dirty + healthy fleet must park, not spring the client into stale reads"
        );
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", Duration::ZERO),
            FallbackIoDisposition::FailFast
        );

        handler.layout_manager.clear_truncate_dirty_if(&gate, 0);
        assert_eq!(
            handler.fallback_io_disposition_bounded("f.bin", CEILING),
            FallbackIoDisposition::FailFast,
            "gate lifted + healthy fleet → back to the trap escape"
        );
    }

    /// LAYOUTGET on a truncate-dirty file must be refused TRYLATER —
    /// a fresh layout would expose stale stripe bytes beyond new EOF.
    #[test]
    fn layoutget_gated_while_truncate_dirty() {
        let (_registry, handler) = pinned_handler(&["ds-1"], "f.bin");
        let p = handler.layout_manager.placement_for("f.bin").unwrap();
        let gate = truncate_gate_key(&p, "f.bin");
        handler.layout_manager.mark_truncate_dirty(&gate, 0);

        let args = LayoutGetArgs {
            signal_layout_avail: false,
            layout_type: LayoutType::NfsV4_1Files,
            iomode: IoMode::Read,
            offset: 0,
            length: 4096,
            minlength: 4096,
            stateid: [0u8; 16],
            maxcount: 4096,
            filehandle: vec![1],
            file_key: "f.bin".to_string(),
            owner: owner(),
        };
        assert!(
            matches!(handler.layoutget(args.clone()), Err(LayoutGetError::TryLater)),
            "dirty file must gate LAYOUTGET"
        );

        handler.layout_manager.clear_truncate_dirty_if(&gate, 0);
        assert!(handler.layoutget(args).is_ok(), "gate lifted → layouts flow again");
    }
}
