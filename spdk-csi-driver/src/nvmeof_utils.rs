//! NVMe-oF utilities and centralized logging
//!
//! This module provides common utilities, error handling, and structured logging
//! for NVMe-oF operations to reduce code duplication and improve observability.

use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{debug, info, warn, error};

/// Structured context for NVMe-oF operations
#[derive(Debug, Clone)]
pub struct NvmfContext {
    pub operation_id: String,
    pub volume_id: Option<String>,
    pub node_id: String,
    pub target_ip: Option<String>,
    pub target_port: Option<String>,
    pub nqn: Option<String>,
    pub bdev_name: Option<String>,
}

impl NvmfContext {
    pub fn new(node_id: String, operation: &str) -> Self {
        let operation_id = format!("{}_{}", operation, uuid::Uuid::new_v4().to_string()[..8].to_string());
        Self {
            operation_id,
            volume_id: None,
            node_id,
            target_ip: None,
            target_port: None,
            nqn: None,
            bdev_name: None,
        }
    }

    pub fn with_volume(mut self, volume_id: String) -> Self {
        self.volume_id = Some(volume_id);
        self
    }

    pub fn with_target(mut self, ip: String, port: String) -> Self {
        self.target_ip = Some(ip);
        self.target_port = Some(port);
        self
    }

    pub fn with_nqn(mut self, nqn: String) -> Self {
        self.nqn = Some(nqn);
        self
    }

    pub fn with_bdev(mut self, bdev_name: String) -> Self {
        self.bdev_name = Some(bdev_name);
        self
    }

    /// Get a formatted context string for logging
    pub fn log_prefix(&self) -> String {
        let mut parts = vec![format!("op:{}", self.operation_id)];
        
        if let Some(ref vol) = self.volume_id {
            parts.push(format!("vol:{}", vol));
        }
        
        parts.push(format!("node:{}", self.node_id));
        
        if let (Some(ref ip), Some(ref port)) = (&self.target_ip, &self.target_port) {
            parts.push(format!("target:{}:{}", ip, port));
        }

        format!("[NVMF:{}]", parts.join(","))
    }
}

/// Performance metrics for NVMe-oF operations
#[derive(Debug, Default)]
pub struct NvmfMetrics {
    pub connection_time_ms: Option<u64>,
    pub verification_time_ms: Option<u64>,
    pub total_time_ms: Option<u64>,
    pub retry_count: u32,
    pub network_test_time_ms: Option<u64>,
    pub rpc_call_time_ms: Option<u64>,
}

impl NvmfMetrics {
    pub fn log_summary(&self, ctx: &NvmfContext) {
        info!(
            prefix = ctx.log_prefix(),
            total_time_ms = ?self.total_time_ms,
            connection_time_ms = ?self.connection_time_ms,
            verification_time_ms = ?self.verification_time_ms,
            network_test_time_ms = ?self.network_test_time_ms,
            rpc_call_time_ms = ?self.rpc_call_time_ms,
            retry_count = self.retry_count,
            "Performance Summary"
        );
    }
}

/// Standardized error types for NVMe-oF operations
#[derive(Debug, Clone)]
pub enum NvmfError {
    NetworkUnreachable { target: String, details: String },
    TargetNotConfigured { nqn: String, details: String },
    BdevNotFound { bdev_name: String, details: String },
    ConnectionExists { nqn: String },
    RpcTimeout { operation: String, timeout_ms: u64 },
    ValidationFailed { resource: String, details: String },
    Unknown { details: String },
}

