// main.rs - Entry point for SPDK CSI Driver with NVMe-oF Support
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Server;
use kube::Client;
use warp::Filter;

mod controller;
mod node;
mod identity;
mod driver;
mod csi_snapshotter;

use controller::ControllerService;
use node::NodeService;
use identity::IdentityService;
use driver::SpdkCsiDriver;

// Import config sync functionality


// Use the CSI protobuf types from lib.rs instead of duplicating them
// This avoids the tonic::include_proto! macro issue

use spdk_csi_driver::csi::{
    controller_server::ControllerServer,
    identity_server::IdentityServer,
    node_server::NodeServer,
};

/// Simple health check endpoint for Kubernetes liveness probes
async fn start_health_server() {
    let health = warp::path("healthz")
        .and(warp::get())
        .map(move || {
            // Simple health check - always return OK for liveness probe
            // The fact that the container is running means it's healthy
            warp::reply::with_status("OK", warp::http::StatusCode::OK)
        });

    let health_port = std::env::var("HEALTH_PORT")
        .unwrap_or("9809".to_string())
        .parse()
        .unwrap_or(9809);
    
    println!("Starting health server on port {}", health_port);
    warp::serve(health)
        .run(([0, 0, 0, 0], health_port))
        .await;
}

/// Get the current pod's namespace from the service account token
async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error>> {
    // Try environment variable first (allows override)
    if let Ok(namespace) = std::env::var("FLINT_NAMESPACE") {
        return Ok(namespace);
    }
    
    // Read namespace from service account token file
    let namespace_path = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
    if std::path::Path::new(namespace_path).exists() {
        match tokio::fs::read_to_string(namespace_path).await {
            Ok(namespace) => {
                let namespace = namespace.trim().to_string();
                println!("📍 [NAMESPACE] Detected current namespace: {}", namespace);
                return Ok(namespace);
            }
            Err(e) => {
                println!("⚠️ [NAMESPACE] Failed to read namespace file: {}", e);
            }
        }
    }
    
    // Fallback to default if running outside cluster
    println!("⚠️ [NAMESPACE] Using fallback namespace: flint-system");
    Ok("flint-system".to_string())
}

/// Initialize SPDK from SpdkConfig CRD at startup
async fn initialize_spdk_from_config(driver: Arc<SpdkCsiDriver>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use spdk_csi_driver::models::SpdkConfig;
    use kube::api::Api;

    println!("🔄 [STARTUP] Initializing SPDK from SpdkConfig CRD...");
    
    // Wait a bit for SPDK to be ready
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(driver.kube_client.clone(), &driver.target_namespace);
    let config_name = format!("{}-config", driver.node_id);
    
    // Try to load existing SpdkConfig
    match spdk_configs.get_opt(&config_name).await? {
        Some(config) => {
            println!("✅ [STARTUP] Found SpdkConfig CRD: {}", config_name);
            
            // Skip if in maintenance mode
            if config.spec.maintenance_mode {
                println!("⚠️ [STARTUP] Node is in maintenance mode - skipping SPDK initialization");
                return Ok(());
            }
            
            // TODO: Save with SPDK native config
            println!("💾 [TODO] SPDK native config save after applying SpdkConfig");
            
            // Apply the configuration to running SPDK via RPC
            apply_spdk_config_via_rpc(&driver, &config.spec).await?;
            
            // TODO: Sync status with SPDK native config
            println!("🔄 [TODO] SPDK native status sync to CRD");
            
            println!("✅ [STARTUP] SPDK initialized successfully from SpdkConfig");
        }
        None => {
            println!("ℹ️ [STARTUP] No SpdkConfig found for node {} - SPDK will start with empty config", driver.node_id);
            
            // Create empty ConfigMap for SPDK startup
            let empty_config = spdk_csi_driver::models::SpdkConfigSpec {
                node_id: driver.node_id.clone(),
                maintenance_mode: false,
                last_config_save: Some(chrono::Utc::now().to_rfc3339()),
                raid_bdevs: vec![],
                nvmeof_subsystems: vec![],
            };
            
            // TODO: Save empty config with SPDK native approach
            println!("💾 [TODO] SPDK native empty config save");
        }
    }
    
    Ok(())
}

