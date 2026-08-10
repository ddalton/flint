//! pNFS gRPC Control Protocol
//!
//! This module provides gRPC-based communication between Data Servers (DS)
//! and the Metadata Server (MDS) for registration, heartbeats, and capacity reporting.
//!
//! # Protocol
//! - DS initiates all communication (client role)
//! - MDS responds to requests (server role)
//! - Protocol defined in proto/pnfs_control.proto

// Include generated protobuf code
pub mod proto {
    tonic::include_proto!("pnfs.control");
}

pub use proto::*;

use tonic::{Request, Response, Status};
use std::sync::Arc;
use tracing::{info, warn};

// Re-export for convenience
pub use proto::mds_control_server::{MdsControl, MdsControlServer};
pub use proto::mds_control_client::MdsControlClient;
pub use proto::ds_control_server::{DsControl, DsControlServer};
pub use proto::ds_control_client::DsControlClient;

/// Client-side control-plane auth: attaches FLINT_PNFS_CONTROL_TOKEN
/// as a Bearer token to every outgoing MdsControl RPC. The token is
/// read once; unset = no header (matches an MDS with auth disabled).
#[derive(Clone)]
pub struct ControlTokenInterceptor;

fn control_token_header() -> Option<&'static tonic::metadata::MetadataValue<tonic::metadata::Ascii>> {
    static HEADER: std::sync::OnceLock<
        Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
    > = std::sync::OnceLock::new();
    HEADER
        .get_or_init(|| {
            std::env::var("FLINT_PNFS_CONTROL_TOKEN")
                .ok()
                .and_then(|t| format!("Bearer {}", t).parse().ok())
        })
        .as_ref()
}

impl tonic::service::Interceptor for ControlTokenInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        if let Some(header) = control_token_header() {
            req.metadata_mut().insert("authorization", header.clone());
        }
        Ok(req)
    }
}

/// An MdsControl client that carries the control-plane token.
pub type AuthedMdsControlClient = MdsControlClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        ControlTokenInterceptor,
    >,
>;

/// Build a token-attaching MdsControl client over `channel`.
pub fn authed_mds_control_client(channel: tonic::transport::Channel) -> AuthedMdsControlClient {
    MdsControlClient::with_interceptor(channel, ControlTokenInterceptor)
}

/// A DsControl client (MDS → DS) that carries the control-plane token.
/// The whole control plane shares one FLINT_PNFS_CONTROL_TOKEN, so the
/// same interceptor serves both directions.
pub type AuthedDsControlClient = DsControlClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        ControlTokenInterceptor,
    >,
>;

/// Build a token-attaching DsControl client over `channel`.
pub fn authed_ds_control_client(channel: tonic::transport::Channel) -> AuthedDsControlClient {
    DsControlClient::with_interceptor(channel, ControlTokenInterceptor)
}

/// Server-side control-plane auth check, shared by the MDS's
/// MdsControl listener and each DS's DsControl listener: when
/// FLINT_PNFS_CONTROL_TOKEN is set, require `authorization: Bearer
/// <token>` on every request; when unset, accept everything (and the
/// process logs a loud WARN at startup).
pub fn check_control_token(req: Request<()>) -> Result<Request<()>, Status> {
    static EXPECTED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let expected = EXPECTED.get_or_init(|| {
        std::env::var("FLINT_PNFS_CONTROL_TOKEN")
            .ok()
            .map(|t| format!("Bearer {}", t))
    });
    match expected {
        None => Ok(req),
        Some(want) => match req.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
            Some(got) if got == want => Ok(req),
            _ => Err(Status::unauthenticated("control-plane token missing or wrong")),
        },
    }
}

/// MDS Control Service Implementation
///
/// This runs on the MDS and handles DS registration, heartbeats, etc.
pub struct MdsControlService {
    device_registry: Arc<crate::pnfs::mds::device::DeviceRegistry>,
    /// Operator-supplied DS endpoints (`device_id → client-reachable
    /// endpoint`). When a DS registers, we *override* the endpoint it
    /// reported with this map: a DS only knows its bind address (often
    /// `0.0.0.0` or a pod-internal IP), but the address the *NFS client*
    /// needs is the externally-routable one configured at MDS deploy
    /// time. Without this, GETDEVICEINFO returns `0.0.0.0.p1.p2` which
    /// the kernel can't reach, and the client silently falls back to
    /// MDS-direct I/O.
    configured_endpoints: std::collections::HashMap<String, String>,
    /// Operator-supplied DsControl endpoint overrides (`device_id →
    /// MDS-reachable "host:port"`). Wins over the default derivation
    /// (client-reachable host + DS-reported control port) — the two
    /// hosts differ when the MDS and the NFS clients take different
    /// network paths to the DSes (lima rig).
    configured_control_endpoints: std::collections::HashMap<String, String>,
    /// Absolute path of the MDS export root. CreateVolume creates files
    /// under this directory; the CSI driver's NodePublish points the
    /// kernel client at this path.
    export_path: std::path::PathBuf,

    /// Layout manager, for dropping a deleted volume's pinned stripe
    /// placement so a re-created volume at the same path gets a fresh
    /// pin instead of inheriting a stale (possibly unsatisfiable) one.
    layout_manager: crate::pnfs::mds::layout::LayoutManager,

    /// The MDS's NFSv4.1 bind port, returned in CreateVolumeResponse so
    /// the CSI driver mounts the right port. The driver reached us over
    /// gRPC — before this field it stamped that gRPC port into the
    /// kernel mount options (found live on runn, 2026-07-06).
    nfs_port: u16,

    /// F68b: dial the client-path endpoint before accepting a DS
    /// registration; NACK if it does not accept a TCP connect. The
    /// DS's registration retry loop (5s) becomes the wait, and the
    /// device never turns grantable while the endpoint the clients
    /// will be handed cannot be reached. A NACK never touches an
    /// existing registry entry, so re-registrations of a live device
    /// are only delayed, never degraded, by a transient dial failure.
    verify_ds_reachability: bool,

    /// Whether this MDS's state backend survives a restart.
    ///
    /// False under `state.backend: memory` — which is not hypothetical:
    /// it is set in `deployments/pnfs-mds-config.yaml` and in half the
    /// lima MDS configs. There, geometry would be accepted, acked, and
    /// silently gone on the next restart, leaving one volume striped two
    /// ways. Accepting a per-volume geometry we cannot keep is worse
    /// than refusing it, so a request carrying geometry is refused
    /// outright.
    durable_state: bool,
}

impl MdsControlService {
    /// Create a new MDS control service. `configured_endpoints` is the
    /// operator's view of `device_id → reachable endpoint` taken from
    /// the MDS config's `dataServers` list. `export_path` is the MDS
    /// export root from the same config.
    pub fn new(
        device_registry: Arc<crate::pnfs::mds::device::DeviceRegistry>,
        configured_endpoints: std::collections::HashMap<String, String>,
        configured_control_endpoints: std::collections::HashMap<String, String>,
        export_path: std::path::PathBuf,
        layout_manager: crate::pnfs::mds::layout::LayoutManager,
        nfs_port: u16,
        durable_state: bool,
        verify_ds_reachability: bool,
    ) -> Self {
        Self {
            device_registry,
            configured_endpoints,
            configured_control_endpoints,
            export_path,
            layout_manager,
            nfs_port,
            verify_ds_reachability,
            durable_state,
        }
    }

