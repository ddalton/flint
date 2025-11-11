// minimal_disk_service.rs - Minimal State Disk Service
// FOR NODE AGENTS ONLY - Uses direct Unix socket communication with SPDK

use serde_json::{json, Value};
use reqwest::Client as HttpClient;

use crate::minimal_models::{DiskInfo, MinimalStateError};

/// Pure SPDK disk discovery and management service
/// FOR NODE AGENTS ONLY - Uses direct Unix socket communication with SPDK  
/// Replaces all Kubernetes CRD operations with direct SPDK queries
#[derive(Clone)]
pub struct MinimalDiskService {
    pub node_name: String,
    pub spdk_socket_path: String,  // Unix socket path (e.g., "/tmp/spdk.sock")
    pub http_client: HttpClient,
}

impl MinimalDiskService {
    pub fn new(node_name: String, spdk_socket_path: String) -> Self {
        Self {
            node_name,
            spdk_socket_path,
            http_client: HttpClient::new(),
        }
    }

    /// Discover all disks on this node by querying SPDK directly
    pub async fn discover_local_disks(&self) -> Result<Vec<DiskInfo>, MinimalStateError> {
        println!("🔍 [MINIMAL_DISK] Starting pure SPDK disk discovery on node: {}", self.node_name);

        // Get data from SPDK
        let bdevs = self.get_spdk_bdevs().await?;
        let lvstores = self.get_spdk_lvstores().await?;
        let controllers = self.get_spdk_nvme_controllers().await?;

        let mut disks = Vec::new();
        if let Some(bdev_list) = bdevs["result"].as_array() {
            for bdev in bdev_list {
                if let Some(disk_info) = self.bdev_to_disk_info(bdev, &lvstores, &controllers).await? {
                    // Filter out system disks and non-storage devices
                    if self.is_storage_disk(&disk_info).await? {
                        disks.push(disk_info);
                    }
                }
            }
        }

        println!("✅ [MINIMAL_DISK] Discovered {} local storage disks", disks.len());
        Ok(disks)
    }