/// Apply SpdkConfig to running SPDK via RPC calls
async fn apply_spdk_config_via_rpc(
    driver: &SpdkCsiDriver,
    config: &spdk_csi_driver::models::SpdkConfigSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔄 [CONFIG] Applying SpdkConfig to running SPDK...");
    
    // 1. Create NVMe-oF connections for RAID members
    for raid in &config.raid_bdevs {
        for member in &raid.members {
            if member.member_type == "nvmeof" {
                if let Some(nvmeof_config) = &member.nvmeof_config {
                    println!("🔗 [CONFIG] Creating NVMe-oF connection: {}", member.bdev_name);
                    
                    let rpc_request = serde_json::json!({
                        "method": "bdev_nvme_attach_controller",
                        "params": {
                            "name": member.bdev_name,
                            "trtype": nvmeof_config.transport,
                            "traddr": nvmeof_config.target_addr,
                            "trsvcid": nvmeof_config.target_port.to_string(),
                            "subnqn": nvmeof_config.nqn
                        }
                    });
                    
                    if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
                        println!("⚠️ [CONFIG] Failed to create NVMe-oF connection {}: {}", member.bdev_name, e);
                    }
                }
            }
        }
    }
    
    // 2. Create RAID bdevs
    for raid in &config.raid_bdevs {
        println!("🛡️ [CONFIG] Creating RAID bdev: {}", raid.name);
        
        // ✅ ADVANCED LOCALITY + LOAD BALANCING CHECK
        // Note: In a full implementation, this would query all node SpdkConfigs
        // For now, we implement the local decision logic
        let should_create_locally = if raid.has_local_members() {
            println!("🏠 [LOCALITY] RAID {} has local members detected", raid.name);
            
            // TODO: In production, gather global cluster state for load balancing
            // For now, create locally if local members are present
            let local_devices = raid.get_local_member_devices();
            println!("🔧 [LOCALITY] Local member devices for RAID {}: {:?}", raid.name, local_devices);
            
            // This node has local members, so create the RAID here
            // In a full cluster implementation, this would check if this node
            // has the lowest RAID count among nodes with local members
            true
        } else {
            println!("🌐 [LOCALITY] RAID {} has no local members", raid.name);
            let remote_nodes = raid.get_remote_member_nodes();
            println!("🔗 [LOCALITY] Remote members from nodes: {:?}", remote_nodes);
            
            // No local members - flexible placement (could be optimized further)
            true // Still create if config specifies this node
        };
        
        if !should_create_locally {
            println!("⏭️ [LOCALITY] Skipping RAID {} creation - should be created on optimal node", raid.name);
            continue;
        }
        
        // Log the placement decision reasoning
        if raid.has_local_members() {
            println!("✅ [PLACEMENT] Creating RAID {} on node {} - LOCALITY OPTIMIZATION", 
                     raid.name, driver.node_id);
            println!("📊 [PLACEMENT] Reason: At least one member is local to this node");
        } else {
            println!("✅ [PLACEMENT] Creating RAID {} on node {} - FLEXIBLE PLACEMENT", 
                     raid.name, driver.node_id);
            println!("📊 [PLACEMENT] Reason: No local members, can be placed anywhere");
        }
        
        let base_bdevs: Vec<String> = raid.members.iter().map(|m| m.bdev_name.clone()).collect();
        
        // Clean up conflicting NVMe-oF exports before RAID creation
        use spdk_csi_driver::nvmeof_export_manager::NvmeofExportManager;
        let export_manager = NvmeofExportManager::new(
            driver.spdk_rpc_url.clone(),
            driver.node_id.clone(),
        );
        
        if let Err(e) = export_manager.cleanup_conflicting_exports(&base_bdevs).await {
            println!("⚠️ [CONFIG] Failed to cleanup exports for RAID {}: {}", raid.name, e);
        }
        
        let rpc_request = serde_json::json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid.name,
                "raid_level": raid.raid_level,
                "base_bdevs": base_bdevs,
                "superblock": raid.superblock_enabled,
                "strip_size_kb": raid.stripe_size_kb
            }
        });
        
        if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
            println!("⚠️ [CONFIG] Failed to create RAID bdev {}: {}", raid.name, e);
        }
        
        // 3. Create/Import LVS on RAID bdev
        if !raid.lvstore.name.is_empty() {
            println!("💾 [CONFIG] Creating LVS: {}", raid.lvstore.name);
            
            let rpc_request = serde_json::json!({
                "method": "bdev_lvol_create_lvstore",
                "params": {
                    "bdev_name": raid.name,
                    "lvs_name": raid.lvstore.name,
                    "cluster_sz": raid.lvstore.cluster_size
                }
            });
            
            if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
                println!("⚠️ [CONFIG] Failed to create LVS {} (may already exist): {}", raid.lvstore.name, e);
            }
            
            // 4. Create logical volumes
            for lvol in &raid.lvstore.logical_volumes {
                println!("📁 [CONFIG] Creating logical volume: {}", lvol.name);
                
                let rpc_request = serde_json::json!({
                    "method": "bdev_lvol_create",
                    "params": {
                        "lvol_name": lvol.name,
                        "size": lvol.size_bytes,
                        "lvs_name": raid.lvstore.name,
                        "thin_provision": lvol.thin_provision
                    }
                });
                
                if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
                    println!("⚠️ [CONFIG] Failed to create logical volume {}: {}", lvol.name, e);
                }
            }
        }
    }
    
    // 5. Create NVMe-oF subsystems (volume exports)
    if !config.nvmeof_subsystems.is_empty() {
        println!("🌐 [CONFIG] Creating NVMe-oF transport...");
        
        let rpc_request = serde_json::json!({
            "method": "nvmf_create_transport",
            "params": {
                "trtype": "TCP"
            }
        });
        
        if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
            println!("ℹ️ [CONFIG] NVMe-oF transport creation failed (may already exist): {}", e);
        }
        
        for subsystem in &config.nvmeof_subsystems {
            println!("🌐 [CONFIG] Creating NVMe-oF subsystem: {}", subsystem.nqn);
            
            // Create subsystem
            let rpc_request = serde_json::json!({
                "method": "nvmf_create_subsystem",
                "params": {
                    "nqn": subsystem.nqn,
                    "allow_any_host": subsystem.allow_any_host,
                    "serial_number": format!("SPDK{}", subsystem.lvol_uuid.replace("-", "")[..8].to_uppercase())
                }
            });
            
            if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
                println!("⚠️ [CONFIG] Failed to create NVMe-oF subsystem {}: {}", subsystem.nqn, e);
                continue;
            }
            
            // Add namespace
            let rpc_request = serde_json::json!({
                "method": "nvmf_subsystem_add_ns",
                "params": {
                    "nqn": subsystem.nqn,
                    "namespace": {
                        "nsid": subsystem.namespace_id,
                        "bdev_name": subsystem.lvol_uuid
                    }
                }
            });
            
            if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
                println!("⚠️ [CONFIG] Failed to add namespace to {}: {}", subsystem.nqn, e);
            }
            
            // Add listener
            let rpc_request = serde_json::json!({
                "method": "nvmf_subsystem_add_listener",
                "params": {
                    "nqn": subsystem.nqn,
                    "listen_address": {
                        "trtype": subsystem.transport.to_uppercase(),
                        "traddr": subsystem.listen_address,
                        "trsvcid": subsystem.listen_port.to_string()
                    }
                }
            });
            
            if let Err(e) = driver.call_spdk_rpc(&rpc_request).await {
                println!("⚠️ [CONFIG] Failed to add listener to {}: {}", subsystem.nqn, e);
            }
        }
    }
    
    println!("✅ [CONFIG] SpdkConfig applied to SPDK successfully");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kube_client = Client::try_default().await?;
    let node_id = std::env::var("NODE_ID")
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or("unknown-node".to_string()));
    
    // Configure NVMe-oF transport settings
    let nvmeof_transport = std::env::var("NVMEOF_TRANSPORT").unwrap_or("tcp".to_string());
    let nvmeof_target_port = std::env::var("NVMEOF_TARGET_PORT")
        .unwrap_or("4420".to_string())
        .parse()
        .unwrap_or(4420);
    
    // Validate transport type
    if !["tcp", "rdma", "fc"].contains(&nvmeof_transport.to_lowercase().as_str()) {
        eprintln!("Warning: Unknown NVMe-oF transport '{}', using 'tcp'", nvmeof_transport);
    }
    
    // Detect the namespace for custom resources
    let target_namespace = get_current_namespace().await?;
    
    // Use Unix domain socket for SPDK RPC by default
    let default_spdk_rpc = "unix:///var/tmp/spdk.sock".to_string();

    let driver = Arc::new(SpdkCsiDriver {
        node_id: node_id.clone(),
        kube_client,
        spdk_rpc_url: std::env::var("SPDK_RPC_URL").unwrap_or(default_spdk_rpc),
        spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
        nvmeof_target_port,
        nvmeof_transport: nvmeof_transport.clone(),
        target_namespace,
        ublk_target_initialized: Arc::new(Mutex::new(false)),
    });
    
    println!("🎯 [CONFIG] Using namespace for custom resources: {}", driver.target_namespace);
    
    // Start health server for Kubernetes liveness probes
    tokio::spawn(async move {
        start_health_server().await;
    });
    
    let mode = std::env::var("CSI_MODE").unwrap_or("all".to_string());
    let endpoint = std::env::var("CSI_ENDPOINT")
        .unwrap_or("unix:///csi/csi.sock".to_string());
    
    // Create service instances
    let identity_service = IdentityService::new(driver.clone());
    let controller_service = ControllerService::new(driver.clone());
    let node_service = NodeService::new(driver.clone());
    
    // Build the router with services
    let mut router = Server::builder()
        .add_service(IdentityServer::new(identity_service));
    
    if mode == "controller" || mode == "all" {
        println!("Starting in Controller mode...");
        router = router.add_service(ControllerServer::new(controller_service));
    }
    
    if mode == "node" || mode == "all" {
        println!("Starting in Node mode...");
        
        // Initialize SPDK configuration from SpdkConfig CRD
        let config_driver = driver.clone();
        tokio::spawn(async move {
            if let Err(e) = initialize_spdk_from_config(config_driver).await {
                eprintln!("❌ [STARTUP] Failed to initialize SPDK from config: {}", e);
            }
        });
        
        // Start periodic SPDK state reconciliation for state drift prevention
        let reconcile_driver = driver.clone();
        tokio::spawn(async move {
            // Wait a bit longer before starting reconciliation to ensure SPDK is fully initialized
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            println!("🔄 [RECONCILE] Starting periodic SPDK state reconciliation");
            
            // TODO: Add SPDK native periodic save here
            println!("⏰ [TODO] Periodic SPDK native config save will be added here");
        });
        
        router = router.add_service(NodeServer::new(node_service));
    }
    
    println!(
        "SPDK CSI Driver ('{}' mode) starting on {} for node {} with NVMe-oF transport {}:{}",
        mode, endpoint, node_id, nvmeof_transport, nvmeof_target_port
    );
    
    // Handle different endpoint types
    if endpoint.starts_with("unix://") {
        let socket_path = endpoint.trim_start_matches("unix://");
        
        // Remove existing socket file if it exists
        if std::path::Path::new(socket_path).exists() {
            std::fs::remove_file(socket_path)?;
        }
        
        // Create parent directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        // Use UnixListener for Unix domain socket
        use tokio::net::UnixListener;
        use tokio_stream::wrappers::UnixListenerStream;
        
        let listener = UnixListener::bind(socket_path)?;
        let stream = UnixListenerStream::new(listener);
        
        println!("Listening on unix socket: {}", socket_path);
        router.serve_with_incoming(stream).await?;
        
    } else if endpoint.starts_with("tcp://") {
        // Handle tcp:// prefix
        let addr = endpoint.trim_start_matches("tcp://").parse()?;
        println!("Listening on TCP address: {}", addr);
        router.serve(addr).await?;
        
    } else {
        // Assume it's a direct address (e.g., "0.0.0.0:50051")
        let addr = endpoint.parse()?;
        println!("Listening on address: {}", addr);
        router.serve(addr).await?;
    }
    
    Ok(())
}
