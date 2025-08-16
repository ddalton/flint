// node_agent/rpc_client.rs - SPDK RPC Communication
//
// This module handles all RPC communication with SPDK target processes.
// It provides a clean interface for making RPC calls over Unix sockets or HTTP.

use serde_json::Value;
use std::io::{Write, BufRead};
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
            
        // Set socket timeout for LVS operations (can take several seconds)
        // Use longer timeout for async operations like bdev_lvol_create_lvstore
        let timeout_secs = if rpc_request["method"].as_str() == Some("bdev_lvol_create_lvstore") {
            120  // 2 minutes for LVS creation
        } else {
            30   // 30 seconds for other operations
        };
        
        println!("🔍 [UNIX_SOCKET] Setting socket timeout to {} seconds for method: {}", 
                 timeout_secs, rpc_request["method"].as_str().unwrap_or("unknown"));
        
        stream.set_read_timeout(Some(std::time::Duration::from_secs(timeout_secs)))
            .map_err(|e| format!("Failed to set socket read timeout: {}", e))?;
        
        // Create proper JSON-RPC 2.0 request (SPDK expects raw JSON, not HTTP)
        let jsonrpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": rpc_request["method"],
            "params": rpc_request.get("params").unwrap_or(&serde_json::json!({})),
            "id": 1
        });
        
        // Send raw JSON with newline (SPDK expects newline-delimited JSON)
        let message = format!("{}\n", jsonrpc_request.to_string());
        println!("🔍 [UNIX_SOCKET] Sending to SPDK: {}", message.trim());
        
        stream.write_all(message.as_bytes())
            .map_err(|e| format!("Failed to write to SPDK socket: {}", e))?;
            
        // Flush to ensure data is sent immediately
        stream.flush()
            .map_err(|e| format!("Failed to flush SPDK socket: {}", e))?;
            
        println!("🔍 [UNIX_SOCKET] Request sent and flushed successfully");
        
        // Read response using BufRead to handle newline-delimited JSON (like Go's json.Decoder)
        let mut reader = std::io::BufReader::new(&mut stream);
        let mut response = String::new();
        
        // Read a complete line (JSON response terminated by newline)
        // Add debugging for LVS creation hangs
        let method_name = rpc_request["method"].as_str().unwrap_or("unknown");
        println!("🔍 [UNIX_SOCKET] Waiting for SPDK response to method: {}", method_name);
        
        // For async operations, track timing
        let start_time = std::time::Instant::now();
        if method_name == "bdev_lvol_create_lvstore" {
            println!("🔍 [UNIX_SOCKET] This is an async operation - will wait up to 2 minutes for response");
        }
        
        let read_result = reader.read_line(&mut response);
        
        // Log timing for async operations
        if method_name == "bdev_lvol_create_lvstore" {
            println!("🔍 [UNIX_SOCKET] Read operation completed after {} seconds", start_time.elapsed().as_secs());
        }
        
        match read_result {
            Ok(bytes_read) => {
                println!("🔍 [UNIX_SOCKET] Read {} bytes from SPDK", bytes_read);
                println!("🔍 [UNIX_SOCKET] Raw response: {:?}", response.as_bytes());
                println!("🔍 [UNIX_SOCKET] Response as string: {}", response.trim());
                
                if bytes_read == 0 {
                    println!("❌ [UNIX_SOCKET] SPDK closed connection (0 bytes read)");
                    return Err("SPDK closed connection without sending response".into());
                }
            }
            Err(e) => {
                println!("❌ [UNIX_SOCKET] Failed to read from SPDK socket: {}", e);
                println!("❌ [UNIX_SOCKET] Error kind: {:?}", e.kind());
                
                // Check for timeout vs connection issues
                if e.kind() == std::io::ErrorKind::TimedOut {
                    println!("❌ [UNIX_SOCKET] Socket read timeout after 30 seconds - SPDK may not be responding");
                } else if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    println!("❌ [UNIX_SOCKET] SPDK closed connection unexpectedly");
                } else if e.kind() == std::io::ErrorKind::ConnectionReset {
                    println!("❌ [UNIX_SOCKET] SPDK reset connection");
                }
                
                return Err(format!("Failed to read JSON response from SPDK socket: {}", e).into());
            }
        }
        
        let response_str = response;
        
        // Parse JSON response directly (no HTTP parsing needed)
        let parsed_response: Value = serde_json::from_str(response_str.trim())
            .map_err(|e| {
                println!("❌ [UNIX_SOCKET] Failed to parse JSON: {} | Raw response: {}", e, response_str);
                format!("Failed to parse JSON response: {}", e)
            })?;
        
        // Debug the parsed response structure
        if let Some(result) = parsed_response.get("result") {
            println!("✅ [UNIX_SOCKET] SPDK success result: {}", result);
        } else if let Some(error) = parsed_response.get("error") {
            println!("❌ [UNIX_SOCKET] SPDK error response: {}", error);
        } else {
            println!("⚠️ [UNIX_SOCKET] Unexpected response format: {}", parsed_response);
        }
        
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
