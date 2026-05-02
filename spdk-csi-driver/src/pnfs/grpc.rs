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
    /// Absolute path of the MDS export root. CreateVolume creates files
    /// under this directory; the CSI driver's NodePublish points the
    /// kernel client at this path.
    export_path: std::path::PathBuf,
}

impl MdsControlService {
    /// Create a new MDS control service. `configured_endpoints` is the
    /// operator's view of `device_id → reachable endpoint` taken from
    /// the MDS config's `dataServers` list. `export_path` is the MDS
    /// export root from the same config.
    pub fn new(
        device_registry: Arc<crate::pnfs::mds::device::DeviceRegistry>,
        configured_endpoints: std::collections::HashMap<String, String>,
        export_path: std::path::PathBuf,
    ) -> Self {
        Self { device_registry, configured_endpoints, export_path }
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
                "📝 DS Registration: device_id={}, ds-reported endpoint={} → using configured endpoint={}",
                req.device_id, req.endpoint, effective_endpoint,
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

        Ok(Response::new(HeartbeatResponse {
            acknowledged: true,
            instructions: vec![],  // TODO: Add instructions based on MDS state
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

    /// Provision a new pNFS volume by creating its metadata file.
    ///
    /// The file lives under the MDS export and is sized to
    /// `size_bytes` via `set_len` (sparse). The kernel client mounts
    /// the export root and discovers this file by name; LAYOUTGET
    /// against it returns segments striped across all registered DSes.
    ///
    /// Idempotent: re-creating an existing volume with the same size
    /// is success; size mismatch is an error so a stale volume_id
    /// can't silently re-use a smaller file.
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
            }));
        }

        let file_path = self.export_path.join(&req.volume_id);
        let export_str = self.export_path.to_string_lossy().into_owned();

        // Existing-file path: if it's already there at the right size,
        // success; if size differs, error so the caller doesn't shrink
        // or grow a live volume by accident.
        if let Ok(meta) = std::fs::metadata(&file_path) {
            if meta.len() == req.size_bytes {
                info!(
                    "📦 CreateVolume: {} already exists at correct size ({} bytes)",
                    req.volume_id, req.size_bytes
                );
                return Ok(Response::new(CreateVolumeResponse {
                    created: true,
                    export_path: export_str,
                    volume_file: req.volume_id,
                    message: "already exists".into(),
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
            }));
        }

        let f = match std::fs::OpenOptions::new()
            .create_new(true).write(true).open(&file_path)
        {
            Ok(f) => f,
            Err(e) => {
                warn!("CreateVolume: open({:?}): {}", file_path, e);
                return Ok(Response::new(CreateVolumeResponse {
                    created: false,
                    export_path: String::new(),
                    volume_file: String::new(),
                    message: format!("create file: {}", e),
                }));
            }
        };
        if let Err(e) = f.set_len(req.size_bytes) {
            warn!("CreateVolume: set_len({}): {}", req.size_bytes, e);
            // Best-effort cleanup so the next attempt isn't blocked by
            // a half-created file.
            let _ = std::fs::remove_file(&file_path);
            return Ok(Response::new(CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: format!("set_len: {}", e),
            }));
        }

        info!(
            "📦 CreateVolume: created {} ({} bytes) at {:?}",
            req.volume_id, req.size_bytes, file_path
        );
        Ok(Response::new(CreateVolumeResponse {
            created: true,
            export_path: export_str,
            volume_file: req.volume_id,
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
        match std::fs::remove_file(&file_path) {
            Ok(()) => {
                info!("🗑️  DeleteVolume: removed {:?}", file_path);
                Ok(Response::new(DeleteVolumeResponse {
                    deleted: true,
                    message: String::new(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("🗑️  DeleteVolume: {} already absent", req.volume_id);
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
        MdsControlService::new(
            Arc::new(DeviceRegistry::new()),
            std::collections::HashMap::new(),
            export.to_path_buf(),
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
        let path = dir.path().join("pvc-abc");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024 * 1024);

        let r = s.delete_volume(Request::new(DeleteVolumeRequest {
            volume_id: "pvc-abc".into(),
        })).await.unwrap().into_inner();
        assert!(r.deleted);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn create_idempotent_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        let req = CreateVolumeRequest { volume_id: "v1".into(), size_bytes: 4096 };
        assert!(s.create_volume(Request::new(req.clone())).await.unwrap().into_inner().created);
        let r = s.create_volume(Request::new(req)).await.unwrap().into_inner();
        assert!(r.created, "second call should also succeed");
        assert_eq!(r.message, "already exists");
    }

    #[tokio::test]
    async fn create_size_mismatch_errors() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "v1".into(), size_bytes: 4096,
        })).await.unwrap();
        let r = s.create_volume(Request::new(CreateVolumeRequest {
            volume_id: "v1".into(), size_bytes: 8192,
        })).await.unwrap().into_inner();
        assert!(!r.created);
        assert!(r.message.contains("refusing to resize"));
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

