// node_agent/rpc_client.rs - SPDK RPC Communication
//
// This module handles all RPC communication with SPDK target processes.
// It provides a clean interface for making RPC calls over Unix sockets or HTTP.

use serde_json::Value;
use std::io::{Write, Read};
use std::os::unix::net::UnixStream;
use tokio::process::Command;

/// SPDK RPC interface for CSI operations
/// 
/// This implementation uses SPDK v25.05.x RPC interface exclusively.
/// All operations are performed via persistent socket connections to the SPDK target process.
/// Implementation matches the official SPDK Go client pattern.
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
    let request_json = rpc_request.to_string();
    
    let result = tokio::task::spawn_blocking(move || -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let mut stream = UnixStream::connect(&socket_path)
            .map_err(|e| format!("Failed to connect to SPDK socket {}: {}", socket_path, e))?;
        
        // Build HTTP-over-Unix request
        let http_request = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            request_json.len(),
            request_json
        );
        
        // Send request
        stream.write_all(http_request.as_bytes())
            .map_err(|e| format!("Failed to write to SPDK socket: {}", e))?;
        
        // Read response
        let mut response = Vec::new();
        stream.read_to_end(&mut response)
            .map_err(|e| format!("Failed to read from SPDK socket: {}", e))?;
        
        Ok(String::from_utf8_lossy(&response).to_string())
    }).await??;
    
    // Parse HTTP response to extract JSON
    parse_http_response(&result)
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

/// Execute SPDK RPC script command (for compatibility with existing scripts)
pub async fn call_spdk_rpc_script(
    socket_path: &str,
    method: &str,
    params: Option<&Value>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut cmd = Command::new("python3");
    cmd.arg("/usr/local/bin/rpc.py");
    cmd.arg("-s").arg(socket_path);
    cmd.arg(method);
    
    // Add parameters if provided
    if let Some(params) = params {
        if let Value::Object(param_map) = params {
            for (key, value) in param_map {
                cmd.arg(format!("--{}", key));
                
                // Convert value to string argument
                match value {
                    Value::String(s) => cmd.arg(s),
                    Value::Number(n) => cmd.arg(n.to_string()),
                    Value::Bool(b) => cmd.arg(b.to_string()),
                    _ => cmd.arg(value.to_string()),
                };
            }
        }
    }
    
    let output = cmd.output().await
        .map_err(|e| format!("Failed to execute rpc.py: {}", e))?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("RPC script failed: {}", stderr).into());
    }
    
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: Value = serde_json::from_str(&stdout)
        .map_err(|e| format!("Failed to parse RPC script output: {}", e))?;
    
    Ok(result)
}
