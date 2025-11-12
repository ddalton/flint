// driver_minimal.rs - Clean minimal state SPDK CSI Driver  
// CONTROLLER implementation - talks to Node Agents via HTTP (NOT directly to SPDK)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::{json, Value};
use reqwest::Client as HttpClient;
use tonic::Status;

// Kubernetes API imports
use kube::{Api, Client};
use k8s_openapi::api::core::v1::Node as k8sNode;

use crate::minimal_models::{MinimalStateError, DiskInfo};

/// Minimal State SPDK CSI Driver
/// Focuses on direct SPDK operations without heavy CRD management
#[derive(Clone)]
pub struct SpdkCsiDriver {
    pub kube_client: Client,
    pub target_namespace: String,
    pub node_id: String,
    pub spdk_rpc_url: String,
    pub nvmeof_transport: String,
    pub nvmeof_target_port: u16,
    
    // Simple caching for efficiency
    pub spdk_node_urls: Arc<Mutex<HashMap<String, String>>>,
    pub ublk_target_initialized: Arc<Mutex<bool>>,
}

impl SpdkCsiDriver {
    /// Create new minimal state driver instance
    pub fn new(
        kube_client: Client,
        target_namespace: String,
        node_id: String,
        spdk_rpc_url: String,
        nvmeof_transport: String,
        nvmeof_target_port: u16,
    ) -> Self {
        Self {
            kube_client,
            target_namespace,
            node_id,
            spdk_rpc_url,
            nvmeof_transport,
            nvmeof_target_port,
            spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
            ublk_target_initialized: Arc::new(Mutex::new(false)),
        }
    }

    /// Create volume using minimal state architecture
    pub async fn create_volume(&self, volume_id: &str, size_bytes: u64, replica_count: u32) -> Result<String, MinimalStateError> {
        println!("🎯 [DRIVER] Creating volume: {} ({} bytes, {} replicas)", volume_id, size_bytes, replica_count);

        // Get actual available disks from discovered nodes
        println!("📊 [DRIVER] Finding available disks from discovered nodes...");
        let node_name = "ublk-2.vpc.cloudera.com"; // Still use ublk-2 for now
        
        // Get real disks from the node agent
        let available_disks = self.get_available_disks_from_node(node_name).await?;
        if available_disks.is_empty() {
            return Err(MinimalStateError::InsufficientCapacity { 
                required: size_bytes, 
                available: 0 
            });
        }
        
        // Use the first available disk (for single replica)
        let selected_disk = &available_disks[0];
        let pci_address = &selected_disk.pci_address;
        println!("✅ [DRIVER] Selected disk: {} ({}GB) on node: {}", 
                 selected_disk.device_name, 
                 selected_disk.size_bytes / (1024*1024*1024), 
                 node_name);
        
        // Try to initialize blobstore on the selected disk
        match self.initialize_blobstore(node_name, pci_address).await {
            Ok(lvs_name) => {
                println!("✅ [DRIVER] Blobstore ready: {}", lvs_name);
                
                // Create logical volume
                let lvol_uuid = self.create_lvol(node_name, &lvs_name, volume_id, size_bytes).await?;
                
                println!("✅ [DRIVER] Volume {} created successfully with lvol UUID: {}", volume_id, lvol_uuid);
                Ok(lvol_uuid)
            }
            Err(e) => {
                println!("❌ [DRIVER] Failed to initialize blobstore: {}", e);
                Err(e)
            }
        }
    }

    /// Get SPDK RPC URL for a specific node (simplified)
    pub async fn get_rpc_url_for_node(&self, node_name: &str) -> Result<String, Status> {
        // For minimal state, we'll use a simple pattern
        // In production, this would query the kubernetes API to find the pod IP
        let url = format!("http://{}:8081/api/spdk/rpc", node_name);
        
        // Cache for efficiency
        let mut cache = self.spdk_node_urls.lock().await;
        cache.insert(node_name.to_string(), url.clone());
        
        Ok(url)
    }

    /// Get node IP address (simplified)
    pub async fn get_node_ip(&self, node_name: &str) -> Result<String, Status> {
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        let node = nodes_api.get(node_name).await
            .map_err(|e| Status::internal(format!("Failed to get node {}: {}", node_name, e)))?;

        if let Some(status) = &node.status {
            if let Some(addresses) = &status.addresses {
                // Prefer InternalIP
                for address in addresses {
                    if address.type_ == "InternalIP" {
                        return Ok(address.address.clone());
                    }
                }
                // Fallback to first address
                if let Some(addr) = addresses.first() {
                    return Ok(addr.address.clone());
                }
            }
        }
        
        Err(Status::not_found(format!("No IP address found for node {}", node_name)))
    }

