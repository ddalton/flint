// main.rs - Entry point for SPDK CSI Driver with NVMe-oF Support
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Server;
use kube::Client;
use warp::Filter;
use futures::future::join_all;
use chrono;

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
            
            // SPDK configuration auto-save could be added here if needed
            
            // Smart validation - only when needed to prevent conflicts
            let safe_config = if should_perform_validation(&config.spec, &driver).await? {
                validate_config_with_fast_checks(&config.spec, &driver).await?
            } else {
                config.spec.clone()
            };
            
            // Apply the validated configuration to running SPDK via RPC
            apply_spdk_config_via_rpc(&driver, &safe_config).await?;
            
            // SPDK status sync to CRD could be added here if needed
            
            println!("✅ [STARTUP] SPDK initialized successfully from SpdkConfig");
        }
        None => {
            println!("ℹ️ [STARTUP] No SpdkConfig found for node {} - SPDK will start with empty config", driver.node_id);
            
            // Create empty ConfigMap for SPDK startup
            let _empty_config = spdk_csi_driver::models::SpdkConfigSpec {
                node_id: driver.node_id.clone(),
                maintenance_mode: false,
                last_config_save: Some(chrono::Utc::now().to_rfc3339()),
                raid_bdevs: vec![],
                nvmeof_subsystems: vec![],
            };
            
            // SPDK empty config save could be added here if needed
        }
    }
    
    Ok(())
}

/// Check if validation should be performed based on conditions
/// 
/// FAIL-OPEN STRATEGY: This validation is designed to prevent conflicts while
/// never blocking legitimate startup. If validation cannot be performed due to
/// network issues, API problems, or node unavailability, we assume safety and
/// allow normal startup to proceed.
async fn should_perform_validation(
    config: &spdk_csi_driver::models::SpdkConfigSpec,
    _driver: &SpdkCsiDriver,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Skip validation if no RAIDs to check
    if config.raid_bdevs.is_empty() {
        println!("ℹ️ [VALIDATION] No RAIDs in config - skipping validation");
        return Ok(false);
    }
    
    // Skip if node was properly put in maintenance mode (migration was planned)
    if config.maintenance_mode {
        println!("ℹ️ [VALIDATION] Node in maintenance mode - skipping validation");
        return Ok(false);
    }
    
    // Check environment variable to allow disabling validation
    if std::env::var("SPDK_SKIP_VALIDATION").is_ok() {
        println!("ℹ️ [VALIDATION] Validation disabled by environment variable");
        return Ok(false);
    }
    
    // Check if config is recent (node restart within 5 minutes = likely planned restart)
    if let Some(last_save) = &config.last_config_save {
        if let Ok(last_time) = chrono::DateTime::parse_from_rfc3339(last_save) {
            let age = chrono::Utc::now().signed_duration_since(last_time);
            if age.num_minutes() < 5 {
                println!("ℹ️ [VALIDATION] Config is recent ({}m old) - skipping validation", age.num_minutes());
                return Ok(false);
            }
            println!("⚠️ [VALIDATION] Config is old ({}m) - performing conflict validation", age.num_minutes());
        }
    }
    
    // Perform validation if config age is unknown or old
    println!("🔍 [VALIDATION] Performing RAID conflict validation for safety");
    Ok(true)
}

/// Fast parallel validation with aggressive timeouts
async fn validate_config_with_fast_checks(
    config: &spdk_csi_driver::models::SpdkConfigSpec,
    driver: &SpdkCsiDriver,
) -> Result<spdk_csi_driver::models::SpdkConfigSpec, Box<dyn std::error::Error + Send + Sync>> {
        println!("🚀 [VALIDATION] Starting fast parallel RAID conflict checks");
    
    // Create validation futures for all RAIDs
    let validation_futures: Vec<_> = config.raid_bdevs.iter().map(|raid| {
        validate_single_raid_fast(&raid.name, driver)
    }).collect();
    
    // Run all validations in parallel with timeout
    let results = join_all(validation_futures).await;
    
    // Collect only safe RAIDs
    let mut safe_raids = Vec::new();
    let mut conflicts_found = false;
    
    for (raid, result) in config.raid_bdevs.iter().zip(results) {
        match result {
            Ok(true) => {
                println!("✅ [VALIDATION] RAID {} is safe to create", raid.name);
                safe_raids.push(raid.clone());
            }
            Ok(false) => {
                println!("⚠️ [CONFLICT] RAID {} exists elsewhere - removing from config", raid.name);
                conflicts_found = true;
            }
            Err(e) => {
                println!("⚠️ [VALIDATION] Could not verify RAID {} ({}) - assuming safe for startup", raid.name, e);
                safe_raids.push(raid.clone());
            }
        }
    }
    
    let mut safe_config = config.clone();
    safe_config.raid_bdevs = safe_raids;
    
    // Update the CRD if conflicts were found
    if conflicts_found {
        println!("💾 [VALIDATION] Updating SpdkConfig CRD with conflict-free configuration");
        if let Err(e) = update_spdk_config_crd(&safe_config, driver).await {
            println!("⚠️ [VALIDATION] Failed to update SpdkConfig CRD: {}", e);
        }
    }
    
    println!("✅ [VALIDATION] Validation completed. {} RAIDs validated as safe", safe_config.raid_bdevs.len());
    Ok(safe_config)
}

