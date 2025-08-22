// node_agent/mod.rs - Node Agent Module Coordination
//
// This module coordinates the various components of the SPDK node agent:
// - Disk discovery and management
// - NVMe-oF export management  
// - HTTP API server for management operations
// - RAID operations and blobstore initialization
// - Health monitoring and status tracking

use kube::{Client, Api};
use tokio::time::Duration;

use serde_json::json;
use anyhow::Result;
use chrono;

use std::env;

// Re-export submodules
pub mod disk_discovery;
pub mod nvmeof_manager;
pub mod http_api;
pub mod raid_operations;
pub mod health_monitor;
pub mod rpc_client;

// Re-export key types and functions
pub use disk_discovery::{discover_and_update_local_disks, NvmeDevice};
pub use nvmeof_manager::{manage_nvmeof_exports_intelligently, repair_spdkraiddisk_members_for_local_disk};
pub use http_api::start_api_server;
pub use raid_operations::initialize_blobstore_on_device;
pub use health_monitor::check_device_health;

// Export the main functions for the binary - defined below
// Removed re-export to prevent confusion - use direct imports instead

// Core dependencies - FlintDiskMetadata and models only used in specific modules

/// Disk allocation status tracking
#[derive(Debug, Clone, PartialEq)]
pub enum DiskStatus {
    Free,           // Available for allocation
    Allocated,      // In use by a SpdkRaidDisk
    System,         // System disk, never allocate
    Unavailable,    // Failed or missing
}

/// Disk inventory entry
#[derive(Debug, Clone)]
pub struct DiskInventoryEntry {
    pub serial_number: String,
    pub device_path: String,
    pub capacity_bytes: i64,
    pub model: String,
    pub status: DiskStatus,
    pub allocated_to: Option<String>,  // SpdkRaidDisk name if allocated
    pub last_seen: chrono::DateTime<chrono::Utc>,
}

/// Core NodeAgent struct - coordinates all node-level operations
#[derive(Clone)]
pub struct NodeAgent {
    pub node_name: String,
    pub kube_client: Client,
    pub spdk_rpc_url: String,
    pub discovery_interval: u64,
    pub auto_initialize_blobstore: bool,
    pub backup_path: String,
    pub target_namespace: String,
    pub cluster_id: String,
    // TODO: Add shared disk inventory (Arc<Mutex<HashMap<String, DiskInventoryEntry>>>)
}

impl NodeAgent {
    pub fn new(
        node_name: String,
        kube_client: Client,
        spdk_rpc_url: String,
        discovery_interval: u64,
        auto_initialize_blobstore: bool,
        backup_path: String,
        target_namespace: String,
        cluster_id: String,
    ) -> Self {
        Self {
            node_name,
            kube_client,
            spdk_rpc_url,
            discovery_interval,
            auto_initialize_blobstore,
            backup_path,
            target_namespace,
            cluster_id,
        }
    }

    /// Build disk inventory from discovered devices and SpdkRaidDisk allocations
    pub async fn build_disk_inventory(&self, discovered_devices: &std::collections::HashMap<String, disk_discovery::NvmeDevice>) -> Result<Vec<DiskInventoryEntry>, Box<dyn std::error::Error + Send + Sync>> {
        println!("📊 [INVENTORY] Building disk inventory for node: {}", self.node_name);
        
        // Get existing SpdkRaidDisk allocations
        let allocated_devices = self.get_allocated_device_serials().await?;
        
        let mut inventory = Vec::new();
        
        for device in discovered_devices.values() {
            // Determine disk status
            let status = if self.comprehensive_system_disk_check(&device.pcie_addr, &device.device_path).await {
                DiskStatus::System
            } else if allocated_devices.contains_key(&device.serial_number) {
                DiskStatus::Allocated
            } else {
                DiskStatus::Free
            };
            
            let allocated_to = if status == DiskStatus::Allocated {
                allocated_devices.get(&device.serial_number).cloned()
            } else {
                None
            };
            
            let entry = DiskInventoryEntry {
                serial_number: device.serial_number.clone(),
                device_path: device.device_path.clone(),
                capacity_bytes: device.capacity,
                model: device.model.clone(),
                status,
                allocated_to,
                last_seen: chrono::Utc::now(),
            };
            
            inventory.push(entry);
        }
        
        println!("📊 [INVENTORY] Built inventory with {} disks", inventory.len());
        Ok(inventory)
    }
    
