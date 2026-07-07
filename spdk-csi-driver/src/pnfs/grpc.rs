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
    ) -> Self {
        Self {
            device_registry,
            configured_endpoints,
            configured_control_endpoints,
            export_path,
            layout_manager,
            nfs_port,
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
        
        // Override the DS-reported endpoint with the operator-configured
        // one for this device_id. The DS only knows its bind address
        // (typically 0.0.0.0); the client needs the externally-reachable
        // endpoint the operator has set up (e.g. a Service IP, an
        // out-of-cluster IP, or in dev a hostname like host.lima.internal).
        let effective_endpoint = self.configured_endpoints
            .get(&req.device_id)
            .cloned()
            .unwrap_or_else(|| req.endpoint.clone());
        if effective_endpoint != req.endpoint {
            info!(
                "📝 DS Registration: device_id={}, ds-reported endpoint={} → using configured endpoint={}, capacity={} bytes",
                req.device_id, req.endpoint, effective_endpoint, req.capacity,
            );
        } else {
            info!(
                "📝 DS Registration: device_id={}, endpoint={}, capacity={} bytes",
                req.device_id, effective_endpoint, req.capacity,
            );
        }

        // Create device info
        let mut device_info = crate::pnfs::mds::device::DeviceInfo::new(
            req.device_id.clone(),
            effective_endpoint,
            req.mount_points.clone(),
        );

        device_info.endpoints = req.multipath_endpoints.clone();
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
                info!(
                    "📦 CreateVolume: directory volume {} already exists",
                    req.volume_id
                );
                return Ok(Response::new(CreateVolumeResponse {
                    created: true,
                    export_path: export_str,
                    volume_file: req.volume_id,
                    message: "already exists".into(),
                    nfs_port: self.nfs_port as u32,
                    directory: true,
                }));
            }
            if meta.len() == req.size_bytes {
                info!(
                    "📦 CreateVolume: legacy file volume {} already exists at correct size ({} bytes)",
                    req.volume_id, req.size_bytes
                );
                return Ok(Response::new(CreateVolumeResponse {
                    created: true,
                    export_path: export_str,
                    volume_file: req.volume_id,
                    message: "already exists".into(),
                    nfs_port: self.nfs_port as u32,
                    directory: false,
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
            }));
        }
        // World-writable like the export rigs: the consuming pod's uid
        // is arbitrary (Spark executors, app containers) and NFS has
        // no idmapping story here. The dir, not the export, is the
        // isolation boundary.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &file_path,
                std::fs::Permissions::from_mode(0o777),
            );
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
        };

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
        )
    }

    #[tokio::test]
    async fn create_then_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        let r = s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "pvc-abc".into(),
            size_bytes: 1024 * 1024,
        })).await.unwrap().into_inner();
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
        let req = CreateVolumeRequest { volume_id: "v1".into(), size_bytes: 4096 };
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
            volume_id: "v-legacy".into(), size_bytes: 4096,
        })).await.unwrap().into_inner();
        assert!(r.created, "same-size re-create of a legacy file: {}", r.message);
        assert!(!r.directory, "legacy volume must NOT be advertised as a directory");

        let r = s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "v-legacy".into(), size_bytes: 8192,
        })).await.unwrap().into_inner();
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
                volume_id: v.into(), size_bytes: 4096,
            })).await.unwrap().into_inner();
            assert!(r.created);
        }
        let rec = |key: &str| crate::state_backend::PlacementRecord {
            file_key: key.into(),
            stripe_size: 8 * 1024 * 1024,
            device_ids: vec!["ds-a".into(), "ds-b".into()],
            file_id: 7,
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
            volume_id: "busy".into(), size_bytes: 4096,
        })).await.unwrap();
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
                volume_id: (*bad).into(), size_bytes: 1024,
            })).await.unwrap().into_inner();
            assert!(!r.created, "should reject {:?}", bad);
        }
    }
}