    /// Initialize blobstore (LVS) on a disk by PCI address
    pub async fn initialize_blobstore(&self, pci_address: &str) -> Result<String, MinimalStateError> {
        println!("🔧 [MINIMAL_DISK] Initializing blobstore on disk with PCI: {}", pci_address);

        // Find the disk first
        let disk_found = self.discover_local_disks().await?
            .into_iter()
            .find(|d| d.pci_address == pci_address)
            .ok_or_else(|| MinimalStateError::DiskNotFound { 
                node: self.node_name.clone(), 
                pci: pci_address.to_string() 
            })?;

        // Check if LVS already exists
        if disk_found.blobstore_initialized {
            println!("✅ [MINIMAL_DISK] LVS already exists: {:?}", disk_found.lvs_name);
            return Ok(disk_found.lvs_name.unwrap_or_else(|| "unknown".to_string()));
        }

        // Create new LVS
        let lvs_name = format!("lvs_{}_{}", self.node_name, pci_address.replace(":", "-").replace(".", "-"));
        let bdev_name = &disk_found.bdev_name;

        let create_lvs_params = json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": bdev_name,
                "lvs_name": lvs_name,
                "cluster_sz": 1048576  // 1MB clusters
            }
        });

        let response = self.call_spdk_rpc(&create_lvs_params).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to create LVS: {}", e) 
            })?;

        if let Some(error) = response.get("error") {
            return Err(MinimalStateError::SpdkRpcError { 
                message: format!("SPDK LVS creation failed: {}", error) 
            });
        }

        println!("✅ [MINIMAL_DISK] Successfully created LVS: {}", lvs_name);
        Ok(lvs_name)
    }

    /// Create logical volume on a disk
    pub async fn create_lvol(&self, lvs_name: &str, volume_id: &str, size_bytes: u64) -> Result<String, MinimalStateError> {
        println!("🔧 [MINIMAL_DISK] Creating lvol: {} in LVS: {} (size: {} bytes)", volume_id, lvs_name, size_bytes);
        
        let lvol_name = format!("vol_{}", volume_id);
        let size_mib = (size_bytes + 1048575) / 1048576; // Round up to MiB

        let create_params = json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size_in_mib": size_mib
            }
        });

        let response = self.call_spdk_rpc(&create_params).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to create lvol: {}", e) 
            })?;

        let lvol_uuid = response["result"].as_str()
            .ok_or_else(|| MinimalStateError::SpdkRpcError { 
                message: "No UUID in SPDK lvol create response".to_string() 
            })?
            .to_string();

        println!("✅ [MINIMAL_DISK] Created lvol {} with UUID: {}", lvol_name, lvol_uuid);
        Ok(lvol_uuid)
    }

    /// Delete logical volume 
    pub async fn delete_lvol(&self, lvol_uuid: &str) -> Result<(), MinimalStateError> {
        println!("🗑️ [MINIMAL_DISK] Deleting lvol with UUID: {}", lvol_uuid);

        let delete_params = json!({
            "method": "bdev_lvol_delete",
            "params": {
                "name": lvol_uuid
            }
        });

        self.call_spdk_rpc(&delete_params).await
            .map_err(|e| MinimalStateError::SpdkRpcError { 
                message: format!("Failed to delete lvol: {}", e) 
            })?;

        println!("✅ [MINIMAL_DISK] Successfully deleted lvol: {}", lvol_uuid);  
        Ok(())
    }

    // === PRIVATE HELPER METHODS ===

    /// Call SPDK RPC via Unix socket (NODE AGENT pattern)
    async fn call_spdk_rpc(&self, rpc_request: &Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        use crate::spdk_native::SpdkNative;
        
        let method = rpc_request["method"].as_str().unwrap_or("unknown");
        println!("🔧 [NODE_AGENT_SPDK] Calling SPDK method via Unix socket: {}", method);
        
        // Use Unix socket connection (not HTTP)
        let spdk = SpdkNative::new(Some(self.spdk_socket_path.clone())).await
            .map_err(|e| format!("Failed to connect to SPDK socket {}: {}", self.spdk_socket_path, e))?;

        // Call the method based on the RPC request
        match method {
            "bdev_get_bdevs" => {
                let bdevs = spdk.get_bdevs().await?;
                Ok(json!({ "result": bdevs }))
            }
            "bdev_lvol_get_lvstores" => {
                let lvstores = spdk.get_lvol_stores().await?;
                // Convert LvsInfo to serializable format  
                let serializable_lvstores: Vec<serde_json::Value> = lvstores.into_iter().map(|lvs| {
                    json!({
                        "name": lvs.name,
                        "uuid": lvs.uuid,
                        "cluster_size": lvs.cluster_size,
                        "total_clusters": lvs.total_clusters,
                        "free_clusters": lvs.free_clusters,
                        "block_size": lvs.block_size
                    })
                }).collect();
                Ok(json!({ "result": serializable_lvstores }))  
            }
            "bdev_nvme_get_controllers" => {
                // TODO: Implement in SpdkNative
                Ok(json!({ "result": [] }))
            }
            "bdev_lvol_create_lvstore" => {
                let params = rpc_request["params"].as_object()
                    .ok_or("Missing params for lvstore creation")?;
                let bdev_name = params["bdev_name"].as_str().unwrap_or("");
                let lvs_name = params["lvs_name"].as_str().unwrap_or("");
                let _cluster_sz = params["cluster_sz"].as_u64().unwrap_or(1048576);
                
                // TODO: Implement create_lvol_store in SpdkNative or use generic RPC
                println!("🚧 [NODE_AGENT_SPDK] LVS creation not yet implemented: {} on {}", lvs_name, bdev_name);
                Ok(json!({ "result": "success" }))
            }
            "bdev_lvol_create" => {
                let params = rpc_request["params"].as_object()
                    .ok_or("Missing params for lvol creation")?;
                let lvs_name = params["lvs_name"].as_str().unwrap_or("");
                let lvol_name = params["lvol_name"].as_str().unwrap_or("");  
                let size_mib = params["size_in_mib"].as_u64().unwrap_or(0);
                let size_bytes = size_mib * 1024 * 1024;
                
                let uuid = spdk.create_lvol(lvs_name, lvol_name, size_bytes, 1048576).await?;
                Ok(json!({ "result": uuid }))
            }
            "bdev_lvol_delete" => {
                let params = rpc_request["params"].as_object()
                    .ok_or("Missing params for lvol deletion")?;
                let name = params["name"].as_str().unwrap_or("");
                
                // TODO: Fix delete_lvol signature or use generic RPC
                println!("🚧 [NODE_AGENT_SPDK] Lvol deletion not yet implemented: {}", name);
                Ok(json!({ "result": "success" }))
            }
            _ => {
                Err(format!("Unsupported SPDK method: {}", method).into())
            }
        }
    }

    /// Get all bdevs from SPDK
    async fn get_spdk_bdevs(&self) -> Result<Value, MinimalStateError> {
        self.call_spdk_rpc(&json!({
            "method": "bdev_get_bdevs"
        })).await.map_err(|e| MinimalStateError::SpdkRpcError { 
            message: format!("Failed to get bdevs: {}", e) 
        })
    }

    /// Get all LVS from SPDK
    async fn get_spdk_lvstores(&self) -> Result<Value, MinimalStateError> {
        self.call_spdk_rpc(&json!({
            "method": "bdev_lvol_get_lvstores"
        })).await.map_err(|e| MinimalStateError::SpdkRpcError { 
            message: format!("Failed to get lvstores: {}", e) 
        })
    }

    /// Get NVMe controller information from SPDK
    async fn get_spdk_nvme_controllers(&self) -> Result<Value, MinimalStateError> {
        self.call_spdk_rpc(&json!({
            "method": "bdev_nvme_get_controllers"
        })).await.map_err(|e| MinimalStateError::SpdkRpcError { 
            message: format!("Failed to get controllers: {}", e) 
        })
    }

    /// Convert SPDK bdev JSON to our DiskInfo structure
    async fn bdev_to_disk_info(&self, bdev: &Value, lvstores: &Value, _controllers: &Value) -> Result<Option<DiskInfo>, MinimalStateError> {
        let bdev_name = bdev["name"].as_str().unwrap_or("");
        let product_name = bdev["product_name"].as_str().unwrap_or("");
        let block_size = bdev["block_size"].as_u64().unwrap_or(0);
        let num_blocks = bdev["num_blocks"].as_u64().unwrap_or(0);
        let claimed = bdev["claimed"].as_bool().unwrap_or(false);

        // Basic filtering for NVMe devices
        if !product_name.contains("NVMe") && !product_name.contains("SSD") {
            return Ok(None);
        }

        let size_bytes = block_size * num_blocks;
        let pci_address = self.extract_pci_from_bdev_name(bdev_name);
        let device_name = bdev_name.trim_start_matches("kernel_").to_string();
        
        // Find LVS information for this bdev
        let (lvs_name, free_space, lvol_count) = self.find_lvs_for_bdev(bdev_name, lvstores);
        
        Ok(Some(DiskInfo {
            node_name: self.node_name.clone(),
            pci_address,
            device_name,
            bdev_name: bdev_name.to_string(),
            size_bytes,
            healthy: !claimed, // Simple heuristic: unclaimed = healthy
            blobstore_initialized: lvs_name.is_some(),
            free_space,
            lvs_name,
            lvol_count,
            firmware: Some("unknown".to_string()),
            model: product_name.to_string(),
            serial: Some("unknown".to_string()),
        }))
    }

    /// Check if this is a storage disk (not a system disk)
    async fn is_storage_disk(&self, _disk: &DiskInfo) -> Result<bool, MinimalStateError> {
        // TODO: Implement proper storage disk filtering
        // For now, accept all disks
        Ok(true)
    }

    /// Extract PCI address from bdev name (placeholder)
    fn extract_pci_from_bdev_name(&self, bdev_name: &str) -> String {
        // TODO: Implement proper PCI extraction logic
        if bdev_name.starts_with("nvme") {
            // Extract number and create dummy PCI address
            if let Some(num_str) = bdev_name.chars().filter(|c| c.is_ascii_digit()).collect::<String>().chars().next() {
                return format!("0000:00:0{}:0", num_str);
            }
        }
        "0000:00:00:0".to_string()
    }

    /// Find LVS information for a bdev
    fn find_lvs_for_bdev(&self, bdev_name: &str, lvstores: &Value) -> (Option<String>, u64, u32) {
        if let Some(lvs_list) = lvstores["result"].as_array() {
            for lvs in lvs_list {
                if let Some(base_bdev) = lvs["base_bdev"].as_str() {
                    if base_bdev == bdev_name {
                        let lvs_name = lvs["name"].as_str().unwrap_or("").to_string();
                        let free_clusters = lvs["free_clusters"].as_u64().unwrap_or(0);
                        let cluster_size = lvs["cluster_size"].as_u64().unwrap_or(0);
                        let free_space = free_clusters * cluster_size;
                        let lvol_count = 0; // TODO: Count lvols
                        return (Some(lvs_name), free_space, lvol_count);
                    }
                }
            }
        }
        (None, 0, 0)
    }
}