    /// Get allocated device serial numbers from SpdkRaidDisk resources
    async fn get_allocated_device_serials(&self) -> Result<std::collections::HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
        use kube::api::ListParams;
        use crate::models::SpdkRaidDisk;
        
        let raids: Api<SpdkRaidDisk> = Api::namespaced(self.kube_client.clone(), &self.target_namespace);
        let raid_list = raids.list(&ListParams::default()).await?;
        
        let mut allocated = std::collections::HashMap::new();
        
        for raid in raid_list.items {
            if let Some(labels) = &raid.metadata.labels {
                if let Some(deviceid) = labels.get("flint.csi.storage.io/deviceid") {
                    let raid_name = raid.metadata.name.unwrap_or_else(|| "unknown".to_string());
                    allocated.insert(deviceid.clone(), raid_name);
                }
            }
        }
        
        println!("📊 [INVENTORY] Found {} allocated devices", allocated.len());
        Ok(allocated)
    }
    
    /// Get free disks with sufficient capacity
    pub async fn get_free_disks(&self, min_capacity_bytes: i64) -> Result<Vec<DiskInventoryEntry>, Box<dyn std::error::Error + Send + Sync>> {
        let discovered_devices = self.discover_devices_by_persistent_paths().await?;
        let mut unique_devices = std::collections::HashMap::new();
        for device in discovered_devices {
            unique_devices.insert(device.device_path.clone(), device);
        }
        
        let inventory = self.build_disk_inventory(&unique_devices).await?;
        
        let free_disks: Vec<DiskInventoryEntry> = inventory.into_iter()
            .filter(|entry| {
                entry.status == DiskStatus::Free && entry.capacity_bytes >= min_capacity_bytes
            })
            .collect();
        
        println!("💾 [FREE_DISKS] Found {} free disks with capacity >= {}GB", 
                 free_disks.len(), min_capacity_bytes / (1024 * 1024 * 1024));
        
        Ok(free_disks)
    }
    
    /// Mark disk as allocated to a SpdkRaidDisk
    pub async fn mark_disk_allocated(&self, serial_number: &str, raid_disk_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔒 [ALLOCATION] Marking disk {} as allocated to {}", serial_number, raid_disk_name);
        // In a full implementation, this would update a persistent inventory store
        // For now, this is tracked via SpdkRaidDisk labels
        Ok(())
    }
    
    /// Mark disk as free (when SpdkRaidDisk is deleted)
    pub async fn mark_disk_free(&self, serial_number: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔓 [DEALLOCATION] Marking disk {} as free", serial_number);
        // In a full implementation, this would update a persistent inventory store
        Ok(())
    }

    /// Get current node IP address from Kubernetes
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::Node as k8sNode;
        
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        
        let node = nodes_api.get(&self.node_name).await
            .map_err(|e| format!("Failed to get node {}: {}", self.node_name, e))?;
        
        // Get internal IP address
        if let Some(status) = node.status {
            if let Some(addresses) = status.addresses {
                for addr in addresses {
                    if addr.type_ == "InternalIP" {
                        return Ok(addr.address);
                    }
                }
            }
        }
        
        Err("No internal IP found for node".into())
    }
}

/// Get Kubernetes cluster ID for cluster identification
pub async fn get_kubernetes_cluster_id() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Try to get cluster ID from kube-system namespace UID
    let client = Client::try_default().await?;
    let ns_api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client);
    
    match ns_api.get("kube-system").await {
        Ok(ns) => {
            if let Some(uid) = ns.metadata.uid {
                // Use first 8 characters of namespace UID as cluster ID
                return Ok(uid.chars().take(8).collect());
            }
        }
        Err(_) => {
            // Fallback to hostname-based cluster ID
            if let Ok(hostname) = env::var("HOSTNAME") {
                return Ok(hostname.chars().take(8).collect());
            }
        }
    }
    
    Ok("unknown".to_string())
}

