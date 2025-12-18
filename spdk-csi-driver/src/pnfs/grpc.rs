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
}

impl MdsControlService {
    /// Create a new MDS control service
    pub fn new(device_registry: Arc<crate::pnfs::mds::device::DeviceRegistry>) -> Self {
        Self { device_registry }
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
        
        info!(
            "📝 DS Registration: device_id={}, endpoint={}, capacity={} bytes",
            req.device_id, req.endpoint, req.capacity
        );

        // Create device info
        let mut device_info = crate::pnfs::mds::device::DeviceInfo::new(
            req.device_id.clone(),
            req.endpoint.clone(),
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

