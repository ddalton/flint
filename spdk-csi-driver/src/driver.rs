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
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error>> {
        let current_node = std::env::var("NODE_NAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| self.node_id.clone());
        
        Ok(self.get_node_ip(&current_node).await?)
    }

    /// Ensure ublk target is created (required before starting disks)
    /// Only calls ublk_create_target once per CSI driver instance
    async fn ensure_ublk_target(&self) -> Result<(), Box<dyn std::error::Error>> {
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
    ) -> Result<String, Box<dyn std::error::Error>> {
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
    ) -> Result<(), Box<dyn std::error::Error>> {
        println!("Deleting ublk device with ID {}", ublk_id);
        
        // Use the same SPDK RPC pattern as node_agent.rs
        let rpc_request = json!({
            "method": "ublk_stop_disk",
            "params": {
                "ublk_id": ublk_id
            }
        });
        
        let response = self.call_spdk_rpc(&rpc_request).await?;
        
        // Check for SPDK RPC errors, but ignore "does not exist" type errors
        if let Some(error) = response.get("error") {
            let error_str = error.to_string();
            if !error_str.contains("does not exist") && !error_str.contains("not found") {
                return Err(format!("SPDK RPC error: {}", error).into());
            }
        }
        
        println!("Deleted ublk device with ID {}", ublk_id);
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
    ) -> Result<(), Box<dyn std::error::Error>> {
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
            return Err(format!("Failed to add namespace to NVMf subsystem: {}", error_text).into());
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
            return Err(format!("Failed to add listener to NVMf subsystem: {}", error_text).into());
        }

        println!("Successfully created NVMe-oF target: {} on {}:{}", nqn, node_ip, self.nvmeof_target_port);
        Ok(())
    }

    /// Call SPDK RPC using the same pattern as node_agent.rs
    async fn call_spdk_rpc(
        &self,
        rpc_request: &serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
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