/// Get current namespace from service account token
pub async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Try environment variable first (allows override)
    if let Ok(ns) = env::var("NAMESPACE") {
        return Ok(ns);
    }
    
    // Try reading from service account
    if let Ok(namespace) = tokio::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").await {
        return Ok(namespace.trim().to_string());
    }
    
    // Default fallback
    Ok("default".to_string())
}

/// Wait for SPDK to become ready by testing RPC connectivity
pub async fn wait_for_spdk_ready(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let max_retries = 36; // 6 minutes total (36 * 10 seconds)
    let mut last_error = String::new();
    
    println!("⏳ [SPDK_READY] Waiting for SPDK to be ready...");
    
    let spdk_socket = if agent.spdk_rpc_url.starts_with("unix://") {
        &agent.spdk_rpc_url[7..] // Remove "unix://" prefix
    } else {
        "N/A (HTTP endpoint)"
    };
    
    for attempt in 1..=max_retries {
        let test_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "spdk_get_version"
        });
        
        match rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &test_request).await {
            Ok(response) => {
                if response.get("result").is_some() {
                    println!("✅ [SPDK_READY] SPDK is ready! Version check successful.");
                    return Ok(());
                } else if let Some(error) = response.get("error") {
                    last_error = format!("SPDK RPC error: {}", error);
                } else {
                    last_error = "Invalid SPDK RPC response format".to_string();
                }
            }
            Err(e) => {
                last_error = e.to_string();
                if attempt == max_retries {
                    println!("❌ [SPDK_READY] SPDK failed to become ready after {} attempts", max_retries);
                    println!("❌ [SPDK_READY] Final error: {}", last_error);
                    println!("❌ [SPDK_READY] Socket path: {}", spdk_socket);
                    println!("❌ [SPDK_READY] Troubleshooting:");
                    println!("   - Check if SPDK target daemon is running");
                    println!("   - Verify socket file exists and has correct permissions");
                    println!("   - Check SPDK logs for startup errors");
                    println!("   - Ensure proper configuration file is loaded");
                    return Err(format!("SPDK failed to become ready within {} minutes. Last error: {}", max_retries / 6, last_error).into());
                }
                
                // Show progress every 5 attempts (50 seconds)
                if attempt % 5 == 0 {
                    println!("⏳ [SPDK_READY] Still waiting for SPDK (attempt {}/{})... Latest error: {}", attempt, max_retries, last_error);
                } else {
                    println!("⏳ [SPDK_READY] Waiting for SPDK to be ready... (attempt {}/{})", attempt, max_retries);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    
    Ok(())
}

/// Reconcile SPDK state on startup - understand what bdevs and LVS already exist
pub async fn reconcile_spdk_state_on_startup(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [STARTUP_RECONCILE] Starting SPDK state reconciliation on node: {}", agent.node_name);
    
    // Wait for SPDK to be ready first
    if let Err(e) = wait_for_spdk_ready(agent).await {
        println!("⚠️ [STARTUP_RECONCILE] SPDK not ready, skipping reconciliation: {}", e);
        return Ok(()); // Non-fatal - discovery will handle this later
    }
    
    // Step 1: Perform disk discovery and attach disks to SPDK
    println!("🔍 [STARTUP_RECONCILE] Starting disk discovery...");
    if let Err(e) = discover_and_update_local_disks(agent).await {
        println!("⚠️ [STARTUP_RECONCILE] Disk discovery failed: {}", e);
    }
    
    // Step 2: Query current bdevs
    let _bdevs = match agent.get_current_spdk_bdevs().await {
        Ok(bdevs) => {
            println!("📊 [STARTUP_RECONCILE] Found {} existing bdevs in SPDK:", bdevs.len());
            for bdev in &bdevs {
                println!("   📀 [STARTUP_RECONCILE] Existing bdev: {}", bdev);
            }
            bdevs
        }
        Err(e) => {
            println!("⚠️ [STARTUP_RECONCILE] Failed to query existing bdevs: {}", e);
            vec![]
        }
    };
    
    // Step 3: Query current LVS stores and populate configuration
    let mut current_config = serde_json::json!({
        "bdevs": [],
        "lvstores": [],
        "subsystems": []
    });
    
    // Get bdev details for configuration
    if let Ok(response) = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({"method": "bdev_get_bdevs"})).await {
        if let Some(bdev_list) = response["result"].as_array() {
            current_config["bdevs"] = serde_json::json!(bdev_list);
        }
    }
    
    // Get LVS stores  
    if let Ok(response) = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({"method": "bdev_lvol_get_lvstores"})).await {
        if let Some(lvstores) = response["result"].as_array() {
            println!("📊 [STARTUP_RECONCILE] Found {} existing LVS stores:", lvstores.len());
            for lvs in lvstores {
                if let Some(name) = lvs["name"].as_str() {
                    println!("   🗄️ [STARTUP_RECONCILE] Existing LVS: {}", name);
                }
            }
            current_config["lvstores"] = serde_json::json!(lvstores);
        }
    }
    
    // Get NVMf subsystems
    if let Ok(response) = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({"method": "nvmf_get_subsystems"})).await {
        if let Some(subsystems) = response["result"].as_array() {
            println!("📊 [STARTUP_RECONCILE] Found {} NVMf subsystems:", subsystems.len());
            current_config["subsystems"] = serde_json::json!(subsystems);
        }
    }
    
    // Step 4: Check ConfigMap and compare with current configuration
    if let Err(e) = check_and_sync_configmap(agent, &current_config).await {
        println!("⚠️ [STARTUP_RECONCILE] ConfigMap sync failed: {}", e);
    }
    
    println!("✅ [STARTUP_RECONCILE] SPDK state reconciliation completed");
    Ok(())
}