    /// Block-class create's target half: the lvol + subsystem + pinned
    /// NGUID + listener must exist before the create is acked, or the
    /// volume is grantable-but-unreachable — every client I/O would need
    /// the MDS fallback lane, which refuses (the zeros belt). No
    /// reconciler attached = no `blockExport` in the MDS config = this
    /// MDS cannot serve the class at all; the refusal names the fix.
    async fn ensure_block_export(&self, volume: &str, size_bytes: u64) -> Result<(), String> {
        let Some(reconciler) = self.layout_manager.block_export() else {
            return Err(
                "this MDS has no blockExport configured (mds.blockExport: spdkSocket/\
                 lvstore/traddr) — a block-class volume would have no NVMe target its \
                 clients could reach"
                    .to_string(),
            );
        };
        reconciler
            .ensure(volume, Some(size_bytes))
            .await
            .map_err(|e| format!("block-class export provisioning failed: {e}"))
    }

    /// F68b reachability dial: can the endpoint clients will be handed
    /// actually be connected to right now? Plain TCP connect with a
    /// bounded timeout — the point is to verify the routing chain
    /// (DNS → Service → Ready endpoint → listener), not to speak NFS.
    async fn ds_endpoint_accepts(endpoint: &str) -> Result<(), String> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(endpoint),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("connect failed: {}", e)),
            Err(_) => Err("connect timed out after 2s".to_string()),
        }
    }
}

#[tonic::async_trait]
impl MdsControl for MdsControlService {
    /// Handle DS registration
    async fn register_data_server(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let req = request.into_inner();
        
        // Override the DS-reported endpoint(s) with the operator-configured
        // ones for this device_id. The DS only knows its bind address
        // (typically 0.0.0.0); the client needs the externally-reachable
        // endpoint the operator has set up (e.g. a Service IP, an
        // out-of-cluster IP, or in dev a hostname like host.lima.internal).
        //
        // Multipath: a comma-separated configured value ("ep1,ep2,…")
        // overrides the WHOLE address list — first entry is the primary,
        // the rest become extra netaddr4 entries in this DS's
        // GETDEVICEINFO multipath_list4. Without an override, the
        // DS-reported primary + multipath_endpoints are used as-is.
        let (effective_endpoint, extra_endpoints) = match self
            .configured_endpoints
            .get(&req.device_id)
        {
            Some(configured) => {
                let (primary, extras) =
                    crate::pnfs::config::split_endpoint_list(configured);
                if primary.is_empty() {
                    (req.endpoint.clone(), req.multipath_endpoints.clone())
                } else {
                    (primary, extras)
                }
            }
            None => (req.endpoint.clone(), req.multipath_endpoints.clone()),
        };
        if effective_endpoint != req.endpoint {
            info!(
                "📝 DS Registration: device_id={}, ds-reported endpoint={} → using configured endpoint={} (+{} multipath), capacity={} bytes",
                req.device_id, req.endpoint, effective_endpoint, extra_endpoints.len(), req.capacity,
            );
        } else {
            info!(
                "📝 DS Registration: device_id={}, endpoint={} (+{} multipath), capacity={} bytes",
                req.device_id, effective_endpoint, extra_endpoints.len(), req.capacity,
            );
        }

        // Create device info
        let mut device_info = crate::pnfs::mds::device::DeviceInfo::new(
            req.device_id.clone(),
            effective_endpoint,
            req.mount_points.clone(),
        );

        device_info.endpoints = extra_endpoints;
        device_info.capacity = req.capacity;
        device_info.used = req.used;
        device_info.identity_created_at = req.identity_created_at;

        // DsControl endpoint: an operator override wins (the MDS may
        // reach the DS on a different host than clients do — lima
        // rig); otherwise pair the effective endpoint's host with the
        // reported control port. 0 = no listener (older DS build /
        // dev config).
        device_info.control_endpoint =
            match (self.configured_control_endpoints.get(&req.device_id), req.control_port) {
                (Some(ce), _) => Some(ce.clone()),
                (None, 0) => None,
                (None, port) => {
                    let host = device_info
                        .primary_endpoint
                        .rsplit_once(':')
                        .map(|(h, _)| h)
                        .unwrap_or(device_info.primary_endpoint.as_str());
                    Some(format!("{}:{}", host, port))
                }
            };

        // F68b: refuse to make this device grantable until the exact
        // endpoint GETDEVICEINFO will advertise accepts a connect. In
        // k8s the pod registers seconds before its per-pod Service has
        // a Ready endpoint; a layout granted in that window points
        // clients at a black hole and the kernel blacklists the
        // deviceid for 120s, silently proxying all I/O via the MDS.
        // The DS retries registration every 5s — this NACK is the
        // wait-loop, and it leaves any existing registry entry intact.
        if self.verify_ds_reachability {
            // Every advertised address gets the dial, not just the
            // primary: a dead multipath endpoint won't fail the mount
            // (the kernel logs and skips a trunk candidate it cannot
            // connect) but it silently halves the trunking the operator
            // paid for — NACK loud instead, same F68b philosophy.
            for ep in std::iter::once(&device_info.primary_endpoint)
                .chain(device_info.endpoints.iter())
            {
                if let Err(e) = Self::ds_endpoint_accepts(ep).await {
                    warn!(
                        "🚧 F68b: NACKing registration of {} — client-path endpoint {} not yet \
                         reachable from the MDS ({}); the DS will retry",
                        req.device_id, ep, e
                    );
                    return Ok(Response::new(RegisterResponse {
                        accepted: false,
                        message: format!(
                            "endpoint {} not reachable yet ({}) — retry registration",
                            ep, e
                        ),
                        assigned_device_id: String::new(),
                    }));
                }
            }
        }

        // Register with device registry
        match self.device_registry.register(device_info) {
            Ok(_) => {
                info!("✅ DS registered successfully: {}", req.device_id);
                
                Ok(Response::new(RegisterResponse {
                    accepted: true,
                    message: format!("Registration successful for {}", req.device_id),
                    assigned_device_id: req.device_id,
                }))
            }
            Err(e) => {
                warn!("❌ DS registration failed: {}", e);
                
                Ok(Response::new(RegisterResponse {
                    accepted: false,
                    message: format!("Registration failed: {}", e),
                    assigned_device_id: String::new(),
                }))
            }
        }
    }

    /// Handle heartbeat
    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        
        // Update heartbeat timestamp
        if let Err(e) = self.device_registry.heartbeat(&req.device_id) {
            warn!("Heartbeat from unknown device {}: {}", req.device_id, e);
            return Ok(Response::new(HeartbeatResponse {
                acknowledged: false,
                instructions: vec![],
            }));
        }

        // Update capacity
        if let Err(e) = self.device_registry.update_capacity(
            &req.device_id,
            req.capacity,
            req.used,
        ) {
            warn!("Failed to update capacity for {}: {}", req.device_id, e);
        }

        // Check health status and update
        let device_status = match req.health() {
            HealthStatus::Healthy => crate::pnfs::mds::device::DeviceStatus::Active,
            HealthStatus::Degraded => crate::pnfs::mds::device::DeviceStatus::Degraded,
            HealthStatus::Unhealthy => crate::pnfs::mds::device::DeviceStatus::Offline,
        };

        if let Err(e) = self.device_registry.update_status(&req.device_id, device_status) {
            warn!("Failed to update status for {}: {}", req.device_id, e);
        }

        // Piggyback pending stripe-file cleanups (from NFS REMOVE /
        // rename-over of striped files) on the heartbeat response.
        // Drained once, best-effort: a DS that dies before applying
        // them leaks orphaned stripe space, never correctness.
        let instructions: Vec<Instruction> = self
            .layout_manager
            .drain_stripe_cleanup(&req.device_id)
            .into_iter()
            .map(|rel_path| Instruction {
                r#type: InstructionType::DeleteStripeFile as i32,
                details: rel_path,
            })
            .collect();
        if !instructions.is_empty() {
            info!(
                "🧹 {} stripe-cleanup instruction(s) → {}",
                instructions.len(),
                req.device_id
            );
        }

