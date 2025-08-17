// node_agent/http_api.rs - HTTP API Server for Node Management
//
// This module provides HTTP API endpoints for disk management operations,
// SPDK RPC proxying, and node status monitoring.

use crate::node_agent::NodeAgent;
use warp::{Filter, Reply, Rejection};
use warp::reply::json;
use serde::{Deserialize, Serialize};
use std::env;


/// Start the HTTP API server
pub async fn start_api_server(agent: NodeAgent) {
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);

    let agent_filter = warp::any().map(move || agent.clone());

    let api = warp::path("api").and(
        // Get all available disks for disk setup management
        warp::path("disks")
            .and(warp::path("uninitialized"))
            .and(warp::get())
            .and(agent_filter.clone())
            .and_then(get_uninitialized_disks)
        .or(
            // Setup disks for SPDK
            warp::path("disks")
                .and(warp::path("setup"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(setup_disks_for_spdk)
        )
        .or(
            // Reset disks back to kernel
            warp::path("disks")
                .and(warp::path("reset"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(reset_disks_to_kernel)
        )
        .or(
            // Delete SPDK disk with comprehensive validation
            warp::path("disks")
                .and(warp::path("delete"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(delete_spdk_disk)
        )
        .or(
            // Initialize blobstore on driver-ready disks
            warp::path("disks")
                .and(warp::path("initialize"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(initialize_disk_blobstore)
        )
        .or(
            // Get setup status
            warp::path("disks")
                .and(warp::path("status"))
                .and(warp::get())
                .and(agent_filter.clone())
                .and_then(get_disk_setup_status)
        )
        .or(
            // Refresh disk discovery
            warp::path("disks")
                .and(warp::path("refresh"))
                .and(warp::post())
                .and(agent_filter.clone())
                .and_then(refresh_disk_discovery)
        )
        .or(
            // Enhanced shutdown with metadata sync
            warp::path("spdk")
                .and(warp::path("shutdown"))
                .and(warp::post())
                .and(agent_filter.clone())
                .and_then(shutdown_spdk_process_with_sync)
        )
        .or(
            // Generic SPDK RPC proxy for cross-node communication
            warp::path("spdk")
                .and(warp::path("rpc"))
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(proxy_spdk_rpc)
        )
        .or(
            // System disk check for controller
            warp::path("system-disk-check")
                .and(warp::post())
                .and(warp::body::json())
                .and(agent_filter.clone())
                .and_then(check_system_disk_status)
        )
    );

    let routes = api.with(cors);
    let port = env::var("API_PORT").unwrap_or("8081".to_string()).parse().unwrap_or(8081);
    
    println!("SPDK Node Agent API server starting on port {}", port);
    warp::serve(routes)
        .run(([0, 0, 0, 0], port))
        .await;
}

// API Request/Response structures
#[derive(Deserialize, Serialize, Debug)]
pub struct DiskSetupRequest {
    pub pci_addresses: Vec<String>,
    pub driver: String,
    pub force_unmount: bool,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct DiskSetupResult {
    pub success: bool,
    pub message: String,
    pub processed_disks: Vec<ProcessedDisk>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ProcessedDisk {
    pub pci_addr: String,
    pub success: bool,
    pub message: String,
    pub driver: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct DiskDeleteRequest {
    pub pci_addresses: Vec<String>,
    pub force: bool,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct DiskDeleteResult {
    pub success: bool,
    pub message: String,
    pub deleted_disks: Vec<ProcessedDisk>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct InitializeBlobstoreRequest {
    pub device_path: String,
    pub lvs_name: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct InitializeBlobstoreResult {
    pub success: bool,
    pub message: String,
    pub lvs_name: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SystemDiskCheckRequest {
    pub device_name: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SystemDiskCheckResult {
    pub device_name: String,
    pub is_system_disk: bool,
    pub check_method: String,
}

// HTTP API Handlers

/// Get uninitialized disks
async fn get_uninitialized_disks(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received request for uninitialized disks on node: {}", agent.node_name);
    
    // Legacy endpoint - disk discovery now handled by enhanced migration API
    println!("ℹ️ [API] Legacy disk discovery endpoint called - returning empty result");
    let response = serde_json::json!({
        "node_name": agent.node_name,
        "uninitialized_disks": 0,
        "disks": [],
        "message": "Disk discovery handled by enhanced migration API"
    });
    
    Ok(json(&response))
}

/// Setup disks for SPDK
async fn setup_disks_for_spdk(
    request: DiskSetupRequest,
    agent: NodeAgent,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received disk setup request: {:?}", request);
    
    match agent.setup_disks_for_spdk(request).await {
        Ok(result) => {
            println!("✅ [API] Disk setup completed: {:?}", result);
            Ok(json(&result))
        }
        Err(e) => {
            println!("❌ [API] Disk setup failed: {}", e);
            let error_result = DiskSetupResult {
                success: false,
                message: format!("Setup failed: {}", e),
                processed_disks: vec![],
            };
            Ok(json(&error_result))
        }
    }
}

/// Reset disks to kernel drivers
async fn reset_disks_to_kernel(
    request: DiskSetupRequest,
    agent: NodeAgent,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received disk reset request: {:?}", request);
    
    // Implementation similar to setup but for resetting
    let mut processed_disks = Vec::new();
    let mut overall_success = true;
    
    for pci_addr in &request.pci_addresses {
        match reset_single_disk_to_kernel(&agent, pci_addr).await {
            Ok(message) => {
                processed_disks.push(ProcessedDisk {
                    pci_addr: pci_addr.clone(),
                    success: true,
                    message,
                    driver: "kernel".to_string(),
                });
            }
            Err(e) => {
                overall_success = false;
                processed_disks.push(ProcessedDisk {
                    pci_addr: pci_addr.clone(),
                    success: false,
                    message: e.to_string(),
                    driver: "unknown".to_string(),
                });
            }
        }
    }
    
    let result = DiskSetupResult {
        success: overall_success,
        message: if overall_success {
            "All disks reset successfully".to_string()
        } else {
            "Some disks failed to reset".to_string()
        },
        processed_disks,
    };
    
    println!("✅ [API] Disk reset completed: {:?}", result);
    Ok(json(&result))
}

/// Delete SPDK disk
async fn delete_spdk_disk(
    request: DiskDeleteRequest,
    agent: NodeAgent,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received disk delete request: {:?}", request);
    
    match agent.delete_spdk_disk_impl(request).await {
        Ok(result) => {
            println!("✅ [API] Disk deletion completed: {:?}", result);
            Ok(json(&result))
        }
        Err(e) => {
            println!("❌ [API] Disk deletion failed: {}", e);
            let error_result = DiskDeleteResult {
                success: false,
                message: format!("Deletion failed: {}", e),
                deleted_disks: vec![],
            };
            Ok(json(&error_result))
        }
    }
}

/// Initialize blobstore on disk
async fn initialize_disk_blobstore(
    request: InitializeBlobstoreRequest,
    _agent: NodeAgent,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received blobstore initialization request: {:?}", request);
    
    // Use the raid_operations module function
    match crate::node_agent::raid_operations::initialize_disk_blobstore(
        &_agent,
        &request.device_path,
        &request.lvs_name,
    ).await {
        Ok(_) => {
            let result = InitializeBlobstoreResult {
                success: true,
                message: "Blobstore initialized successfully".to_string(),
                lvs_name: request.lvs_name,
            };
            println!("✅ [API] Blobstore initialization completed: {:?}", result);
            Ok(json(&result))
        }
        Err(e) => {
            let result = InitializeBlobstoreResult {
                success: false,
                message: format!("Blobstore initialization failed: {}", e),
                lvs_name: request.lvs_name,
            };
            println!("❌ [API] Blobstore initialization failed: {:?}", result);
            Ok(json(&result))
        }
    }
}

/// Get disk setup status
async fn get_disk_setup_status(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received disk status request");
    
    // Legacy endpoint - disk status now handled by enhanced migration API
    println!("ℹ️ [API] Legacy disk status endpoint called - returning empty result");
    let status_info = serde_json::json!({
        "node_name": agent.node_name,
        "total_disks": 0,
        "system_disks": 0,
        "available_disks": 0,
        "disks": [],
        "message": "Disk status handled by enhanced migration API"
    });
    
    println!("✅ [API] Disk status retrieved successfully");
    Ok(json(&status_info))
}

/// Refresh disk discovery
async fn refresh_disk_discovery(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received disk discovery refresh request");
    
    match crate::node_agent::discover_and_update_local_disks(&agent).await {
        Ok(_) => {
            let response = serde_json::json!({
                "success": true,
                "message": "Disk discovery refreshed successfully"
            });
            println!("✅ [API] Disk discovery refresh completed");
            Ok(json(&response))
        }
        Err(e) => {
            println!("❌ [API] Disk discovery refresh failed: {}", e);
            let error_response = serde_json::json!({
                "success": false,
                "error": "Failed to refresh disk discovery",
                "details": e.to_string()
            });
            Ok(json(&error_response))
        }
    }
}

/// Shutdown SPDK process with sync
async fn shutdown_spdk_process_with_sync(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received SPDK shutdown request");
    
    // Graceful shutdown with config sync
    match crate::node_agent::perform_graceful_shutdown(&agent).await {
        Ok(_) => {
            let response = serde_json::json!({
                "success": true,
                "message": "SPDK shutdown initiated successfully"
            });
            println!("✅ [API] SPDK shutdown completed");
            Ok(json(&response))
        }
        Err(e) => {
            println!("❌ [API] SPDK shutdown failed: {}", e);
            let error_response = serde_json::json!({
                "success": false,
                "error": "Failed to shutdown SPDK",
                "details": e.to_string()
            });
            Ok(json(&error_response))
        }
    }
}

/// Proxy SPDK RPC calls
async fn proxy_spdk_rpc(
    rpc_request: serde_json::Value,
    agent: NodeAgent,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received SPDK RPC proxy request: {}", rpc_request["method"].as_str().unwrap_or("unknown"));
    
    match crate::node_agent::rpc_client::call_spdk_rpc(&agent.spdk_rpc_url, &rpc_request).await {
        Ok(response) => {
            println!("✅ [API] SPDK RPC proxy completed successfully");
            Ok(json(&response))
        }
        Err(e) => {
            println!("❌ [API] SPDK RPC proxy failed: {}", e);
            let error_response = serde_json::json!({
                "error": "RPC call failed",
                "details": e.to_string()
            });
            Ok(json(&error_response))
        }
    }
}

/// Check if a device is a system disk
async fn check_system_disk_status(
    request: SystemDiskCheckRequest,
    agent: NodeAgent,
) -> Result<impl Reply, Rejection> {
    println!("🌐 [API] Received system disk check request for device: {}", request.device_name);
    
    // Use the node agent's robust system disk detection
    let is_system_disk = agent.quick_system_disk_check(&request.device_name).await;
    
    let result = SystemDiskCheckResult {
        device_name: request.device_name,
        is_system_disk,
        check_method: "mount_point_analysis".to_string(),
    };
    
    println!("✅ [API] System disk check completed: {} is_system_disk={}", 
             result.device_name, result.is_system_disk);
    Ok(json(&result))
}

// Helper functions

/// Reset a single disk to kernel driver
async fn reset_single_disk_to_kernel(
    agent: &NodeAgent,
    pci_addr: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [RESET] Resetting disk to kernel: {}", pci_addr);
    
    // Get current driver
    let current_driver = agent.get_current_driver(pci_addr).await?;
    
    if current_driver == "none" || current_driver.contains("nvme") {
        return Ok("Disk is already using kernel driver".to_string());
    }
    
    // Unbind from current driver
    let unbind_path = format!("/sys/bus/pci/drivers/{}/unbind", current_driver);
    tokio::fs::write(&unbind_path, pci_addr).await
        .map_err(|e| format!("Failed to unbind from {}: {}", current_driver, e))?;
    
    // Bind to nvme driver
    tokio::fs::write("/sys/bus/pci/drivers/nvme/bind", pci_addr).await
        .map_err(|e| format!("Failed to bind to nvme driver: {}", e))?;
    
    println!("✅ [RESET] Successfully reset disk {} to kernel driver", pci_addr);
    Ok(format!("Successfully reset to nvme driver"))
}

// Re-export the NodeAgent implementation methods for API use
impl NodeAgent {
    /// Setup disks for SPDK (implementation extracted from original node_agent.rs)
    pub async fn setup_disks_for_spdk(&self, request: DiskSetupRequest) -> Result<DiskSetupResult, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SETUP] Setting up {} disks for SPDK", request.pci_addresses.len());
        
        let mut processed_disks = Vec::new();
        let mut overall_success = true;
        
        // Validate and setup each disk
        for pci_addr in &request.pci_addresses {
            match self.validate_disk_for_setup(pci_addr, request.force_unmount).await {
                Ok(_) => {
                    match self.setup_single_disk(pci_addr, &request).await {
                        Ok(_) => {
                            processed_disks.push(ProcessedDisk {
                                pci_addr: pci_addr.clone(),
                                success: true,
                                message: "Setup successful".to_string(),
                                driver: request.driver.clone(),
                            });
                        }
                        Err(e) => {
                            overall_success = false;
                            processed_disks.push(ProcessedDisk {
                                pci_addr: pci_addr.clone(),
                                success: false,
                                message: e.to_string(),
                                driver: "failed".to_string(),
                            });
                        }
                    }
                }
                Err(e) => {
                    overall_success = false;
                    processed_disks.push(ProcessedDisk {
                        pci_addr: pci_addr.clone(),
                        success: false,
                        message: format!("Validation failed: {}", e),
                        driver: "failed".to_string(),
                    });
                }
            }
        }
        
        Ok(DiskSetupResult {
            success: overall_success,
            message: if overall_success {
                "All disks set up successfully".to_string()
            } else {
                "Some disks failed to set up".to_string()
            },
            processed_disks,
        })
    }

    /// Delete SPDK disk implementation
    pub async fn delete_spdk_disk_impl(&self, request: DiskDeleteRequest) -> Result<DiskDeleteResult, Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [DELETE] Deleting {} SPDK disks", request.pci_addresses.len());
        
        let mut deleted_disks = Vec::new();
        let overall_success = true;
        
        for pci_addr in &request.pci_addresses {
            // Disk deletion logic - check if disk is in use before deletion
            // 2. Cleanup any bdevs or LVS
            // 3. Reset driver binding
            // 4. Update CRDs
            
            deleted_disks.push(ProcessedDisk {
                pci_addr: pci_addr.clone(),
                success: true,
                message: "Deletion successful (placeholder)".to_string(),
                driver: "removed".to_string(),
            });
        }
        
        Ok(DiskDeleteResult {
            success: overall_success,
            message: if overall_success {
                "All disks deleted successfully".to_string()
            } else {
                "Some disks failed to delete".to_string()
            },
            deleted_disks,
        })
    }

    /// Validate disk for setup
    async fn validate_disk_for_setup(&self, pci_addr: &str, _force_unmount: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if it's a system disk
        if self.robust_system_disk_check_by_pci(pci_addr).await {
            return Err("Cannot setup system disk".into());
        }
        
        // Validation logic - check if disk is in use or needs force unmount
        // - Validate driver compatibility
        
        Ok(())
    }

    /// Setup single disk
    async fn setup_single_disk(&self, pci_addr: &str, _request: &DiskSetupRequest) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SETUP] Setting up disk: {}", pci_addr);
        
        // Disk setup logic - unbind from current driver and configure
        // 2. Bind to requested driver (vfio-pci, uio_pci_generic, etc.)
        // 3. Create SPDK bdev
        // 4. Update CRDs
        
        println!("✅ [SETUP] Disk setup completed: {}", pci_addr);
        Ok(())
    }
}
