//! NVMe-oF utilities and centralized logging
//! 
//! This module provides common utilities, error handling, and structured logging
//! for NVMe-oF operations to reduce code duplication and improve observability.

use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::time::timeout;

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
        println!("{} 📊 Performance Summary:", ctx.log_prefix());
        
        if let Some(total) = self.total_time_ms {
            println!("{}   Total Operation Time: {}ms", ctx.log_prefix(), total);
        }
        
        if let Some(conn) = self.connection_time_ms {
            println!("{}   Connection Time: {}ms", ctx.log_prefix(), conn);
        }
        
        if let Some(verify) = self.verification_time_ms {
            println!("{}   Verification Time: {}ms", ctx.log_prefix(), verify);
        }
        
        if let Some(network) = self.network_test_time_ms {
            println!("{}   Network Test Time: {}ms", ctx.log_prefix(), network);
        }
        
        if let Some(rpc) = self.rpc_call_time_ms {
            println!("{}   RPC Call Time: {}ms", ctx.log_prefix(), rpc);
        }
        
        if self.retry_count > 0 {
            println!("{}   Retry Count: {}", ctx.log_prefix(), self.retry_count);
        }
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
        match self {
            NvmfError::NetworkUnreachable { target, details } => {
                println!("{}❌ Network Unreachable:", ctx.log_prefix());
                println!("{}   Target: {}", ctx.log_prefix(), target);
                println!("{}   Details: {}", ctx.log_prefix(), details);
                println!("{}   💡 Troubleshooting:", ctx.log_prefix());
                println!("{}     - Check if target node is running", ctx.log_prefix());
                println!("{}     - Verify firewall allows port access", ctx.log_prefix());
                println!("{}     - Test with: telnet {} <port>", ctx.log_prefix(), target);
            }
            NvmfError::TargetNotConfigured { nqn, details } => {
                println!("{}❌ Target Not Configured:", ctx.log_prefix());
                println!("{}   NQN: {}", ctx.log_prefix(), nqn);
                println!("{}   Details: {}", ctx.log_prefix(), details);
                println!("{}   💡 Troubleshooting:", ctx.log_prefix());
                println!("{}     - Check if subsystem {} exists", ctx.log_prefix(), nqn);
                println!("{}     - Verify listener is configured on correct port", ctx.log_prefix());
                println!("{}     - Run: spdk_rpc nvmf_get_subsystems", ctx.log_prefix());
            }
            NvmfError::BdevNotFound { bdev_name, details } => {
                println!("{}❌ Block Device Not Found:", ctx.log_prefix());
                println!("{}   Bdev: {}", ctx.log_prefix(), bdev_name);
                println!("{}   Details: {}", ctx.log_prefix(), details);
                println!("{}   💡 Troubleshooting:", ctx.log_prefix());
                println!("{}     - Run: spdk_rpc bdev_get_bdevs", ctx.log_prefix());
                println!("{}     - Check if logical volume exists", ctx.log_prefix());
            }
            NvmfError::ConnectionExists { nqn } => {
                println!("{}ℹ️ Connection Already Exists:", ctx.log_prefix());
                println!("{}   NQN: {}", ctx.log_prefix(), nqn);
                println!("{}   This is usually not an error - connection reuse is normal", ctx.log_prefix());
            }
            _ => {
                println!("{}❌ Error: {}", ctx.log_prefix(), self.user_message());
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
    // Check for SPDK RPC errors
    if let Some(error) = response.get("error") {
        let error_text = error.to_string();
        println!("{}❌ SPDK RPC Error in {}:", ctx.log_prefix(), operation);
        println!("{}   Raw Error: {}", ctx.log_prefix(), error_text);
        
        let structured_error = NvmfError::from_spdk_error(&error_text, operation);
        structured_error.log_detailed(ctx);
        
        return Err(structured_error);
    }

    // Extract result
    if let Some(result) = response.get("result") {
        println!("{}✅ SPDK RPC Success: {}", ctx.log_prefix(), operation);
        Ok(result.clone())
    } else {
        println!("{}⚠️ SPDK RPC Response Missing Result Field:", ctx.log_prefix());
        println!("{}   Response: {}", ctx.log_prefix(), response);
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
    println!("{}🔍 Testing network connectivity...", ctx.log_prefix());
    println!("{}   Target: {}:{}", ctx.log_prefix(), target_ip, target_port);
    
    let start = Instant::now();
    
    match timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(format!("{}:{}", target_ip, target_port))
    ).await {
        Ok(Ok(_stream)) => {
            let duration = start.elapsed();
            println!("{}✅ Network connectivity test passed in {:?}", ctx.log_prefix(), duration);
            println!("{}   Connection established successfully", ctx.log_prefix());
            Ok(duration)
        }
        Ok(Err(e)) => {
            let duration = start.elapsed();
            println!("{}❌ Network connectivity test failed after {:?}", ctx.log_prefix(), duration);
            println!("{}   Error: {}", ctx.log_prefix(), e);
            
            Err(NvmfError::NetworkUnreachable {
                target: format!("{}:{}", target_ip, target_port),
                details: e.to_string(),
            })
        }
        Err(_) => {
            let duration = start.elapsed();
            println!("{}❌ Network connectivity test timed out after {:?}", ctx.log_prefix(), duration);
            
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
    let mut delay = initial_delay;
    
    for attempt in 1..=max_retries {
        println!("{}🔄 {} - Attempt {}/{}", ctx.log_prefix(), operation_name, attempt, max_retries);
        
        let start = Instant::now();
        match operation().await {
            Ok(result) => {
                let duration = start.elapsed();
                println!("{}✅ {} succeeded on attempt {} after {:?}", 
                         ctx.log_prefix(), operation_name, attempt, duration);
                return Ok(result);
            }
            Err(e) => {
                let duration = start.elapsed();
                println!("{}❌ {} failed on attempt {} after {:?}: {}", 
                         ctx.log_prefix(), operation_name, attempt, duration, e);
                
                if attempt < max_retries {
                    println!("{}⏳ Retrying in {:?}...", ctx.log_prefix(), delay);
                    tokio::time::sleep(delay).await;
                    delay = Duration::from_millis((delay.as_millis() as u64 * 2).min(30000)); // Cap at 30s
                } else {
                    println!("{}💥 {} failed after {} attempts", ctx.log_prefix(), operation_name, max_retries);
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
    println!("{}📊 Connection Health Check:", ctx.log_prefix());
    
    // Get NVMe controllers status - Note: call_spdk_rpc would need to be imported
    // This is a placeholder for connection health logging
    println!("{}   Health check would query NVMe controllers here", ctx.log_prefix());
    
    // Use call_spdk_rpc when available for health checking
    /*
    if let Ok(controllers) = call_spdk_rpc(spdk_rpc_url, &json!({
        "method": "bdev_nvme_get_controllers"
    })).await {
        if let Some(controller_list) = controllers["result"].as_array() {
            println!("{}   Active NVMe Controllers: {}", ctx.log_prefix(), controller_list.len());
            
            for controller in controller_list {
                if let Some(name) = controller.get("name").and_then(|v| v.as_str()) {
                    let state = controller.get("state").and_then(|v| v.as_str()).unwrap_or("unknown");
                    println!("{}     {}: {}", ctx.log_prefix(), name, state);
                }
            }
        }
    }
    */
    
    // Get bdev status - placeholder
    println!("{}   Health check would query bdev status here", ctx.log_prefix());
    
    // Implement actual health checks when call_spdk_rpc is available
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