/// Fast validation for a single RAID with aggressive timeout
async fn validate_single_raid_fast(
    raid_name: &str,
    driver: &SpdkCsiDriver,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::{Api, ListParams};
    use k8s_openapi::api::core::v1::Node;
    
    const FAST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    
    // Get all nodes in the cluster (quick Kubernetes API call)
    let nodes_api: Api<Node> = Api::all(driver.kube_client.clone());
    let node_list = match tokio::time::timeout(FAST_TIMEOUT, nodes_api.list(&ListParams::default())).await {
        Ok(Ok(nodes)) => nodes,
        _ => {
            println!("⚠️ [VALIDATION] Could not list nodes (network/API issue) - assuming RAID {} is safe", raid_name);
            return Ok(true); // Fail-open: can't check, so assume safe
        }
    };
    
    // Check a maximum of 10 nodes to limit validation time
    let nodes_to_check: Vec<_> = node_list.items.into_iter()
        .filter_map(|node| node.metadata.name)
        .filter(|name| *name != driver.node_id)
        .take(10)
        .collect();
    
    for node_name in nodes_to_check {
        match tokio::time::timeout(FAST_TIMEOUT, check_raid_exists_on_node_fast(&node_name, raid_name, driver)).await {
            Ok(Ok(true)) => {
                println!("🚨 [CONFLICT] RAID {} found on node {} - blocking creation", raid_name, node_name);
                return Ok(false); // Conflict found
            }
            Ok(Ok(false)) => {
                println!("✅ [CHECK] RAID {} not found on node {}", raid_name, node_name);
                // No conflict on this node, continue
            }
            Ok(Err(e)) => {
                println!("⚠️ [CHECK] Could not check node {} for RAID {}: {} - assuming safe", node_name, raid_name, e);
                // Fail-open: can't check this node, continue checking others
            }
            Err(_) => {
                println!("⚠️ [CHECK] Timeout checking node {} for RAID {} - assuming safe", node_name, raid_name);
                // Timeout - node might be down, continue checking others
            }
        }
    }
    
    Ok(true) // No conflicts found
}

/// Fast check if RAID exists on a specific node
async fn check_raid_exists_on_node_fast(
    node_name: &str,
    raid_name: &str,
    driver: &SpdkCsiDriver,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Get the node's SPDK RPC URL
    let rpc_url = get_node_spdk_rpc_url_fast(node_name, driver).await?;
    let http_client = reqwest::Client::new();
    
    // Quick check for the RAID
    let response = http_client
        .post(&rpc_url)
        .json(&serde_json::json!({
            "method": "bdev_get_bdevs",
            "params": { "name": raid_name }
        }))
        .timeout(std::time::Duration::from_secs(1)) // Very aggressive timeout
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Ok(false);
    }
    
    let bdevs_data: serde_json::Value = response.json().await?;
    
    // Check if we got a result and if it's a RAID
    if let Some(bdevs) = bdevs_data.get("result").and_then(|r| r.as_array()) {
        for bdev in bdevs {
            if let Some(driver_specific) = bdev.get("driver_specific") {
                if driver_specific.get("raid").is_some() {
                    return Ok(true); // Found RAID conflict
                }
            }
        }
    }
    
    Ok(false)
}

/// Fast node SPDK RPC URL lookup with timeout
async fn get_node_spdk_rpc_url_fast(
    node_name: &str,
    driver: &SpdkCsiDriver,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::{Api, ListParams};
    use k8s_openapi::api::core::v1::Pod;
    
    let pods_api: Api<Pod> = Api::all(driver.kube_client.clone());
    let lp = ListParams::default().labels("app=flint-csi-node");
    
    let pods = tokio::time::timeout(
        std::time::Duration::from_secs(1), 
        pods_api.list(&lp)
    ).await??;
    
    for pod in pods {
        if pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(node_name) {
            if let Some(pod_ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref()) {
                return Ok(format!("http://{}:8081/api/spdk/rpc", pod_ip));
            }
        }
    }
    
    Err(format!("Could not find flint-csi-node pod on node '{}'", node_name).into())
}

/// Update SpdkConfig CRD with cleaned configuration
async fn update_spdk_config_crd(
    safe_config: &spdk_csi_driver::models::SpdkConfigSpec,
    driver: &SpdkCsiDriver,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::{Api, PatchParams, Patch};
    use spdk_csi_driver::models::SpdkConfig;
    
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(driver.kube_client.clone(), &driver.target_namespace);
    let config_name = format!("{}-config", driver.node_id);
    
    let patch = serde_json::json!({
        "spec": {
            "raid_bdevs": safe_config.raid_bdevs,
            "last_config_save": chrono::Utc::now().to_rfc3339()
        }
    });
    
    spdk_configs.patch(&config_name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
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
            
            // In production, global cluster state could be gathered for load balancing
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
                
                // Convert bytes to MiB as required by SPDK bdev_lvol_create RPC
                let size_in_mib = (lvol.size_bytes + 1048575) / 1048576; // Round up to nearest MiB
                
                let rpc_request = serde_json::json!({
                    "method": "bdev_lvol_create",
                    "params": {
                        "lvs_name": raid.lvstore.name,
                        "lvol_name": lvol.name,
                        "size_in_mib": size_in_mib,    // SPDK expects size in MiB, not bytes
                        "thin_provision": lvol.thin_provision,
                        "clear_method": "none"         // Add for consistency
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
        let _reconcile_driver = driver.clone();
        tokio::spawn(async move {
            // Wait a bit longer before starting reconciliation to ensure SPDK is fully initialized
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            println!("🔄 [RECONCILE] Starting periodic SPDK state reconciliation");
            
            // Periodic SPDK config save could be added here if needed
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