        Ok(Response::new(HeartbeatResponse {
            acknowledged: true,
            instructions,
        }))
    }

    /// Handle capacity update
    async fn update_capacity(
        &self,
        request: Request<CapacityUpdate>,
    ) -> Result<Response<CapacityResponse>, Status> {
        let req = request.into_inner();
        
        if let Err(e) = self.device_registry.update_capacity(
            &req.device_id,
            req.capacity,
            req.used,
        ) {
            warn!("Capacity update failed for {}: {}", req.device_id, e);
            return Ok(Response::new(CapacityResponse {
                acknowledged: false,
            }));
        }

        Ok(Response::new(CapacityResponse {
            acknowledged: true,
        }))
    }

    /// Provision a new pNFS volume as a **directory subtree**
    /// `<export>/<volume_id>/` (directory-per-volume model).
    ///
    /// Pods mount `MDS:/<volume_id>` — an isolated shared POSIX
    /// namespace per PVC. Files inside stripe across the DSes exactly
    /// as before (LAYOUTGET is per-file; the placement keys just gain
    /// a `<volume_id>/` prefix). The original model — one sparse file
    /// sized with `set_len`, export ROOT mounted by every pod — gave
    /// no isolation: every PVC saw the whole export (Spark dry-run
    /// Finding 1, docs/plans/pnfs-csi-rwx-and-committer-fixes.md).
    ///
    /// Capacity: `size_bytes` is recorded in the CSI response only.
    /// Enforcement is pool-level (DS statvfs + bounded ENOSPC, P0-4),
    /// which is unchanged — the sparse file's size never bounded
    /// writes either (a consumer could always write past it into pool
    /// space). Per-volume quota is future work.
    ///
    /// Idempotent: re-creating an existing directory volume is
    /// success. A pre-existing legacy *file* volume keeps its old
    /// semantics (size match = success, mismatch = error).
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = request.into_inner();

        // Volume IDs that contain path separators or NULs would let a
        // malicious caller escape the export. Reject early.
        if req.volume_id.is_empty()
            || req.volume_id.contains('/')
            || req.volume_id.contains('\0')
        {
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: "volume_id must be non-empty and contain no '/' or NUL".into(),
                nfs_port: self.nfs_port as u32,
                directory: false,
                effective_stripe_size: 0,
                effective_stripe_width: 0,
                effective_layout_class: String::new(),
            }));
        }

        // Refuse geometry we cannot keep. Echoing 0/0 instead would make
        // the driver blame image skew; naming the real cause here is what
        // an operator can act on.
        if !self.durable_state && (req.stripe_size != 0 || req.stripe_width != 0) {
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: "this MDS runs with state.backend: memory, which does not survive a \
                          restart; per-volume stripe geometry would be silently lost. Switch the \
                          MDS to state.backend: sqlite, or drop the pnfs.flint.io/stripeSize and \
                          stripeWidth parameters from the StorageClass"
                    .into(),
                nfs_port: self.nfs_port as u32,
                directory: false,
                effective_stripe_size: 0,
                effective_stripe_width: 0,
                effective_layout_class: String::new(),
            }));
        }

        // Layout class: a closed set, refused rather than defaulted — a
        // class misread as File would provision a volume whose layouts
        // are stripe maps while the caller believes it is extent-backed.
        // "" is what an OLDER DRIVER sends (proto3 drops the field), and
        // parses as File, which is exactly what that driver means.
        let Some(layout_class) =
            crate::pnfs::mds::layout::LayoutClass::parse(&req.layout_class)
        else {
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: format!(
                    "unknown layout_class {:?}; this MDS supports \"file\" and \"scsi\"",
                    req.layout_class,
                ),
                nfs_port: self.nfs_port as u32,
                directory: false,
                effective_stripe_size: 0,
                effective_stripe_width: 0,
                effective_layout_class: String::new(),
            }));
        };
        // Block-class volumes are extent-backed: the allocator's state IS
        // the volume's map, and §8 makes its durability sqlite-only. A
        // memory-backed MDS cannot hold it — same refusal shape as the
        // stripe-geometry guard above.
        if !self.durable_state && layout_class == crate::pnfs::mds::layout::LayoutClass::Scsi {
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: "block-class (layout: pnfs-block) volumes require the durable sqlite \
                          state backend: the extent map IS the volume's data map, and losing it \
                          while the data survives serves silent zeros (F67). Switch the MDS to \
                          state.backend: sqlite"
                    .into(),
                nfs_port: self.nfs_port as u32,
                directory: false,
                effective_stripe_size: 0,
                effective_stripe_width: 0,
                effective_layout_class: String::new(),
            }));
        }

        let file_path = self.export_path.join(&req.volume_id);
        let export_str = self.export_path.to_string_lossy().into_owned();

        // Existing-volume path. A directory is the current model —
        // idempotent success. A file is a legacy sparse-file volume:
        // keep its old semantics (size match = success, mismatch =
        // refuse) so pre-existing PVs behave unchanged.
        if let Ok(meta) = std::fs::metadata(&file_path) {
            if meta.is_dir() {
                // The CSI provisioner re-issues CreateVolume by name, so
                // this is a ROUTINE path, not an exceptional one — and it
                // must echo the geometry actually in force. Echoing zeros
                // here made the driver's version-skew check fail the PVC
                // with "this MDS does not support per-volume stripe
                // geometry" on a volume that had been created perfectly
                // well, permanently, on nothing worse than a retry.
                //
                // `ensure_` rather than a plain read: it also repairs a
                // create that crashed between mkdir and recording the
                // geometry. An existing record wins over the request, so a
                // StorageClass edited between attempts surfaces as an echo
                // mismatch instead of silently re-striping.
                let geometry = self.layout_manager.ensure_volume_geometry(
                    &req.volume_id,
                    crate::pnfs::mds::layout::VolumeGeometry {
                        stripe_size: req.stripe_size,
                        stripe_width: req.stripe_width,
                        layout_class,
                    },
                ).await;
                info!(
                    "📦 CreateVolume: directory volume {} already exists (stripe_size={} width={})",
                    req.volume_id, geometry.stripe_size, geometry.stripe_width
                );
                // Re-register the arena on the retry path too: it is
                // idempotent, and this is what repairs a create that
                // crashed between recording geometry and the arena.
                if geometry.layout_class == crate::pnfs::mds::layout::LayoutClass::Scsi {
                    if let Err(e) = self
                        .layout_manager
                        .register_extent_arena(&req.volume_id, req.size_bytes)
                        .await
                    {
                        return Ok(Response::new(CreateVolumeResponse {
                            created: false,
                            export_path: String::new(),
                            volume_file: String::new(),
                            message: format!(
                                "block-class volume exists but its extent arena could not \
                                 be ensured: {e}"
                            ),
                            nfs_port: self.nfs_port as u32,
                            directory: false,
                            effective_stripe_size: 0,
                            effective_stripe_width: 0,
                            effective_layout_class: String::new(),
                        }));
                    }
                    // And the export chain, same repair reasoning: this
                    // is the path that finishes a create that crashed
                    // between the arena and the lvol/subsystem.
                    if let Err(msg) =
                        self.ensure_block_export(&req.volume_id, req.size_bytes).await
                    {
                        return Ok(Response::new(CreateVolumeResponse {
                            created: false,
                            export_path: String::new(),
                            volume_file: String::new(),
                            message: msg,
                            nfs_port: self.nfs_port as u32,
                            directory: false,
                            effective_stripe_size: 0,
                            effective_stripe_width: 0,
                            effective_layout_class: String::new(),
                        }));
                    }
                }
                return Ok(Response::new(CreateVolumeResponse {
                    created: true,
                    export_path: export_str,
                    volume_file: req.volume_id,
                    message: "already exists".into(),
                    nfs_port: self.nfs_port as u32,
                    directory: true,
                    effective_stripe_size: geometry.stripe_size,
                    effective_stripe_width: geometry.stripe_width,
                    effective_layout_class: geometry.layout_class.as_str().to_string(),
                }));
            }
            if meta.len() == req.size_bytes {
                info!(
                    "📦 CreateVolume: legacy file volume {} already exists at correct size ({} bytes)",
                    req.volume_id, req.size_bytes
                );
                let geometry = self.layout_manager.ensure_volume_geometry(
                    &req.volume_id,
                    crate::pnfs::mds::layout::VolumeGeometry {
                        stripe_size: req.stripe_size,
                        stripe_width: req.stripe_width,
                        layout_class,
                    },
                ).await;
                return Ok(Response::new(CreateVolumeResponse {
                    created: true,
                    export_path: export_str,
                    volume_file: req.volume_id,
                    message: "already exists".into(),
                    nfs_port: self.nfs_port as u32,
                    directory: false,
                    effective_stripe_size: geometry.stripe_size,
                    effective_stripe_width: geometry.stripe_width,
                    effective_layout_class: geometry.layout_class.as_str().to_string(),
                }));
            }
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: format!(
                    "volume {} exists at size {}, requested {}; refusing to resize",
                    req.volume_id, meta.len(), req.size_bytes,
                ),
                nfs_port: self.nfs_port as u32,
                directory: false,
                effective_stripe_size: 0,
                effective_stripe_width: 0,
                effective_layout_class: String::new(),
            }));
        }

        // Make sure the export dir itself exists. The MDS config
        // creates it on startup, but a manual rm of /tmp on dev
        // machines is a real failure mode worth handling.
        if let Err(e) = std::fs::create_dir_all(&self.export_path) {
            warn!("CreateVolume: cannot ensure export dir {:?}: {}", self.export_path, e);
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: format!("export dir not writable: {}", e),
                nfs_port: self.nfs_port as u32,
                directory: false,
                effective_stripe_size: 0,
                effective_stripe_width: 0,
                effective_layout_class: String::new(),
            }));
        }

        if let Err(e) = std::fs::create_dir(&file_path) {
            warn!("CreateVolume: create_dir({:?}): {}", file_path, e);
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: format!("create dir: {}", e),
                nfs_port: self.nfs_port as u32,
                directory: false,
                effective_stripe_size: 0,
                effective_stripe_width: 0,
                effective_layout_class: String::new(),
            }));
        }
        // Directory ownership and mode.
        //
        // The historical behaviour was an unconditional 0777 — the
        // consuming pod's uid is arbitrary (Spark executors, app
        // containers) and NFS has no idmapping story here, so
        // world-writable was the only thing guaranteed to work. It is
        // also the loosest possible setting, and gives a PVC author no
        // way to ask for anything tighter.
        //
        // `dir_gid` + `dir_mode` (StorageClass `pnfs.flint.io/dirGid`
        // and `dirMode`) let a workload run with a `fsGroup` instead:
        // chgrp the volume root to that gid, set the mode, and set the
        // setgid bit so files created inside inherit the group. Without
        // setgid, a 0770 directory would be writable by the group but
        // every file inside would land in the creator's primary group
        // and be unreadable by the volume's other consumers.
        //
        // Defaults preserve 0777 with no chgrp, so existing volumes and
        // classes are unaffected.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if req.dir_mode != 0 { req.dir_mode } else { 0o777 };
            let mode = if req.dir_gid != 0 { mode | 0o2000 } else { mode };
            if req.dir_gid != 0 {
                if let Err(e) = std::os::unix::fs::chown(&file_path, None, Some(req.dir_gid)) {
                    warn!(
                        "CreateVolume: chgrp {:?} to gid {} failed: {} — \
                         a pod relying on fsGroup will not be able to write",
                        file_path, req.dir_gid, e
                    );
                }
            }
            let _ = std::fs::set_permissions(
                &file_path,
                std::fs::Permissions::from_mode(mode),
            );
        }

        // Record the volume's stripe geometry before returning, so the
        // very first file created in it is placed correctly. Echoed back
        // so the caller can detect a driver/MDS version skew instead of
        // silently getting the default.
        let geometry = self.layout_manager.set_volume_geometry(
            &req.volume_id,
            crate::pnfs::mds::layout::VolumeGeometry {
                stripe_size: req.stripe_size,
                stripe_width: req.stripe_width,
                layout_class,
            },
        ).await;

        // A block-class volume without its extent arena cannot grant a
        // single extent — the arena row is as load-bearing as the
        // directory itself, so its failure fails the create.
        if geometry.layout_class == crate::pnfs::mds::layout::LayoutClass::Scsi {
            if let Err(e) = self
                .layout_manager
                .register_extent_arena(&req.volume_id, req.size_bytes)
                .await
            {
                return Ok(Response::new(CreateVolumeResponse {
                    created: false,
                    export_path: String::new(),
                    volume_file: String::new(),
                    message: format!("block-class extent arena registration failed: {e}"),
                    nfs_port: self.nfs_port as u32,
                    directory: false,
                    effective_stripe_size: 0,
                    effective_stripe_width: 0,
                    effective_layout_class: String::new(),
                }));
            }
            if let Err(msg) = self.ensure_block_export(&req.volume_id, req.size_bytes).await {
                return Ok(Response::new(CreateVolumeResponse {
                    created: false,
                    export_path: String::new(),
                    volume_file: String::new(),
                    message: msg,
                    nfs_port: self.nfs_port as u32,
                    directory: false,
                    effective_stripe_size: 0,
                    effective_stripe_width: 0,
                    effective_layout_class: String::new(),
                }));
            }
        }

        info!(
            "📦 CreateVolume: created directory volume {} at {:?} ({} bytes requested, pool-enforced)",
            req.volume_id, file_path, req.size_bytes
        );
        Ok(Response::new(CreateVolumeResponse {
            created: true,
            export_path: export_str,
            volume_file: req.volume_id,
            message: String::new(),
            nfs_port: self.nfs_port as u32,
            directory: true,
            effective_stripe_size: geometry.stripe_size,
            effective_stripe_width: geometry.stripe_width,
            effective_layout_class: geometry.layout_class.as_str().to_string(),
        }))
    }

    /// Grow a volume's recorded capacity.
    ///
    /// Directory volumes — the current model — hold no per-volume size on
    /// the MDS at all: `CreateVolume` logs the requested bytes as
    /// "pool-enforced" and stores nothing. So expansion is genuinely a
    /// metadata-only acknowledgement, and refusing it (as the driver did
    /// before) left a PVC permanently wedged in `Resizing` for an
    /// operation that had nothing to do. Legacy sparse-file volumes DO
    /// carry a real length and are grown in place.
    ///
    /// Shrinking is refused in both shapes: CSI forbids it, and for a
    /// legacy file it would discard data.
    async fn expand_volume(
        &self,
        request: Request<ExpandVolumeRequest>,
    ) -> Result<Response<ExpandVolumeResponse>, Status> {
        let req = request.into_inner();
        if req.volume_id.is_empty()
            || req.volume_id.contains('/')
            || req.volume_id.contains('\0')
        {
            return Ok(Response::new(ExpandVolumeResponse {
                expanded: false,
                size_bytes: 0,
                message: "volume_id must be non-empty and contain no '/' or NUL".into(),
            }));
        }

        let path = self.export_path.join(&req.volume_id);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                return Ok(Response::new(ExpandVolumeResponse {
                    expanded: false,
                    size_bytes: 0,
                    message: format!("volume {} not found: {}", req.volume_id, e),
                }));
            }
        };

        if meta.is_dir() {
            info!(
                "📏 ExpandVolume: directory volume {} → {} bytes (metadata-only; \
                 capacity is pool-side at the data servers)",
                req.volume_id, req.size_bytes
            );
            return Ok(Response::new(ExpandVolumeResponse {
                expanded: true,
                size_bytes: req.size_bytes,
                message: String::new(),
            }));
        }

        if req.size_bytes < meta.len() {
            return Ok(Response::new(ExpandVolumeResponse {
                expanded: false,
                size_bytes: meta.len(),
                message: format!(
                    "cannot shrink legacy file volume {} from {} to {} bytes",
                    req.volume_id,
                    meta.len(),
                    req.size_bytes
                ),
            }));
        }
        if req.size_bytes > meta.len() {
            if let Err(e) = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(req.size_bytes))
            {
                return Ok(Response::new(ExpandVolumeResponse {
                    expanded: false,
                    size_bytes: meta.len(),
                    message: format!("grow {}: {}", req.volume_id, e),
                }));
            }
            info!(
                "📏 ExpandVolume: legacy file volume {} grown {} → {} bytes",
                req.volume_id,
                meta.len(),
                req.size_bytes
            );
        }
        Ok(Response::new(ExpandVolumeResponse {
            expanded: true,
            size_bytes: req.size_bytes,
            message: String::new(),
        }))
    }

    /// Delete a pNFS volume's metadata file. Idempotent — deleting an
    /// absent volume returns success so retries from a flaky CSI
    /// provisioner don't fail.
    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let req = request.into_inner();

        if req.volume_id.is_empty()
            || req.volume_id.contains('/')
            || req.volume_id.contains('\0')
        {
            return Ok(Response::new(DeleteVolumeResponse {
                deleted: false,
                message: "volume_id must be non-empty and contain no '/' or NUL".into(),
            }));
        }

        let file_path = self.export_path.join(&req.volume_id);

        // Reclaim DS stripes for everything this volume pinned.
        // Directory volumes own the whole `<volume_id>/…` key prefix;
        // the exact key covers legacy single-file volumes. Identity-
        // keyed pins only — legacy pins have no MDS-side rel-path
        // knowledge here and just leak until scrubbed. Runs on the
        // already-absent path too: placements can outlive the tree
        // (e.g. a crash between rm and this reply).
        let reclaim = |mgr: &crate::pnfs::mds::layout::LayoutManager| {
            for (key, p) in mgr.forget_placements_under(&req.volume_id) {
                if p.file_id != 0 {
                    mgr.enqueue_stripe_cleanup(&p, &key);
                }
            }
            if let Some(p) = mgr.forget_placement(&req.volume_id) {
                if p.file_id != 0 {
                    mgr.enqueue_stripe_cleanup(&p, &req.volume_id);
                }
            }
            // Geometry outlives nothing: a volume re-created at the same
            // name must get the new StorageClass's geometry, not inherit
            // the old one the way a stale placement pin would.
            mgr.forget_volume_geometry(&req.volume_id);
        };

        // Tear down the NVMe export chain FIRST — deleting the subsystem
        // severs every client's data path before the extent map beneath
        // it goes away. Same class-or-no-class convergence as the row
        // sweep below (both halves tolerate absence), so a class-confused
        // or half-created state still cleans up. Failure leaks the
        // subsystem+lvol loudly; rows are swept regardless, because a
        // re-created same-name volume inheriting stale GRANTS is worse
        // than one colliding with a stale export (the create's ensure
        // pass converges the latter).
        if let Some(reconciler) = self.layout_manager.block_export() {
            if let Err(e) = reconciler.delete_volume_export(&req.volume_id).await {
                warn!(
                    "DeleteVolume {}: export teardown failed: {} — subsystem/lvol leak \
                     until re-create or operator sweep",
                    req.volume_id, e
                );
            }
        }

        // Sweep every extent-allocator row for the volume, class or no
        // class (dropping zero rows is free, and it also cleans a
        // class-confused state). Without this a re-created same-name
        // volume inherits the old arena and extent rows: stale grants
        // blocking every reclaim, and a watermark claiming space the
        // new lvol never allocated.
        match self
            .layout_manager
            .state_backend()
            .extent_drop_volume(&req.volume_id)
            .await
        {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => info!("🗑  DeleteVolume {}: dropped {} extent row(s)", req.volume_id, n),
            Ok(Err(e)) => warn!(
                "DeleteVolume {}: extent sweep refused: {} — rows leak until re-create",
                req.volume_id, e
            ),
            Err(e) => warn!(
                "DeleteVolume {}: extent sweep failed: {} — rows leak until re-create",
                req.volume_id, e
            ),
        }

        let removed = match std::fs::symlink_metadata(&file_path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&file_path),
            Ok(_) => std::fs::remove_file(&file_path),
            Err(e) => Err(e),
        };

        match removed {
            Ok(()) => {
                info!("🗑️  DeleteVolume: removed {:?}", file_path);
                reclaim(&self.layout_manager);
                Ok(Response::new(DeleteVolumeResponse {
                    deleted: true,
                    message: String::new(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("🗑️  DeleteVolume: {} already absent", req.volume_id);
                reclaim(&self.layout_manager);
                Ok(Response::new(DeleteVolumeResponse {
                    deleted: true,
                    message: "already absent".into(),
                }))
            }
            Err(e) => {
                warn!("DeleteVolume: remove({:?}): {}", file_path, e);
                Ok(Response::new(DeleteVolumeResponse {
                    deleted: false,
                    message: format!("{}", e),
                }))
            }
        }
    }

    /// Handle DS unregistration
    async fn unregister_data_server(
        &self,
        request: Request<UnregisterRequest>,
    ) -> Result<Response<UnregisterResponse>, Status> {
        let req = request.into_inner();
        
        info!("📤 DS Unregistration: device_id={}, reason={}", req.device_id, req.reason);

        match self.device_registry.unregister(&req.device_id) {
            Ok(_) => {
                info!("✅ DS unregistered successfully: {}", req.device_id);
                Ok(Response::new(UnregisterResponse {
                    acknowledged: true,
                }))
            }
            Err(e) => {
                warn!("❌ DS unregistration failed: {}", e);
                Ok(Response::new(UnregisterResponse {
                    acknowledged: false,
                }))
            }
        }
    }
}

#[cfg(test)]
mod create_volume_tests {
    use super::*;
    use crate::pnfs::mds::device::DeviceRegistry;

    fn cvreq(id: &str, size: u64) -> CreateVolumeRequest {
        CreateVolumeRequest {
            volume_id: id.into(),
            size_bytes: size,
            stripe_size: 0,
            stripe_width: 0,
            dir_gid: 0,
            dir_mode: 0,
            layout_class: String::new(),
        }
    }

    #[tokio::test]
    async fn unknown_layout_class_is_refused_not_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        let mut req = cvreq("pvc-weird", 1 << 20);
        req.layout_class = "weird".into();
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(!r.created, "unknown class must refuse, never default to file");
        assert!(r.message.contains("unknown layout_class"), "got: {}", r.message);
    }

    #[tokio::test]
    async fn block_class_requires_durable_state() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc_nondurable(dir.path());
        let mut req = cvreq("pvc-blk", 1 << 20);
        req.layout_class = "scsi".into();
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(!r.created);
        assert!(r.message.contains("sqlite"), "got: {}", r.message);
    }

    /// durable_state=true but the backend is MemoryBackend: the arena
    /// registration itself refuses (extent state is sqlite-only, §8),
    /// and the create fails LOUDLY instead of acking a block volume
    /// that could never grant an extent. This is the memory backend's
    /// refusal doing its job end-to-end.
    #[tokio::test]
    async fn block_class_arena_failure_fails_the_create() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        let mut req = cvreq("pvc-blk2", 1 << 20);
        req.layout_class = "scsi".into();
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(!r.created, "an unregistrable arena must fail the create");
        assert!(r.message.contains("extent arena"), "got: {}", r.message);
    }

    /// A durable sqlite MDS with NO blockExport config: the arena would
    /// register fine, but a block volume with no NVMe target is a volume
    /// no client can reach — refuse at provision, naming the config key.
    #[tokio::test]
    async fn block_class_create_refused_without_block_export_config() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _tgt) = svc_sqlite(dir.path(), false);
        let mut req = cvreq("pvc-blk3", 1 << 20);
        req.layout_class = "scsi".into();
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(!r.created, "no reconciler = no reachable target = refuse");
        assert!(r.message.contains("blockExport"), "got: {}", r.message);
        assert!(
            !dir.path().join("pvc-blk3").exists() || dir.path().join("pvc-blk3").is_dir(),
            "directory state is fine either way; the ack is what must not happen"
        );
    }

    /// The whole block-class provision chain: arena + lvol + subsystem
    /// (pinned NGUID, default-closed) + listener, and the retry path
    /// repairs/no-ops instead of failing.
    #[tokio::test]
    async fn block_class_create_provisions_the_export_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (s, tgt) = svc_sqlite(dir.path(), true);
        let mut req = cvreq("pvc-blk4", 8 << 20);
        req.layout_class = "scsi".into();
        let r = s.create_volume(Request::new(req.clone())).await.unwrap().into_inner();
        assert!(r.created, "{}", r.message);
        assert_eq!(r.effective_layout_class, "scsi");

        let nqn = crate::identity::block_volume_export_nqn("pvc-blk4");
        let subs = tgt.subsystems.lock().unwrap();
        let sub = subs.get(&nqn).expect("subsystem exists");
        let (_, nguid) = crate::nvmeof_export::stable_ns_identity("pvc-blk4");
        assert_eq!(
            sub["namespaces"][0]["nguid"], nguid,
            "the namespace carries the SAME identity GETDEVICEINFO advertises"
        );
        assert_eq!(sub["allow_any_host"], false, "default-closed");
        drop(subs);
        assert!(
            tgt.bdevs.lock().unwrap().contains_key("lvs_test/pvc-blk4"),
            "backing lvol exists"
        );

        // Idempotent retry (the provisioner does this routinely).
        let again = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(again.created, "retry must succeed: {}", again.message);
        assert_eq!(again.effective_layout_class, "scsi");
    }

    /// DeleteVolume of a block volume tears the whole chain down —
    /// subsystem first (severs the data path), then the lvol, then the
    /// allocator rows — so a re-created same-name volume starts clean.
    #[tokio::test]
    async fn block_class_delete_tears_down_the_export_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (s, tgt) = svc_sqlite(dir.path(), true);
        let mut req = cvreq("pvc-blk5", 1 << 20);
        req.layout_class = "scsi".into();
        assert!(s.create_volume(Request::new(req)).await.unwrap().into_inner().created);
        assert!(!tgt.subsystems.lock().unwrap().is_empty());

        let r = s
            .delete_volume(Request::new(DeleteVolumeRequest { volume_id: "pvc-blk5".into() }))
            .await
            .unwrap()
            .into_inner();
        assert!(r.deleted, "{}", r.message);
        assert!(tgt.subsystems.lock().unwrap().is_empty(), "subsystem torn down");
        assert!(tgt.bdevs.lock().unwrap().is_empty(), "lvol torn down");
    }

    /// Expanding a directory volume must SUCCEED. It stores no
    /// per-volume size — capacity is pool-side at the data servers — so
    /// there is genuinely nothing to do, and refusing left the PVC stuck
    /// in `Resizing` forever: FAILED_PRECONDITION is final for
    /// external-resizer, so the condition never cleared.
    #[tokio::test]
    async fn expanding_a_directory_volume_is_a_metadata_only_success() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        assert!(s.create_volume(Request::new(cvreq("vol", 1 << 20))).await.unwrap().into_inner().created);

        let r = s
            .expand_volume(Request::new(ExpandVolumeRequest {
                volume_id: "vol".into(),
                size_bytes: 100 << 20,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(r.expanded, "{}", r.message);
        assert_eq!(r.size_bytes, 100 << 20);
    }

    /// A legacy sparse-file volume DOES carry a real length, so expand
    /// must actually grow the file — reporting success without growing
    /// it would leave writes past the old EOF failing.
    #[tokio::test]
    async fn expanding_a_legacy_file_volume_grows_the_file() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        let path = d.path().join("legacy");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        let r = s
            .expand_volume(Request::new(ExpandVolumeRequest {
                volume_id: "legacy".into(),
                size_bytes: 8192,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(r.expanded, "{}", r.message);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);
    }

    #[tokio::test]
    async fn shrinking_a_legacy_file_volume_is_refused() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        std::fs::write(d.path().join("legacy"), vec![0u8; 8192]).unwrap();

        let r = s
            .expand_volume(Request::new(ExpandVolumeRequest {
                volume_id: "legacy".into(),
                size_bytes: 4096,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!r.expanded);
        assert!(r.message.contains("cannot shrink"), "{}", r.message);
        assert_eq!(std::fs::metadata(d.path().join("legacy")).unwrap().len(), 8192);
    }

    #[tokio::test]
    async fn expanding_an_absent_volume_is_refused() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        let r = s
            .expand_volume(Request::new(ExpandVolumeRequest {
                volume_id: "nope".into(),
                size_bytes: 4096,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!r.expanded);
        assert!(r.message.contains("not found"), "{}", r.message);
    }

    /// A path-traversing volume_id must be refused on the expand verb
    /// too — the create verb guards it, and an unguarded sibling is how
    /// that kind of check gets bypassed.
    #[tokio::test]
    async fn expand_refuses_a_path_traversing_volume_id() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        for bad in ["../escape", "a/b", ""] {
            let r = s
                .expand_volume(Request::new(ExpandVolumeRequest {
                    volume_id: bad.into(),
                    size_bytes: 4096,
                }))
                .await
                .unwrap()
                .into_inner();
            assert!(!r.expanded, "{bad} should be refused");
        }
    }

    /// The default stays 0777 with no group — every volume provisioned
    /// before dirGid/dirMode existed depends on it.
    #[tokio::test]
    async fn the_default_volume_directory_is_world_writable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        assert!(s.create_volume(Request::new(cvreq("v", 1 << 20))).await.unwrap().into_inner().created);
        let mode = std::fs::metadata(d.path().join("v")).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o777, "default mode changed: {:o}", mode);
    }

    /// Asking for a group sets the setgid bit as well as the mode.
    /// Without setgid a 0770 volume is group-writable but every file
    /// created inside lands in its creator's primary group, unreadable
    /// to the volume's other consumers — which is the whole point of
    /// pointing an fsGroup at a shared volume.
    #[tokio::test]
    async fn a_requested_group_sets_the_setgid_bit() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        let gid = nix_current_gid();
        let mut req = cvreq("g", 1 << 20);
        req.dir_gid = gid;
        req.dir_mode = 0o770;
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(r.created, "{}", r.message);

        let mode = std::fs::metadata(d.path().join("g")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o770, "mode not applied: {:o}", mode);
        assert_eq!(mode & 0o2000, 0o2000, "setgid not set: {:o}", mode);
    }

    /// A durable sqlite-backed service, optionally with a FakeTgt-backed
    /// block-export reconciler attached — the block-class provision
    /// tests' shape. Returns the FakeTgt so tests can inspect the world
    /// the reconciler built.
    fn svc_sqlite(
        export: &std::path::Path,
        with_block_export: bool,
    ) -> (MdsControlService, Arc<crate::pnfs::mds::block_export::tests::FakeTgt>) {
        let registry = Arc::new(DeviceRegistry::new());
        let backend: Arc<dyn crate::state_backend::StateBackend> =
            Arc::new(crate::state_backend::SqliteBackend::open_in_memory().unwrap());
        let layout_manager = crate::pnfs::mds::layout::LayoutManager::new(
            Arc::clone(&registry),
            crate::pnfs::config::LayoutPolicy::Stripe,
            8 * 1024 * 1024,
            Arc::clone(&backend),
        );
        let tgt = Arc::new(crate::pnfs::mds::block_export::tests::FakeTgt::new());
        if with_block_export {
            layout_manager.attach_block_export(Arc::new(
                crate::pnfs::mds::block_export::BlockExportReconciler::new(
                    Arc::clone(&tgt) as Arc<dyn crate::nvmeof_export::SpdkRpcTransport + Send + Sync>,
                    backend,
                    "lvs_test".into(),
                    "10.0.0.9".into(),
                    4420,
                    "/var/tmp".into(),
                ),
            ));
        }
        (
            MdsControlService::new(
                registry,
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                export.to_path_buf(),
                layout_manager,
                2049,
                true,
                false,
            ),
            tgt,
        )
    }

    fn svc_nondurable(export: &std::path::Path) -> MdsControlService {
        let registry = Arc::new(DeviceRegistry::new());
        let layout_manager = crate::pnfs::mds::layout::LayoutManager::new(
            Arc::clone(&registry),
            crate::pnfs::config::LayoutPolicy::Stripe,
            8 * 1024 * 1024,
            Arc::new(crate::state_backend::MemoryBackend::new()),
        );
        MdsControlService::new(
            registry,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            export.to_path_buf(),
            layout_manager,
            2049,
            false,
            false,
        )
    }

    /// An MDS whose state does not survive a restart must REFUSE
    /// geometry rather than accept and lose it. `state.backend: memory`
    /// is not hypothetical — it is set in deployments/pnfs-mds-config.yaml
    /// and half the lima MDS configs. Accepting there would ack a
    /// provision, then silently revert to the fleet default on the next
    /// restart, leaving one volume striped two ways.
    #[tokio::test]
    async fn a_non_durable_mds_refuses_geometry() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc_nondurable(d.path());
        let mut req = cvreq("v", 1 << 20);
        req.stripe_size = 1 << 20;

        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(!r.created, "must refuse");
        assert!(r.message.contains("state.backend: memory"), "{}", r.message);
        assert!(!d.path().join("v").exists(), "nothing should have been created");
    }

    /// ...but a request WITHOUT geometry must still work there, or every
    /// existing dev rig and lima config breaks.
    #[tokio::test]
    async fn a_non_durable_mds_still_serves_volumes_without_geometry() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc_nondurable(d.path());
        let r = s.create_volume(Request::new(cvreq("plain", 1 << 20))).await.unwrap().into_inner();
        assert!(r.created, "{}", r.message);
        assert!(d.path().join("plain").is_dir());
    }

    /// An idempotent retry — which the CSI provisioner does routinely —
    /// must echo the geometry ACTUALLY IN FORCE, not zeros. Echoing zeros
    /// made the driver read "this MDS is too old", failing a PVC for a
    /// volume that had been created perfectly well. Regression test for a
    /// bug introduced by patching every response literal alike, without
    /// distinguishing the success paths from the failure paths.
    #[tokio::test]
    async fn an_idempotent_retry_echoes_the_recorded_geometry() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        let mut req = cvreq("retried", 1 << 20);
        req.stripe_size = 1 << 20;
        req.stripe_width = 2;

        let first = s.create_volume(Request::new(req.clone())).await.unwrap().into_inner();
        assert!(first.created);
        assert_eq!(first.effective_stripe_size, 1 << 20);

        let again = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(again.created, "retry must succeed");
        assert_eq!(again.message, "already exists");
        assert_eq!(again.effective_stripe_size, 1 << 20, "retry echoed the wrong stripe size");
        assert_eq!(again.effective_stripe_width, 2, "retry echoed the wrong stripe width");
    }

    /// A retry on a volume created BEFORE geometry existed must echo the
    /// MDS default, not zeros — otherwise every pre-existing pNFS PVC
    /// fails its next provisioner retry after the upgrade.
    #[tokio::test]
    async fn a_retry_on_a_volume_without_geometry_echoes_the_default() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        // Simulate a pre-upgrade volume: the directory exists, no
        // geometry was ever recorded for it.
        std::fs::create_dir(d.path().join("old")).unwrap();

        let r = s.create_volume(Request::new(cvreq("old", 1 << 20))).await.unwrap().into_inner();
        assert!(r.created);
        assert_eq!(r.effective_stripe_size, 8 * 1024 * 1024, "must echo the fleet default");
    }

    /// The geometry the MDS records must be echoed back, so the driver
    /// can tell "recorded what I asked" from "silently ignored it".
    #[tokio::test]
    async fn create_volume_echoes_the_geometry_it_recorded() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        let mut req = cvreq("geo", 1 << 20);
        req.stripe_size = 1 << 20;
        req.stripe_width = 3;
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(r.created, "{}", r.message);
        assert_eq!(r.effective_stripe_size, 1 << 20);
        assert_eq!(r.effective_stripe_width, 3);
    }

    /// An unset stripe size must be echoed as the MDS's DEFAULT, not as
    /// 0 — the driver reads a zero echo as "this MDS is too old".
    #[tokio::test]
    async fn an_unset_stripe_size_echoes_the_mds_default() {
        let d = tempfile::TempDir::new().unwrap();
        let s = svc(d.path());
        let r = s.create_volume(Request::new(cvreq("plain", 1 << 20))).await.unwrap().into_inner();
        assert_eq!(r.effective_stripe_size, 8 * 1024 * 1024);
    }

    /// The caller's real gid, so the chgrp in create_volume can succeed
    /// without root. Using an arbitrary gid would make the test assert
    /// on a chgrp that always fails.
    fn nix_current_gid() -> u32 {
        unsafe { libc::getgid() }
    }

    fn svc(export: &std::path::Path) -> MdsControlService {
        let registry = Arc::new(DeviceRegistry::new());
        let layout_manager = crate::pnfs::mds::layout::LayoutManager::new(
            Arc::clone(&registry),
            crate::pnfs::config::LayoutPolicy::Stripe,
            8 * 1024 * 1024,
            Arc::new(crate::state_backend::MemoryBackend::new()),
        );
        MdsControlService::new(
            registry,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            export.to_path_buf(),
            layout_manager,
            2049,
            true,
            false,
        )
    }

    /// F68b: with `verifyDsReachability` on, a registration whose
    /// client-path endpoint does not accept connects is NACKed and the
    /// registry stays empty — the device must never turn grantable
    /// while the endpoint GETDEVICEINFO would advertise is a black
    /// hole (the kernel blacklists such a deviceid for 120s and
    /// silently proxies all I/O via the MDS). Once a listener exists,
    /// the same registration is accepted.
    #[tokio::test]
    async fn f68b_registration_gated_on_endpoint_reachability() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(DeviceRegistry::new());
        let layout_manager = crate::pnfs::mds::layout::LayoutManager::new(
            Arc::clone(&registry),
            crate::pnfs::config::LayoutPolicy::Stripe,
            8 * 1024 * 1024,
            Arc::new(crate::state_backend::MemoryBackend::new()),
        );
        // Reserve a port, then close the listener: connect-refused,
        // deterministically, with no risk of colliding with a real
        // service.
        let parked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = parked.local_addr().unwrap().to_string();
        drop(parked);

        let s = MdsControlService::new(
            Arc::clone(&registry),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            dir.path().to_path_buf(),
            layout_manager,
            2049,
            false,
            true, // verify_ds_reachability
        );
        let reg_req = |ep: &str| RegisterRequest {
            device_id: "ds-f68b".into(),
            endpoint: ep.into(),
            multipath_endpoints: vec![],
            mount_points: vec!["/data".into()],
            capacity: 1 << 30,
            used: 0,
            protocol_version: 1,
            identity_created_at: 0,
            control_port: 0,
        };

        let r = s
            .register_data_server(Request::new(reg_req(&dead_addr)))
            .await
            .unwrap()
            .into_inner();
        assert!(!r.accepted, "unreachable endpoint must NACK");
        assert!(r.message.contains("not reachable"), "message: {}", r.message);
        assert_eq!(registry.count(), 0, "a NACKed device must not be registered");

        let live = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = live.local_addr().unwrap().to_string();
        let r = s
            .register_data_server(Request::new(reg_req(&live_addr)))
            .await
            .unwrap()
            .into_inner();
        assert!(r.accepted, "reachable endpoint must register: {}", r.message);
        assert_eq!(registry.count(), 1);

        // Multipath: the gate dials EVERY advertised address. A live
        // primary with a dead extra must still NACK — a broken trunk
        // address never fails the mount, it just silently loses the
        // bandwidth it was supposed to add.
        let mut req = reg_req(&live_addr);
        req.device_id = "ds-f68b-mp".into();
        req.multipath_endpoints = vec![dead_addr.clone()];
        let r = s
            .register_data_server(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        assert!(!r.accepted, "dead multipath extra must NACK");
        assert!(r.message.contains(&dead_addr), "message names the dead extra: {}", r.message);

        // All-live multipath registers, and the registry carries the
        // extras (they become GETDEVICEINFO netaddr4 entries).
        let live2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live2_addr = live2.local_addr().unwrap().to_string();
        let mut req = reg_req(&live_addr);
        req.device_id = "ds-f68b-mp".into();
        req.multipath_endpoints = vec![live2_addr.clone()];
        let r = s
            .register_data_server(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        assert!(r.accepted, "all-live multipath must register: {}", r.message);
        let d = registry.get("ds-f68b-mp").unwrap();
        assert_eq!(d.endpoints, vec![live2_addr]);
    }

    #[tokio::test]
    async fn create_then_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        let r = s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "pvc-abc".into(),
            size_bytes: 1024 * 1024, stripe_size: 0, stripe_width: 0, dir_gid: 0, dir_mode: 0, layout_class: String::new(), })).await.unwrap().into_inner();
        assert!(r.created, "create should succeed: {}", r.message);
        assert_eq!(r.volume_file, "pvc-abc");
        assert!(r.directory, "new volumes are directory subtrees");
        let path = dir.path().join("pvc-abc");
        assert!(std::fs::metadata(&path).unwrap().is_dir());

        let r = s.delete_volume(Request::new(DeleteVolumeRequest {
            volume_id: "pvc-abc".into(),
        })).await.unwrap().into_inner();
        assert!(r.deleted);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn create_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        let req = CreateVolumeRequest { volume_id: "v1".into(), size_bytes: 4096, stripe_size: 0, stripe_width: 0, dir_gid: 0, dir_mode: 0, layout_class: String::new(), };
        assert!(s.create_volume(Request::new(req.clone())).await.unwrap().into_inner().created);
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(r.created, "second call should also succeed");
        assert_eq!(r.message, "already exists");
        assert!(r.directory);
    }

    /// A pre-upgrade sparse-file volume keeps its legacy semantics:
    /// same-size re-create succeeds (directory=false so NodePublish
    /// keeps the root mount), size mismatch is refused, delete removes
    /// the file.
    #[tokio::test]
    async fn legacy_file_volume_semantics_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        let path = dir.path().join("v-legacy");
        let f = std::fs::OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
        f.set_len(4096).unwrap();

        let r = s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "v-legacy".into(), size_bytes: 4096, stripe_size: 0, stripe_width: 0, dir_gid: 0, dir_mode: 0, layout_class: String::new(), })).await.unwrap().into_inner();
        assert!(r.created, "same-size re-create of a legacy file: {}", r.message);
        assert!(!r.directory, "legacy volume must NOT be advertised as a directory");

        let r = s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "v-legacy".into(), size_bytes: 8192, stripe_size: 0, stripe_width: 0, dir_gid: 0, dir_mode: 0, layout_class: String::new(), })).await.unwrap().into_inner();
        assert!(!r.created);
        assert!(r.message.contains("refusing to resize"));

        let r = s.delete_volume(Request::new(DeleteVolumeRequest {
            volume_id: "v-legacy".into(),
        })).await.unwrap().into_inner();
        assert!(r.deleted);
        assert!(!path.exists());
    }

    /// Deleting a directory volume reclaims every placement under its
    /// `<volume_id>/` prefix — and never a sibling volume whose name
    /// merely starts with the same characters.
    #[tokio::test]
    async fn delete_directory_volume_sweeps_placements_prefix_safe() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        for v in ["vol", "volume2"] {
            let r = s.create_volume(Request::new(CreateVolumeRequest {
                volume_id: v.into(), size_bytes: 4096, stripe_size: 0, stripe_width: 0, dir_gid: 0, dir_mode: 0, layout_class: String::new(), })).await.unwrap().into_inner();
            assert!(r.created);
        }
        let rec = |key: &str| crate::state_backend::PlacementRecord {
            file_key: key.into(),
            stripe_size: 8 * 1024 * 1024,
            device_ids: vec!["ds-a".into(), "ds-b".into()],
            file_id: 7,
            truncate_pending: None,
            truncate_since_unix: None,
        };
        s.layout_manager.load_placement_records(vec![
            rec("vol/a.parquet"),
            rec("vol/sub/b.parquet"),
            rec("volume2/c.parquet"),
        ]);

        let r = s.delete_volume(Request::new(DeleteVolumeRequest {
            volume_id: "vol".into(),
        })).await.unwrap().into_inner();
        assert!(r.deleted);
        assert!(!dir.path().join("vol").exists());
        assert!(!s.layout_manager.has_placement("vol/a.parquet"));
        assert!(!s.layout_manager.has_placement("vol/sub/b.parquet"));
        assert!(
            s.layout_manager.has_placement("volume2/c.parquet"),
            "prefix sweep must not cross into volume2 (foo vs foobar)"
        );
        assert!(dir.path().join("volume2").exists());
    }

    /// A directory volume with content deletes cleanly (remove_dir_all,
    /// not the old remove_file which would EISDIR / ENOTEMPTY).
    #[tokio::test]
    async fn delete_directory_volume_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "busy".into(), size_bytes: 4096, stripe_size: 0, stripe_width: 0, dir_gid: 0, dir_mode: 0, layout_class: String::new(), })).await.unwrap();
        std::fs::create_dir_all(dir.path().join("busy/deep/tree")).unwrap();
        std::fs::write(dir.path().join("busy/deep/tree/f.bin"), b"data").unwrap();

        let r = s.delete_volume(Request::new(DeleteVolumeRequest {
            volume_id: "busy".into(),
        })).await.unwrap().into_inner();
        assert!(r.deleted, "{}", r.message);
        assert!(!dir.path().join("busy").exists());
    }

    #[tokio::test]
    async fn delete_absent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        let r = s.delete_volume(Request::new(DeleteVolumeRequest {
            volume_id: "never-existed".into(),
        })).await.unwrap().into_inner();
        assert!(r.deleted);
        assert_eq!(r.message, "already absent");
    }

    #[tokio::test]
    async fn rejects_path_traversal_in_volume_id() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        for bad in &["", "../escape", "a/b", "with\0nul"] {
            let r = s.create_volume(Request::new(CreateVolumeRequest {
                volume_id: (*bad).into(), size_bytes: 1024, stripe_size: 0, stripe_width: 0, dir_gid: 0, dir_mode: 0, layout_class: String::new(), })).await.unwrap().into_inner();
            assert!(!r.created, "should reject {:?}", bad);
        }
    }
}

