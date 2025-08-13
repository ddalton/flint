// driver.rs - Cleaned up driver types and utilities
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use kube::Client;
use k8s_openapi::api::core::v1::{Node as k8sNode, Pod};
use kube::Api;
use tonic::Status;
use reqwest::Client as HttpClient;
use serde_json::json;
use std::os::unix::net::UnixStream;
use std::io::{Write, Read};
use spdk_csi_driver::models::NvmeClientDevice;

#[derive(Clone)]
pub struct SpdkCsiDriver {
    pub node_id: String,
    pub kube_client: Client,
    pub spdk_rpc_url: String,
    pub spdk_node_urls: Arc<Mutex<HashMap<String, String>>>,
    pub nvmeof_target_port: u16,
    pub nvmeof_transport: String,
    pub target_namespace: String,
    // Removed ublk_target_initialized - no longer needed with NVMe-oF
}

impl SpdkCsiDriver {
    /// Gets the SPDK RPC URL for a specific node by finding the 'node_agent' pod
    pub async fn get_rpc_url_for_node(&self, node_name: &str) -> Result<String, Status> {
        // If this driver runs in the same Pod as node-agent, prefer localhost
        if node_name == self.node_id {
            // Always prefer unix socket for the local node
            return Ok(std::env::var("SPDK_RPC_URL").unwrap_or_else(|_| "unix:///var/tmp/spdk.sock".to_string()));
        }

        let mut cache = self.spdk_node_urls.lock().await;
        if let Some(url) = cache.get(node_name) {
            return Ok(url.clone());
        }

        println!("Discovering flint-csi-node pod for node '{}'...", node_name);
        let pods_api: Api<Pod> = Api::all(self.kube_client.clone());
        let lp = kube::api::ListParams::default().labels("app=flint-csi-node");
        let pods = pods_api
            .list(&lp)
            .await
            .map_err(|e| Status::internal(format!("Failed to list flint-csi-node pods: {}", e)))?;

        for pod in pods {
            let pod_node = pod.spec.as_ref().and_then(|s| s.node_name.as_deref());
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref());
            if let (Some(p_node), Some(p_ip)) = (pod_node, pod_ip) {
                // For remote nodes, we still cache an HTTP proxy endpoint in case consumers need it.
                // But callers should prefer the local unix socket when node matches.
                let url = format!("http://{}:8081/api/spdk/rpc", p_ip);
                cache.insert(p_node.to_string(), url);
            }
        }