// Removed run_discovery_loop - now using event-driven architecture
// Disk discovery happens:
// 1. Once on startup (reconcile_spdk_state_on_startup)
// 2. On-demand during PVC provisioning (via controller)

/// Check and sync ConfigMap with current SPDK configuration
async fn check_and_sync_configmap(agent: &NodeAgent, current_config: &serde_json::Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::api::{Api, PostParams};
    
    println!("🔄 [CONFIGMAP_SYNC] Checking ConfigMap for SPDK configuration");
    
    let configmaps: Api<ConfigMap> = Api::namespaced(agent.kube_client.clone(), &agent.target_namespace);
    let configmap_name = format!("spdk-tgt-config-{}", agent.node_name);
    
    // Try to get existing ConfigMap
    match configmaps.get(&configmap_name).await {
        Ok(existing_cm) => {
            println!("📊 [CONFIGMAP_SYNC] Found existing ConfigMap: {}", configmap_name);
            
            // Merge configurations - preserve dynamic state from ConfigMap
            if let Some(data) = &existing_cm.data {
                if let Some(stored_config_str) = data.get("spdk-config.json") {
                    match serde_json::from_str::<serde_json::Value>(stored_config_str) {
                        Ok(stored_config) => {
                            // Merge configurations: preserve dynamic state (NVMe-oF, RAID) from ConfigMap
                            // while updating hardware discovery results
                            let merged_config = merge_spdk_configurations(&stored_config, current_config);
                            
                            println!("🔄 [CONFIGMAP_SYNC] Merging configurations:");
                            println!("  - Preserving NVMe-oF subsystems from ConfigMap");
                            println!("  - Preserving RAID configurations from ConfigMap");
                            println!("  - Updating hardware bdevs from discovery");
                            
                            // Apply merged configuration to SPDK if needed
                            if let Err(e) = apply_dynamic_config_to_spdk(agent, &stored_config).await {
                                println!("⚠️ [CONFIGMAP_SYNC] Failed to apply dynamic config: {}", e);
                            }
                            
                            // Save merged configuration to SPDK
                            println!("💾 [CONFIGMAP_SYNC] Saving merged configuration to SPDK");
                            save_spdk_configuration(agent).await?;
                            
                            // Update ConfigMap with merged configuration
                            update_configmap(&configmaps, &configmap_name, &merged_config).await?;
                        }
                        Err(e) => {
                            println!("⚠️ [CONFIGMAP_SYNC] Failed to parse stored config: {}", e);
                            // Update with current config
                            update_configmap(&configmaps, &configmap_name, current_config).await?;
                            save_spdk_configuration(agent).await?;
                        }
                    }
                } else {
                    println!("⚠️ [CONFIGMAP_SYNC] No spdk-config.json in ConfigMap");
                    // Update with current config
                    update_configmap(&configmaps, &configmap_name, current_config).await?;
                    save_spdk_configuration(agent).await?;
                }
            } else {
                println!("⚠️ [CONFIGMAP_SYNC] ConfigMap has no data");
                // Update with current config
                update_configmap(&configmaps, &configmap_name, current_config).await?;
                save_spdk_configuration(agent).await?;
            }
        }
        Err(_) => {
            println!("📝 [CONFIGMAP_SYNC] ConfigMap not found, creating new one");
            
            // Create new ConfigMap with current configuration
            let mut cm = ConfigMap::default();
            cm.metadata.name = Some(configmap_name.clone());
            cm.metadata.namespace = Some(agent.target_namespace.clone());
            
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("app".to_string(), "spdk-tgt".to_string());
            labels.insert("node".to_string(), agent.node_name.clone());
            cm.metadata.labels = Some(labels);
            
            let mut data = std::collections::BTreeMap::new();
            data.insert("spdk-config.json".to_string(), serde_json::to_string_pretty(current_config)?);
            data.insert("last-updated".to_string(), chrono::Utc::now().to_rfc3339());
            cm.data = Some(data);
            
            configmaps.create(&PostParams::default(), &cm).await
                .map_err(|e| format!("Failed to create ConfigMap: {}", e))?;
            
            println!("✅ [CONFIGMAP_SYNC] Created new ConfigMap with current configuration");
            
            // Save configuration to SPDK
            save_spdk_configuration(agent).await?;
        }
    }
    
    Ok(())
}