impl NvmfError {
    /// Parse SPDK error responses into structured error types
    pub fn from_spdk_error(error_text: &str, operation: &str) -> Self {
        let error_lower = error_text.to_lowercase();
        
        if error_lower.contains("connection refused") || error_lower.contains("no route to host") {
            NvmfError::NetworkUnreachable {
                target: "unknown".to_string(),
                details: error_text.to_string(),
            }
        } else if error_lower.contains("no such device") || error_lower.contains("not found") {
            NvmfError::TargetNotConfigured {
                nqn: "unknown".to_string(),
                details: error_text.to_string(),
            }
        } else if error_lower.contains("already exists") {
            NvmfError::ConnectionExists {
                nqn: "unknown".to_string(),
            }
        } else if error_lower.contains("timeout") {
            NvmfError::RpcTimeout {
                operation: operation.to_string(),
                timeout_ms: 30000, // Default assumption
            }
        } else {
            NvmfError::Unknown {
                details: error_text.to_string(),
            }
        }
    }

    /// Get user-friendly error message with troubleshooting hints
    pub fn user_message(&self) -> String {
        match self {
            NvmfError::NetworkUnreachable { target, .. } => {
                format!("Cannot reach NVMe-oF target {}. Check network connectivity and firewall rules.", target)
            }
            NvmfError::TargetNotConfigured { nqn, .. } => {
                format!("NVMe-oF target {} is not properly configured. Verify the subsystem exists and is listening.", nqn)
            }
            NvmfError::BdevNotFound { bdev_name, .. } => {
                format!("Block device {} not found. Verify the device exists in SPDK.", bdev_name)
            }
            NvmfError::ConnectionExists { nqn } => {
                format!("NVMe-oF connection to {} already exists (this is usually okay).", nqn)
            }
            NvmfError::RpcTimeout { operation, timeout_ms } => {
                format!("Operation {} timed out after {}ms. Target may be overloaded or unreachable.", operation, timeout_ms)
            }
            NvmfError::ValidationFailed { resource, details } => {
                format!("Validation failed for {}: {}", resource, details)
            }
            NvmfError::Unknown { details } => {
                format!("Unknown error: {}", details)
            }
        }
    }

    /// Log detailed error information with troubleshooting context
    pub fn log_detailed(&self, ctx: &NvmfContext) {
        let prefix = ctx.log_prefix();
        match self {
            NvmfError::NetworkUnreachable { target, details } => {
                error!(
                    prefix,
                    target,
                    details,
                    hint = "Check if target node is running, verify firewall allows port access",
                    "Network Unreachable"
                );
            }
            NvmfError::TargetNotConfigured { nqn, details } => {
                error!(
                    prefix,
                    nqn,
                    details,
                    hint = "Check if subsystem exists, verify listener is configured on correct port",
                    "Target Not Configured"
                );
            }
            NvmfError::BdevNotFound { bdev_name, details } => {
                error!(
                    prefix,
                    bdev_name,
                    details,
                    hint = "Run spdk_rpc bdev_get_bdevs, check if logical volume exists",
                    "Block Device Not Found"
                );
            }
            NvmfError::ConnectionExists { nqn } => {
                info!(
                    prefix,
                    nqn,
                    "Connection Already Exists (this is usually not an error)"
                );
            }
            _ => {
                error!(prefix, message = self.user_message(), "Error");
            }
        }
    }
}

/// Centralized SPDK RPC response handler with standardized error parsing
pub async fn handle_spdk_response(
    response: Value,
    operation: &str,
    ctx: &NvmfContext,
) -> Result<Value, NvmfError> {
    let prefix = ctx.log_prefix();

    // Check for SPDK RPC errors
    if let Some(error) = response.get("error") {
        let error_text = error.to_string();
        error!(prefix, operation, error_text, "SPDK RPC Error");

        let structured_error = NvmfError::from_spdk_error(&error_text, operation);
        structured_error.log_detailed(ctx);

        return Err(structured_error);
    }

    // Extract result
    if let Some(result) = response.get("result") {
        debug!(prefix, operation, "SPDK RPC Success");
        Ok(result.clone())
    } else {
        warn!(prefix, ?response, "SPDK RPC Response Missing Result Field");
        Err(NvmfError::ValidationFailed {
            resource: format!("SPDK RPC {}", operation),
            details: "Response missing 'result' field".to_string(),
        })
    }
}