    /// Get current node IP (cached)
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [MINIMAL_DRIVER] Getting IP for node: {}", self.node_id);
        Ok(self.get_node_ip(&self.node_id).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?)
    }

    /// Call Node Agent HTTP API (CONTROLLER pattern - not direct SPDK)
    pub async fn call_node_agent(&self, node_name: &str, endpoint: &str, payload: &Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        println!("🌐 [CONTROLLER_HTTP] Calling node agent: {} endpoint: {}", node_name, endpoint);
        
        // Get the node agent URL (HTTP, not direct SPDK socket)
        let node_agent_url = self.get_node_agent_url(node_name).await?;
        let full_url = format!("{}{}", node_agent_url, endpoint);
        
        let http_client = HttpClient::new();
        
        // All endpoints use POST (RPC-style communication)
        let response = http_client.post(&full_url).json(payload).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(format!("Node agent HTTP call failed: {}", error_text).into());
        }

        let json_response: Value = response.json().await?;
        Ok(json_response)
    }

    /// Get node agent HTTP URL (not SPDK socket)
    async fn get_node_agent_url(&self, node_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Check cache first
        {
            let cache = self.spdk_node_urls.lock().await;
            if let Some(url) = cache.get(node_name) {
                return Ok(url.clone());
            }
        }

        // Find the node agent pod IP
        let pod_ip = self.get_node_agent_pod_ip(node_name).await?;
        let node_agent_url = format!("http://{}:8081", pod_ip);
        
        // Cache it
        {
            let mut cache = self.spdk_node_urls.lock().await;
            cache.insert(node_name.to_string(), node_agent_url.clone());
        }
        
        Ok(node_agent_url)
    }

    /// Find node agent pod IP via Kubernetes API
    async fn get_node_agent_pod_ip(&self, node_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use kube::{api::ListParams, Api};
        use k8s_openapi::api::core::v1::Pod;
        
        let pods_api: Api<Pod> = Api::namespaced(self.kube_client.clone(), &self.target_namespace);
        let list_params = ListParams::default()
            .labels("app=flint-csi-node")
            .fields(&format!("spec.nodeName={}", node_name));
            
        let pods = pods_api.list(&list_params).await?;
        
        for pod in pods.items {
            if let Some(status) = pod.status {
                if let Some(pod_ip) = status.pod_ip {
                    println!("✅ [CONTROLLER_HTTP] Found node agent for {}: {}", node_name, pod_ip);
                    return Ok(pod_ip);
                }
            }
        }
        
        Err(format!("No node agent pod found for node {}", node_name).into())
    }

    /// Initialize blobstore on a disk (CONTROLLER calls Node Agent via HTTP)
    pub async fn initialize_blobstore(&self, node_name: &str, disk_pci_address: &str) -> Result<String, MinimalStateError> {
        println!("🔧 [CONTROLLER] Requesting blobstore initialization on node: {} disk: {}", node_name, disk_pci_address);

        let payload = json!({
            "pci_address": disk_pci_address
        });

        let response = self.call_node_agent(node_name, "/api/disks/initialize_blobstore", &payload).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to initialize blobstore via node agent: {}", e) 
            })?;

        let lvs_name = response["lvs_name"].as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError { 
                message: "No LVS name in node agent response".to_string() 
            })?
            .to_string();

        println!("✅ [CONTROLLER] Blobstore initialized via node agent: {}", lvs_name);
        Ok(lvs_name)
    }

    /// Create logical volume (CONTROLLER calls Node Agent via HTTP)  
    pub async fn create_lvol(&self, node_name: &str, lvs_name: &str, volume_id: &str, size_bytes: u64) -> Result<String, MinimalStateError> {
        println!("🔧 [CONTROLLER] Requesting lvol creation on node: {} LVS: {} volume: {}", node_name, lvs_name, volume_id);
        
        let payload = json!({
            "lvs_name": lvs_name,
            "volume_id": volume_id,
            "size_bytes": size_bytes
        });

        let response = self.call_node_agent(node_name, "/api/volumes/create_lvol", &payload).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to create lvol via node agent: {}", e) 
            })?;

        let lvol_uuid = response["lvol_uuid"].as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError { 
                message: "No lvol_uuid in node agent response".to_string() 
            })?
            .to_string();

        println!("✅ [CONTROLLER] Lvol created via node agent: {}", lvol_uuid);
        Ok(lvol_uuid)
    }

    /// Delete logical volume (CONTROLLER calls Node Agent via HTTP)
    pub async fn delete_lvol(&self, node_name: &str, lvol_uuid: &str) -> Result<(), MinimalStateError> {
        println!("🗑️ [CONTROLLER] Requesting lvol deletion on node: {} UUID: {}", node_name, lvol_uuid);

        let payload = json!({
            "lvol_uuid": lvol_uuid
        });

        self.call_node_agent(node_name, "/api/volumes/delete_lvol", &payload).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to delete lvol via node agent: {}", e) 
            })?;

        println!("✅ [CONTROLLER] Lvol deleted via node agent: {}", lvol_uuid);
        Ok(())
    }

    /// Create NVMe-oF target (minimal implementation - will be enhanced later)
    pub async fn create_nvmeof_target(&self, bdev_name: &str, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚧 [MINIMAL_NVMEOF] Creating NVMe-oF target for bdev: {}, nqn: {}", bdev_name, nqn);
        
        // For now, we'll implement a basic version
        // TODO: Enhance with full functionality later
        
        // 1. Create subsystem
        let subsystem_params = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true,
                "serial_number": format!("SPDK{:016x}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64),
                "model_number": "SPDK CSI Volume"
            }
        });

        // NOTE: For NVMe-oF, the controller should delegate to the target node
        // This is a placeholder - in real implementation, use call_node_agent
        match self.call_node_agent(&self.node_id, "/api/nvmeof/create_subsystem", &subsystem_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Subsystem created: {}", nqn),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [MINIMAL_NVMEOF] Subsystem already exists: {}", nqn);
            }
            Err(e) => return Err(e),
        }

        // 2. Add namespace
        let namespace_params = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "nsid": 1,
                    "bdev_name": bdev_name
                }
            }
        });

        match self.call_node_agent(&self.node_id, "/api/nvmeof/add_namespace", &namespace_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Namespace added for bdev: {}", bdev_name),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [MINIMAL_NVMEOF] Namespace already exists for bdev: {}", bdev_name);
            }
            Err(e) => return Err(e),
        }

        // 3. Add listener
        let node_ip = self.get_current_node_ip().await?;
        let listener_params = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": self.nvmeof_transport.to_uppercase(),
                    "traddr": node_ip,
                    "trsvcid": self.nvmeof_target_port.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });

        match self.call_node_agent(&self.node_id, "/api/nvmeof/add_listener", &listener_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Listener added: {}:{}", node_ip, self.nvmeof_target_port),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [MINIMAL_NVMEOF] Listener already exists: {}:{}", node_ip, self.nvmeof_target_port);
            }
            Err(e) => return Err(e),
        }

        println!("🎉 [MINIMAL_NVMEOF] NVMe-oF target setup completed: {}", nqn);
        Ok(())
    }

    /// Cleanup NVMe-oF target (minimal implementation)
    pub async fn cleanup_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🧹 [MINIMAL_NVMEOF] Cleaning up NVMe-oF target: {}", nqn);

        let delete_params = json!({
            "method": "nvmf_delete_subsystem",
            "params": {
                "nqn": nqn
            }
        });

        match self.call_node_agent(&self.node_id, "/api/nvmeof/delete_subsystem", &delete_params).await {
            Ok(_) => println!("✅ [MINIMAL_NVMEOF] Successfully deleted subsystem: {}", nqn),
            Err(e) => {
                println!("⚠️ [MINIMAL_NVMEOF] Failed to delete subsystem (may not exist): {}", e);
                // Don't fail - cleanup is best effort
            }
        }

        Ok(())
    }

    /// Create ublk device (simplified - keeping core functionality)
    pub async fn create_ublk_device(&self, bdev_name: &str, ublk_id: u32) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [MINIMAL_UBLK] Creating ublk device for bdev: {} with ID: {}", bdev_name, ublk_id);

        // Ensure ublk target exists first
        self.ensure_ublk_target().await?;

        let ublk_params = json!({
            "method": "ublk_start_disk",
            "params": {
                "bdev_name": bdev_name,
                "ublk_id": ublk_id
            }
        });

        self.call_node_agent(&self.node_id, "/api/ublk/create", &ublk_params).await?;

        let device_path = format!("/dev/ublkb{}", ublk_id);
        
        // Wait for device to appear
        for attempt in 1..=30 {
            if std::path::Path::new(&device_path).exists() {
                println!("✅ [MINIMAL_UBLK] Device created: {}", device_path);
                return Ok(device_path);
            }
            
            if attempt % 10 == 0 {
                println!("🔧 [MINIMAL_UBLK] Waiting for device... ({}/30)", attempt);
            }
            
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        Err(format!("Device {} did not appear after 3 seconds", device_path).into())
    }

    /// Delete ublk device (simplified)
    pub async fn delete_ublk_device(&self, ublk_id: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [MINIMAL_UBLK] Deleting ublk device with ID: {}", ublk_id);

        let delete_params = json!({
            "method": "ublk_stop_disk",
            "params": {
                "ublk_id": ublk_id
            }
        });

        match self.call_node_agent(&self.node_id, "/api/ublk/delete", &delete_params).await {
            Ok(_) => println!("✅ [MINIMAL_UBLK] Successfully deleted ublk device: {}", ublk_id),
            Err(e) => {
                println!("⚠️ [MINIMAL_UBLK] Failed to delete device (may not exist): {}", e);
                // Don't fail - cleanup is best effort
            }
        }

        Ok(())
    }

    /// Ensure ublk target exists (simplified)
    async fn ensure_ublk_target(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut initialized = self.ublk_target_initialized.lock().await;
        
        if *initialized {
            return Ok(());
        }

        let target_params = json!({
            "method": "ublk_create_target",
            "params": {}
        });

        match self.call_node_agent(&self.node_id, "/api/ublk/create_target", &target_params).await {
            Ok(_) => {
                println!("✅ [MINIMAL_UBLK] ublk target created");
                *initialized = true;
                Ok(())
            }
            Err(e) if e.to_string().contains("Method not found") => {
                println!("ℹ️ [MINIMAL_UBLK] SPDK doesn't support ublk - skipping");
                *initialized = true;
                Ok(())
            }
            Err(e) => {
                println!("⚠️ [MINIMAL_UBLK] ublk target creation failed: {}", e);
                *initialized = true; // Avoid infinite retries
                Ok(())
            }
        }
    }

    /// Generate predictable UUID from NQN for namespace consistency  
    pub fn generate_namespace_uuid_from_nqn(nqn: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        nqn.hash(&mut hasher);
        let hash = hasher.finish();
        
        // Convert to UUID format
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            (hash >> 32) as u32,
            ((hash >> 16) & 0xFFFF) as u16,
            (hash & 0xFFFF) as u16,
            (hash >> 48) as u16,
            hash & 0xFFFFFFFFFFFF
        )
    }

    /// Get available disks from a specific node
    pub async fn get_available_disks_from_node(&self, node_name: &str) -> Result<Vec<DiskInfo>, MinimalStateError> {
        println!("🔍 [DRIVER] Getting available disks from node: {}", node_name);
        
        let response = self.call_node_agent(node_name, "/api/disks/uninitialized", &serde_json::json!({})).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to get disks from node agent: {}", e) 
            })?;

        let disks_array = response["uninitialized_disks"].as_array()
            .ok_or_else(|| MinimalStateError::InternalError { 
                message: "No uninitialized_disks array in response".to_string() 
            })?;

        let mut disks = Vec::new();
        for disk_json in disks_array {
            let disk = DiskInfo {
                node_name: node_name.to_string(),
                pci_address: disk_json["pci_address"].as_str().unwrap_or("unknown").to_string(),
                device_name: disk_json["device_name"].as_str().unwrap_or("unknown").to_string(),
                bdev_name: format!("kernel_{}", disk_json["device_name"].as_str().unwrap_or("unknown")),
                size_bytes: disk_json["size_bytes"].as_u64().unwrap_or(0),
                free_space: disk_json["size_bytes"].as_u64().unwrap_or(0), // Assume all free for uninitialized
                model: disk_json["model"].as_str().unwrap_or("unknown").to_string(),
                serial: Some("unknown".to_string()),
                firmware: Some("unknown".to_string()),
                healthy: disk_json["healthy"].as_bool().unwrap_or(false),
                blobstore_initialized: false, // These are uninitialized disks
                lvs_name: None,
                lvol_count: 0,
            };
            disks.push(disk);
        }

        println!("✅ [DRIVER] Found {} available disks on node {}", disks.len(), node_name);
        Ok(disks)
    }

    /// Generate consistent ublk device ID from volume ID
    pub fn generate_ublk_id(&self, volume_id: &str) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        volume_id.hash(&mut hasher);
        let hash = hasher.finish();
        
        // Use lower 16 bits to keep ID manageable
        (hash & 0xFFFF) as u32
    }

    /// Verify bdev exists (simplified)
    pub async fn verify_bdev_exists(&self, bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [MINIMAL_DRIVER] Verifying bdev exists: {}", bdev_name);

        let bdev_params = json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": bdev_name
            }
        });

        let response = self.call_node_agent(&self.node_id, "/api/spdk/rpc", &bdev_params).await?;
        
        if let Some(result) = response.get("result") {
            if let Some(bdev_list) = result.as_array() {
                if !bdev_list.is_empty() {
                    println!("✅ [MINIMAL_DRIVER] Bdev verified: {}", bdev_name);
                    return Ok(());
                }
            }
        }

        Err(format!("Bdev '{}' not found in SPDK", bdev_name).into())
    }

    /// Get volume information (which node it's on, lvol UUID, etc.)
    pub async fn get_volume_info(&self, volume_id: &str) -> Result<VolumeInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DRIVER] Getting info for volume: {}", volume_id);
        
        // For now, we need to query all nodes to find where the volume is
        // In production, this would query a metadata store
        // For this implementation, we'll check the hardcoded node
        let node_name = "ublk-2.vpc.cloudera.com";
        
        // Query the node agent for volume info
        let payload = json!({
            "volume_id": volume_id
        });
        
        match self.call_node_agent(node_name, "/api/volumes/get_info", &payload).await {
            Ok(response) => {
                let lvol_uuid = response["lvol_uuid"].as_str()
                    .ok_or("No lvol_uuid in response")?
                    .to_string();
                let lvs_name = response["lvs_name"].as_str()
                    .ok_or("No lvs_name in response")?
                    .to_string();
                let size_bytes = response["size_bytes"].as_u64()
                    .ok_or("No size_bytes in response")?;
                
                Ok(VolumeInfo {
                    volume_id: volume_id.to_string(),
                    node_name: node_name.to_string(),
                    lvol_uuid,
                    lvs_name,
                    size_bytes,
                })
            }
            Err(_) => {
                Err(format!("Volume {} not found", volume_id).into())
            }
        }
    }

    /// Setup NVMe-oF target and return connection info
    pub async fn setup_nvmeof_target_on_node(&self, node_name: &str, bdev_name: &str, volume_id: &str) -> Result<NvmeofConnectionInfo, Box<dyn std::error::Error + Send + Sync>> {
        println!("🌐 [DRIVER] Setting up NVMe-oF target on node: {} for bdev: {}", node_name, bdev_name);
        
        let nqn = format!("nqn.2024.com.flint:volume:{}", volume_id);
        
        // Create subsystem
        let subsystem_params = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": nqn,
                "allow_any_host": true,
                "serial_number": format!("SPDK{:016x}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64),
                "model_number": "SPDK CSI Volume"
            }
        });

        match self.call_node_agent(node_name, "/api/spdk/rpc", &subsystem_params).await {
            Ok(_) => println!("✅ [DRIVER] Subsystem created: {}", nqn),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [DRIVER] Subsystem already exists: {}", nqn);
            }
            Err(e) => return Err(format!("Failed to create subsystem: {}", e).into()),
        }

        // Add namespace
        let namespace_params = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": nqn,
                "namespace": {
                    "nsid": 1,
                    "bdev_name": bdev_name
                }
            }
        });

        match self.call_node_agent(node_name, "/api/spdk/rpc", &namespace_params).await {
            Ok(_) => println!("✅ [DRIVER] Namespace added for bdev: {}", bdev_name),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [DRIVER] Namespace already exists for bdev: {}", bdev_name);
            }
            Err(e) => return Err(format!("Failed to add namespace: {}", e).into()),
        }

        // Add listener
        let node_ip = self.get_node_ip(node_name).await
            .map_err(|e| format!("Failed to get node IP: {}", e))?;
        
        let listener_params = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": nqn,
                "listen_address": {
                    "trtype": self.nvmeof_transport.to_uppercase(),
                    "traddr": node_ip,
                    "trsvcid": self.nvmeof_target_port.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });

        match self.call_node_agent(node_name, "/api/spdk/rpc", &listener_params).await {
            Ok(_) => println!("✅ [DRIVER] Listener added: {}:{}", node_ip, self.nvmeof_target_port),
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [DRIVER] Listener already exists: {}:{}", node_ip, self.nvmeof_target_port);
            }
            Err(e) => return Err(format!("Failed to add listener: {}", e).into()),
        }

        println!("🎉 [DRIVER] NVMe-oF target setup completed: {}", nqn);
        
        Ok(NvmeofConnectionInfo {
            nqn: nqn.clone(),
            target_ip: node_ip.clone(),
            target_port: self.nvmeof_target_port,
            transport: self.nvmeof_transport.clone(),
        })
    }

    /// Connect to NVMe-oF target from current node
    pub async fn connect_to_nvmeof_target(&self, conn_info: &NvmeofConnectionInfo) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔌 [DRIVER] Connecting to NVMe-oF target: {} at {}:{}", 
                 conn_info.nqn, conn_info.target_ip, conn_info.target_port);
        
        let controller_name = format!("nvme_{}", conn_info.nqn.replace(":", "_").replace(".", "_"));
        
        let attach_params = json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": controller_name,
                "trtype": conn_info.transport.to_uppercase(),
                "traddr": conn_info.target_ip,
                "trsvcid": conn_info.target_port.to_string(),
                "subnqn": conn_info.nqn,
                "adrfam": "IPv4"
            }
        });

        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &attach_params).await {
            Ok(response) => {
                // The response should contain the bdev names created
                if let Some(result) = response.get("result") {
                    if let Some(bdev_names) = result.as_array() {
                        if let Some(first_bdev) = bdev_names.first() {
                            if let Some(bdev_name) = first_bdev.as_str() {
                                println!("✅ [DRIVER] Connected to NVMe-oF target, bdev created: {}", bdev_name);
                                return Ok(bdev_name.to_string());
                            }
                        }
                    }
                }
                // Fallback - construct expected bdev name
                let bdev_name = format!("{}n1", controller_name);
                println!("✅ [DRIVER] Connected to NVMe-oF target, expected bdev: {}", bdev_name);
                Ok(bdev_name)
            }
            Err(e) if e.to_string().contains("already exists") => {
                println!("ℹ️ [DRIVER] Already connected to NVMe-oF target");
                let bdev_name = format!("{}n1", controller_name);
                Ok(bdev_name)
            }
            Err(e) => {
                Err(format!("Failed to attach NVMe controller: {}", e).into())
            }
        }
    }

    /// Disconnect from NVMe-oF target
    pub async fn disconnect_from_nvmeof_target(&self, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔌 [DRIVER] Disconnecting from NVMe-oF target: {}", nqn);
        
        let controller_name = format!("nvme_{}", nqn.replace(":", "_").replace(".", "_"));
        
        let detach_params = json!({
            "method": "bdev_nvme_detach_controller",
            "params": {
                "name": controller_name
            }
        });

        match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &detach_params).await {
            Ok(_) => {
                println!("✅ [DRIVER] Disconnected from NVMe-oF target: {}", nqn);
                Ok(())
            }
            Err(e) if e.to_string().contains("does not exist") => {
                println!("ℹ️ [DRIVER] NVMe controller not found (already disconnected): {}", controller_name);
                Ok(())
            }
            Err(e) => {
                println!("⚠️ [DRIVER] Failed to disconnect (continuing anyway): {}", e);
                Ok(()) // Best effort cleanup
            }
        }
    }

    /// Remove NVMe-oF target from a node
    pub async fn remove_nvmeof_target(&self, node_name: &str, nqn: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🧹 [DRIVER] Removing NVMe-oF target from node: {} nqn: {}", node_name, nqn);
        
        let delete_params = json!({
            "method": "nvmf_delete_subsystem",
            "params": {
                "nqn": nqn
            }
        });

        match self.call_node_agent(node_name, "/api/spdk/rpc", &delete_params).await {
            Ok(_) => {
                println!("✅ [DRIVER] Successfully removed NVMe-oF target: {}", nqn);
                Ok(())
            }
            Err(e) if e.to_string().contains("does not exist") => {
                println!("ℹ️ [DRIVER] NVMe-oF target not found (already removed): {}", nqn);
                Ok(())
            }
            Err(e) => {
                println!("⚠️ [DRIVER] Failed to remove target (continuing anyway): {}", e);
                Ok(()) // Best effort cleanup
            }
        }
    }
}

/// Volume information
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub volume_id: String,
    pub node_name: String,
    pub lvol_uuid: String,
    pub lvs_name: String,
    pub size_bytes: u64,
}

/// NVMe-oF connection information
#[derive(Debug, Clone)]
pub struct NvmeofConnectionInfo {
    pub nqn: String,
    pub target_ip: String,
    pub target_port: u16,
    pub transport: String,
}