/// Merge SPDK configurations - preserve dynamic state while updating hardware
fn merge_spdk_configurations(stored_config: &serde_json::Value, current_config: &serde_json::Value) -> serde_json::Value {
    let mut merged = current_config.clone();
    
    // Build a set of current bdev names for validation
    let current_bdev_names: std::collections::HashSet<String> = 
        if let Some(bdevs) = current_config["bdevs"].as_array() {
            bdevs.iter()
                .filter_map(|b| b["name"].as_str().map(String::from))
                .collect()
        } else {
            std::collections::HashSet::new()
        };
    
    // Preserve NVMe-oF subsystems from stored config (these are dynamically created)
    if let Some(stored_subsystems) = stored_config["subsystems"].as_array() {
        // Filter out discovery subsystem and validate backing bdevs still exist
        let app_subsystems: Vec<_> = stored_subsystems.iter()
            .filter(|s| {
                // Skip discovery subsystem
                if let Some(nqn) = s["nqn"].as_str() {
                    if nqn.contains("discovery") {
                        return false;
                    }
                }
                
                // Validate that backing bdevs still exist
                if let Some(namespaces) = s["namespaces"].as_array() {
                    let all_bdevs_exist = namespaces.iter().all(|ns| {
                        if let Some(bdev_name) = ns["bdev_name"].as_str() {
                            if current_bdev_names.contains(bdev_name) {
                                true
                            } else {
                                println!("⚠️ [MERGE_CONFIG] Skipping NVMe-oF subsystem - backing bdev '{}' no longer exists", bdev_name);
                                false
                            }
                        } else {
                            true
                        }
                    });
                    all_bdevs_exist
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        
        if !app_subsystems.is_empty() {
            println!("📦 [MERGE_CONFIG] Preserving {} valid NVMe-oF subsystems from ConfigMap", app_subsystems.len());
            merged["subsystems"] = serde_json::json!(app_subsystems);
        }
    }
    
    // Preserve RAID bdevs from stored config (only if member disks still exist)
    if let Some(stored_bdevs) = stored_config["bdevs"].as_array() {
        if let Some(current_bdevs) = merged["bdevs"].as_array_mut() {
            // Find RAID bdevs in stored config
            for stored_bdev in stored_bdevs {
                if let Some(product_name) = stored_bdev["product_name"].as_str() {
                    if product_name == "Raid Volume" {
                        let raid_name = stored_bdev["name"].as_str().unwrap_or("");
                        
                        // Check if this RAID bdev already exists in current config
                        let exists_in_current = current_bdevs.iter().any(|b| {
                            b["name"].as_str() == Some(raid_name)
                        });
                        
                        if !exists_in_current {
                            // Validate that all member disks still exist
                            let members_valid = if let Some(base_bdevs) = stored_bdev["base_bdevs"].as_array() {
                                base_bdevs.iter().all(|member| {
                                    if let Some(member_name) = member.as_str() {
                                        if current_bdev_names.contains(member_name) {
                                            true
                                        } else {
                                            println!("⚠️ [MERGE_CONFIG] RAID member '{}' no longer exists for RAID '{}'", member_name, raid_name);
                                            false
                                        }
                                    } else {
                                        false
                                    }
                                })
                            } else {
                                false
                            };
                            
                            if members_valid {
                                println!("📦 [MERGE_CONFIG] Preserving RAID bdev: {} (all members present)", raid_name);
                                current_bdevs.push(stored_bdev.clone());
                            } else {
                                println!("⚠️ [MERGE_CONFIG] Skipping RAID bdev: {} (missing members)", raid_name);
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Preserve logical volumes from stored config (only if base bdev still exists)
    if let Some(stored_lvstores) = stored_config["lvstores"].as_array() {
        if stored_lvstores.len() > 0 {
            // Validate and filter LVS stores based on base bdev availability
            let valid_lvstores: Vec<_> = stored_lvstores.iter()
                .filter(|lvs| {
                    if let Some(base_bdev) = lvs["base_bdev"].as_str() {
                        if current_bdev_names.contains(base_bdev) {
                            true
                        } else {
                            if let Some(lvs_name) = lvs["name"].as_str() {
                                println!("⚠️ [MERGE_CONFIG] Skipping LVS '{}' - base bdev '{}' no longer exists", lvs_name, base_bdev);
                            }
                            false
                        }
                    } else {
                        // No base_bdev specified, might be a special case
                        true
                    }
                })
                .cloned()
                .collect();
            
            if !valid_lvstores.is_empty() {
                println!("📦 [MERGE_CONFIG] Preserving {} valid LVS stores from ConfigMap", valid_lvstores.len());
                
                // Merge with current LVS stores (avoid duplicates)
                if let Some(current_lvstores) = current_config["lvstores"].as_array() {
                    let mut merged_lvstores = valid_lvstores;
                    for current_lvs in current_lvstores {
                        let current_name = current_lvs["name"].as_str().unwrap_or("");
                        let exists_in_stored = merged_lvstores.iter().any(|s| {
                            s["name"].as_str() == Some(current_name)
                        });
                        if !exists_in_stored {
                            merged_lvstores.push(current_lvs.clone());
                        }
                    }
                    merged["lvstores"] = serde_json::json!(merged_lvstores);
                } else {
                    merged["lvstores"] = serde_json::json!(valid_lvstores);
                }
            }
        }
    }
    
    merged
}

/// Apply dynamic configuration from ConfigMap to SPDK
async fn apply_dynamic_config_to_spdk(agent: &NodeAgent, stored_config: &serde_json::Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [APPLY_CONFIG] Applying dynamic configuration to SPDK");
    
    // Re-create NVMe-oF subsystems if they don't exist
    if let Some(subsystems) = stored_config["subsystems"].as_array() {
        for subsystem in subsystems {
            if let Some(nqn) = subsystem["nqn"].as_str() {
                // Skip discovery subsystem
                if nqn.contains("discovery") {
                    continue;
                }
                
                // Check if subsystem exists
                let check_response = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                    "method": "nvmf_get_subsystems"
                })).await?;
                
                let exists = if let Some(current_subsystems) = check_response["result"].as_array() {
                    current_subsystems.iter().any(|s| s["nqn"].as_str() == Some(nqn))
                } else {
                    false
                };
                
                if !exists {
                    println!("🔧 [APPLY_CONFIG] Re-creating NVMe-oF subsystem: {}", nqn);
                    
                    // Create subsystem
                    let create_result = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                        "method": "nvmf_create_subsystem",
                        "params": {
                            "nqn": nqn,
                            "allow_any_host": true,
                            "serial_number": subsystem["serial_number"],
                            "model_number": subsystem["model_number"]
                        }
                    })).await;
                    
                    if let Err(e) = create_result {
                        println!("⚠️ [APPLY_CONFIG] Failed to create subsystem {}: {}", nqn, e);
                    }
                    
                    // Re-add namespaces
                    if let Some(namespaces) = subsystem["namespaces"].as_array() {
                        for ns in namespaces {
                            if let (Some(bdev_name), Some(nsid)) = (ns["bdev_name"].as_str(), ns["nsid"].as_u64()) {
                                let ns_result = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                                    "method": "nvmf_subsystem_add_ns",
                                    "params": {
                                        "nqn": nqn,
                                        "namespace": {
                                            "bdev_name": bdev_name,
                                            "nsid": nsid
                                        }
                                    }
                                })).await;
                                
                                if let Err(e) = ns_result {
                                    println!("⚠️ [APPLY_CONFIG] Failed to add namespace to {}: {}", nqn, e);
                                }
                            }
                        }
                    }
                    
                    // Re-add listeners
                    if let Some(listen_addresses) = subsystem["listen_addresses"].as_array() {
                        for addr in listen_addresses {
                            if let (Some(trtype), Some(traddr), Some(trsvcid)) = 
                                (addr["trtype"].as_str(), addr["traddr"].as_str(), addr["trsvcid"].as_str()) {
                                let listener_result = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({
                                    "method": "nvmf_subsystem_add_listener",
                                    "params": {
                                        "nqn": nqn,
                                        "listen_address": {
                                            "trtype": trtype,
                                            "traddr": traddr,
                                            "trsvcid": trsvcid,
                                            "adrfam": addr["adrfam"].as_str().unwrap_or("IPv4")
                                        }
                                    }
                                })).await;
                                
                                if let Err(e) = listener_result {
                                    println!("⚠️ [APPLY_CONFIG] Failed to add listener to {}: {}", nqn, e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Note: RAID bdevs and logical volumes are typically recreated automatically
    // when their underlying bdevs are present, so we don't need to explicitly recreate them
    
    println!("✅ [APPLY_CONFIG] Dynamic configuration applied");
    Ok(())
}

/// Update existing ConfigMap with new configuration
async fn update_configmap(
    configmaps: &Api<k8s_openapi::api::core::v1::ConfigMap>,
    name: &str,
    config: &serde_json::Value
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::{PatchParams, Patch};
    
    println!("📝 [CONFIGMAP_UPDATE] Updating ConfigMap with new configuration");
    
    let patch = serde_json::json!({
        "data": {
            "spdk-config.json": serde_json::to_string_pretty(config)?,
            "last-updated": chrono::Utc::now().to_rfc3339()
        }
    });
    
    configmaps.patch(name, &PatchParams::default(), &Patch::Merge(patch)).await
        .map_err(|e| format!("Failed to update ConfigMap: {}", e))?;
    
    println!("✅ [CONFIGMAP_UPDATE] ConfigMap updated successfully");
    Ok(())
}

/// Save SPDK configuration using save_config RPC
async fn save_spdk_configuration(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("💾 [SAVE_CONFIG] Saving SPDK configuration");
    
    // Call save_config RPC to persist the current SPDK state
    let response = rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "save_config"
    })).await?;
    
    if response.get("error").is_some() {
        return Err(format!("SPDK save_config failed: {:?}", response["error"]).into());
    }
    
    println!("✅ [SAVE_CONFIG] SPDK configuration saved successfully");
    Ok(())
}

/// Perform graceful shutdown of node agent
pub async fn perform_graceful_shutdown(_agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🛑 [SHUTDOWN] Starting graceful shutdown sequence...");
    
    // Graceful shutdown logic - stop discovery loop and cleanup resources
    // - Save final state
    
    println!("✅ [SHUTDOWN] Node agent shutdown complete");
    Ok(())
}
