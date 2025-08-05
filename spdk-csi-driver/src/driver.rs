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

#[derive(Clone)]
pub struct SpdkCsiDriver {
    pub node_id: String,
    pub kube_client: Client,
    pub spdk_rpc_url: String,
    pub spdk_node_urls: Arc<Mutex<HashMap<String, String>>>,
    pub nvmeof_target_port: u16,
    pub nvmeof_transport: String,
    pub target_namespace: String,
    pub ublk_target_initialized: Arc<Mutex<bool>>,
}

impl SpdkCsiDriver {
    /// Gets the SPDK RPC URL for a specific node by finding the 'node_agent' pod
    pub async fn get_rpc_url_for_node(&self, node_name: &str) -> Result<String, Status> {
        let mut cache = self.spdk_node_urls.lock().await;

        if let Some(url) = cache.get(node_name) {
            return Ok(url.clone());
        }

        println!("Discovering flint-csi-node pod for node '{}'...", node_name);
        let pods_api: Api<Pod> = Api::all(self.kube_client.clone());
        let lp = kube::api::ListParams::default().labels("app=flint-csi-node");

        let pods = pods_api.list(&lp).await
            .map_err(|e| Status::internal(format!("Failed to list flint-csi-node pods: {}", e)))?;

        for pod in pods {
            let pod_node = pod.spec.as_ref().and_then(|s| s.node_name.as_deref());
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref());

            if let (Some(p_node), Some(p_ip)) = (pod_node, pod_ip) {
                let url = format!("http://{}:8081/api/spdk/rpc", p_ip);
                cache.insert(p_node.to_string(), url);
            }
        }

        cache.get(node_name).cloned()
            .ok_or_else(|| Status::not_found(format!("Could not find flint-csi-node pod on node '{}'", node_name)))
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
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let current_node = std::env::var("NODE_NAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| self.node_id.clone());
        
        Ok(self.get_node_ip(&current_node).await?)
    }

    /// Ensure ublk target is created (required before starting disks)
    /// Only calls ublk_create_target once per CSI driver instance
    async fn ensure_ublk_target(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut initialized = self.ublk_target_initialized.lock().await;
        
        if *initialized {
            // Target already created, nothing to do
            return Ok(());
        }
        
        println!("Creating ublk target (first time)");
        
        let rpc_request = json!({
            "method": "ublk_create_target",
            "params": {}
        });
        
        let response = self.call_spdk_rpc(&rpc_request).await?;
        
        // Check for SPDK RPC errors
        if let Some(error) = response.get("error") {
            return Err(format!("Failed to create ublk target: {}", error).into());
        }
        
        // Mark as initialized
        *initialized = true;
        println!("ublk target created successfully");
        Ok(())
    }

