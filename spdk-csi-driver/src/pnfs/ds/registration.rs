//! MDS Registration
//!
//! Handles Data Server registration with the Metadata Server using gRPC.
//! Maintains connectivity through periodic heartbeats.
//!
//! # Communication Protocol
//! 
//! Uses gRPC (tonic) for MDS-DS communication:
//! - Type-safe protocol buffers
//! - Automatic retries and connection management
//! - Streaming support for future optimizations

use crate::pnfs::Result;
use crate::pnfs::grpc::{MdsControlClient, RegisterRequest, HeartbeatRequest, UnregisterRequest, HealthStatus};
use std::time::Duration;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

/// MDS registration client (gRPC-based)
pub struct RegistrationClient {
    device_id: String,
    mds_endpoint: String,
    grpc_client: Option<MdsControlClient<Channel>>,
}

impl RegistrationClient {
    /// Create a new registration client
    pub fn new(
        device_id: String,
        mds_endpoint: String,
        _heartbeat_interval: Duration,
    ) -> Self {
        Self {
            device_id,
            mds_endpoint,
            grpc_client: None,
        }
    }

    /// Connect to MDS gRPC service
    async fn connect(&mut self) -> Result<()> {
        if self.grpc_client.is_some() {
            return Ok(());  // Already connected
        }

        // Add http:// prefix if not present
        let endpoint = if self.mds_endpoint.starts_with("http://") || 
                          self.mds_endpoint.starts_with("https://") {
            self.mds_endpoint.clone()
        } else {
            format!("http://{}", self.mds_endpoint)
        };

        info!("Connecting to MDS gRPC service at {}", endpoint);

        match MdsControlClient::connect(endpoint).await {
            Ok(client) => {
                info!("✅ Connected to MDS gRPC service");
                self.grpc_client = Some(client);
                Ok(())
            }
            Err(e) => {
                error!("❌ Failed to connect to MDS: {}", e);
                Err(crate::pnfs::Error::Registration(format!(
                    "gRPC connection failed: {}",
                    e
                )))
            }
        }
    }

    /// Register with MDS via gRPC
    pub async fn register(
        &mut self,
        device_id: String,
        endpoint: String,
        mount_points: Vec<String>,
        capacity: u64,
        used: u64,
    ) -> Result<bool> {
        info!(
            "Registering device {} with MDS at {}",
            device_id, self.mds_endpoint
        );

        // Ensure connected
        self.connect().await?;

        let client = self.grpc_client.as_mut()
            .ok_or_else(|| crate::pnfs::Error::Registration(
                "Not connected to MDS".to_string()
            ))?;

        let request = tonic::Request::new(RegisterRequest {
            device_id: device_id.clone(),
            endpoint,
            multipath_endpoints: vec![],
            mount_points,
            capacity,
            used,
            protocol_version: 1,
        });

        match client.register_data_server(request).await {
            Ok(response) => {
                let resp = response.into_inner();
                if resp.accepted {
                    info!("✅ Registration successful: {}", resp.message);
                    Ok(true)
                } else {
                    warn!("❌ Registration rejected: {}", resp.message);
                    Ok(false)
                }
            }
            Err(e) => {
                error!("❌ Registration gRPC call failed: {}", e);
                Err(crate::pnfs::Error::Registration(format!(
                    "gRPC call failed: {}",
                    e
                )))
            }
        }
    }

    /// Send heartbeat to MDS via gRPC
    pub async fn heartbeat(
        &mut self,
        capacity: u64,
        used: u64,
        active_connections: u32,
    ) -> Result<bool> {
        // Ensure connected
        if let Err(e) = self.connect().await {
            warn!("Failed to connect for heartbeat: {}", e);
            return Ok(false);
        }

        let client = self.grpc_client.as_mut()
            .ok_or_else(|| crate::pnfs::Error::Registration(
                "Not connected to MDS".to_string()
            ))?;

        let request = tonic::Request::new(HeartbeatRequest {
            device_id: self.device_id.clone(),
            capacity,
            used,
            active_connections,
            health: HealthStatus::Healthy as i32,
        });

        match client.heartbeat(request).await {
            Ok(response) => {
                let resp = response.into_inner();
                if resp.acknowledged {
                    debug!("✅ Heartbeat acknowledged by MDS");
                    Ok(true)
                } else {
                    warn!("⚠️ Heartbeat not acknowledged by MDS");
                    Ok(false)
                }
            }
            Err(e) => {
                warn!("❌ Heartbeat gRPC call failed: {}", e);
                Ok(false)  // Don't fail, just log
            }
        }
    }

    /// Unregister from MDS (clean shutdown)
    pub async fn unregister(&mut self) -> Result<()> {
        info!(
            "Unregistering device {} from MDS at {}",
            self.device_id, self.mds_endpoint
        );

        if let Some(client) = self.grpc_client.as_mut() {
            let request = tonic::Request::new(UnregisterRequest {
                device_id: self.device_id.clone(),
                reason: "Clean shutdown".to_string(),
            });

            match client.unregister_data_server(request).await {
                Ok(_) => {
                    info!("✅ Unregistered successfully from MDS");
                }
                Err(e) => {
                    warn!("Failed to unregister from MDS: {}", e);
                }
            }
        }

        Ok(())
    }
}