        cache.get(node_name).cloned().ok_or_else(|| {
            Status::not_found(format!(
                "Could not resolve SPDK RPC endpoint for node '{}'",
                node_name
            ))
        })
    }

    /// Get node IP address from Kubernetes API
    pub async fn get_node_ip(&self, node_name: &str) -> Result<String, Status> {
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        
        let node = nodes_api.get(node_name).await
            .map_err(|e| Status::not_found(format!("Node {} not found: {}", node_name, e)))?;

        if let Some(status) = &node.status {
            if let Some(addresses) = &status.addresses {
                // Prefer InternalIP for NVMe-oF connections
                for address in addresses {
                    if address.type_ == "InternalIP" {
                        return Ok(address.address.clone());
                    }
                }
                // Fallback to any address
                if let Some(addr) = addresses.first() {
                    return Ok(addr.address.clone());
                }
            }
        }

        Err(Status::not_found(format!("No IP address found for node {}", node_name)))
    }

    /// Get current node IP address (cached for efficiency)
    /// Uses the driver's node_id field instead of environment variables to support cross-node operations
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Use the driver's node_id which is set correctly for the target node
        // This supports controller operations on behalf of other nodes
        println!("🔍 [NODE_IP_DEBUG] Getting IP for node: {}", self.node_id);
        
        let node_ip = self.get_node_ip(&self.node_id).await?;
        println!("✅ [NODE_IP_DEBUG] Resolved node '{}' to IP: {}", self.node_id, node_ip);
        
        Ok(node_ip)
    }



    /// Start ublk device (replaces connect_nvme_device)
    /// This creates a userspace block device that connects to the SPDK target
    pub async fn ublk_start_disk(
        &self,
        bdev_name: &str,
        volume_id: &str,
        target_addr: &str,
        target_port: u16,
    ) -> Result<UblkDevice, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔗 [UBLK_START] Starting ublk device for bdev {} (volume {})", bdev_name, volume_id);
        
        // Step 1: Check if ublk device already exists
        if let Ok(existing_device) = self.find_existing_ublk_device(volume_id).await {
            println!("ℹ️ [UBLK_START] ublk device already exists: {}", existing_device.device_path);
            return Ok(existing_device);
        }
        
        // Step 2: Call SPDK RPC to create ublk target
        let http_client = reqwest::Client::new();
        let ublk_id = self.generate_ublk_id(volume_id);
        
        let rpc_request = serde_json::json!({
            "method": "ublk_start_disk",
            "params": {
                "bdev_name": bdev_name,
                "ublk_id": ublk_id
            }
        });
        
        println!("🔧 [UBLK_START] Creating ublk device with ID {}", ublk_id);
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&rpc_request)
            .send()
            .await?;
        
        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("SPDK RPC request failed: {}", error_text).into());
        }
        
        let response_json: serde_json::Value = response.json().await?;
        
        // Check for SPDK RPC errors
        if let Some(error) = response_json.get("error") {
            let error_code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let error_msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
            
            println!("⚠️ [UBLK_START] ublk_start_disk failed: code={}, message={}", error_code, error_msg);
            
            // Handle specific error cases
            match error_code {
                -32603 => {
                    if error_msg.contains("Device or resource busy") || error_msg.contains("No such file or directory") {
                        return Err("ublk_drv kernel module not available or ublk device busy".into());
                    }
                }
                -32601 => {
                    return Err("SPDK doesn't support ublk methods - check SPDK version".into());
                }
                _ => {}
            }
            
            return Err(format!("ublk_start_disk failed: {}", error_msg).into());
        }
        
        // Step 3: Wait for ublk device to appear
        println!("🔧 [UBLK_START] Waiting for ublk device to appear...");
        let mut attempts = 0;
        let max_attempts = 10;
        
        while attempts < max_attempts {
            if let Ok(device) = self.find_existing_ublk_device(volume_id).await {
                println!("✅ [UBLK_START] Successfully created ublk device: {}", device.device_path);
                return Ok(device);
            }
            
            attempts += 1;
            println!("🔧 [UBLK_START] Attempt {}/{}: Waiting for device...", attempts, max_attempts);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        
        return Err(format!("ublk device did not appear after {} seconds", max_attempts).into());
    }
    
    /// Find existing NVMe connection by NQN
    pub async fn find_existing_nvme_connection(&self, nqn: &str) -> Result<NvmeClientDevice, Box<dyn std::error::Error + Send + Sync>> {
        // Run nvme list-subsys to find connected devices
        let output = tokio::process::Command::new("nvme")
            .args(&["list-subsys", "-o", "json"])
            .output()
            .await?;
        
        if !output.status.success() {
            return Err("Failed to list NVMe subsystems".into());
        }
        
        let json_str = String::from_utf8(output.stdout)?;
        let subsystems: serde_json::Value = serde_json::from_str(&json_str)?;
        
        // Parse the JSON to find our NQN
        if let Some(subsys_array) = subsystems["Subsystems"].as_array() {
            for subsys in subsys_array {
                if let Some(subsys_nqn) = subsys["NQN"].as_str() {
                    if subsys_nqn == nqn {
                        // Find the namespace path
                        if let Some(namespaces) = subsys["Namespaces"].as_array() {
                            for ns in namespaces {
                                if let Some(device_path) = ns["NameSpace"].as_str() {
                                    // Extract controller ID from device path
                                    let controller_id = device_path
                                        .strip_prefix("/dev/")
                                        .and_then(|s| s.strip_suffix("n1"))
                                        .map(|s| s.to_string());
                                    
                                    let device = NvmeClientDevice {
                                        device_path: device_path.to_string(),
                                        nqn: nqn.to_string(),
                                        transport: "tcp".to_string(), // TODO: Get actual transport
                                        target_addr: "unknown".to_string(), // TODO: Get actual address
                                        target_port: 0, // TODO: Get actual port
                                        connected_at: chrono::Utc::now().to_rfc3339(),
                                        node: self.node_id.clone(),
                                        controller_id,
                                    };
                                    
                                    return Ok(device);
                                }
                            }
                        }
                    }
                }
            }
        }
        
        Err("NVMe connection not found".into())
    }
    
    /// Stop ublk device (replaces disconnect_nvme_device)
    /// This removes the userspace block device
    pub async fn ublk_stop_disk(
        &self,
        volume_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔌 [UBLK_STOP] Stopping ublk device for volume {}", volume_id);
        
        // Step 1: Check if ublk device exists
        if let Err(_) = self.find_existing_ublk_device(volume_id).await {
            println!("ℹ️ [UBLK_STOP] ublk device for volume {} not found", volume_id);
            return Ok(()); // Already stopped
        }
        
        // Step 2: Call SPDK RPC to stop ublk device
        let http_client = reqwest::Client::new();
        let ublk_id = self.generate_ublk_id(volume_id);
        
        let rpc_request = serde_json::json!({
            "method": "ublk_stop_disk",
            "params": {
                "ublk_id": ublk_id
            }
        });
        
        println!("🔧 [UBLK_STOP] Stopping ublk device with ID {}", ublk_id);
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&rpc_request)
            .send()
            .await?;
        
        if !response.status.is_success() {
            let error_text = response.text().await?;
            return Err(format!("SPDK RPC request failed: {}", error_text).into());
        }
        
        let response_json: serde_json::Value = response.json().await?;
        
        // Check for SPDK RPC errors
        if let Some(error) = response_json.get("error") {
            let error_msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
            
            // Some errors are acceptable (device already stopped)
            if error_msg.contains("not found") || error_msg.contains("does not exist") {
                println!("ℹ️ [UBLK_STOP] ublk device already stopped: {}", error_msg);
                return Ok(());
            }
            
            println!("❌ [UBLK_STOP] ublk_stop_disk failed: {}", error_msg);
            return Err(format!("ublk_stop_disk failed: {}", error_msg).into());
        }
        
        // Step 3: Wait for ublk device to disappear
        println!("🔧 [UBLK_STOP] Waiting for ublk device to disappear...");
        let mut attempts = 0;
        let max_attempts = 10;
        
        while attempts < max_attempts {
            if let Err(_) = self.find_existing_ublk_device(volume_id).await {
                println!("✅ [UBLK_STOP] Successfully stopped ublk device for volume {}", volume_id);
                return Ok(());
            }
            
            attempts += 1;
            println!("🔧 [UBLK_STOP] Attempt {}/{}: Waiting for device to disappear...", attempts, max_attempts);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        println!("⚠️ [UBLK_STOP] Device may still exist after {} attempts", max_attempts);
        Ok(()) // Don't fail - stop command succeeded
    }
    
    /// Generate ublk ID for volume (replaces NQN generation)
    pub fn generate_ublk_id(&self, volume_id: &str) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        volume_id.hash(&mut hasher);
        // Use a reasonable range for ublk IDs (0-65535)
        (hasher.finish() % 65536) as u32
    }
    
    /// Find existing ublk device by volume ID
    pub async fn find_existing_ublk_device(&self, volume_id: &str) -> Result<UblkDevice, Box<dyn std::error::Error + Send + Sync>> {
        let ublk_id = self.generate_ublk_id(volume_id);
        let expected_device_path = format!("/dev/ublkb{}", ublk_id);
        
        // Check if the device file exists
        if !std::path::Path::new(&expected_device_path).exists() {
            return Err("ublk device not found".into());
        }
        
        // Get device information
        let device_info = self.get_ublk_device_info(ublk_id).await?;
        
        Ok(UblkDevice {
            id: ublk_id,
            device_path: expected_device_path,
            volume_id: volume_id.to_string(),
            bdev_name: device_info.bdev_name,
            queue_depth: device_info.queue_depth,
            block_size: device_info.block_size,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
    }
    
    /// Get ublk device information from SPDK
    async fn get_ublk_device_info(&self, ublk_id: u32) -> Result<UblkDeviceInfo, Box<dyn std::error::Error + Send + Sync>> {
        let http_client = reqwest::Client::new();
        
        let rpc_request = serde_json::json!({
            "method": "ublk_get_disks",
            "params": {}
        });
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&rpc_request)
            .send()
            .await?;
        
        if !response.status().is_success() {
            return Err("Failed to get ublk device list".into());
        }
        
        let response_json: serde_json::Value = response.json().await?;
        
        if let Some(error) = response_json.get("error") {
            return Err(format!("ublk_get_disks failed: {}", error).into());
        }
        
        // Parse the result to find our device
        if let Some(result) = response_json.get("result") {
            if let Some(devices) = result.as_array() {
                for device in devices {
                    if let Some(id) = device.get("id").and_then(|v| v.as_u64()) {
                        if id as u32 == ublk_id {
                            return Ok(UblkDeviceInfo {
                                bdev_name: device.get("bdev_name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string(),
                                queue_depth: device.get("queue_depth")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(128) as u32,
                                block_size: device.get("block_size")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(4096) as u32,
                            });
                        }
                    }
                }
            }
        }
        
        Err("ublk device not found in SPDK".into())
    }
    
    /// Generate NQN for volume (kept for backward compatibility)
    pub fn generate_nqn(&self, volume_id: &str) -> String {
        format!("nqn.2023.io.flint:volume-{}", volume_id)
    }



    /// Create NVMe-oF target with validation and retry logic
    pub async fn create_nvmeof_target_with_validation(
        &self,
        bdev_name: &str,
        nqn: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let max_retries = 3;
        let retry_delay = std::time::Duration::from_secs(2);

        for attempt in 1..=max_retries {
            println!("🔧 [NVMEOF_CREATE] Attempt {}/{} to create NVMe-oF target for {}", attempt, max_retries, nqn);
            
            match self.create_nvmeof_target(bdev_name, nqn).await {
                Ok(_) => {
                    // Validate that the target is accessible
                    println!("🔍 [NVMEOF_VALIDATE] Validating NVMe-oF target accessibility...");
                    
                    match self.validate_nvmeof_target(nqn).await {
                        Ok(_) => {
                            println!("✅ [NVMEOF_VALIDATE] NVMe-oF target validation successful");
                            return Ok(());
                        }
                        Err(e) => {
                            println!("❌ [NVMEOF_VALIDATE] Target validation failed on attempt {}: {}", attempt, e);
                            
                            if attempt == max_retries {
                                // Clean up on final failure
                                let _ = self.cleanup_nvmeof_target(nqn).await;
                                return Err(format!("NVMe-oF target validation failed after {} attempts: {}", max_retries, e).into());
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("⚠️ [NVMEOF_CREATE] Creation failed on attempt {}: {}", attempt, e);
                    
                    if attempt == max_retries {
                        return Err(format!("Failed to create NVMe-oF target after {} attempts: {}", max_retries, e).into());
                    }
                }
            }
            
            if attempt < max_retries {
                println!("⏳ [NVMEOF_RETRY] Waiting {}s before retry...", retry_delay.as_secs());
                tokio::time::sleep(retry_delay).await;
            }
        }

        Err("Unexpected error in NVMe-oF target creation retry loop".into())
    }

    /// Validate that an NVMe-oF target is accessible with comprehensive debugging
    async fn validate_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        
        println!("🔍 [NVMEOF_DEBUG] Starting comprehensive NVMe-oF target validation for: {}", nqn);
        println!("🔍 [NVMEOF_DEBUG] Expected port: {}", self.nvmeof_target_port);
        println!("🔍 [NVMEOF_DEBUG] Transport: {}", self.nvmeof_transport);
        
        // Step 1: Check if subsystem exists and is configured
        println!("🔍 [NVMEOF_DEBUG] Step 1: Querying SPDK subsystems...");
        let subsystem_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_get_subsystems",
                "params": {}
            }))
            .send()
            .await?;

        if !subsystem_response.status().is_success() {
            let error_text = subsystem_response.text().await?;
            println!("❌ [NVMEOF_DEBUG] Failed to query subsystems: {}", error_text);
            return Err("Failed to query NVMe-oF subsystems".into());
        }

        let subsystems: serde_json::Value = subsystem_response.json().await?;
        
        // CRITICAL FIX: SPDK responses have {"result": [...]} structure, not direct array
        let subsystem_list = subsystems.get("result").and_then(|r| r.as_array());
        let subsystem_count = subsystem_list.map(|a| a.len()).unwrap_or(0);
        
        println!("🔍 [NVMEOF_DEBUG] Retrieved {} subsystems from SPDK", subsystem_count);
        println!("🔍 [NVMEOF_DEBUG] Raw response structure: {}", 
                 serde_json::to_string_pretty(&subsystems).unwrap_or_else(|_| "Parse error".to_string()));
        
        // Debug: List all subsystems
        if let Some(subsystem_list) = subsystem_list {
            println!("🔍 [NVMEOF_DEBUG] All configured subsystems:");
            for (i, subsystem) in subsystem_list.iter().enumerate() {
                if let Some(existing_nqn) = subsystem.get("nqn").and_then(|v| v.as_str()) {
                    println!("🔍 [NVMEOF_DEBUG]   {}: {}", i + 1, existing_nqn);
                }
            }
        }
        
        // Step 2: Find our specific subsystem
        println!("🔍 [NVMEOF_DEBUG] Step 2: Searching for target subsystem: {}", nqn);
        if let Some(subsystem_list) = subsystem_list {
            for subsystem in subsystem_list {
                if let Some(subsystem_nqn) = subsystem.get("nqn").and_then(|v| v.as_str()) {
                    if subsystem_nqn == nqn {
                        println!("✅ [NVMEOF_DEBUG] Found target subsystem: {}", nqn);
                        
                        // Step 3: Validate namespaces
                        println!("🔍 [NVMEOF_DEBUG] Step 3: Checking namespaces...");
                        if let Some(namespaces) = subsystem.get("namespaces").and_then(|v| v.as_array()) {
                            println!("🔍 [NVMEOF_DEBUG] Found {} namespaces", namespaces.len());
                            if namespaces.is_empty() {
                                println!("❌ [NVMEOF_DEBUG] ERROR: Subsystem has no namespaces!");
                                return Err("Subsystem exists but has no namespaces".into());
                            }
                            
                            // Debug: Show namespace details
                            for (i, ns) in namespaces.iter().enumerate() {
                                println!("🔍 [NVMEOF_DEBUG]   Namespace {}: {}", i + 1, ns);
                            }
                        }
                        
                        // Step 4: Validate listeners
                        println!("🔍 [NVMEOF_DEBUG] Step 4: Checking listeners...");
                        if let Some(listeners) = subsystem.get("listen_addresses").and_then(|v| v.as_array()) {
                            println!("🔍 [NVMEOF_DEBUG] Found {} listeners", listeners.len());
                            if listeners.is_empty() {
                                println!("❌ [NVMEOF_DEBUG] ERROR: Subsystem has no listeners!");
                                return Err("Subsystem exists but has no listeners".into());
                            }
                            
                            // Debug: Show listener details and verify port
                            let target_port = self.nvmeof_target_port.to_string();
                            let mut has_correct_port = false;
                            
                            for (i, listener) in listeners.iter().enumerate() {
                                let listener_port = listener.get("trsvcid").and_then(|v| v.as_str()).unwrap_or("unknown");
                                let listener_addr = listener.get("traddr").and_then(|v| v.as_str()).unwrap_or("unknown");
                                let listener_type = listener.get("trtype").and_then(|v| v.as_str()).unwrap_or("unknown");
                                
                                println!("🔍 [NVMEOF_DEBUG]   Listener {}: {}://{}:{}", 
                                         i + 1, listener_type, listener_addr, listener_port);
                                
                                if listener_port == target_port {
                                    has_correct_port = true;
                                    println!("✅ [NVMEOF_DEBUG]   ✓ Found matching port: {}", listener_port);
                                }
                            }
                            
                            if !has_correct_port {
                                println!("❌ [NVMEOF_DEBUG] ERROR: Expected port {} not found in listeners!", target_port);
                                return Err(format!("Subsystem exists but is not listening on port {}", target_port).into());
                            }
                        }
                        
                        // Step 5: Additional network validation
                        println!("🔍 [NVMEOF_DEBUG] Step 5: Testing network connectivity...");
                        if let Err(e) = self.test_network_connectivity().await {
                            println!("⚠️ [NVMEOF_DEBUG] Network connectivity test failed: {}", e);
                            // Don't fail validation for network issues, just warn
                        }
                        
                        println!("✅ [NVMEOF_VALIDATE] Subsystem {} is properly configured and accessible", nqn);
                        return Ok(());
                    }
                }
            }
        }
        
        println!("❌ [NVMEOF_DEBUG] ERROR: Target subsystem not found in SPDK configuration");
        println!("🔍 [NVMEOF_DEBUG] This indicates the subsystem creation failed or was not persisted");
        Err(format!("Subsystem {} not found in SPDK configuration", nqn).into())
    }

    /// Clean up NVMe-oF target on failure
    async fn cleanup_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        
        println!("🧹 [NVMEOF_CLEANUP] Cleaning up NVMe-oF target: {}", nqn);
        
        // Try to delete the subsystem (this will remove namespaces and listeners)
        let delete_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_delete_subsystem",
                "params": {
                    "nqn": nqn
                }
            }))
            .send()
            .await?;

        if !delete_response.status().is_success() {
            let error_text = delete_response.text().await?;
            // Don't fail cleanup if subsystem doesn't exist
            if !error_text.contains("does not exist") {
                println!("⚠️ [NVMEOF_CLEANUP] Warning: Failed to delete subsystem {}: {}", nqn, error_text);
            }
        } else {
            println!("✅ [NVMEOF_CLEANUP] Successfully cleaned up subsystem: {}", nqn);
        }

        Ok(())
    }

    /// Test network connectivity for NVMe-oF debugging
    async fn test_network_connectivity(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🌐 [NETWORK_TEST] Testing NVMe-oF network connectivity...");
        
        let node_ip = self.get_current_node_ip().await?;
        println!("🌐 [NETWORK_TEST] Current node IP: {}", node_ip);
        println!("🌐 [NETWORK_TEST] Target port: {}", self.nvmeof_target_port);
        
        // Test if we can bind to the port (indicates it's available for listening)
        match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", self.nvmeof_target_port)).await {
            Ok(listener) => {
                println!("✅ [NETWORK_TEST] Port {} is available for binding", self.nvmeof_target_port);
                drop(listener); // Release the port
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    println!("✅ [NETWORK_TEST] Port {} is in use (expected for active NVMe-oF target)", self.nvmeof_target_port);
                } else {
                    println!("⚠️ [NETWORK_TEST] Port {} test failed: {}", self.nvmeof_target_port, e);
                    return Err(format!("Network port test failed: {}", e).into());
                }
            }
        }
        
        Ok(())
    }

    /// Enhanced NVMe-oF target creation with detailed logging and metrics
    pub async fn create_nvmeof_target(
        &self,
        bdev_name: &str,
        nqn: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use spdk_csi_driver::nvmeof_utils::*;
        use std::time::Instant;

        let overall_start = Instant::now();
        let http_client = HttpClient::new();
        let node_ip = self.get_current_node_ip().await?;

        // Create structured logging context
        let ctx = NvmfContext::new(self.node_id.clone(), "create_target")
            .with_nqn(nqn.to_string())
            .with_bdev(bdev_name.to_string())
            .with_target(node_ip.clone(), self.nvmeof_target_port.to_string());

        let mut metrics = NvmfMetrics::default();

        println!("{}🚀 Starting NVMe-oF target creation", ctx.log_prefix());
        println!("{}   Transport: {} on port {}", ctx.log_prefix(), self.nvmeof_transport, self.nvmeof_target_port);

        // Step 1: Verify bdev exists before creating subsystem
        println!("{}🔍 Step 1: Verifying bdev exists...", ctx.log_prefix());
        let verify_start = Instant::now();
        match self.verify_bdev_exists(bdev_name).await {
            Ok(_) => {
                let verify_time = verify_start.elapsed().as_millis() as u64;
                println!("{}✅ Bdev {} verified in {}ms", ctx.log_prefix(), bdev_name, verify_time);
            }
            Err(e) => {
                let nvmf_error = NvmfError::BdevNotFound {
                    bdev_name: bdev_name.to_string(),
                    details: e.to_string(),
                };
                nvmf_error.log_detailed(&ctx);
                return Err(format!("Cannot create NVMe-oF target: {}", nvmf_error.user_message()).into());
            }
        }

        // Step 2: Create NVMe-oF subsystem with detailed logging (simplified to avoid closure complexity)
        println!("{}🔍 Step 2: Creating NVMe-oF subsystem...", ctx.log_prefix());
        let subsystem_payload = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true,
                "serial_number": format!("SPDK{:016x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64),
                "model_number": "SPDK CSI Volume",
                "max_namespaces": 1,
                // Explicitly set these for v25.05.x compatibility
                "ana_reporting": false,
                "min_cntlid": 1,
                "max_cntlid": 65519
            }
        });

        let subsystem_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&subsystem_payload)
            .send()
            .await?;

        if !subsystem_response.status().is_success() {
            let error_text = subsystem_response.text().await?;
            
            // Handle "already exists" as acceptable
            if error_text.contains("already exists") || error_text.contains("Subsystem NQN") && error_text.contains("already exists") {
                println!("{}ℹ️ Subsystem already exists (acceptable)", ctx.log_prefix());
            } else {
                let nvmf_error = NvmfError::from_spdk_error(&error_text, "nvmf_create_subsystem");
                nvmf_error.log_detailed(&ctx);
                return Err(format!("Failed to create subsystem: {}", nvmf_error.user_message()).into());
            }
        } else {
            println!("{}✅ Subsystem created successfully", ctx.log_prefix());
        }

        // Verify allow_any_host was set correctly
        println!("{}🔍 Verifying allow_any_host configuration...", ctx.log_prefix());
        let verify_payload = json!({
            "method": "nvmf_get_subsystems",
            "params": {}
        });

        let verify_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&verify_payload)
            .send()
            .await?;

        if verify_response.status().is_success() {
            let subsystems: serde_json::Value = verify_response.json().await?;
            if let Some(subsystem_list) = subsystems.get("result").and_then(|r| r.as_array()) {
                for subsystem in subsystem_list {
                    if let Some(subsystem_nqn) = subsystem.get("nqn").and_then(|v| v.as_str()) {
                        if subsystem_nqn == nqn {
                            let allow_any_host = subsystem.get("allow_any_host").and_then(|v| v.as_bool()).unwrap_or(false);
                            if allow_any_host {
                                println!("{}✅ Allow any host is correctly enabled", ctx.log_prefix());
                            } else {
                                println!("{}⚠️ Warning: allow_any_host is not enabled, connections may fail", ctx.log_prefix());
                            }
                            break;
                        }
                    }
                }
            }
        }

        // Step 3: Add namespace to subsystem with predictable UUID
        println!("{}🔍 Step 3: Adding namespace to subsystem...", ctx.log_prefix());
        
        let volume_uuid = Self::generate_namespace_uuid_from_nqn(nqn);
        
        println!("{}🔍 Using namespace UUID: {}", ctx.log_prefix(), volume_uuid);
        
        let namespace_payload = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "nsid": 1,
                    "bdev_name": bdev_name,
                    "uuid": volume_uuid
                }
            }
        });

        let namespace_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&namespace_payload)
            .send()
            .await?;

        if !namespace_response.status().is_success() {
            let error_text = namespace_response.text().await?;
            
            // Handle "already exists" for namespace
            if error_text.contains("already exists") || error_text.contains("Namespace already exists") {
                println!("{}ℹ️ Namespace already exists (acceptable)", ctx.log_prefix());
            } else {
                let nvmf_error = NvmfError::from_spdk_error(&error_text, "nvmf_subsystem_add_ns");
                nvmf_error.log_detailed(&ctx);
                return Err(format!("Failed to add namespace: {}", nvmf_error.user_message()).into());
            }
        } else {
            println!("{}✅ Namespace added successfully", ctx.log_prefix());
        }

        // Step 4: Add listener to subsystem (using specific node IP for better access control)
        println!("{}🔍 Step 4: Adding listener to subsystem...", ctx.log_prefix());
        
        // Get the current node's IP address for the specific listener
        let node_ip = self.get_current_node_ip().await
            .map_err(|e| format!("Failed to get node IP for listener: {}", e))?;
        println!("{}🔍 Adding listener for IP: {}", ctx.log_prefix(), node_ip);
        
        // Use specific node IP for precise access control (this fixes the SPDK v25.05.x access issue)
        let adrfam = Self::determine_address_family(&self.nvmeof_transport, &node_ip)?;
        let listener_payload = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": self.nvmeof_transport.to_uppercase(),
                    "traddr": node_ip, // Use specific node IP to fix access control
                    "trsvcid": self.nvmeof_target_port.to_string(),
                    "adrfam": adrfam
                }
            }
        });

        let listener_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&listener_payload)
            .send()
            .await?;

        if !listener_response.status().is_success() {
            let error_text = listener_response.text().await?;
            
            // Handle "already exists" for listener
            if error_text.contains("already exists") || error_text.contains("Listener already exists") {
                println!("{}ℹ️ Listener already exists (acceptable)", ctx.log_prefix());
            } else {
                let nvmf_error = NvmfError::from_spdk_error(&error_text, "nvmf_subsystem_add_listener");
                nvmf_error.log_detailed(&ctx);
                return Err(format!("Failed to add listener: {}", nvmf_error.user_message()).into());
            }
        } else {
            println!("{}✅ Listener added successfully", ctx.log_prefix());
        }

        // Step 5: Verify the complete configuration
        println!("{}🔍 Step 5: Verifying complete configuration...", ctx.log_prefix());
        let validation_start = Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await; // Allow configuration to settle
        
        match self.validate_nvmeof_target(nqn).await {
            Ok(_) => {
                metrics.verification_time_ms = Some(validation_start.elapsed().as_millis() as u64);
                println!("{}✅ NVMe-oF target validation successful", ctx.log_prefix());
                println!("{}🚀 Target accessible at: {}:{} (transport: {})", 
                         ctx.log_prefix(), node_ip, self.nvmeof_target_port, self.nvmeof_transport);
            }
            Err(e) => {
                let nvmf_error = NvmfError::ValidationFailed {
                    resource: format!("NVMe-oF target {}", nqn),
                    details: e.to_string(),
                };
                nvmf_error.log_detailed(&ctx);
                return Err(format!("NVMe-oF target created but validation failed: {}", nvmf_error.user_message()).into());
            }
        }

        // Log performance metrics
        metrics.total_time_ms = Some(overall_start.elapsed().as_millis() as u64);
        metrics.log_summary(&ctx);

        println!("{}🎉 NVMe-oF target creation completed successfully", ctx.log_prefix());
        Ok(())
    }

    /// Verify that a bdev exists in SPDK with enhanced logging
    async fn verify_bdev_exists_impl(&self, bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [BDEV_VERIFY_DEBUG] Checking if bdev '{}' exists...", bdev_name);
        
        let bdev_payload = json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": bdev_name
            }
        });
        println!("🔍 [BDEV_VERIFY_DEBUG] RPC payload: {}", bdev_payload);
        
        let response = self.call_spdk_rpc(&bdev_payload).await?;
        println!("🔍 [BDEV_VERIFY_DEBUG] RPC response: {}", response);
        
        // Check for SPDK RPC errors
        if let Some(error) = response.get("error") {
            println!("❌ [BDEV_VERIFY_DEBUG] SPDK RPC error: {}", error);
            return Err(format!("Failed to query bdev {}: {}", bdev_name, error).into());
        }
        
        if let Some(result) = response.get("result") {
            if let Some(bdev_list) = result.as_array() {
                if bdev_list.is_empty() {
                    println!("❌ [BDEV_VERIFY_DEBUG] Bdev '{}' not found", bdev_name);
                    
                    // Debug: List all available bdevs
                    println!("🔍 [BDEV_VERIFY_DEBUG] Listing all available bdevs...");
                    let all_bdevs_payload = json!({
                        "method": "bdev_get_bdevs",
                        "params": {}
                    });
                    if let Ok(all_response) = self.call_spdk_rpc(&all_bdevs_payload).await {
                        if let Some(all_result) = all_response.get("result") {
                            if let Some(all_bdev_list) = all_result.as_array() {
                                let bdev_names: Vec<String> = all_bdev_list
                                    .iter()
                                    .filter_map(|b| b.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                                    .collect();
                                println!("🔍 [BDEV_VERIFY_DEBUG] Available bdevs: {:?}", bdev_names);
                            }
                        }
                    }
                    
                    return Err(format!("Bdev '{}' does not exist", bdev_name).into());
                }
                
                println!("✅ [BDEV_VERIFY_DEBUG] Bdev '{}' exists", bdev_name);
                
                // Debug: Show bdev details
                for bdev in bdev_list {
                    if let Some(name) = bdev.get("name").and_then(|v| v.as_str()) {
                        let size = bdev.get("num_blocks").and_then(|v| v.as_u64()).unwrap_or(0);
                        let block_size = bdev.get("block_size").and_then(|v| v.as_u64()).unwrap_or(0);
                        let bdev_type = bdev.get("product_name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        println!("🔍 [BDEV_VERIFY_DEBUG] Bdev details: name={}, type={}, blocks={}, block_size={}", 
                                 name, bdev_type, size, block_size);
                    }
                }
            }
        } else {
            println!("❌ [BDEV_VERIFY_DEBUG] Unexpected response format: missing 'result' field");
            return Err("Unexpected response format from SPDK".into());
        }
        
        Ok(())
    }
    
    /// Verify that a bdev exists in SPDK (legacy function for compatibility)
    async fn verify_bdev_exists(&self, bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.verify_bdev_exists_impl(bdev_name).await
    }

    /// Delete a logical volume for cleanup purposes
    // REMOVED: delete_lvol - unused in simplified architecture
    // removed: delete_lvol_removed (unused)





    /// Call SPDK RPC using the same pattern as node_agent.rs
    async fn call_spdk_rpc(
        &self,
        rpc_request: &serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        if self.spdk_rpc_url.starts_with("unix://") {
            // Unix socket connection
            let socket_path = self.spdk_rpc_url.trim_start_matches("unix://");
            let mut stream = UnixStream::connect(socket_path)?;
            
            // Convert to proper JSON-RPC 2.0 format
            let jsonrpc_request = json!({
                "jsonrpc": "2.0",
                "method": rpc_request["method"],
                "params": rpc_request.get("params").unwrap_or(&json!({})),
                "id": 1
            });
            let message = format!("{}\n", jsonrpc_request.to_string());
            stream.write_all(message.as_bytes())?;
            
            let mut buffer = [0; 8192];
            let bytes_read = stream.read(&mut buffer)?;
            let response_str = String::from_utf8_lossy(&buffer[..bytes_read]);
            
            let response: serde_json::Value = serde_json::from_str(&response_str)?;
            Ok(response)
        } else {
            // HTTP connection fallback
            let http_client = HttpClient::new();
            let response = http_client
                .post(&self.spdk_rpc_url)
                .json(rpc_request)
                .send()
                .await?;
                
            if !response.status().is_success() {
                let error_text = response.text().await?;
                return Err(format!("HTTP RPC failed: {}", error_text).into());
            }
            
            let response_json = response.json().await?;
            Ok(response_json)
        }
    }

    /// Generate predictable namespace UUID from NQN for consistent client-server naming
    pub fn generate_namespace_uuid_from_nqn(nqn: &str) -> String {
        // Extract volume ID from NQN to create a predictable namespace UUID
        // NQN format: nqn.2025-05.io.spdk:volume-pvc-XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX-replica-N
        if let Some(start) = nqn.find("volume-pvc-") {
            let uuid_start = start + "volume-pvc-".len();
            if let Some(end) = nqn[uuid_start..].find("-replica-") {
                let uuid_part = &nqn[uuid_start..uuid_start + end];
                // Convert PVC UUID to namespace UUID by using last 12 chars
                return format!("00000000-0000-0000-0000-{}", &uuid_part[uuid_part.len().saturating_sub(12)..]);
            }
        }
        
        // Fallback: generate a UUID based on the NQN hash  
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        nqn.hash(&mut hasher);
        format!("00000000-0000-0000-0000-{:012x}", hasher.finish() % 0x1000000000000)
    }

         /// Determine the appropriate address family for NVMe-oF transport
     fn determine_address_family(transport: &str, target_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
         match transport.to_lowercase().as_str() {
             "tcp" => {
                 // TCP transport: determine IPv4 vs IPv6
                 if target_addr.contains(':') && !target_addr.starts_with('[') {
                     // Simple IPv6 detection (more sophisticated parsing could be added)
                     Ok("ipv6".to_string())
                 } else {
                     Ok("ipv4".to_string())
                 }
             }
             "rdma" => {
                 // RDMA transport: could be IB, RoCE (IPv4/IPv6), or iWARP
                 if target_addr.contains(':') && !target_addr.starts_with('[') {
                     Ok("ipv6".to_string()) // RoCE v2 over IPv6
                 } else if target_addr.chars().all(|c| c.is_ascii_digit() || c == '.') {
                     Ok("ipv4".to_string()) // RoCE v2 over IPv4 or iWARP
                 } else {
                     // InfiniBand GID or other IB addressing
                     Ok("ib".to_string())
                 }
             }
             "fc" => {
                 // Fibre Channel
                 Ok("fc".to_string())
             }
             _ => {
                 // Default to IPv4 for unknown transports
                 println!("⚠️ Unknown transport '{}', defaulting to IPv4", transport);
                 Ok("ipv4".to_string())
             }
         }
     }

     // Create Kubernetes event for missing ublk_drv kernel module (removed)
    // removed: create_ublk_kernel_missing_event_removed
}