    /// Create ublk device for a bdev
    pub async fn create_ublk_device(
        &self,
        bdev_name: &str,
        ublk_id: u32,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("Creating ublk device for bdev {} with ID {}", bdev_name, ublk_id);
        
        // Check if device already exists
        let device_path = format!("/dev/ublkb{}", ublk_id);
        if std::path::Path::new(&device_path).exists() {
            println!("ublk device {} already exists, cleaning up first", device_path);
            // Try to stop the existing device
            if let Err(e) = self.delete_ublk_device(ublk_id).await {
                println!("Warning: Failed to cleanup existing ublk device {}: {}", ublk_id, e);
            }
            // Wait a moment for cleanup
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        
        // Ensure ublk target exists first
        self.ensure_ublk_target().await?;
        
        // Use the same SPDK RPC pattern as node_agent.rs
        let rpc_request = json!({
            "method": "ublk_start_disk",
            "params": {
                "bdev_name": bdev_name,
                "ublk_id": ublk_id
            }
        });
        
        let response = self.call_spdk_rpc(&rpc_request).await?;
        
        // Check for SPDK RPC errors
        if let Some(error) = response.get("error") {
            return Err(format!("SPDK RPC error: {}", error).into());
        }
        
        println!("Successfully created ublk device: {} -> {}", bdev_name, device_path);
        Ok(device_path)
    }
    
    /// Delete ublk device
    pub async fn delete_ublk_device(
        &self,
        ublk_id: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [UBLK_DELETE] Deleting ublk device with ID {}", ublk_id);
        
        // Use the same SPDK RPC pattern as node_agent.rs
        let rpc_request = json!({
            "method": "ublk_stop_disk",
            "params": {
                "ublk_id": ublk_id
            }
        });
        
        // Add timeout protection for the RPC call
        let timeout_duration = tokio::time::Duration::from_secs(10);
        let response = match tokio::time::timeout(timeout_duration, self.call_spdk_rpc(&rpc_request)).await {
            Ok(result) => result?,
            Err(_) => {
                println!("⚠️ [UBLK_DELETE] RPC call timed out for ublk device {}", ublk_id);
                return Err("SPDK RPC call timed out".into());
            }
        };
        
        // Check for SPDK RPC errors, but ignore "does not exist" type errors
        if let Some(error) = response.get("error") {
            let error_str = error.to_string();
            if error_str.contains("does not exist") || error_str.contains("not found") {
                println!("ℹ️ [UBLK_DELETE] ublk device {} already deleted or doesn't exist", ublk_id);
                return Ok(()); // Not an error - device is already gone
            } else {
                println!("❌ [UBLK_DELETE] SPDK RPC error for ublk device {}: {}", ublk_id, error);
                return Err(format!("SPDK RPC error: {}", error).into());
            }
        }
        
        println!("✅ [UBLK_DELETE] Successfully deleted ublk device with ID {}", ublk_id);
        Ok(())
    }
    
    /// Generate a unique ublk ID based on volume ID
    pub fn generate_ublk_id(&self, volume_id: &str) -> u32 {
        // Simple hash-based ID generation (0-1023 range for ublk)
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        volume_id.hash(&mut hasher);
        (hasher.finish() % 1024) as u32
    }

    /// Create NVMe-oF target for a volume
    pub async fn create_nvmeof_target(
        &self,
        bdev_name: &str,
        nqn: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        let node_ip = self.get_current_node_ip().await?;

        println!("Creating NVMe-oF target for bdev {} with NQN {}", bdev_name, nqn);

        // 1. Create NVMe-oF subsystem
        let subsystem_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_create_subsystem",
                "params": {
                    "nqn": nqn,
                    "allow_any_host": true,
                    "serial_number": format!("SPDK-{}", uuid::Uuid::new_v4()),
                    "model_number": "SPDK CSI Volume",
                    "max_namespaces": 1
                }
            }))
            .send()
            .await?;

        if !subsystem_response.status().is_success() {
            let error_text = subsystem_response.text().await?;
            // Ignore "already exists" errors
            if !error_text.contains("already exists") {
                return Err(format!("Failed to create NVMf subsystem: {}", error_text).into());
            }
        }

        // 2. Add namespace to subsystem
        let namespace_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_subsystem_add_ns",
                "params": {
                    "nqn": nqn,
                    "namespace": {
                        "nsid": 1,
                        "bdev_name": bdev_name
                    }
                }
            }))
            .send()
            .await?;

        if !namespace_response.status().is_success() {
            let error_text = namespace_response.text().await?;
            // Handle "already exists" for namespace
            if !error_text.contains("already exists") && !error_text.contains("Namespace already exists") {
                return Err(format!("Failed to add namespace to NVMf subsystem: {}", error_text).into());
            }
        }

        // 3. Add listener to subsystem
        let listener_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_subsystem_add_listener",
                "params": {
                    "nqn": nqn,
                    "listen_address": {
                        "trtype": self.nvmeof_transport.to_uppercase(),
                        "traddr": "0.0.0.0", // Listen on all interfaces
                        "trsvcid": self.nvmeof_target_port.to_string(),
                        "adrfam": "ipv4"
                    }
                }
            }))
            .send()
            .await?;

        if !listener_response.status().is_success() {
            let error_text = listener_response.text().await?;
            // Handle "already exists" for listener
            if !error_text.contains("already exists") && !error_text.contains("Listener already exists") {
                return Err(format!("Failed to add listener to NVMf subsystem: {}", error_text).into());
            }
        }

        println!("Successfully created NVMe-oF target: {} on {}:{}", nqn, node_ip, self.nvmeof_target_port);
        Ok(())
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
        println!("🔍 [NVMEOF_DEBUG] Retrieved {} subsystems from SPDK", 
                 subsystems.as_array().map(|a| a.len()).unwrap_or(0));
        
        // Debug: List all subsystems
        if let Some(subsystem_list) = subsystems.as_array() {
            println!("🔍 [NVMEOF_DEBUG] All configured subsystems:");
            for (i, subsystem) in subsystem_list.iter().enumerate() {
                if let Some(existing_nqn) = subsystem.get("nqn").and_then(|v| v.as_str()) {
                    println!("🔍 [NVMEOF_DEBUG]   {}: {}", i + 1, existing_nqn);
                }
            }
        }
        
        // Step 2: Find our specific subsystem
        println!("🔍 [NVMEOF_DEBUG] Step 2: Searching for target subsystem: {}", nqn);
        if let Some(subsystem_list) = subsystems.as_array() {
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

    /// Enhanced NVMe-oF target creation with detailed debugging
    pub async fn create_nvmeof_target_debug(
        &self,
        bdev_name: &str,
        nqn: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        let node_ip = self.get_current_node_ip().await?;

        println!("🚀 [NVMEOF_CREATE_DEBUG] Starting NVMe-oF target creation");
        println!("🚀 [NVMEOF_CREATE_DEBUG] Parameters:");
        println!("🚀 [NVMEOF_CREATE_DEBUG]   bdev_name: {}", bdev_name);
        println!("🚀 [NVMEOF_CREATE_DEBUG]   nqn: {}", nqn);
        println!("🚀 [NVMEOF_CREATE_DEBUG]   node_ip: {}", node_ip);
        println!("🚀 [NVMEOF_CREATE_DEBUG]   port: {}", self.nvmeof_target_port);
        println!("🚀 [NVMEOF_CREATE_DEBUG]   transport: {}", self.nvmeof_transport);

        // Step 1: Verify bdev exists before creating subsystem
        println!("🔍 [NVMEOF_CREATE_DEBUG] Step 1: Verifying bdev exists...");
        match self.verify_bdev_exists(bdev_name).await {
            Ok(_) => println!("✅ [NVMEOF_CREATE_DEBUG] Bdev {} exists", bdev_name),
            Err(e) => {
                println!("❌ [NVMEOF_CREATE_DEBUG] Bdev {} not found: {}", bdev_name, e);
                return Err(format!("Cannot create NVMe-oF target: bdev {} does not exist: {}", bdev_name, e).into());
            }
        }

        // Step 2: Create NVMe-oF subsystem with detailed logging
        println!("🔍 [NVMEOF_CREATE_DEBUG] Step 2: Creating NVMe-oF subsystem...");
        let subsystem_payload = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true,
                "serial_number": format!("SPDK-{}", uuid::Uuid::new_v4()),
                "model_number": "SPDK CSI Volume",
                "max_namespaces": 1
            }
        });
        println!("🔍 [NVMEOF_CREATE_DEBUG] Subsystem RPC payload: {}", subsystem_payload);

        let subsystem_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&subsystem_payload)
            .send()
            .await?;

        if !subsystem_response.status().is_success() {
            let error_text = subsystem_response.text().await?;
            println!("⚠️ [NVMEOF_CREATE_DEBUG] Subsystem creation response: {}", error_text);
            
            // Ignore "already exists" errors but log them
            if error_text.contains("already exists") {
                println!("ℹ️ [NVMEOF_CREATE_DEBUG] Subsystem already exists (continuing)");
            } else {
                println!("❌ [NVMEOF_CREATE_DEBUG] Subsystem creation failed: {}", error_text);
                return Err(format!("Failed to create NVMf subsystem: {}", error_text).into());
            }
        } else {
            println!("✅ [NVMEOF_CREATE_DEBUG] Subsystem created successfully");
        }

        // Step 3: Add namespace to subsystem with detailed logging
        println!("🔍 [NVMEOF_CREATE_DEBUG] Step 3: Adding namespace to subsystem...");
        let namespace_payload = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "nsid": 1,
                    "bdev_name": bdev_name
                }
            }
        });
        println!("🔍 [NVMEOF_CREATE_DEBUG] Namespace RPC payload: {}", namespace_payload);

        let namespace_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&namespace_payload)
            .send()
            .await?;

        if !namespace_response.status().is_success() {
            let error_text = namespace_response.text().await?;
            println!("⚠️ [NVMEOF_CREATE_DEBUG] Namespace addition response: {}", error_text);
            
            // Handle "already exists" for namespace
            if error_text.contains("already exists") || error_text.contains("Namespace already exists") {
                println!("ℹ️ [NVMEOF_CREATE_DEBUG] Namespace already exists (continuing)");
            } else {
                println!("❌ [NVMEOF_CREATE_DEBUG] Namespace addition failed: {}", error_text);
                return Err(format!("Failed to add namespace to NVMf subsystem: {}", error_text).into());
            }
        } else {
            println!("✅ [NVMEOF_CREATE_DEBUG] Namespace added successfully");
        }

        // Step 4: Add listener to subsystem with detailed logging
        println!("🔍 [NVMEOF_CREATE_DEBUG] Step 4: Adding listener to subsystem...");
        let listener_payload = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": self.nvmeof_transport.to_uppercase(),
                    "traddr": "0.0.0.0", // Listen on all interfaces
                    "trsvcid": self.nvmeof_target_port.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });
        println!("🔍 [NVMEOF_CREATE_DEBUG] Listener RPC payload: {}", listener_payload);

        let listener_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&listener_payload)
            .send()
            .await?;

        if !listener_response.status().is_success() {
            let error_text = listener_response.text().await?;
            println!("⚠️ [NVMEOF_CREATE_DEBUG] Listener addition response: {}", error_text);
            
            // Handle "already exists" for listener
            if error_text.contains("already exists") || error_text.contains("Listener already exists") {
                println!("ℹ️ [NVMEOF_CREATE_DEBUG] Listener already exists (continuing)");
            } else {
                println!("❌ [NVMEOF_CREATE_DEBUG] Listener addition failed: {}", error_text);
                return Err(format!("Failed to add listener to NVMf subsystem: {}", error_text).into());
            }
        } else {
            println!("✅ [NVMEOF_CREATE_DEBUG] Listener added successfully");
        }

        // Step 5: Verify the complete configuration
        println!("🔍 [NVMEOF_CREATE_DEBUG] Step 5: Verifying complete configuration...");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await; // Small delay for consistency
        
        match self.validate_nvmeof_target(nqn).await {
            Ok(_) => {
                println!("✅ [NVMEOF_CREATE_DEBUG] Complete NVMe-oF target creation and validation successful");
                println!("🚀 [NVMEOF_CREATE_DEBUG] Target accessible at: {}:{} (transport: {})", 
                         node_ip, self.nvmeof_target_port, self.nvmeof_transport);
            }
            Err(e) => {
                println!("❌ [NVMEOF_CREATE_DEBUG] Post-creation validation failed: {}", e);
                return Err(format!("NVMe-oF target created but validation failed: {}", e).into());
            }
        }

        Ok(())
    }

    /// Verify that a bdev exists in SPDK
    async fn verify_bdev_exists(&self, bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        
        println!("🔍 [BDEV_VERIFY] Checking if bdev '{}' exists...", bdev_name);
        
        let bdev_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_get_bdevs",
                "params": {
                    "name": bdev_name
                }
            }))
            .send()
            .await?;

        if !bdev_response.status().is_success() {
            let error_text = bdev_response.text().await?;
            println!("❌ [BDEV_VERIFY] Failed to query bdev: {}", error_text);
            return Err(format!("Failed to query bdev {}: {}", bdev_name, error_text).into());
        }

        let bdevs: serde_json::Value = bdev_response.json().await?;
        
        if let Some(bdev_list) = bdevs.as_array() {
            if bdev_list.is_empty() {
                println!("❌ [BDEV_VERIFY] Bdev '{}' not found", bdev_name);
                return Err(format!("Bdev '{}' does not exist", bdev_name).into());
            }
            
            println!("✅ [BDEV_VERIFY] Bdev '{}' exists", bdev_name);
            
            // Debug: Show bdev details
            for bdev in bdev_list {
                if let Some(name) = bdev.get("name").and_then(|v| v.as_str()) {
                    let size = bdev.get("num_blocks").and_then(|v| v.as_u64()).unwrap_or(0);
                    let block_size = bdev.get("block_size").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!("🔍 [BDEV_VERIFY] Bdev details: name={}, blocks={}, block_size={}", 
                             name, size, block_size);
                }
            }
        }
        
        Ok(())
    }

    /// Delete a logical volume for cleanup purposes
    pub async fn delete_lvol(&self, lvs_name: &str, lvol_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let http_client = HttpClient::new();
        let lvol_bdev_name = format!("{}/{}", lvs_name, lvol_name);
        
        println!("🗑️ [LVOL_DELETE] Deleting logical volume: {}", lvol_bdev_name);
        
        let delete_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_lvol_delete",
                "params": {
                    "name": lvol_bdev_name
                }
            }))
            .send()
            .await?;

        if !delete_response.status().is_success() {
            let error_text = delete_response.text().await?;
            // Don't fail if volume doesn't exist
            if !error_text.contains("does not exist") && !error_text.contains("not found") {
                return Err(format!("Failed to delete logical volume {}: {}", lvol_bdev_name, error_text).into());
            }
        }

        println!("✅ [LVOL_DELETE] Successfully deleted logical volume: {}", lvol_bdev_name);
        Ok(())
    }

    /// Enhanced ublk device creation with comprehensive debugging
    pub async fn create_ublk_device_debug(&self, bdev_name: &str, ublk_id: u32) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [UBLK_CREATE_DEBUG] Starting ublk device creation with debugging");
        println!("🔧 [UBLK_CREATE_DEBUG] Parameters:");
        println!("🔧 [UBLK_CREATE_DEBUG]   bdev_name: {}", bdev_name);
        println!("🔧 [UBLK_CREATE_DEBUG]   ublk_id: {}", ublk_id);

        // Step 1: Pre-flight checks
        println!("🔧 [UBLK_CREATE_DEBUG] Step 1: Pre-flight checks...");
        
        // Check if device already exists
        let device_path = format!("/dev/ublkc{}", ublk_id);
        if std::path::Path::new(&device_path).exists() {
            println!("⚠️ [UBLK_CREATE_DEBUG] ublk device {} already exists", device_path);
            return Ok(device_path);
        }

        // Verify bdev exists
        match self.verify_bdev_exists(bdev_name).await {
            Ok(_) => println!("✅ [UBLK_CREATE_DEBUG] Target bdev verified"),
            Err(e) => {
                println!("❌ [UBLK_CREATE_DEBUG] Target bdev verification failed: {}", e);
                return Err(format!("Cannot create ublk device: target bdev '{}' not found: {}", bdev_name, e).into());
            }
        }

        // Step 2: Create ublk device with detailed logging
        println!("🔧 [UBLK_CREATE_DEBUG] Step 2: Creating ublk device...");
        let ublk_payload = json!({
            "method": "ublk_start_disk",
            "params": {
                "ublk_id": ublk_id,
                "bdev_name": bdev_name
            }
        });
        println!("🔧 [UBLK_CREATE_DEBUG] SPDK RPC payload: {}", ublk_payload);

        let http_client = HttpClient::new();
        let ublk_response = http_client
            .post(&self.spdk_rpc_url)
            .json(&ublk_payload)
            .send()
            .await?;

        if !ublk_response.status().is_success() {
            let error_text = ublk_response.text().await?;
            println!("❌ [UBLK_CREATE_DEBUG] ublk creation failed: {}", error_text);
            
            // Analyze the error
            if error_text.contains("No such device") || error_text.contains("-19") {
                println!("🔍 [UBLK_CREATE_DEBUG] Error analysis: bdev not found or not accessible");
                println!("🔍 [UBLK_CREATE_DEBUG] This typically means:");
                println!("🔍 [UBLK_CREATE_DEBUG]   1. The NVMe-oF connection failed");
                println!("🔍 [UBLK_CREATE_DEBUG]   2. The remote bdev name is incorrect");
                println!("🔍 [UBLK_CREATE_DEBUG]   3. The target subsystem is not properly configured");
            } else if error_text.contains("already exists") {
                println!("ℹ️ [UBLK_CREATE_DEBUG] Device already exists (acceptable)");
            } else {
                println!("🔍 [UBLK_CREATE_DEBUG] Unexpected error: {}", error_text);
            }
            
            return Err(format!("ublk device creation failed: {}", error_text).into());
        } else {
            println!("✅ [UBLK_CREATE_DEBUG] ublk RPC call successful");
        }

        // Step 3: Wait for device to appear and verify
        println!("🔧 [UBLK_CREATE_DEBUG] Step 3: Waiting for device to appear...");
        let mut attempts = 0;
        let max_attempts = 30; // 30 seconds maximum
        
        while attempts < max_attempts {
            if std::path::Path::new(&device_path).exists() {
                println!("✅ [UBLK_CREATE_DEBUG] Device {} appeared after {} seconds", device_path, attempts);
                break;
            }
            
            attempts += 1;
            if attempts % 5 == 0 {
                println!("🔧 [UBLK_CREATE_DEBUG] Still waiting for device... ({}/{})", attempts, max_attempts);
            }
            
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        if !std::path::Path::new(&device_path).exists() {
            println!("❌ [UBLK_CREATE_DEBUG] Device {} did not appear after {} seconds", device_path, max_attempts);
            return Err(format!("ublk device {} did not appear within timeout", device_path).into());
        }

        // Step 4: Final verification
        println!("🔧 [UBLK_CREATE_DEBUG] Step 4: Final device verification...");
        
        // Check device properties
        if let Ok(metadata) = std::fs::metadata(&device_path) {
            println!("🔧 [UBLK_CREATE_DEBUG] Device properties:");
            println!("🔧 [UBLK_CREATE_DEBUG]   Path: {}", device_path);
            println!("🔧 [UBLK_CREATE_DEBUG]   Size: {} bytes", metadata.len());
            println!("🔧 [UBLK_CREATE_DEBUG]   Type: {:?}", metadata.file_type());
        }

        println!("✅ [UBLK_CREATE_DEBUG] ublk device creation complete: {}", device_path);
        Ok(device_path)
    }

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
}
