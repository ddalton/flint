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

#[derive(Clone)]
pub struct SpdkCsiDriver {
    pub node_id: String,
    pub kube_client: Client,
    pub spdk_rpc_url: String,
    pub spdk_node_urls: Arc<Mutex<HashMap<String, String>>>,
    pub nvmeof_target_port: u16,
    pub nvmeof_transport: String,
}

impl SpdkCsiDriver {
    /// Gets the SPDK RPC URL for a specific node by finding the 'node_agent' pod
    pub async fn get_rpc_url_for_node(&self, node_name: &str) -> Result<String, Status> {
        let mut cache = self.spdk_node_urls.lock().await;

        if let Some(url) = cache.get(node_name) {
            return Ok(url.clone());
        }

        println!("Discovering spdk-node-agent pod for node '{}'...", node_name);
        let pods_api: Api<Pod> = Api::all(self.kube_client.clone());
        let lp = kube::api::ListParams::default().labels("app=spdk-node-agent");

        let pods = pods_api.list(&lp).await
            .map_err(|e| Status::internal(format!("Failed to list spdk-node-agent pods: {}", e)))?;

        for pod in pods {
            let pod_node = pod.spec.as_ref().and_then(|s| s.node_name.as_deref());
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref());

            if let (Some(p_node), Some(p_ip)) = (pod_node, pod_ip) {
                let url = format!("http://{}:5260", p_ip);
                cache.insert(p_node.to_string(), url);
            }
        }

        cache.get(node_name).cloned()
            .ok_or_else(|| Status::not_found(format!("Could not find spdk-node-agent pod on node '{}'", node_name)))
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

    /// Helper method to check if a bdev exists
    pub async fn check_bdev_exists(&self, bdev_name: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();
        
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "bdev_get_bdevs",
                "params": {
                    "name": bdev_name
                }
            }))
            .send()
            .await?;

        if response.status().is_success() {
            let bdevs: serde_json::Value = response.json().await?;
            if let Some(bdev_list) = bdevs["result"].as_array() {
                return Ok(!bdev_list.is_empty());
            }
        }

        Ok(false)
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

    /// Delete NVMe-oF target
    pub async fn delete_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        println!("Deleting NVMe-oF target: {}", nqn);

        // Delete subsystem (this also removes namespaces and listeners)
        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": "nvmf_delete_subsystem",
                "params": {
                    "nqn": nqn
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            // Ignore "does not exist" errors
            if !error_text.contains("does not exist") {
                return Err(format!("Failed to delete NVMf subsystem: {}", error_text).into());
            }
        }

        println!("Deleted NVMe-oF target: {}", nqn);
        Ok(())
    }

    /// Import NVMe-oF bdev as initiator (for connecting to remote targets)
    pub async fn connect_nvmeof_bdev(
        &self,
        target_node: &str,
        nqn: &str,
        target_ip: &str,
        bdev_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rpc_url = self.get_rpc_url_for_node(target_node).await?;
        let http_client = HttpClient::new();

        println!("Connecting to NVMe-oF target {} on {} as bdev {}", nqn, target_ip, bdev_name);

        let response = http_client
            .post(&rpc_url)
            .json(&json!({
                "method": "bdev_nvme_attach_controller",
                "params": {
                    "name": bdev_name,
                    "trtype": self.nvmeof_transport.to_uppercase(),
                    "traddr": target_ip,
                    "adrfam": "ipv4",
                    "trsvcid": self.nvmeof_target_port.to_string(),
                    "subnqn": nqn
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Failed to attach NVMf controller: {}", error_text).into());
        }

        println!("Successfully connected to NVMf target {} as bdev {}", nqn, bdev_name);
        Ok(())
    }

    /// Disconnect from NVMe-oF target
    pub async fn disconnect_nvmeof_bdev(
        &self,
        target_node: &str,
        bdev_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rpc_url = self.get_rpc_url_for_node(target_node).await?;
        let http_client = HttpClient::new();

        println!("Disconnecting NVMe-oF bdev: {}", bdev_name);

        let response = http_client
            .post(&rpc_url)
            .json(&json!({
                "method": "bdev_nvme_detach_controller",
                "params": {
                    "name": bdev_name
                }
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            // Log warning but don't fail - device might already be disconnected
            eprintln!("Warning: Failed to detach NVMf controller {}: {}", bdev_name, error_text);
        } else {
            println!("Disconnected NVMe-oF bdev: {}", bdev_name);
        }

        Ok(())
    }

    /// Execute a generic SPDK RPC call
    pub async fn spdk_rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let http_client = HttpClient::new();

        let response = http_client
            .post(&self.spdk_rpc_url)
            .json(&json!({
                "method": method,
                "params": params
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("SPDK RPC {} failed: {}", method, error_text).into());
        }

        let result: serde_json::Value = response.json().await?;
        Ok(result)
    }
}

/// Helper function to get the current pod's node
pub async fn get_pod_node(client: &kube::Client) -> Result<String, Box<dyn std::error::Error>> {
    // Try environment variables first (most reliable)
    if let Ok(node_name) = std::env::var("NODE_NAME") {
        return Ok(node_name);
    }

    // Fallback to pod API lookup
    let pod_name = std::env::var("POD_NAME")?;
    let namespace = std::env::var("POD_NAMESPACE").unwrap_or("default".to_string());
    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);

    for attempt in 0..3 {
        match pods.get(&pod_name).await {
            Ok(pod) => {
                if let Some(node_name) = pod.spec.and_then(|s| s.node_name) {
                    return Ok(node_name);
                }
            }
            Err(e) => {
                if attempt == 2 {
                    return Err(format!("Failed to get pod after {} attempts: {}", attempt + 1, e).into());
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    Err("Pod node not assigned after retries".into())
}