/// Perform network connectivity test with timing and detailed diagnostics
pub async fn test_network_connectivity(
    target_ip: &str,
    target_port: &str,
    ctx: &NvmfContext,
) -> Result<Duration, NvmfError> {
    let prefix = ctx.log_prefix();
    debug!(prefix, target_ip, target_port, "Testing network connectivity");

    let start = Instant::now();

    match timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(format!("{}:{}", target_ip, target_port))
    ).await {
        Ok(Ok(_stream)) => {
            let duration = start.elapsed();
            debug!(prefix, ?duration, "Network connectivity test passed");
            Ok(duration)
        }
        Ok(Err(e)) => {
            let duration = start.elapsed();
            error!(prefix, ?duration, error = %e, "Network connectivity test failed");

            Err(NvmfError::NetworkUnreachable {
                target: format!("{}:{}", target_ip, target_port),
                details: e.to_string(),
            })
        }
        Err(_) => {
            let duration = start.elapsed();
            error!(prefix, ?duration, "Network connectivity test timed out");

            Err(NvmfError::RpcTimeout {
                operation: "network_test".to_string(),
                timeout_ms: 10000,
            })
        }
    }
}

/// Enhanced retry logic with exponential backoff and detailed logging
pub async fn retry_with_backoff<F, T, E>(
    operation_name: &str,
    ctx: &NvmfContext,
    max_retries: u32,
    initial_delay: Duration,
    operation: F,
) -> Result<T, E>
where
    F: Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, E>> + Send>>,
    E: std::fmt::Display,
{
    let prefix = ctx.log_prefix();
    let mut delay = initial_delay;

    for attempt in 1..=max_retries {
        debug!(prefix, operation_name, attempt, max_retries, "Attempting operation");

        let start = Instant::now();
        match operation().await {
            Ok(result) => {
                let duration = start.elapsed();
                debug!(prefix, operation_name, attempt, ?duration, "Operation succeeded");
                return Ok(result);
            }
            Err(e) => {
                let duration = start.elapsed();
                warn!(prefix, operation_name, attempt, ?duration, error = %e, "Operation failed");

                if attempt < max_retries {
                    debug!(prefix, ?delay, "Retrying after delay");
                    tokio::time::sleep(delay).await;
                    delay = Duration::from_millis((delay.as_millis() as u64 * 2).min(30000)); // Cap at 30s
                } else {
                    error!(prefix, operation_name, max_retries, "Operation failed after all attempts");
                    return Err(e);
                }
            }
        }
    }

    unreachable!()
}

/// Log connection state and health information
pub async fn log_connection_health(
    ctx: &NvmfContext,
    _spdk_rpc_url: &str,
) {
    let prefix = ctx.log_prefix();
    debug!(prefix, "Connection Health Check (placeholder - actual health checks not yet implemented)");

    // TODO: Import and use call_spdk_rpc when available to query:
    // - NVMe controllers status via bdev_nvme_get_controllers
    // - bdev status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nvmf_context_log_prefix() {
        let ctx = NvmfContext::new("node1".to_string(), "connect")
            .with_volume("vol-123".to_string())
            .with_target("192.168.1.100".to_string(), "4420".to_string())
            .with_nqn("nqn.2023.com.example:volume".to_string());
        
        let prefix = ctx.log_prefix();
        assert!(prefix.contains("op:connect_"));
        assert!(prefix.contains("vol:vol-123"));
        assert!(prefix.contains("node:node1"));
        assert!(prefix.contains("target:192.168.1.100:4420"));
    }

    #[test]
    fn test_nvmf_error_classification() {
        let error = NvmfError::from_spdk_error("Connection refused", "connect");
        matches!(error, NvmfError::NetworkUnreachable { .. });
        
        let error = NvmfError::from_spdk_error("already exists", "create");
        matches!(error, NvmfError::ConnectionExists { .. });
    }
}