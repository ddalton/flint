// node_agent/rpc_client.rs - SPDK RPC Communication
//
// This module handles all RPC communication with SPDK target processes.
// It provides a clean interface for making RPC calls over Unix sockets or HTTP.

use serde_json::Value;
use std::io::{Write, Read};
use std::os::unix::net::UnixStream;

/// Trait to ensure only approved RPC implementations are used
/// This prevents calling wrong RPC functions by accident
pub trait SpdkRpcClient {
    async fn call_rpc(&self, method: &str, params: Option<Value>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>>;
}

/// SPDK RPC interface for CSI operations
/// 
/// This implementation uses SPDK v25.05.x RPC interface exclusively.
/// All operations are performed via persistent socket connections to the SPDK target process.
/// Implementation matches the official SPDK Go client pattern.
/// 
/// ⚠️  WARNING: This is the ONLY approved SPDK RPC client function.
/// ❌ Do NOT create alternative implementations or use re-exports.
/// ✅ Always import as: `use crate::node_agent::rpc_client::call_spdk_rpc;`
pub async fn call_spdk_rpc(
    spdk_rpc_url: &str,
    rpc_request: &Value,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    if spdk_rpc_url.starts_with("unix://") {
        call_spdk_rpc_unix(spdk_rpc_url, rpc_request).await
    } else {
        call_spdk_rpc_http(spdk_rpc_url, rpc_request).await
    }
}

/// Make RPC call over Unix domain socket
async fn call_spdk_rpc_unix(
    spdk_rpc_url: &str,
    rpc_request: &Value,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let socket_path = &spdk_rpc_url[7..]; // Remove "unix://" prefix
    
    // Use tokio::task::spawn_blocking for blocking socket operations
    let socket_path = socket_path.to_string();
    let rpc_request = rpc_request.clone();
    
    let result = tokio::task::spawn_blocking(move || -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let mut stream = UnixStream::connect(&socket_path)
            .map_err(|e| format!("Failed to connect to SPDK socket {}: {}", socket_path, e))?;
        
        // Create proper JSON-RPC 2.0 request (SPDK expects raw JSON, not HTTP)
        let jsonrpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": rpc_request["method"],
            "params": rpc_request.get("params").unwrap_or(&serde_json::json!({})),
            "id": 1
        });
        
        // Send raw JSON with newline (SPDK expects newline-delimited JSON)
        let message = format!("{}\n", jsonrpc_request.to_string());
        stream.write_all(message.as_bytes())
            .map_err(|e| format!("Failed to write to SPDK socket: {}", e))?;
        
        // Read response
        let mut response = Vec::new();
        stream.read_to_end(&mut response)
            .map_err(|e| format!("Failed to read from SPDK socket: {}", e))?;
        
        let response_str = String::from_utf8_lossy(&response);
        
        // Parse JSON response directly (no HTTP parsing needed)
        let parsed_response: Value = serde_json::from_str(response_str.trim())
            .map_err(|e| format!("Failed to parse JSON response: {}", e))?;
        
        Ok(parsed_response)
    }).await??;
    
    // Return parsed JSON response directly
    Ok(result)
}

/// Make RPC call over HTTP
async fn call_spdk_rpc_http(
    spdk_rpc_url: &str,
    rpc_request: &Value,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    
    let response = client
        .post(spdk_rpc_url)
        .json(rpc_request)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;
    
    let json_response: Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse JSON response: {}", e))?;
    
    Ok(json_response)
}

/// Parse HTTP response to extract JSON body
fn parse_http_response(response: &str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    // Find the start of JSON content (after HTTP headers)
    if let Some(json_start) = response.find('{') {
        let json_part = &response[json_start..];
        
        // Find the end of JSON (first complete JSON object)
        let mut brace_count = 0;
        let mut end_pos = 0;
        
        for (i, ch) in json_part.char_indices() {
            match ch {
                '{' => brace_count += 1,
                '}' => {
                    brace_count -= 1;
                    if brace_count == 0 {
                        end_pos = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        
        if end_pos > 0 {
            let json_str = &json_part[..end_pos];
            let parsed: Value = serde_json::from_str(json_str)
                .map_err(|e| format!("Failed to parse JSON: {}", e))?;
            Ok(parsed)
        } else {
            Err("Incomplete JSON in response".into())
        }
    } else {
        Err("No JSON found in HTTP response".into())
    }
}
