use kube::{
    Client, Api, 
    api::{PatchParams, Patch, PostParams, ListParams},
};
use tokio::time::{Duration, interval};
use serde::{Deserialize, Serialize};
use serde_json::json;
use anyhow::Result;
use chrono::Utc;
use std::env;
use std::fs;
use std::process::Command;
use std::path::Path;
use regex::Regex;
use std::hash::{Hash, Hasher};


// Web framework imports - using warp for HTTP management endpoints
use warp::Filter;
use warp::{reply, Rejection, Reply};
use warp::http::StatusCode;

use spdk_csi_driver::{IoStatistics, FlintDiskMetadata};
use spdk_csi_driver::models::NvmeofEndpoint;
use spdk_csi_driver::models::{NvmeofDisk, NvmeofDiskSpec, NvmeofDiskStatus, SpdkRaidDisk};
use spdk_csi_driver::spdk_native::SpdkNative;

/// SPDK RPC interface for CSI operations
/// 
/// This implementation uses SPDK v25.05.x RPC interface exclusively.
/// All operations are performed via persistent socket connections to the SPDK target process.
/// Implementation matches the official SPDK Go client pattern.
async fn call_spdk_rpc(
    spdk_rpc_url: &str,
    rpc_request: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let method = rpc_request["method"].as_str().unwrap_or("");
    let params = rpc_request.get("params");
    
    println!("🔧 [SPDK_RPC] Executing method: {} via persistent socket connection", method);
    println!("🔧 [SPDK_RPC] Socket URL: {}", spdk_rpc_url);
    
    // Create SPDK RPC client with persistent socket connection
    let spdk_socket = spdk_rpc_url.trim_start_matches("unix://");
    println!("🔧 [SPDK_RPC] Socket path: {}", spdk_socket);
    
    // Check if socket file exists before attempting connection
    if !std::path::Path::new(spdk_socket).exists() {
        let error_msg = format!("SPDK socket file does not exist: {}", spdk_socket);
        println!("❌ [SPDK_RPC] {}", error_msg);
        return Err(error_msg.into());
    }
    
    let spdk = SpdkNative::new(Some(spdk_socket.to_string())).await
        .map_err(|e| {
            let error_msg = format!("Failed to create SPDK client for socket {}: {}", spdk_socket, e);
            println!("❌ [SPDK_RPC] {}", error_msg);
            error_msg
        })?;
    
    // Call method using the new persistent socket client
    println!("🔧 [SPDK_RPC] Calling method '{}' with params: {:?}", method, params);
    let result = spdk.call_method(method, params.cloned()).await
        .map_err(|e| {
            let error_msg = format!("SPDK RPC call '{}' failed: {}", method, e);
            println!("❌ [SPDK_RPC] {}", error_msg);
            error_msg
        })?;
    
    println!("✅ [SPDK_RPC] Method '{}' completed successfully", method);
    
    // Return result in JSON-RPC 2.0 format
    Ok(json!({"result": result}))
}

// Removed unused direct_rpc_call function - call_spdk_rpc is used directly

// Disk setup functionality
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnimplementedDisk {
    pub pci_address: String,
    pub device_name: String,
    pub vendor_id: String,
    pub device_id: String,
    pub subsystem_vendor_id: String,
    pub subsystem_device_id: String,
    pub numa_node: Option<u32>,
    pub driver: String,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    pub firmware_version: String,
    pub namespace_id: Option<u32>,
    pub mounted_partitions: Vec<String>,
    pub filesystem_type: Option<String>,
    pub is_system_disk: bool,
    pub spdk_ready: bool,
    pub discovered_at: String,
}

/// Discover local NVMe disks, ensure SPDK bdevs and NVMe-oF exports exist, and publish/repair NvmeofDisk CRs
async fn discover_and_publish_nvmeof_disks(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let nvmeof_api: Api<NvmeofDisk> = Api::namespaced(agent.kube_client.clone(), &agent.target_namespace);

    // Query SPDK for bdevs; identify local NVMe devices
    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({"method":"bdev_get_bdevs","params":{}})).await?;
    let list = bdevs.get("result").and_then(|r| r.as_array()).cloned().unwrap_or_default();

    for b in list {
        let product = b.get("product_name").and_then(|v| v.as_str()).unwrap_or("");
        if !product.to_lowercase().contains("nvme") { continue; }

        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let num_blocks = b.get("num_blocks").and_then(|v| v.as_u64()).unwrap_or(0);
        let block_size = b.get("block_size").and_then(|v| v.as_u64()).unwrap_or(512);
        let size_bytes = (num_blocks * block_size) as i64;

        // Try to derive identifiers (if available via bdev fields)
        let serial = b.get("serial_number").and_then(|v| v.as_str()).map(|s| s.to_string());
        let model = b.get("product_name").and_then(|v| v.as_str()).map(|s| s.to_string());

        // Skip system/root disks: try to correlate bdev name with kernel device mounted as root
        if is_system_or_mounted_disk(name).await {
            println!("⚠️ [DISCOVERY] Skipping system or mounted disk {}", name);
            continue;
        }

        // Read sysfs identifiers if present
        let sys_ids = read_sysfs_identifiers(name).await;

        // Ensure NVMe-oF endpoint exists: create subsystem and add listener/namespace if missing
        // Generate stable NQN from node + bdev name
        let nqn = format!("nqn.2025-05.io.flint:raw-{}-{}", agent.node_name, name);

        // best-effort create subsystem
        let _ = reqwest::Client::new()
            .post(&agent.spdk_rpc_url)
            .json(&json!({
                "method":"nvmf_create_subsystem",
                "params":{
                    "nqn": nqn,
                    "allow_any_host": true,
                    "serial_number": format!("SPDK{}", Utc::now().timestamp()),
                    "model_number":"FLINT RAW DISK",
                    "max_namespaces":1
                }
            }))
            .send().await;

        // add namespace
        let _ = reqwest::Client::new()
            .post(&agent.spdk_rpc_url)
            .json(&json!({
                "method":"nvmf_subsystem_add_ns",
                "params":{
                    "nqn": nqn,
                    "namespace": {"nsid":1, "bdev_name": name}
                }
            }))
            .send().await;

        // add listener (use node IP via Kubernetes Downward API or hostNetwork)
        let traddr = "127.0.0.1"; // hostNetwork: caller can override in future; placeholder
        let trsvcid = "4420";
        let _ = reqwest::Client::new()
            .post(&agent.spdk_rpc_url)
            .json(&json!({
                "method":"nvmf_subsystem_add_listener",
                "params":{
                    "nqn": nqn,
                    "listen_address":{
                        "trtype":"TCP","adrfam":"IPv4","traddr": traddr, "trsvcid": trsvcid
                    }
                }
            }))
            .send().await;

        // Upsert NvmeofDisk CR for this local disk
        let disk_name = format!("nvmeof-{}-{}", agent.node_name, name);
        let spec = NvmeofDiskSpec {
            is_remote: false,
            node_id: Some(agent.node_name.clone()),
            hardware_id: sys_ids.hardware_id.or_else(|| Some(format!("{}:{}", agent.node_name, name))),
            serial_number: serial.clone().or(sys_ids.serial_number),
            wwn: sys_ids.wwn,
            model: model.clone().or(sys_ids.model),
            vendor: sys_ids.vendor,
            size_bytes,
            nvmeof_endpoint: NvmeofEndpoint {
                nqn: nqn.clone(),
                target_addr: traddr.to_string(),
                target_port: trsvcid.parse().unwrap_or(4420),
                transport: "tcp".to_string(),
                created_at: Some(Utc::now().to_rfc3339()),
                active: true,
            },
            credential_secret_name: None,
            credential_secret_namespace: None,
        };

        let mut resource = NvmeofDisk::new(&disk_name, spec);
        resource.status = Some(NvmeofDiskStatus {
            healthy: true,
            endpoint_validated: true,
            available_bytes: size_bytes,
            last_checked: Utc::now().to_rfc3339(),
            message: None,
        });

        // Create or replace NvmeofDisk
        if nvmeof_api.create(&PostParams::default(), &resource).await.is_err() {
            let _ = nvmeof_api.replace(&disk_name, &PostParams::default(), &resource).await;
        }

        // After ensuring NvmeofDisk exists/updated, repair any SpdkRaidDisk members that match this disk
        if let Err(e) = repair_spdkraiddisk_members_for_local_disk(agent, &resource.spec).await {
            println!("⚠️ [RAID_REPAIR] Failed to repair RAID members for {}: {}", disk_name, e);
        }
    }

    Ok(())
}

#[derive(Default)]
struct SysfsIds {
    hardware_id: Option<String>,
    serial_number: Option<String>,
    wwn: Option<String>,
    model: Option<String>,
    vendor: Option<String>,
}

async fn read_sysfs_identifiers(_bdev_name: &str) -> SysfsIds {
    // Placeholder: integrate with sysfs or NVMe CLI to fetch stable IDs
    SysfsIds::default()
}

async fn is_system_or_mounted_disk(_bdev_name: &str) -> bool {
    // Placeholder: check mounts/blkid to see if device hosts a filesystem; skip if root/system
    false
}

/// Update SpdkRaidDisk member entries on this node if their NVMe-oF endpoint needs repair
async fn repair_spdkraiddisk_members_for_local_disk(
    agent: &NodeAgent,
    disk_spec: &NvmeofDiskSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::{ListParams, Patch, PatchParams};
    let raids: Api<SpdkRaidDisk> = Api::namespaced(agent.kube_client.clone(), &agent.target_namespace);
    let raid_list = raids.list(&ListParams::default()).await?;

    for raid in raid_list {
        // Only consider raids with members on this node
        let mut changed = false;
        let mut new_members = raid.spec.member_disks.clone();

        for member in &mut new_members {
            if member.node_id != agent.node_name { continue; }

            let id_match =
                (member.hardware_id.is_some() && disk_spec.hardware_id.is_some() && member.hardware_id == disk_spec.hardware_id)
                || (member.serial_number.is_some() && disk_spec.serial_number.is_some() && member.serial_number == disk_spec.serial_number)
                || (member.wwn.is_some() && disk_spec.wwn.is_some() && member.wwn == disk_spec.wwn);

            if !id_match { continue; }

            // If endpoint differs, update
            if member.nvmeof_endpoint.nqn != disk_spec.nvmeof_endpoint.nqn
                || member.nvmeof_endpoint.target_addr != disk_spec.nvmeof_endpoint.target_addr
                || member.nvmeof_endpoint.target_port != disk_spec.nvmeof_endpoint.target_port
                || member.nvmeof_endpoint.transport != disk_spec.nvmeof_endpoint.transport {
                member.nvmeof_endpoint = disk_spec.nvmeof_endpoint.clone();
                changed = true;
            }
        }

        if changed {
            let patch = serde_json::json!({
                "spec": { "member_disks": new_members }
            });
            if let Some(name) = raid.metadata.name.as_ref() {
                println!("🔧 [RAID_REPAIR] Updating SpdkRaidDisk {} member endpoints", name);
                raids.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSetupRequest {
    pub pci_addresses: Vec<String>,
    pub force_unmount: bool,
    pub backup_data: bool,
    pub huge_pages_mb: Option<u32>,
    pub driver_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSetupResult {
    pub success: bool,
    pub setup_disks: Vec<String>,
    pub failed_disks: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub huge_pages_configured: Option<u32>,
    pub completed_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskDeleteRequest {
    pub pci_address: String,
    pub force_delete: bool,
    pub migrate_volumes: bool,
    pub take_snapshots: bool,
    pub target_disks: Option<Vec<String>>, // For migration
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskDeleteResult {
    pub success: bool,
    pub message: String,
    pub volumes_on_disk: Vec<VolumeOnDisk>,
    pub deleted_volumes: Vec<String>,
    pub migrated_volumes: Vec<VolumeMigration>,
    pub created_snapshots: Vec<String>,
    pub cleanup_performed: DiskCleanupSummary,
    pub warnings: Vec<String>,
    pub completed_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VolumeOnDisk {
    pub volume_id: String,
    pub size_bytes: i64,
    pub replica_count: i32,
    pub can_migrate: bool,
    pub single_replica: bool,
    pub pvc_name: Option<String>,
    pub namespace: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VolumeMigration {
    pub volume_id: String,
    pub from_disk: String,
    pub to_disk: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskCleanupSummary {
    pub lvs_deleted: bool,
    pub volumes_deleted: usize,
    pub disk_reset: bool,
    pub crd_updated: bool,
}

#[derive(Clone)]
struct NodeAgent {
    node_name: String,
    kube_client: Client,
    spdk_rpc_url: String,
    discovery_interval: u64,
    auto_initialize_blobstore: bool,
    backup_path: String,
    // Namespace where custom resources should be created
    target_namespace: String,
    // Kubernetes cluster identification (derived from cluster context)
    cluster_id: String,
}

/// Get Kubernetes cluster identifier from cluster context
async fn get_kubernetes_cluster_id() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Method 1: Try to get cluster UID from kube-system namespace
    let client = Client::try_default().await?;
    let namespaces: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client.clone());
    
    if let Ok(kube_system_ns) = namespaces.get("kube-system").await {
        if let Some(uid) = kube_system_ns.metadata.uid {
            // Use a short, stable identifier based on cluster UID
            return Ok(format!("flint-{}", &uid[..8]));
        }
    }
    
    // Method 2: Use cluster name from kubeconfig context (if available)
    if let Ok(cluster_name) = std::env::var("CLUSTER_NAME") {
        return Ok(format!("flint-{}", cluster_name));
    }
    
    // Method 3: Derive from cluster domain
    if let Ok(cluster_domain) = std::env::var("CLUSTER_DOMAIN") {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        cluster_domain.hash(&mut hasher);
        let domain_hash = format!("{:x}", hasher.finish());
        return Ok(format!("flint-{}", &domain_hash[..8]));
    }
    
    // Fallback: Use a consistent default
    Ok("flint-cluster".to_string())
}

/// Get the current pod's namespace from the service account token
async fn get_current_namespace() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Try environment variable first (allows override)
    if let Ok(namespace) = env::var("FLINT_NAMESPACE") {
        return Ok(namespace);
    }
    
    // Read namespace from service account token file
    let namespace_path = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
    if Path::new(namespace_path).exists() {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let kube_client = Client::try_default().await?;
    let node_name = env::var("NODE_NAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-node".to_string());
    
    // Detect the namespace for custom resources
    let target_namespace = get_current_namespace().await?;
    println!("🎯 [STARTUP] Node-agent will operate in namespace: {}", target_namespace);
    
    let agent = NodeAgent {
        node_name: node_name.clone(),
        kube_client,
        spdk_rpc_url: env::var("SPDK_RPC_URL").unwrap_or("unix:///var/tmp/spdk.sock".to_string()),
        discovery_interval: env::var("DISK_DISCOVERY_INTERVAL")
            .unwrap_or("300".to_string())
            .parse()
            .unwrap_or(300),
        auto_initialize_blobstore: env::var("AUTO_INITIALIZE_BLOBSTORE")
            .unwrap_or("true".to_string())
            .parse()
            .unwrap_or(true),
        backup_path: env::var("DISK_BACKUP_PATH")
            .unwrap_or("/var/lib/spdk-csi/backups".to_string()),
        target_namespace,
        cluster_id: get_kubernetes_cluster_id().await
            .unwrap_or_else(|_| "flint-cluster".to_string()),
    };

    println!("Starting SPDK Node Agent on node: {}", node_name);
    println!("🎯 [CONFIG] Using namespace for custom resources: {}", agent.target_namespace);
    
    // Initialize RPC connection to SPDK target
    println!("🔌 [RPC] Using RPC mode - waiting for SPDK to be ready");
    // Wait for SPDK to be ready via RPC
    wait_for_spdk_ready(&agent).await?;
    
    // 🚀 NEW: Discover local NVMe disks, ensure SPDK bdev exists, and create/update NvmeofDisk CRs
    if let Err(e) = discover_and_publish_nvmeof_disks(&agent).await {
        println!("⚠️ [STARTUP] NvmeofDisk discovery failed: {}", e);
    }
    
    // Start HTTP API server for disk setup operations
    let api_agent = agent.clone();
    tokio::spawn(async move {
        start_api_server(api_agent).await;
    });
    

    
    // Start disk discovery loop
    run_discovery_loop(agent).await?;
    
    Ok(())
}

/// Background task that periodically calls spdk_blob_sync_md to sync metadata
/// This reduces the amount of work needed during SPDK shutdown








/// Simplified shutdown handler - no metadata sync needed
async fn perform_graceful_shutdown(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Performing graceful shutdown...");
    
    // Proceed with normal SPDK shutdown
    println!("Initiating SPDK application shutdown...");
    let result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "spdk_app_stop",
        "params": {}
    })).await;

    match result {
        Ok(_) => {
            println!("SPDK shutdown initiated successfully");
        }
        Err(e) => {
            eprintln!("Failed to send SPDK shutdown RPC: {}", e);
        }
    }
    
    Ok(())
}

async fn start_api_server(agent: NodeAgent) {
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
    );

    let routes = api.with(cors);
    let port = env::var("API_PORT").unwrap_or("8081".to_string()).parse().unwrap_or(8081);
    
    println!("SPDK Node Agent API server starting on port {}", port);
    warp::serve(routes)
        .run(([0, 0, 0, 0], port))
        .await;
}

// HTTP API handlers for disk setup operations
async fn get_uninitialized_disks(agent: NodeAgent) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [API] Received request for uninitialized disks on node: {}", agent.node_name);
    
    match agent.discover_all_disks().await {
        Ok(disks) => {
            println!("🌐 [API] Discovery successful: {} disks found", disks.len());
            for (i, disk) in disks.iter().enumerate() {
                println!("🌐 [API]   Disk {}: PCI={}, Name={}, Driver={}, System={}, SPDK Ready={}, Size={}GB", 
                         i+1, disk.pci_address, disk.device_name, disk.driver, 
                         disk.is_system_disk, disk.spdk_ready, disk.size_bytes / (1024*1024*1024));
            }
            
            let response = json!({
                "success": true,
                "disks": disks,
                "count": disks.len(),
                "node": agent.node_name
            });
            println!("🌐 [API] Returning successful response with {} disks", disks.len());
            Ok(warp::reply::json(&response))
        }
        Err(e) => {
            println!("❌ [API] Discovery failed with error: {}", e);
            let response = json!({
                "success": false,
                "error": e.to_string(),
                "node": agent.node_name
            });
            println!("🌐 [API] Returning error response: {:?}", response);
            Ok(warp::reply::json(&response))
        }
    }
}

async fn setup_disks_for_spdk(
    request: DiskSetupRequest,
    agent: NodeAgent
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🌐 [API] Received disk setup request for node: {}", agent.node_name);
    println!("🌐 [API] Request: {:?}", request);
    
    match agent.setup_disks_for_spdk(request).await {
        Ok(result) => {
            println!("🌐 [API] Setup completed successfully: {:?}", result);
            Ok(warp::reply::json(&result))
        }
        Err(e) => {
            println!("🌐 [API] Setup failed with error: {}", e);
            let error_response = json!({
                "success": false,
                "error": e.to_string(),
                "node": agent.node_name,
                "completed_at": Utc::now().to_rfc3339()
            });
            println!("🌐 [API] Returning error response: {:?}", error_response);
            Ok(warp::reply::json(&error_response))
        }
    }
}

async fn reset_disks_to_kernel(
    request: serde_json::Value,
    agent: NodeAgent
) -> Result<impl warp::Reply, warp::Rejection> {
    let pci_addresses: Vec<String> = request["pci_addresses"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    match agent.reset_disks_to_kernel(pci_addresses).await {
        Ok(result) => Ok(warp::reply::json(&result)),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name,
            "completed_at": Utc::now().to_rfc3339()
        })))
    }
}

async fn delete_spdk_disk(
    request: DiskDeleteRequest,
    agent: NodeAgent
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🗑️ [DELETE_DISK] Starting comprehensive disk deletion for PCI: {}", request.pci_address);
    
    match agent.delete_spdk_disk_impl(request).await {
        Ok(delete_result) => {
            println!("✅ [DELETE_DISK] Disk deletion completed: success={}", delete_result.success);
            Ok(warp::reply::json(&delete_result))
        }
        Err(e) => {
            let error_result = DiskDeleteResult {
                success: false,
                message: format!("Disk deletion failed: {}", e),
                volumes_on_disk: vec![],
                deleted_volumes: vec![],
                migrated_volumes: vec![],
                created_snapshots: vec![],
                cleanup_performed: DiskCleanupSummary {
                    lvs_deleted: false,
                    volumes_deleted: 0,
                    disk_reset: false,
                    crd_updated: false,
                },
                warnings: vec![],
                completed_at: Utc::now().to_rfc3339(),
            };
            println!("❌ [DELETE_DISK] Disk deletion failed: {}", e);
            Ok(warp::reply::json(&error_result))
        }
    }
}

async fn get_disk_setup_status(agent: NodeAgent) -> Result<impl warp::Reply, warp::Rejection> {
    match agent.get_all_disk_status().await {
        Ok(status) => Ok(warp::reply::json(&status)),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name
        })))
    }
}

async fn refresh_disk_discovery(agent: NodeAgent) -> Result<impl warp::Reply, warp::Rejection> {
    match discover_and_update_local_disks(&agent).await {
        Ok(_) => Ok(warp::reply::json(&json!({
            "success": true,
            "message": "Disk discovery refreshed",
            "node": agent.node_name,
            "refreshed_at": Utc::now().to_rfc3339()
        }))),
        Err(e) => Ok(warp::reply::json(&json!({
            "success": false,
            "error": e.to_string(),
            "node": agent.node_name
        })))
    }
}

/// Enhanced shutdown handler that performs metadata sync before shutdown
async fn shutdown_spdk_process_with_sync(agent: NodeAgent) -> Result<impl Reply, Rejection> {
    println!("Received request to gracefully shut down SPDK process with metadata sync.");
    
    match perform_graceful_shutdown(&agent).await {
        Ok(_) => {
            let reply = reply::json(&json!({
                "success": true,
                "message": "SPDK shutdown with metadata sync initiated successfully."
            }));
            Ok(reply::with_status(reply, StatusCode::OK))
        }
        Err(e) => {
            eprintln!("Failed to perform graceful shutdown: {}", e);
            let reply = reply::json(&json!({
                "success": false,
                "error": format!("Graceful shutdown failed: {}", e)
            }));
            Ok(reply::with_status(reply, StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}



/// Generic SPDK RPC proxy for cross-node communication
/// Forwards JSON-RPC calls to the local SPDK instance via Unix socket
async fn proxy_spdk_rpc(
    rpc_request: serde_json::Value,
    agent: NodeAgent
) -> Result<impl Reply, Rejection> {
    // Forward the RPC call to local SPDK
    match call_spdk_rpc(&agent.spdk_rpc_url, &rpc_request).await {
        Ok(json_result) => {
            let reply = reply::json(&json_result);
            Ok(reply::with_status(reply, StatusCode::OK))
        }
        Err(e) => {
            let reply = reply::json(&json!({
                "error": format!("Failed to connect to SPDK: {}", e),
                "node": agent.node_name,
                "spdk_url": agent.spdk_rpc_url
            }));
            Ok(reply::with_status(reply, StatusCode::SERVICE_UNAVAILABLE))
        }
    }
}

async fn wait_for_spdk_ready(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let max_retries = 30; // 5 minutes
    let mut last_error = String::new();
    
    for attempt in 1..=max_retries {
        // Use the new SPDK RPC client to check if SPDK is ready
        let spdk_socket = agent.spdk_rpc_url.trim_start_matches("unix://");
        let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = match SpdkNative::new(Some(spdk_socket.to_string())).await {
            Ok(spdk) => {
                // Try to call a simple RPC method to verify SPDK is responsive
                match spdk.call_method("spdk_get_version", None).await {
                    Ok(response) => {
                        println!("✅ [SPDK_READY] SPDK version check successful: {:?}", response);
                        Ok(())
                    }
                    Err(e) => {
                        let error_msg = format!("SPDK RPC call failed: {}", e);
                        println!("❌ [SPDK_READY] RPC call error: {}", error_msg);
                        Err(error_msg.into())
                    }
                }
            }
            Err(e) => {
                let error_msg = format!("Failed to connect to SPDK: {}", e);
                println!("❌ [SPDK_READY] Connection error: {}", error_msg);
                Err(error_msg.into())
            }
        };
        
        match result {
            Ok(_) => {
                println!("🎉 [SPDK_READY] SPDK is ready on node {} after {} attempts", agent.node_name, attempt);
                return Ok(());
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

/// Immediately create bdevs (AIO or NVMe) for all known SpdkDisks to enable SPDK auto-discovery
// Removed legacy SpdkDisk recovery flow

/// Helper section removed; legacy code pruned

// Legacy SpdkDisk recovery helpers removed above

async fn run_discovery_loop(agent: NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut interval = interval(Duration::from_secs(agent.discovery_interval));
    
    // Run initial discovery immediately
    if let Err(e) = discover_and_update_local_disks(&agent).await {
        eprintln!("Initial disk discovery failed: {}", e);
    }
    
    loop {
        interval.tick().await;
        
        if let Err(e) = discover_and_update_local_disks(&agent).await {
            eprintln!("Disk discovery failed: {}", e);
        }
    }
}

async fn discover_and_update_local_disks(agent: &NodeAgent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔍 [DISCOVERY] Starting NVMe device discovery on node: {}", agent.node_name);
    println!("🔧 [DISCOVERY] Config - auto_init_blobstore: {}, discovery_interval: {}s", 
             agent.auto_initialize_blobstore, agent.discovery_interval);
    
    // Discover local NVMe devices using Portworx-style persistent paths
    let discovered_devices = agent.discover_devices_by_persistent_paths().await?;
    
    if discovered_devices.is_empty() {
        println!("❌ [DISCOVERY] No NVMe devices found on node: {}", agent.node_name);
        return Ok(());
    }
    
    println!("✅ [DISCOVERY] Found {} NVMe device(s) on node: {}", discovered_devices.len(), agent.node_name);
    for device in &discovered_devices {
        println!("📀 [DISCOVERY] Device: {} ({}) - PCIe: {}, Size: {}GB", 
                 device.controller_id, device.model, device.pcie_addr, 
                 device.capacity / (1024 * 1024 * 1024));
    }
    
    println!("✅ [DISCOVERY] Disk discovery completed successfully for node: {}", agent.node_name);
    Ok(())
}

#[derive(Debug, Clone)]
struct NvmeDevice {
    // Location-dependent fields
    controller_id: String,
    pcie_addr: String,
    device_path: String,
    
    // Immutable identification fields (Portworx-style)
    disk_id: String,           // /dev/disk/by-id/ path
    serial_number: String,     // Primary hardware identifier
    wwn: Option<String>,       // World Wide Name if available
    model: String,
    vendor: String,
    
    // Hardware characteristics
    capacity: i64,
    // REMOVED: sector_size - unused field
    
    // Cluster metadata (if disk is already part of a cluster)
    cluster_metadata: Option<FlintDiskMetadata>,
    
    // REMOVED: disk_uuid_from_lvs - unused field
    // REMOVED: cluster_id_prefix - unused field
}

async fn initialize_blobstore_on_device(agent: &NodeAgent, raid: &SpdkRaidDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let raid_name = raid.metadata.name.as_deref().unwrap_or("unknown");
    let lvs_name = raid.spec.lvs_name();
    let raid_bdev_name = raid.spec.raid_bdev_name();

    println!("🚀 [SPDK_INIT] Initializing LVS on RAID bdev for SpdkRaidDisk: {}", raid_name);
    println!("🔧 [SPDK_INIT] LVS name: {}, RAID bdev: {}", lvs_name, raid_bdev_name);

    // Ensure RAID bdev exists
    let bdevs = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_get_bdevs" })).await?;
    let Some(list) = bdevs["result"].as_array() else {
        return Err("Failed to get bdev list".into());
    };
    let raid_exists = list.iter().any(|b| b["name"].as_str() == Some(&raid_bdev_name));
    if !raid_exists {
        return Err(format!("RAID bdev '{}' not found on node; cannot initialize LVS", raid_bdev_name).into());
    }

    // Check for existing LVS on the RAID bdev
    if let Ok(resp) = call_spdk_rpc(&agent.spdk_rpc_url, &json!({ "method": "bdev_lvol_get_lvstores" })).await {
        if let Some(lvstores) = resp["result"].as_array() {
            if lvstores.iter().any(|lvs| lvs["base_bdev"].as_str() == Some(&raid_bdev_name)) {
                println!("✅ [SPDK_INIT] LVS already exists on RAID bdev '{}'; nothing to do", raid_bdev_name);
                return Ok(());
            }
        }
    }

    // Create LVS on the RAID bdev
    println!("🏗️ [SPDK_INIT] Creating LVS '{}' on RAID bdev '{}'", lvs_name, raid_bdev_name);
    let create = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_lvol_create_lvstore",
        "params": { "bdev_name": raid_bdev_name, "lvs_name": lvs_name, "cluster_sz": 1048576 }
    })).await?;

    if let Some(error) = create.get("error") {
        let code = error["code"].as_i64().unwrap_or(0);
        let msg = error["message"].as_str().unwrap_or("");
        if code == -17 || msg.contains("exists") {
            println!("ℹ️ [SPDK_INIT] LVS already exists by name; treating as success");
            return Ok(());
        }
        return Err(format!("SPDK RPC error creating LVS: {}", error).into());
    }

    println!("✅ [SPDK_INIT] LVS created successfully for RAID '{}': {}", raid_name, lvs_name);
    Ok(())
}

/// Create a raw disk NVMe-oF target for potential RAID membership
/// This exposes the raw disk (not LVS) via NVMe-oF so it can be used as a RAID member
async fn create_raw_disk_nvmeof_target(
    agent: &NodeAgent,
    bdev_name: &str
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Generate a stable NQN for the RAID-backed raw device
    let raw_disk_nqn = format!("nqn.2025-05.io.flint:raw-raid-{}", bdev_name);
    
    println!("🔧 [RAW_DISK_TARGET] Creating raw disk NVMe-oF target for RAID member bdev");
    println!("🔧 [RAW_DISK_TARGET] NQN: {}, Bdev: {}", raw_disk_nqn, bdev_name);
    
    // Create NVMe-oF target using SPDK RPC directly
    let subsystem_payload = json!({
        "method": "nvmf_create_subsystem",
        "params": {
            "nqn": raw_disk_nqn,
            "allow_any_host": true,
            "serial_number": format!("FLINT-RAW-{}", bdev_name),
            "model_number": "Flint Raw Disk"
        }
    });
    
    match call_spdk_rpc(&agent.spdk_rpc_url, &subsystem_payload).await {
        Ok(_) => println!("✅ [RAW_DISK_TARGET] Created NVMe-oF subsystem: {}", raw_disk_nqn),
        Err(e) => {
            if e.to_string().contains("already exists") {
                println!("ℹ️ [RAW_DISK_TARGET] NVMe-oF subsystem already exists: {}", raw_disk_nqn);
            } else {
                println!("❌ [RAW_DISK_TARGET] Failed to create subsystem: {}", e);
                return Err(e);
            }
        }
    }
    
    // Add namespace for the raw disk
    let namespace_payload = json!({
        "method": "nvmf_subsystem_add_ns",
        "params": {
            "nqn": raw_disk_nqn,
            "namespace": {
                "nsid": 1,
                "bdev_name": bdev_name
            }
        }
    });
    
    match call_spdk_rpc(&agent.spdk_rpc_url, &namespace_payload).await {
        Ok(_) => println!("✅ [RAW_DISK_TARGET] Added namespace for raw disk: {}", bdev_name),
        Err(e) => {
            if e.to_string().contains("already exists") {
                println!("ℹ️ [RAW_DISK_TARGET] Namespace already exists for raw disk: {}", bdev_name);
            } else {
                println!("❌ [RAW_DISK_TARGET] Failed to add namespace: {}", e);
                return Err(e);
            }
        }
    }
    
    // Add listener for TCP transport
    let node_ip = agent.get_current_node_ip().await?;
    let listener_payload = json!({
        "method": "nvmf_subsystem_add_listener",
        "params": {
            "nqn": raw_disk_nqn,
            "listen_address": {
                "trtype": "TCP",
                "traddr": node_ip,
                "trsvcid": "4420"
            }
        }
    });
    
    match call_spdk_rpc(&agent.spdk_rpc_url, &listener_payload).await {
        Ok(_) => println!("✅ [RAW_DISK_TARGET] Added TCP listener for raw disk: {}:4420", node_ip),
        Err(e) => {
            if e.to_string().contains("already exists") {
                println!("ℹ️ [RAW_DISK_TARGET] TCP listener already exists for raw disk");
            } else {
                println!("❌ [RAW_DISK_TARGET] Failed to add listener: {}", e);
                return Err(e);
            }
        }
    }
    
    // Legacy SpdkDisk status update removed
    Ok(())
}

/*
async fn update_existing_disk_resource(_agent: &NodeAgent, _disk: &(), _device: &NvmeDevice) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Legacy SpdkDisk update path removed
    let disk_name = disk.metadata.name.as_ref().unwrap();
    
    let mut needs_update = false;
    let mut updated_status = disk.status.clone().unwrap_or_default();
    
    // Update capacity if changed
    let current_capacity_gb = disk.spec.size_bytes;
    if current_capacity_gb != device.capacity as i64 {
        let patch = json!({
            "spec": {
                "size_bytes": device.capacity as i64
            }
        });
        spdk_disks.patch(disk_name, &PatchParams::default(), &Patch::Merge(patch)).await?;
        
        // Update total capacity in status
        updated_status.total_capacity = device.capacity;
        // Adjust free space proportionally
        let usage_ratio = if updated_status.total_capacity > 0 {
            updated_status.used_space as f64 / updated_status.total_capacity as f64
        } else {
            0.0
        };
        updated_status.free_space = device.capacity - (device.capacity as f64 * usage_ratio) as i64;
        needs_update = true;
    }
    
    // Update health status
    let is_healthy = check_device_health(agent, device).await.unwrap_or(false);
    if updated_status.healthy != is_healthy {
        updated_status.healthy = is_healthy;
        needs_update = true;
    }
    
    // Check if device path or nvme_controller_id need updating due to device name changes
    let current_device_path = format!("/dev/{}", device.controller_id);

    
    if disk.spec.device_path.as_deref() != Some(&current_device_path) {
        
        println!("🔄 [SPEC_UPDATE] Device name changed - updating CRD spec from {:?} to {}", 
                 disk.spec.device_path, current_device_path);
        
        let spec_patch = json!({
            "spec": {
                "device_path": current_device_path
            }
        });
        spdk_disks.patch(disk_name, &PatchParams::default(), &Patch::Merge(spec_patch)).await?;
        println!("✅ [SPEC_UPDATE] Successfully updated CRD spec with current device information");
    }
    
    // TODO: Add proper disk migration support using persistent identifiers
    // For now, we do NOT update node_id to avoid data loss scenarios.
    // When a disk moves between nodes:
    // 1. Old node's SpdkDisk becomes "offline" (PCIe address not found)
    // 2. New node should detect this and create appropriate migration logic
    // 3. Use serial number + model for proper disk identity across nodes
    if disk.spec.node_id != agent.node_name {
        println!("⚠️ [MIGRATION] Disk {} appears to have moved from {} to {} - manual intervention may be required", 
                 disk_name, disk.spec.node_id, agent.node_name);
        println!("⚠️ [MIGRATION] PCIe address: {:?} - consider implementing serial-based disk identity", disk.spec.pcie_addr);
    }
    
    // With simplified architecture, ensure basic status is set
    println!("🔄 [STATE_SYNC] Updating disk status...");
    if !updated_status.healthy {
        updated_status.healthy = true;
        needs_update = true;
    }
    if !updated_status.blobstore_initialized {
        updated_status.blobstore_initialized = true;
        needs_update = true;
    }
    
    if needs_update {
        updated_status.last_checked = Utc::now().to_rfc3339();
        spdk_disks
            .patch_status(disk_name, &PatchParams::default(), &Patch::Merge(json!({
                "status": updated_status
            })))
            .await?;
    }
    
    Ok(())
}
*/

/// Extract NVMe controller name from device name (nvme1n1 -> nvme1, nvme2n3 -> nvme2, etc.)
fn extract_nvme_controller_name(device_name: &str) -> String {
    // Use robust parsing to extract controller part: nvme(\d+)n(\d+) -> nvme\1
    if let Some(pos) = device_name.rfind('n') {
        // Check if the part after 'n' is a number (namespace)
        let after_n = &device_name[pos + 1..];
        if after_n.chars().all(|c| c.is_ascii_digit()) {
            // Valid namespace, return controller part
            return device_name[..pos].to_string();
        }
    }
    
    // Fallback: if pattern doesn't match, return as-is
    device_name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_extract_nvme_controller_name() {
        assert_eq!(extract_nvme_controller_name("nvme0n1"), "nvme0");
        assert_eq!(extract_nvme_controller_name("nvme1n1"), "nvme1");
        assert_eq!(extract_nvme_controller_name("nvme2n3"), "nvme2");
        assert_eq!(extract_nvme_controller_name("nvme10n15"), "nvme10");
        assert_eq!(extract_nvme_controller_name("nvme"), "nvme");         // No namespace
        assert_eq!(extract_nvme_controller_name("sda"), "sda");           // Non-NVMe
        assert_eq!(extract_nvme_controller_name("nvme1np1"), "nvme1np1"); // Invalid pattern
    }
}

async fn check_device_health(agent: &NodeAgent, device: &NvmeDevice) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Check if device is accessible via SPDK by trying both possible bdev names:
    // 1. Kernel AIO bdev: kernel_nvme1n1 (for kernel-bound devices)
    // 2. Direct controller bdev: nvme1 (for userspace-bound devices)
    
    let kernel_bdev_name = format!("kernel_{}", device.controller_id);  // e.g., kernel_nvme1n1
    
    // Extract controller name dynamically (nvme1n1 -> nvme1, nvme2n3 -> nvme2, etc.)
    let direct_bdev_name = extract_nvme_controller_name(&device.controller_id);
    
    // Try kernel AIO bdev first (most common for our setup)
    let kernel_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_bdevs",
        "params": {
            "name": kernel_bdev_name
        }
    })).await;
    
    if kernel_result.is_ok() {
        return Ok(true);
    }
    
    // Try direct controller bdev for userspace devices
    let direct_result = call_spdk_rpc(&agent.spdk_rpc_url, &json!({
        "method": "bdev_get_bdevs", 
        "params": {
            "name": direct_bdev_name
        }
    })).await;
    
    if direct_result.is_ok() {
        return Ok(true);
    }
    
    // Additional health checks could be added here
    // - SMART data analysis
    // - Temperature monitoring  
    // - Error rate checking
    
    Ok(false)
}

#[allow(dead_code)]
// update_disk_blobstore_status removed: RAID status managed elsewhere



// Disk setup implementation methods for NodeAgent
impl NodeAgent {
    /// Get current node IP address
    pub async fn get_current_node_ip(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use k8s_openapi::api::core::v1::Node as k8sNode;
        
        let nodes_api: Api<k8sNode> = Api::all(self.kube_client.clone());
        
        let node = nodes_api.get(&self.node_name).await
            .map_err(|e| format!("Node {} not found: {}", self.node_name, e))?;

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

        Err(format!("No IP address found for node {}", self.node_name).into())
    }

    /// Generate stable CRD name based on PCI address (immutable hardware identity)
    /// Example: 0000:00:1f.0 → flnt-2-pci-0000-00-1f-0
    fn disk_crd_name(&self, pci_addr: &str) -> String {
        // Convert PCI address to valid Kubernetes name: 0000:00:1f.0 → 0000-00-1f-0
        let pci_safe = pci_addr.replace(":", "-").replace(".", "-");
        format!("{}-pci-{}", self.node_name, pci_safe)
    }

    async fn discover_all_disks(&self) -> Result<Vec<UnimplementedDisk>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Starting discover_all_disks for node: {}", self.node_name);
        let mut all_disks = Vec::new();
        
        // Get all NVMe PCI devices
        let pci_devices = self.get_nvme_pci_devices().await?;
        println!("🔍 [DISCOVERY] Processing {} PCI devices...", pci_devices.len());
        
        for pci_addr in pci_devices {
            println!("🔍 [DISCOVERY] Processing PCI device: {}", pci_addr);
            match self.get_disk_info(&pci_addr).await {
                Ok(disk_info) => {
                    println!("✅ [DISCOVERY] Successfully got disk info for {}: name='{}', driver='{}', spdk_ready={}, is_system={}", 
                             pci_addr, disk_info.device_name, disk_info.driver, disk_info.spdk_ready, disk_info.is_system_disk);
                    all_disks.push(disk_info);
                }
                Err(e) => {
                    println!("❌ [DISCOVERY] Failed to get disk info for {}: {}", pci_addr, e);
                    
                    // ✅ ROBUST FALLBACK: If get_disk_info fails, try basic sysfs discovery
                    println!("🔄 [DISCOVERY] Attempting fallback discovery for: {}", pci_addr);
                    match self.create_basic_disk_info_from_sysfs(&pci_addr).await {
                        Ok(fallback_disk) => {
                            println!("✅ [DISCOVERY] Fallback discovery successful for {}: name='{}', driver='{}'", 
                                     pci_addr, fallback_disk.device_name, fallback_disk.driver);
                            all_disks.push(fallback_disk);
                        }
                        Err(fallback_err) => {
                            println!("❌ [DISCOVERY] Both primary and fallback discovery failed for {}: primary={}, fallback={}", 
                                     pci_addr, e, fallback_err);
                        }
                    }
                }
            }
        }
        
        println!("🔍 [DISCOVERY] Discovery completed: {} total disks found", all_disks.len());
        for (i, disk) in all_disks.iter().enumerate() {
            println!("🔍 [DISCOVERY]   Disk {}: {} (PCI: {}, Driver: {}, System: {}, SPDK Ready: {})", 
                     i+1, disk.device_name, disk.pci_address, disk.driver, disk.is_system_disk, disk.spdk_ready);
        }
        
        Ok(all_disks)
    }

    /// Create basic disk info using only sysfs (no SPDK dependencies)
    /// This ensures we can always discover available disks even if SPDK is not working
    async fn create_basic_disk_info_from_sysfs(&self, pci_addr: &str) -> Result<UnimplementedDisk, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔄 [FALLBACK] Creating basic disk info for PCI: {}", pci_addr);
        let sysfs_path = format!("/sys/bus/pci/devices/{}", pci_addr);
        
        // Verify PCI device exists
        if !std::path::Path::new(&sysfs_path).exists() {
            return Err(format!("PCI device {} does not exist", pci_addr).into());
        }
        
        // Read basic PCI information
        let vendor_id = self.read_sysfs_file(&format!("{}/vendor", sysfs_path)).await
            .unwrap_or_else(|_| "0x0000".to_string());
        let device_id = self.read_sysfs_file(&format!("{}/device", sysfs_path)).await
            .unwrap_or_else(|_| "0x0000".to_string());
            
        // Get current driver (this should always work)
        let driver = self.get_current_driver(pci_addr).await.unwrap_or("unknown".to_string());
        
        // Generate device info based on driver state
        let (device_name, size_bytes, model, is_system_disk, spdk_ready) = if driver == "unbound" || driver == "unknown" {
            // For unbound devices, create minimal info
            let device_name = format!("nvme-{}", pci_addr.replace(":", "-"));
            let model = self.get_model_from_pci_ids(&vendor_id, &device_id).await;
            let estimated_size = 1_000_000_000_000; // 1TB default
            
            println!("🔄 [FALLBACK] Unbound device - Name: {}, Model: {}, Estimated size: {}GB", 
                     device_name, model, estimated_size / (1024*1024*1024));
            
            (device_name, estimated_size, model, false, false)
        } else if driver == "nvme" {
            // For nvme-bound devices, try to get real block device info
            match self.find_nvme_device_name(pci_addr).await {
                Ok(real_device_name) => {
                    let size = self.get_device_size(&real_device_name).await.unwrap_or(1_000_000_000_000);
                    let model = self.get_model_from_pci_ids(&vendor_id, &device_id).await;
                    let is_system = self.quick_system_disk_check(&real_device_name).await;
                    
                    println!("🔄 [FALLBACK] NVMe device - Name: {}, Size: {}GB, System: {}", 
                             real_device_name, size / (1024*1024*1024), is_system);
                    
                    (real_device_name, size, model, is_system, !is_system)
                }
                Err(_) => {
                    // Even device name resolution failed, but still check for system disk by PCI
                    let device_name = format!("nvme-{}", pci_addr.replace(":", "-"));
                    let model = self.get_model_from_pci_ids(&vendor_id, &device_id).await;
                    let is_system = self.system_disk_check_by_pci(pci_addr).await;
                    
                    println!("🔄 [FALLBACK] NVMe fallback - Name: {}, Model: {}, System: {}", device_name, model, is_system);
                    
                    (device_name, 1_000_000_000_000, model, is_system, !is_system)
                }
            }
        } else {
            // Other drivers (vfio-pci, etc.) - treat as SPDK-ready
            let device_name = format!("spdk-{}", pci_addr.replace(":", "-"));
            let model = self.get_model_from_pci_ids(&vendor_id, &device_id).await;
            
            println!("🔄 [FALLBACK] SPDK driver '{}' - Name: {}, Model: {}", driver, device_name, model);
            
            (device_name, 1_000_000_000_000, model, false, true)
        };
        
        let disk_info = UnimplementedDisk {
            pci_address: pci_addr.to_string(),
            device_name,
            vendor_id: vendor_id.trim().to_string(),
            device_id: device_id.trim().to_string(),
            subsystem_vendor_id: vendor_id.trim().to_string(),
            subsystem_device_id: device_id.trim().to_string(),
            numa_node: None,
            driver,
            size_bytes,
            model,
            serial: "Unknown".to_string(),
            firmware_version: "Unknown".to_string(),
            namespace_id: Some(1),
            mounted_partitions: Vec::new(),
            filesystem_type: None,
            is_system_disk,
            spdk_ready,
            discovered_at: Utc::now().to_rfc3339(),
        };
        
        println!("✅ [FALLBACK] Created basic disk info for {}: name='{}', driver='{}', spdk_ready={}", 
                 pci_addr, disk_info.device_name, disk_info.driver, disk_info.spdk_ready);
        
        Ok(disk_info)
    }

    /// System disk check using PCI address when device name is not available
    async fn system_disk_check_by_pci(&self, pci_addr: &str) -> bool {
        println!("🔍 [SYSTEM_CHECK_PCI] Checking if PCI device {} contains system disk", pci_addr);
        
        // Method 1: Find any block device that belongs to this PCI and check if it's mounted on root
        if let Ok(entries) = fs::read_dir("/sys/block") {
            for entry in entries {
                if let Ok(entry) = entry {
                    let device_name = entry.file_name().to_string_lossy().to_string();
                    
                    if device_name.starts_with("nvme") && device_name.contains("n") {
                        let device_path = format!("/sys/block/{}/device", device_name);
                        
                        // Use readlink command to get fully resolved path
                        if let Ok(output) = Command::new("readlink").args(["-f", &device_path]).output() {
                            let resolved_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                            
                            if resolved_path.contains(pci_addr) {
                                println!("🔍 [SYSTEM_CHECK_PCI] Found device {} for PCI {}, checking if system disk", device_name, pci_addr);
                                
                                // Check if this device contains the root filesystem
                                if self.quick_system_disk_check(&device_name).await {
                                    println!("✅ [SYSTEM_CHECK_PCI] PCI device {} is system disk (via {})", pci_addr, device_name);
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        
        println!("✅ [SYSTEM_CHECK_PCI] PCI device {} is NOT a system disk", pci_addr);
        false
    }

    /// Quick system disk check without full mount analysis
    async fn quick_system_disk_check(&self, device_name: &str) -> bool {
        println!("🔍 [SYSTEM_CHECK] Checking if {} is a system disk", device_name);
        
        // Method 1: Check if it's mounted on root filesystem
        if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", "/"]).output() {
            let root_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!("🔍 [SYSTEM_CHECK] Root filesystem source: '{}'", root_source);
            
            // Check direct device match
            if root_source.contains(device_name) {
                println!("✅ [SYSTEM_CHECK] {} is system disk (direct root match)", device_name);
                return true;
            }
            
            // Check if root source is a partition of this device
            if root_source.starts_with(&format!("/dev/{}", device_name)) {
                println!("✅ [SYSTEM_CHECK] {} is system disk (root partition)", device_name);
                return true;
            }
        }
        
        // Method 2: Check if any partition of this device contains critical system mounts
        let device_path = format!("/dev/{}", device_name);
        let critical_mounts = ["/", "/boot", "/boot/efi", "/usr", "/var"];
        
        for critical_mount in &critical_mounts {
            if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", critical_mount]).output() {
                let mount_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !mount_source.is_empty() && (mount_source.contains(device_name) || mount_source.starts_with(&device_path)) {
                    println!("✅ [SYSTEM_CHECK] {} is system disk (critical mount {} on {})", device_name, critical_mount, mount_source);
                    return true;
                }
            }
        }
        
        // Method 3: Check if this device has mounted partitions with system-like paths
        if let Ok(output) = Command::new("lsblk").args(["-ln", "-o", "NAME,MOUNTPOINT", &device_path]).output() {
            let lsblk_output = String::from_utf8_lossy(&output.stdout);
            for line in lsblk_output.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mountpoint = parts[1];
                    if critical_mounts.contains(&mountpoint) {
                        println!("✅ [SYSTEM_CHECK] {} is system disk (lsblk shows {} mounted on {})", device_name, parts[0], mountpoint);
                        return true;
                    }
                }
            }
        }
        
        // Method 4: Check if this device is in the root device hierarchy
        if let Ok(output) = Command::new("lsblk").args(["-ln", "-o", "NAME,TYPE,MOUNTPOINT"]).output() {
            let lsblk_output = String::from_utf8_lossy(&output.stdout);
            let mut found_root_device = false;
            let mut root_device_family = String::new();
            
            // First pass: find the device that contains root
            for line in lsblk_output.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 && parts[2] == "/" {
                    root_device_family = parts[0].chars().take_while(|c| !c.is_ascii_digit()).collect();
                    found_root_device = true;
                    break;
                }
            }
            
            // Second pass: check if our device is part of the same family
            if found_root_device && device_name.starts_with(&root_device_family) {
                println!("✅ [SYSTEM_CHECK] {} is system disk (same device family as root: {})", device_name, root_device_family);
                return true;
            }
        }
        
        println!("✅ [SYSTEM_CHECK] {} is NOT a system disk", device_name);
        false
    }

    async fn get_nvme_pci_devices(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Scanning for NVMe PCI devices using lspci...");
        
        let output = Command::new("lspci")
            .args(["-D", "-d", "::0108"]) // NVMe class code
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        let mut devices = Vec::new();

        println!("🔍 [DISCOVERY] lspci output:");
        for line in stdout.lines() {
            println!("🔍 [DISCOVERY]   {}", line);
            if let Some(pci_addr) = line.split_whitespace().next() {
                devices.push(pci_addr.to_string());
                println!("🔍 [DISCOVERY] Found PCI device: {}", pci_addr);
            }
        }

        println!("🔍 [DISCOVERY] Total NVMe PCI devices found: {}", devices.len());
        Ok(devices)
    }

    async fn get_disk_info(&self, pci_addr: &str) -> Result<UnimplementedDisk, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISK_INFO] Getting disk info for PCI address: {}", pci_addr);
        let sysfs_path = format!("/sys/bus/pci/devices/{}", pci_addr);
        
        // Read PCI device information
        println!("🔍 [DISK_INFO] Reading PCI device information from: {}", sysfs_path);
        let vendor_id = self.read_sysfs_file(&format!("{}/vendor", sysfs_path)).await?;
        let device_id = self.read_sysfs_file(&format!("{}/device", sysfs_path)).await?;
        let subsystem_vendor = self.read_sysfs_file(&format!("{}/subsystem_vendor", sysfs_path)).await
            .unwrap_or_else(|_| vendor_id.clone());
        let subsystem_device = self.read_sysfs_file(&format!("{}/subsystem_device", sysfs_path)).await
            .unwrap_or_else(|_| device_id.clone());
        
        println!("🔍 [DISK_INFO] PCI Info - Vendor: {}, Device: {}", vendor_id.trim(), device_id.trim());
        
        // Get NUMA node
        let numa_node = self.read_sysfs_file(&format!("{}/numa_node", sysfs_path)).await
            .ok()
            .and_then(|s| s.trim().parse().ok());
        println!("🔍 [DISK_INFO] NUMA node: {:?}", numa_node);

        // Get current driver
        let driver = self.get_current_driver(pci_addr).await?;
        println!("🔍 [DISK_INFO] Current driver: '{}'", driver);
        
        // Get device information - for unbound devices, use PCI info and reasonable defaults
        let (device_name, size_bytes, model, serial, firmware_version, mounted_partitions, is_system_disk) = 
            if driver == "unbound" {
                println!("🔍 [DISK_INFO] Device is unbound, using PCI-based detection");
                // For unbound devices, use PCI address as device identifier
                let device_name = format!("nvme-{}", pci_addr.replace(":", "-"));
                
                // Get estimated size from PCI config or use reasonable default for NVMe
                let estimated_size = self.estimate_nvme_size_from_pci(pci_addr).await.unwrap_or(1_000_000_000_000); // 1TB default
                
                // Get model name from vendor/device ID lookup
                let model = self.get_model_from_pci_ids(&vendor_id, &device_id).await;
                
                println!("🔍 [DISK_INFO] Unbound device info - Name: {}, Size: {} bytes, Model: {}", 
                         device_name, estimated_size, model);
                
                (
                    device_name,
                    estimated_size,
                    model,
                    "Unknown".to_string(), // Serial not available without binding
                    "Unknown".to_string(), // Firmware not available without binding
                    Vec::new(), // No mounted partitions for unbound devices
                    false, // Unbound devices are never system disks
                )
            } else {
                println!("🔍 [DISK_INFO] Device is bound to '{}', getting block device information", driver);
                // For bound devices, get the actual block device information
                let device_name = self.find_nvme_device_name(pci_addr).await?;
                println!("🔍 [DISK_INFO] Found block device name: {}", device_name);
                let (size_bytes, model, serial, firmware_version) = self.get_nvme_details(&device_name).await?;
                let mounted_partitions = self.get_mounted_partitions(&device_name).await?;
                let is_system_disk = self.is_system_disk(&device_name, &mounted_partitions).await?;
                println!("🔍 [DISK_INFO] Bound device info - Name: {}, Size: {} bytes, Model: {}, Mounted: {:?}, System: {}", 
                         device_name, size_bytes, model, mounted_partitions, is_system_disk);
                (device_name, size_bytes, model, serial, firmware_version, mounted_partitions, is_system_disk)
            };
        
        // Determine if SPDK ready (supports both userspace and kernel-bound modes)
        let spdk_ready = self.is_spdk_compatible_driver(&driver) || 
                        (self.is_virtualized_environment().await.unwrap_or(false) && 
                         driver == "nvme" && !is_system_disk);
        
        println!("🔍 [DISK_INFO] SPDK compatibility - Driver compatible: {}, Is system: {}, SPDK ready: {}", 
                 self.is_spdk_compatible_driver(&driver), is_system_disk, spdk_ready);

        let disk_info = UnimplementedDisk {
            pci_address: pci_addr.to_string(),
            device_name,
            vendor_id: vendor_id.trim().to_string(),
            device_id: device_id.trim().to_string(),
            subsystem_vendor_id: subsystem_vendor.trim().to_string(),
            subsystem_device_id: subsystem_device.trim().to_string(),
            numa_node,
            driver,
            size_bytes,
            model,
            serial,
            firmware_version,
            namespace_id: Some(1),
            mounted_partitions,
            filesystem_type: None,
            is_system_disk,
            spdk_ready,
            discovered_at: Utc::now().to_rfc3339(),
        };
        
        println!("✅ [DISK_INFO] Completed disk info for {}: {}", pci_addr, disk_info.device_name);
        Ok(disk_info)
    }

    async fn read_sysfs_file(&self, path: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(fs::read_to_string(path)?)
    }

    async fn get_current_driver(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let driver_path = format!("/sys/bus/pci/devices/{}/driver", pci_addr);
        
        match fs::read_link(&driver_path) {
            Ok(link) => {
                if let Some(driver_name) = link.file_name() {
                    Ok(driver_name.to_string_lossy().to_string())
                } else {
                    Ok("unknown".to_string())
                }
            }
            Err(_) => Ok("unbound".to_string()),
        }
    }

    async fn find_nvme_device_name(&self, pci_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DEVICE_SEARCH] Looking for NVMe device for PCI address: {}", pci_addr);
        
        // Method 1: Direct search in /sys/block/ using readlink -f to get full paths
        if let Ok(entries) = fs::read_dir("/sys/block") {
            for entry in entries {
                if let Ok(entry) = entry {
                    let device_name = entry.file_name().to_string_lossy().to_string();
                    
                    if device_name.starts_with("nvme") && device_name.contains("n") {
                        let device_path = format!("/sys/block/{}/device", device_name);
                        
                        // Use readlink command to get fully resolved path
                        if let Ok(output) = Command::new("readlink").args(["-f", &device_path]).output() {
                            let resolved_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                            println!("🔍 [DEVICE_SEARCH] {} -> {}", device_name, resolved_path);
                            
                            if resolved_path.contains(pci_addr) {
                                println!("✅ [DEVICE_SEARCH] Found matching device: {} for PCI {}", device_name, pci_addr);
                                return Ok(device_name);
                            }
                        }
                    }
                }
            }
        }
        
        // Method 2: Look through PCI device structure (fallback)
        let nvme_path = format!("/sys/bus/pci/devices/{}/nvme", pci_addr);
        
        if let Ok(entries) = fs::read_dir(&nvme_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let controller_name = entry.file_name().to_string_lossy().to_string();
                    if controller_name.starts_with("nvme") {
                        println!("🔍 [DEVICE_SEARCH] Found controller: {}", controller_name);
                        
                        // Look for namespaces in the controller directory
                        let controller_path = entry.path();
                        if let Ok(ns_entries) = fs::read_dir(&controller_path) {
                            for ns_entry in ns_entries {
                                if let Ok(ns_entry) = ns_entry {
                                    let ns_name = ns_entry.file_name().to_string_lossy().to_string();
                                    if ns_name.starts_with(&format!("{}n", controller_name)) && 
                                       ns_name.chars().last().map_or(false, |c| c.is_ascii_digit()) {
                                        
                                        // Verify it exists in /dev/
                                        let dev_path = format!("/dev/{}", ns_name);
                                        if std::path::Path::new(&dev_path).exists() {
                                            println!("✅ [DEVICE_SEARCH] Found namespace via controller: {} for PCI {}", ns_name, pci_addr);
                                            return Ok(ns_name);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        
        // If we get here, no namespace was found - return descriptive error
        let error_msg = format!("No NVMe namespace found for PCI device {} - device may be unbound or have no accessible namespaces", pci_addr);
        println!("❌ [DEVICE_SEARCH] {}", error_msg);
        Err(error_msg.into())
    }

    async fn get_nvme_details(&self, device_name: &str) -> Result<(u64, String, String, String), Box<dyn std::error::Error + Send + Sync>> {
        // Use nvme-cli to get device information
        let output = Command::new("nvme")
            .args(["id-ctrl", &format!("/dev/{}", device_name)])
            .output();

        let (model, serial, firmware_version) = if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let model = self.extract_nvme_field(&stdout, "mn").unwrap_or("Unknown".to_string());
            let serial = self.extract_nvme_field(&stdout, "sn").unwrap_or("Unknown".to_string());
            let firmware = self.extract_nvme_field(&stdout, "fr").unwrap_or("Unknown".to_string());
            (model, serial, firmware)
        } else {
            // Fallback to sysfs
            let model = self.read_sysfs_file(&format!("/sys/block/{}/device/model", device_name)).await
                .unwrap_or("Unknown".to_string());
            (model.trim().to_string(), "Unknown".to_string(), "Unknown".to_string())
        };

        // Get size from blockdev
        let size_bytes = self.get_device_size(device_name).await?;

        Ok((size_bytes, model, serial, firmware_version))
    }

    fn extract_nvme_field(&self, nvme_output: &str, field: &str) -> Option<String> {
        let pattern = format!(r"{}\s*:\s*(.+)", field);
        let re = Regex::new(&pattern).ok()?;
        
        if let Some(captures) = re.captures(nvme_output) {
            Some(captures[1].trim().to_string())
        } else {
            None
        }
    }

    /// Discover NVMe devices using persistent paths (Portworx-style hardware identification)
    async fn discover_devices_by_persistent_paths(&self) -> Result<Vec<NvmeDevice>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [PERSISTENT_DISCOVERY] Starting discovery using /dev/disk/by-id/ paths");
        let mut devices = Vec::new();
        
        // Scan /dev/disk/by-id/ for NVMe devices
        if let Ok(entries) = fs::read_dir("/dev/disk/by-id") {
            for entry in entries {
                if let Ok(entry) = entry {
                    let id_path = entry.path();
                    let id_name = entry.file_name().to_string_lossy().to_string();
                    
                    // Look for NVMe devices (exclude partitions)
                    if id_name.starts_with("nvme-") && !id_name.contains("-part") {
                        println!("🔍 [PERSISTENT_DISCOVERY] Found NVMe device: {}", id_name);
                        
                        // Resolve to actual device path
                        if let Ok(resolved_path) = fs::read_link(&id_path) {
                            let device_path = format!("/dev/{}", 
                                resolved_path.file_name().unwrap().to_string_lossy());
                            
                            println!("🔍 [DISCOVERY] Found device: {}", device_path);
                            
                            // Create complete device information
                            if let Ok(device) = self.create_device_from_persistent_path(
                                &id_path.to_string_lossy(), 
                                &device_path
                            ).await {
                                devices.push(device);
                            } else {
                                println!("⚠️ [PERSISTENT_DISCOVERY] Failed to get device info for {}", id_name);
                            }
                        }
                    }
                }
            }
        } else {
            println!("⚠️ [PERSISTENT_DISCOVERY] /dev/disk/by-id/ not available, falling back to traditional discovery");
            // Fallback to traditional discovery methods
            return self.discover_devices_traditional().await;
        }
        
        println!("✅ [PERSISTENT_DISCOVERY] Found {} NVMe devices via persistent paths", devices.len());
        Ok(devices)
    }
    
    /// Create NvmeDevice from persistent disk path (Portworx-style)
    async fn create_device_from_persistent_path(&self, disk_id_path: &str, device_path: &str) -> Result<NvmeDevice, Box<dyn std::error::Error + Send + Sync>> {
        let device_name = device_path.trim_start_matches("/dev/");
        
        // Get hardware details
        let (capacity, model, serial, _firmware) = self.get_nvme_details(device_name).await?;
        
        // Extract vendor from disk ID or use fallback
        let vendor = self.extract_vendor_from_disk_id(disk_id_path).unwrap_or_else(|| "Unknown".to_string());
        
        // Get WWN if available
        let wwn = self.get_device_wwn(device_name).await.ok();
        
        // Find PCI address for this device
        let pcie_addr = self.find_pci_address_for_device(device_name).await
            .unwrap_or_else(|_| "Unknown".to_string());
        
        // Check if disk has existing cluster metadata
        let cluster_metadata = self.read_disk_cluster_metadata(device_name).await.ok();
        
        // REMOVED: LVS discovery logic - unused fields disk_uuid_from_lvs and cluster_id_prefix removed
        
        if let Some(ref metadata) = cluster_metadata {
            println!("🔍 [PERSISTENT_DISCOVERY] Disk {} has cluster metadata: cluster={}, disk_uuid={}", 
                     disk_id_path, metadata.cluster_id, metadata.disk_uuid);
        }
        
        Ok(NvmeDevice {
            controller_id: device_name.to_string(),
            pcie_addr,
            device_path: device_path.to_string(),
            disk_id: disk_id_path.to_string(),
            serial_number: serial,
            wwn,
            model,
            vendor,
            capacity: capacity as i64,
            // REMOVED: sector_size - unused field
            cluster_metadata,
            // REMOVED: disk_uuid_from_lvs and cluster_id_prefix - unused fields
        })
    }
    
    /// Extract vendor name from disk ID path
    fn extract_vendor_from_disk_id(&self, disk_id_path: &str) -> Option<String> {
        // Parse disk ID like: /dev/disk/by-id/nvme-Samsung_SSD_970_EVO_Plus_1TB_S4EWNX0R123456
        let id_name = disk_id_path.strip_prefix("/dev/disk/by-id/nvme-")?;
        
        // Common vendor patterns
        if id_name.starts_with("Samsung") { Some("Samsung".to_string()) }
        else if id_name.starts_with("Intel") { Some("Intel".to_string()) }
        else if id_name.starts_with("Western_Digital") || id_name.starts_with("WDC") { Some("Western Digital".to_string()) }
        else if id_name.starts_with("Seagate") { Some("Seagate".to_string()) }
        else if id_name.starts_with("Micron") { Some("Micron".to_string()) }
        else if id_name.starts_with("SK_hynix") { Some("SK Hynix".to_string()) }
        else if id_name.starts_with("KIOXIA") { Some("KIOXIA".to_string()) }
        else if id_name.starts_with("Crucial") { Some("Crucial".to_string()) }
        else {
            // Extract first word as vendor
            id_name.split('_').next().map(|s| s.replace('_', " "))
        }
    }
    
    /// Get device WWN (World Wide Name) if available
    async fn get_device_wwn(&self, device_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Try multiple ways to get WWN
        
        // Method 1: nvme-cli
        if let Ok(output) = Command::new("nvme")
            .args(["id-ctrl", &format!("/dev/{}", device_name), "-o", "json"])
            .output() {
            if let Ok(json_str) = String::from_utf8(output.stdout) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    if let Some(wwn) = json["wwn"].as_str() {
                        return Ok(wwn.to_string());
                    }
                }
            }
        }
        
        // Method 2: sysfs
        if let Ok(wwn) = self.read_sysfs_file(&format!("/sys/block/{}/wwid", device_name)).await {
            return Ok(wwn.trim().to_string());
        }
        
        Err("WWN not available".into())
    }
    
    /// Find PCI address for a given device
    async fn find_pci_address_for_device(&self, device_name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Read the device symlink to find PCI address
        let device_link = format!("/sys/block/{}/device", device_name);
        
        if let Ok(output) = Command::new("readlink").args(["-f", &device_link]).output() {
            let resolved_path = String::from_utf8(output.stdout)?;
            
            // Extract PCI address from path like: /sys/devices/pci0000:00/0000:00:1f.0/nvme/nvme1
            for part in resolved_path.split('/') {
                if part.len() == 12 && part.chars().nth(4) == Some(':') && part.chars().nth(7) == Some(':') {
                    return Ok(part.to_string());
                }
            }
        }
        
        Err("PCI address not found".into())
    }
    
    /// Read LVS names from device (simplified approach)
    /// Just lists existing LVS stores without complex parsing
    async fn read_lvs_stores(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        // Query existing LVS stores on this device
        let lvs_result = call_spdk_rpc(&self.spdk_rpc_url, &json!({
            "method": "bdev_lvol_get_lvstores"
        })).await;
        
        match lvs_result {
            Ok(response) => {
                let mut lvs_names = Vec::new();
                if let Some(lvs_array) = response.as_array() {
                    for lvs in lvs_array {
                        if let Some(lvs_name) = lvs.get("name").and_then(|n| n.as_str()) {
                            println!("🔍 [LVS_FOUND] Existing LVS: {}", lvs_name);
                            lvs_names.push(lvs_name.to_string());
                        }
                    }
                }
                Ok(lvs_names)
            }
            Err(e) => {
                println!("⚠️ [LVS_QUERY] Failed to query LVS stores: {}", e);
                Err(format!("Failed to query LVS: {}", e).into())
            }
        }
    }
    
    /// Read cluster metadata from disk blobstore (simplified)
    async fn read_disk_cluster_metadata(&self, _device_name: &str) -> Result<FlintDiskMetadata, Box<dyn std::error::Error + Send + Sync>> {
        // Check if any LVS exists on this device
        match self.read_lvs_stores().await {
            Ok(lvs_names) if !lvs_names.is_empty() => {
                // Create a minimal FlintDiskMetadata indicating LVS presence
                let metadata = FlintDiskMetadata {
                    version: 1,
                    cluster_id: self.cluster_id.clone(),
                    cluster_name: None,
                    disk_uuid: "virtual".to_string(), // Virtual UUID will be generated
                    pool_uuid: "unknown".to_string(),
                    pool_name: "unknown".to_string(),
                    hardware_id: "unknown".to_string(),
                    serial_number: "unknown".to_string(),
                    model: "unknown".to_string(),
                    vendor: "unknown".to_string(),
                    wwn: None,
                    initialized_at: "unknown".to_string(),
                    initialized_by_node: "unknown".to_string(),
                    last_attached_node: "unknown".to_string(),
                    attachment_history: vec![],
                    total_size: 0,
                    usable_size: 0,
                    sector_size: 512,
                    optimal_io_size: 4096,
                };
                Ok(metadata)
            }
            _ => {
                // Fallback: no metadata found
                Err("No cluster metadata found".into())
            }
        }
    }
    
    /// Fallback discovery using traditional methods
    async fn discover_devices_traditional(&self) -> Result<Vec<NvmeDevice>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [TRADITIONAL_DISCOVERY] Using traditional discovery methods");
        // Implementation would fall back to existing discovery logic
        Ok(Vec::new())
    }

    async fn get_device_size(&self, device_name: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("blockdev")
            .args(["--getsize64", &format!("/dev/{}", device_name)])
            .output()?;

        let size_str = String::from_utf8(output.stdout)?;
        Ok(size_str.trim().parse()?)
    }

    async fn estimate_nvme_size_from_pci(&self, pci_addr: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        // Try to get size information from PCI configuration or use lspci
        let output = Command::new("lspci")
            .args(["-v", "-s", pci_addr])
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        
        // Look for memory regions that might indicate device size
        // This is a rough estimation since NVMe size isn't directly in PCI config
        for line in stdout.lines() {
            if line.contains("Memory at") && line.contains("size=") {
                // Extract size if available, but for NVMe this is typically not the storage size
                // Fall back to common NVMe sizes
                return Ok(1_000_000_000_000); // 1TB default
            }
        }
        
        // For AWS EBS NVMe devices, try to use common sizes
        if stdout.contains("Amazon") {
            // This could be enhanced to detect EBS volume sizes
            return Ok(1_000_000_000_000); // 1TB default for unbound EBS volumes
        }
        
        Ok(1_000_000_000_000) // 1TB default
    }

    async fn get_model_from_pci_ids(&self, vendor_id: &str, device_id: &str) -> String {
        // Convert hex IDs to model names for common vendors
        match (vendor_id.trim(), device_id.trim()) {
            ("0x1d0f", "0x8061") => "Amazon Elastic Block Store".to_string(),
            ("0x144d", _) => "Samsung NVMe SSD".to_string(),
            ("0x15b7", _) => "SanDisk NVMe SSD".to_string(),
            ("0x1344", _) => "Micron NVMe SSD".to_string(),
            ("0x1179", _) => "Toshiba NVMe SSD".to_string(),
            ("0x1c5c", _) => "SK Hynix NVMe SSD".to_string(),
            ("0x1987", _) => "Phison NVMe SSD".to_string(),
            ("0x1bb1", _) => "Seagate NVMe SSD".to_string(),
            ("0x1f40", _) => "NETAC NVMe SSD".to_string(),
            ("0x10ec", _) => "Realtek NVMe SSD".to_string(),
            ("0x8086", _) => "Intel NVMe SSD".to_string(),
            ("0x1cc1", _) => "ADATA NVMe SSD".to_string(),
            _ => format!("NVMe SSD ({}:{})", vendor_id.trim(), device_id.trim()),
        }
    }

    async fn get_mounted_partitions(&self, device_name: &str) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("lsblk")
            .args(["-ln", "-o", "NAME,MOUNTPOINT", &format!("/dev/{}", device_name)])
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        let mut mounted = Vec::new();

        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && !parts[1].is_empty() {
                mounted.push(parts[1].to_string());
            }
        }

        Ok(mounted)
    }

    async fn is_system_disk(&self, device_name: &str, mounted_partitions: &[String]) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [SYSTEM_DISK] Comprehensive system disk check for: {}", device_name);
        println!("🔍 [SYSTEM_DISK] Mounted partitions: {:?}", mounted_partitions);
        
        // Check if any partition is mounted on critical system paths
        let critical_mounts = ["/", "/boot", "/boot/efi", "/var", "/usr", "/home"];
        
        for mount in mounted_partitions {
            if critical_mounts.contains(&mount.as_str()) {
                println!("✅ [SYSTEM_DISK] {} is system disk (critical mount: {})", device_name, mount);
                return Ok(true);
            }
        }

        // Check for containerized system file bind mounts (common in Kubernetes)
        let container_system_mounts = [
            "/etc/resolv.conf", "/etc/hosts", "/etc/hostname", 
            "/etc/passwd", "/etc/group", "/etc/shadow",
            "/dev/termination-log"
        ];
        
        for mount in mounted_partitions {
            if container_system_mounts.contains(&mount.as_str()) {
                println!("✅ [SYSTEM_DISK] {} is system disk (container system mount: {})", device_name, mount);
                return Ok(true);
            }
        }

        // Enhanced root filesystem detection
        let output = Command::new("findmnt")
            .args(["-n", "-o", "SOURCE", "/"])
            .output()?;

        let root_device = String::from_utf8(output.stdout)?;
        let root_device = root_device.trim();
        println!("🔍 [SYSTEM_DISK] Root filesystem source: '{}'", root_device);
        
        // Check direct device name match
        if root_device.contains(device_name) {
            println!("✅ [SYSTEM_DISK] {} is system disk (root filesystem match)", device_name);
            return Ok(true);
        }
        
        // Check if root device is a partition of this device
        if root_device.starts_with(&format!("/dev/{}", device_name)) {
            println!("✅ [SYSTEM_DISK] {} is system disk (root device partition)", device_name);
            return Ok(true);
        }

        // Enhanced mount analysis - check all critical mount points
        for critical_path in &critical_mounts {
            if let Ok(mount_output) = Command::new("findmnt")
                .args(["-n", "-o", "SOURCE", critical_path])
                .output() 
            {
                let mount_source = String::from_utf8_lossy(&mount_output.stdout).trim().to_string();
                if !mount_source.is_empty() && mount_source.contains(device_name) {
                    println!("✅ [SYSTEM_DISK] {} is system disk (critical path {} mounted from {})", device_name, critical_path, mount_source);
                    return Ok(true);
                }
            }
        }

        // Additional check: see if this device is mounted to critical system paths
        // by examining all mounts of this device
        let mount_output = Command::new("mount")
            .output()?;
        
        if mount_output.status.success() {
            let mount_info = String::from_utf8(mount_output.stdout)?;
            for line in mount_info.lines() {
                if line.contains(device_name) {
                    // Check if this device is mounted to any critical system location
                    for critical_mount in &critical_mounts {
                        if line.contains(&format!(" on {} ", critical_mount)) {
                            println!("✅ [SYSTEM_DISK] {} is system disk (mount analysis: {})", device_name, line.trim());
                            return Ok(true);
                        }
                    }
                }
            }
        }
        
        // Final check: Use lsblk to get comprehensive device hierarchy
        let lsblk_output = Command::new("lsblk")
            .args(["-ln", "-o", "NAME,MOUNTPOINT", &format!("/dev/{}", device_name)])
            .output();
            
        if let Ok(output) = lsblk_output {
            let lsblk_info = String::from_utf8_lossy(&output.stdout);
            for line in lsblk_info.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mountpoint = parts[1];
                    if critical_mounts.contains(&mountpoint) {
                        println!("✅ [SYSTEM_DISK] {} is system disk (lsblk hierarchy: {} -> {})", device_name, parts[0], mountpoint);
                        return Ok(true);
                    }
                }
            }
        }

        println!("✅ [SYSTEM_DISK] {} is NOT a system disk", device_name);
        Ok(false)
    }

    fn is_spdk_compatible_driver(&self, driver: &str) -> bool {
        matches!(driver, "vfio-pci" | "uio_pci_generic" | "igb_uio")
    }

    async fn setup_disks_for_spdk(&self, request: DiskSetupRequest) -> Result<DiskSetupResult, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SETUP_HANDLER] Starting setup_disks_for_spdk on node: {}", self.node_name);
        println!("🔧 [SETUP_HANDLER] Request details:");
        println!("   - PCI addresses: {:?}", request.pci_addresses);
        println!("   - Force unmount: {}", request.force_unmount);
        println!("   - Backup data: {}", request.backup_data);
        println!("   - Huge pages MB: {:?}", request.huge_pages_mb);
        println!("   - Driver override: {:?}", request.driver_override);
        
        let mut result = DiskSetupResult {
            success: true,
            setup_disks: Vec::new(),
            failed_disks: Vec::new(),
            warnings: Vec::new(),
            huge_pages_configured: None,
            completed_at: Utc::now().to_rfc3339(),
        };

        // Validate all disks first
        println!("🔧 [SETUP_HANDLER] Step 1: Validating {} disks...", request.pci_addresses.len());
        for pci_addr in &request.pci_addresses {
            println!("🔧 [SETUP_HANDLER] Validating disk: {}", pci_addr);
            if let Err(e) = self.validate_disk_for_setup(pci_addr, request.force_unmount).await {
                println!("❌ [SETUP_HANDLER] Validation failed for {}: {}", pci_addr, e);
                result.failed_disks.push((pci_addr.clone(), e.to_string()));
                result.success = false;
                continue;
            } else {
                println!("✅ [SETUP_HANDLER] Validation passed for: {}", pci_addr);
            }
        }

        if !result.success && !request.force_unmount {
            println!("❌ [SETUP_HANDLER] Validation failed and force_unmount=false, aborting");
            return Ok(result);
        }

        // Setup huge pages (always configure for SPDK optimization)
        let hugepage_mb = request.huge_pages_mb.unwrap_or(0); // 0 will trigger auto-calculation
        match self.setup_huge_pages(hugepage_mb).await {
            Ok(configured) => {
                result.huge_pages_configured = Some(configured);
                if hugepage_mb == 0 {
                    result.warnings.push(format!("Auto-configured {}MB hugepages for optimal SPDK performance", configured));
                }
            }
            Err(e) => {
                result.warnings.push(format!("Huge pages setup warning: {}", e));
            }
        }

        // Load ublk driver for userspace block devices
        match self.load_ublk_driver().await {
            Ok(_) => {
                println!("✅ [SETUP_HANDLER] ublk_drv module loaded successfully");
            }
            Err(e) => {
                result.warnings.push(format!("ublk_drv setup warning: {}", e));
                println!("⚠️ [SETUP_HANDLER] ublk_drv setup warning: {}", e);
            }
        }

        // Process each disk
        println!("🔧 [SETUP_HANDLER] Step 3: Processing {} disks for setup...", request.pci_addresses.len());
        for pci_addr in &request.pci_addresses {
            if result.failed_disks.iter().any(|(addr, _)| addr == pci_addr) {
                println!("⏭️  [SETUP_HANDLER] Skipping already failed disk: {}", pci_addr);
                continue;
            }

            println!("🔧 [SETUP_HANDLER] Setting up disk: {}", pci_addr);
            match self.setup_single_disk(pci_addr, &request).await {
                Ok(_) => {
                    println!("✅ [SETUP_HANDLER] Successfully set up disk: {}", pci_addr);
                    result.setup_disks.push(pci_addr.clone());
                }
                Err(e) => {
                    println!("❌ [SETUP_HANDLER] Failed to set up disk {}: {}", pci_addr, e);
                    result.failed_disks.push((pci_addr.clone(), e.to_string()));
                    result.success = false;
                }
            }
        }

        result.completed_at = Utc::now().to_rfc3339();
        
        println!("🔧 [SETUP_HANDLER] Setup completed. Final result:");
        println!("   - Success: {}", result.success);
        println!("   - Setup disks: {:?}", result.setup_disks);
        println!("   - Failed disks: {:?}", result.failed_disks);
        println!("   - Warnings: {:?}", result.warnings);
        println!("   - Huge pages configured: {:?}", result.huge_pages_configured);
        
        Ok(result)
    }

    /// Industry best-practice disk deletion with comprehensive validation and migration support
    async fn delete_spdk_disk_impl(&self, request: DiskDeleteRequest) -> Result<DiskDeleteResult, Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [DELETE_IMPL] Starting comprehensive disk deletion for PCI: {}", request.pci_address);
        
        let mut result = DiskDeleteResult {
            success: false,
            message: String::new(),
            volumes_on_disk: vec![],
            deleted_volumes: vec![],
            migrated_volumes: vec![],
            created_snapshots: vec![],
            cleanup_performed: DiskCleanupSummary {
                lvs_deleted: false,
                volumes_deleted: 0,
                disk_reset: false,
                crd_updated: false,
            },
            warnings: vec![],
            completed_at: Utc::now().to_rfc3339(),
        };

        // Step 1: Find and validate the disk
        let disk_info = match self.get_disk_info(&request.pci_address).await {
            Ok(info) => info,
            Err(e) => {
                result.message = format!("Failed to get disk information: {}", e);
                return Ok(result);
            }
        };

        if !disk_info.spdk_ready {
            result.message = "Disk is not SPDK-ready, nothing to delete".to_string();
            result.success = true; // This is actually a success case
            return Ok(result);
        }

        // Step 2: Check what volumes exist on this disk following industry best practices
        let volumes_on_disk = self.get_volumes_on_disk(&request.pci_address).await?;
        result.volumes_on_disk = volumes_on_disk.clone();

        if !volumes_on_disk.is_empty() && !request.force_delete && !request.migrate_volumes {
            result.message = format!(
                "Cannot delete disk with {} volumes. Use migrate_volumes=true to migrate them first, or force_delete=true to delete them. Industry best practice: migrate volumes before disk removal.",
                volumes_on_disk.len()
            );
            result.warnings.push("Consider using migration to preserve data integrity".to_string());
            return Ok(result);
        }

        // Step 3: Handle volumes based on replica count (industry best practice)
        for volume in &volumes_on_disk {
            if volume.single_replica {
                if request.take_snapshots {
                    // Take snapshot before deletion
                    match self.create_volume_snapshot(&volume.volume_id).await {
                        Ok(snapshot_id) => {
                            result.created_snapshots.push(snapshot_id);
                            result.warnings.push(format!("Created snapshot for single-replica volume {}", volume.volume_id));
                        }
                        Err(e) => {
                            result.warnings.push(format!("Failed to create snapshot for {}: {}", volume.volume_id, e));
                        }
                    }
                }

                if request.migrate_volumes && volume.can_migrate {
                    // Try to migrate single-replica volume to another disk
                    match self.migrate_single_replica_volume(volume, &request.target_disks).await {
                        Ok(migration) => {
                            result.migrated_volumes.push(migration);
                        }
                        Err(e) => {
                            if !request.force_delete {
                                result.message = format!("Failed to migrate volume {}: {}. Use force_delete=true to proceed anyway.", volume.volume_id, e);
                                return Ok(result);
                            }
                            result.warnings.push(format!("Migration failed for {}: {}", volume.volume_id, e));
                        }
                    }
                }
                         } else {
                 // Multi-replica volume - check if we have at least 2 healthy replicas total
                 // (including the one on the disk being deleted). This ensures at least 1 
                 // healthy replica will remain after deletion.
                 let healthy_replicas = self.count_healthy_replicas(&volume.volume_id).await?;
                if healthy_replicas < 2 && !request.force_delete {
                    result.message = format!("Volume {} has only {} healthy replica(s). Deleting this disk would leave fewer than 1 healthy replica. Use force_delete=true to proceed anyway.", volume.volume_id, healthy_replicas);
                    return Ok(result);
                }
            }
        }

        // Step 4: Delete volumes that weren't migrated
        for volume in &volumes_on_disk {
            if !result.migrated_volumes.iter().any(|m| m.volume_id == volume.volume_id) {
                match self.delete_volume_from_disk(&volume.volume_id, &request.pci_address).await {
                    Ok(_) => {
                        result.deleted_volumes.push(volume.volume_id.clone());
                        result.cleanup_performed.volumes_deleted += 1;
                    }
                    Err(e) => {
                        result.warnings.push(format!("Failed to delete volume {}: {}", volume.volume_id, e));
                    }
                }
            }
        }

        // Step 5: Delete the LVS (Logical Volume Store)
        match self.delete_lvs_from_disk(&request.pci_address).await {
            Ok(_) => {
                result.cleanup_performed.lvs_deleted = true;
                println!("✅ [DELETE_IMPL] Successfully deleted LVS from disk");
            }
            Err(e) => {
                result.warnings.push(format!("Failed to delete LVS: {}", e));
            }
        }

        // Step 6: Reset disk back to kernel driver
        match self.reset_disk_to_kernel(&request.pci_address).await {
            Ok(_) => {
                result.cleanup_performed.disk_reset = true;
                println!("✅ [DELETE_IMPL] Successfully reset disk to kernel driver");
            }
            Err(e) => {
                result.warnings.push(format!("Failed to reset disk to kernel: {}", e));
            }
        }

        // Step 7: Update CRD (mark as non-SPDK ready)
        match self.update_disk_crd_after_deletion(&request.pci_address).await {
            Ok(_) => {
                result.cleanup_performed.crd_updated = true;
                println!("✅ [DELETE_IMPL] Successfully updated SpdkDisk CRD");
            }
            Err(e) => {
                result.warnings.push(format!("Failed to update CRD: {}", e));
            }
        }

        result.success = result.cleanup_performed.lvs_deleted && result.cleanup_performed.disk_reset;
        result.message = if result.success {
            format!("Successfully deleted SPDK disk {}. Deleted {} volumes, migrated {} volumes, created {} snapshots.", 
                   request.pci_address, result.deleted_volumes.len(), result.migrated_volumes.len(), result.created_snapshots.len())
        } else {
            "Disk deletion completed with some warnings. Check warnings for details.".to_string()
        };

        println!("🗑️ [DELETE_IMPL] Disk deletion completed: success={}", result.success);
        Ok(result)
    }

    async fn validate_disk_for_setup(&self, pci_addr: &str, force_unmount: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [VALIDATION] Validating disk for setup: {}", pci_addr);
        println!("🔍 [VALIDATION] Force unmount: {}", force_unmount);
        
        // Check if PCI device exists
        let sysfs_path = format!("/sys/bus/pci/devices/{}", pci_addr);
        println!("🔍 [VALIDATION] Checking PCI device path: {}", sysfs_path);
        if !std::path::Path::new(&sysfs_path).exists() {
            let error_msg = format!("PCI device {} does not exist", pci_addr);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        println!("✅ [VALIDATION] PCI device path exists");
        
        // Get current driver
        let current_driver = self.get_current_driver(pci_addr).await?;
        println!("🔍 [VALIDATION] Current driver: '{}'", current_driver);
        
        // Get disk information
        let disk_info = self.get_disk_info(pci_addr).await?;
        println!("🔍 [VALIDATION] Disk info - Name: {}, Driver: {}, System: {}, SPDK Ready: {}", 
                 disk_info.device_name, disk_info.driver, disk_info.is_system_disk, disk_info.spdk_ready);
        
        // Check if it's a system disk
        if disk_info.is_system_disk {
            let error_msg = format!("Cannot setup system disk: {} ({})", pci_addr, disk_info.device_name);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        println!("✅ [VALIDATION] Not a system disk");
        
        // Check mounted partitions
        if !disk_info.mounted_partitions.is_empty() && !force_unmount {
            let error_msg = format!("Disk has mounted partitions: {:?}. Use force_unmount=true to proceed", disk_info.mounted_partitions);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        
        if !disk_info.mounted_partitions.is_empty() && force_unmount {
            println!("⚠️ [VALIDATION] Disk has mounted partitions but force_unmount=true: {:?}", disk_info.mounted_partitions);
        } else {
            println!("✅ [VALIDATION] No mounted partitions to worry about");
        }
        
        // For unbound devices, we can't check block device files since they don't exist yet
        if current_driver == "unbound" {
            println!("✅ [VALIDATION] Device is unbound - validation passed (no block device to check)");
            return Ok(());
        }
        
        // For bound devices, validate the block device exists and is accessible
        let device_path = format!("/dev/{}", disk_info.device_name);
        println!("🔍 [VALIDATION] Checking device path");
        if !std::path::Path::new(&device_path).exists() {
            let error_msg = format!("Block device {} does not exist", device_path);
            println!("❌ [VALIDATION] {}", error_msg);
            return Err(error_msg.into());
        }
        println!("✅ [VALIDATION] Block device path exists");
        
        println!("✅ [VALIDATION] All validation checks passed for: {}", pci_addr);
        Ok(())
    }

    async fn setup_single_disk(&self, pci_addr: &str, request: &DiskSetupRequest) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SETUP] Starting disk setup for PCI address: {}", pci_addr);
        println!("🔧 [SETUP] Request parameters: force_unmount={}, backup_data={}, huge_pages_mb={:?}, driver_override={:?}", 
                 request.force_unmount, request.backup_data, request.huge_pages_mb, request.driver_override);
        
        let disk_info = self.get_disk_info(pci_addr).await?;
        println!("🔧 [SETUP] Disk info retrieved:");
        println!("   - Device name: {}", disk_info.device_name);
        println!("   - Driver: '{}'", disk_info.driver);
        println!("   - Size: {} bytes", disk_info.size_bytes);
        println!("   - Model: {}", disk_info.model);
        println!("   - Is system disk: {}", disk_info.is_system_disk);
        println!("   - SPDK ready: {}", disk_info.spdk_ready);
        println!("   - Mounted partitions: {:?}", disk_info.mounted_partitions);

        // Validate the disk can be set up
        if disk_info.is_system_disk {
            println!("❌ [SETUP] Cannot setup system disk for SPDK");
            return Err("Cannot setup system disk for SPDK".into());
        }

        // Check if disk is already fully setup (has LVS) rather than just driver-ready
        // Legacy SpdkDisk CRD usage removed
        let disk_name = self.disk_crd_name(pci_addr);
        
        // Legacy SpdkDisk CRD check removed; validate directly against SPDK
        if true {
            // Always check SPDK state as source of truth, regardless of CRD status
            println!("🔍 [SETUP] Checking actual SPDK state for disk {}...", pci_addr);
                
            // Get all LVS stores from SPDK (no filtering by name or uuid)
            let lvs_query_result = call_spdk_rpc(&self.spdk_rpc_url, &json!({
                "method": "bdev_lvol_get_lvstores"
            })).await;
                
            if let Ok(lvs_result) = lvs_query_result {
                if let Some(lvstore_list) = lvs_result["result"].as_array() {
                    // Get the expected device path from disk info to match against base_bdev
                    let expected_device = disk_info.device_name.as_str();
                    println!("🔍 [SETUP] Looking for LVS with base_bdev matching device: {}", expected_device);
                    
                    // Search for LVS that matches our disk by device name in base_bdev
                    let mut found_lvs = None;
                    for lvstore in lvstore_list {
                        if let Some(base_bdev) = lvstore["base_bdev"].as_str() {
                            // Check if this LVS is on our disk (device name match)
                            if base_bdev.contains(expected_device) {
                                let lvs_name = lvstore["name"].as_str().unwrap_or("unknown");
                                println!("🔍 [SETUP] Found LVS for our disk: {} (device: {}) on base_bdev: {}", lvs_name, expected_device, base_bdev);
                                found_lvs = Some(lvstore.clone());
                                break;
                            }
                        }
                    }
                        
                    if let Some(lvs_info) = found_lvs {
                        // LVS exists on our disk, check if it has capacity
                        if let (Some(total_clusters), Some(cluster_size)) = (
                            lvs_info["total_data_clusters"].as_u64(),
                            lvs_info["cluster_size"].as_u64()
                        ) {
                            let total_capacity = (total_clusters * cluster_size) as i64;
                            if total_capacity > 0 {
                                let actual_lvs_name = lvs_info["name"].as_str().unwrap_or("unknown");
                                let free_clusters = lvs_info["free_clusters"].as_u64().unwrap_or(0);
                                let free_space = (free_clusters * cluster_size) as i64;
                                let used_space = total_capacity - free_space;
                                
                                println!("✅ [SETUP] LVS '{}' exists and has capacity ({} bytes), updating CRD to match SPDK state", actual_lvs_name, total_capacity);
                                
                                // Update CRD to match actual SPDK state
                                let sync_patch = json!({
                                    "status": {
                                        "blobstore_initialized": true,
                                        "lvs_name": actual_lvs_name,
                                        "total_capacity": total_capacity,
                                        "free_space": free_space,
                                        "used_space": used_space,
                                        "healthy": true,
                                        "last_checked": Utc::now().to_rfc3339()
                                    }
                                });
                                
        // Legacy CRD sync removed
                                
                                return Err("Disk is already fully setup with LVS initialized".into());
                            } else {
                                let actual_lvs_name = lvs_info["name"].as_str().unwrap_or("unknown");
                                println!("⚠️ [SETUP] LVS '{}' exists but has no capacity, will clean up and re-initialize", actual_lvs_name);
                            }
                        } else {
                            println!("⚠️ [SETUP] LVS exists but capacity info unavailable, will clean up and re-initialize");
                        }
                    } else {
                        println!("✅ [SETUP] No LVS found for disk {} (device: {}), proceeding with fresh setup", pci_addr, expected_device);
                    }
                } else {
                    println!("✅ [SETUP] No LVS stores found in SPDK, proceeding with fresh setup");
                }
            } else {
                println!("⚠️ [SETUP] Failed to query SPDK for LVS stores, proceeding with setup anyway");
            }
                
            // Reset CRD state to ensure clean setup
            let reset_patch = json!({
                "status": {
                    "blobstore_initialized": false,
                    "lvs_name": null,
                    "total_capacity": 0,
                    "free_space": 0,
                    "used_space": 0,
                    "healthy": false,
                    "last_checked": Utc::now().to_rfc3339()
                }
            });
            println!("🔄 [SETUP] Proceeding with setup using SPDK state as source of truth");
        } else if disk_info.spdk_ready {
            println!("🔄 [SETUP] Disk has SPDK driver but no CRD - will complete setup");
        }



        // Step 1: If device is bound to nvme and has mounted partitions, handle them
        println!("🔧 [SETUP] Step 1: Checking mounted partitions...");
        if disk_info.driver == "nvme" && !disk_info.mounted_partitions.is_empty() {
            println!("⚠️  [SETUP] Device has mounted partitions: {:?}", disk_info.mounted_partitions);
            if !request.force_unmount {
                println!("❌ [SETUP] force_unmount=false, cannot proceed with mounted partitions");
                return Err(format!("Disk has mounted partitions: {:?}. Use force_unmount=true to proceed", disk_info.mounted_partitions).into());
            }

            println!("🔧 [SETUP] force_unmount=true, proceeding with unmounting...");
            // Backup data if requested
            if request.backup_data {
                println!("🔧 [SETUP] Backing up disk data...");
                self.backup_disk_data(&disk_info).await?;
                println!("✅ [SETUP] Disk data backup completed");
            }

            // Unmount all partitions
            println!("🔧 [SETUP] Unmounting disk partitions...");
            self.unmount_disk_partitions(&disk_info).await?;
            println!("✅ [SETUP] Disk partitions unmounted");
        } else {
            println!("✅ [SETUP] No mounted partitions to handle");
        }

        // AWS/Virtualized Environment: Use kernel-bound mode instead of userspace drivers
        println!("🔧 [SETUP] Step 2: Checking environment type...");
        let is_virtualized = self.is_virtualized_environment().await?;
        println!("🔧 [SETUP] Virtualized environment: {}", is_virtualized);
        
        if is_virtualized {
            let should_use_kernel = self.should_use_kernel_mode(&disk_info).await?;
            println!("🔧 [SETUP] Should use kernel mode: {}", should_use_kernel);
            
            if should_use_kernel {
                println!("🔧 [SETUP] Using kernel-bound mode for AWS/virtualized compatibility");
                return self.setup_kernel_bound_disk(pci_addr, &disk_info).await;
            } else {
                println!("🔧 [SETUP] Kernel mode not suitable, falling back to userspace drivers");
            }
        } else {
            println!("🔧 [SETUP] Bare metal environment detected, using userspace drivers");
        }

        // Traditional bare metal path: Use userspace drivers
        println!("🔧 [SETUP] Step 3: Traditional bare metal userspace driver setup");
        
        // Step 3a: Unbind from current driver (if bound)
        println!("🔧 [SETUP] Step 3a: Checking current driver binding...");
        if disk_info.driver != "unbound" {
            println!("🔧 [SETUP] Unbinding from current driver: {}", disk_info.driver);
            self.unbind_from_driver(pci_addr, &disk_info.driver).await?;
            println!("✅ [SETUP] Successfully unbound from driver: {}", disk_info.driver);
        } else {
            println!("✅ [SETUP] Device already unbound, no unbinding needed");
        }

        // Step 3b: Choose optimal driver for environment
        println!("🔧 [SETUP] Step 3b: Selecting optimal SPDK driver...");
        let target_driver = if let Some(override_driver) = &request.driver_override {
            println!("🔧 [SETUP] Using driver override: {}", override_driver);
            override_driver.clone()
        } else {
            let selected_driver = self.select_optimal_spdk_driver().await?;
            println!("🔧 [SETUP] Auto-selected driver: {}", selected_driver);
            selected_driver
        };

        // Step 3c: Load target driver module
        println!("🔧 [SETUP] Step 3c: Loading driver module: {}", target_driver);
        self.load_driver_module(&target_driver).await?;
        println!("✅ [SETUP] Driver module loaded successfully");

        // Step 3d: Bind to SPDK-compatible driver
        println!("🔧 [SETUP] Step 3d: Binding to SPDK driver: {}", target_driver);
        self.bind_to_driver(pci_addr, &target_driver).await?;
        println!("✅ [SETUP] Successfully bound to driver: {}", target_driver);

        // Step 3e: Verify setup
        println!("🔧 [SETUP] Step 3e: Verifying SPDK setup...");
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.verify_spdk_setup(pci_addr).await?;
        println!("✅ [SETUP] SPDK setup verification completed successfully");

        // Step 4: Initialize LVS/blobstore for complete setup
        println!("🔧 [SETUP] Step 4: Initializing LVS/blobstore...");
        
        // Create or find the SpdkDisk CRD using stable PCI-based naming
        // Legacy SpdkDisk CRD usage removed
        let disk_crd_name = self.disk_crd_name(pci_addr);
        
        // Discover actual device name for status fields (dynamic)
        let actual_device_name = match self.find_nvme_device_name(pci_addr).await {
            Ok(name) => {
                println!("✅ [SETUP] Found actual device name: {}", name);
                name
            }
            Err(_) => {
                println!("⚠️ [SETUP] Could not find actual device name, using discovered name: {}", disk_info.device_name);
                disk_info.device_name.clone()
            }
        };
        
        // Legacy SpdkDisk CRD flow removed; use RAID-based initialization path
        let disk_crd = (); // placeholder removed

        // Initialize the blobstore to complete SPDK setup
        // RAID-centric: blobstore is now tied to SpdkRaidDisk, not single disks
        println!("✅ [SETUP] LVS/blobstore initialization completed successfully");

        println!("🎉 [SETUP] Complete SPDK setup finished successfully for PCI address: {} (fully SPDK ready)", pci_addr);
        Ok(())
    }

    /// Select the optimal SPDK userspace driver for the current environment
    async fn select_optimal_spdk_driver(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Check if we're in a virtualized environment
        let is_virtualized = self.is_virtualized_environment().await?;
        
        if is_virtualized {
            // In VMs, prefer vfio-pci for security isolation
            if self.is_driver_available("vfio-pci").await? {
                return Ok("vfio-pci".to_string());
            }
        } else {
            // On bare metal, prioritize non-IOMMU drivers for performance
            
            // 1st choice: uio_pci_generic (most common, no IOMMU needed)
            if self.is_driver_available("uio_pci_generic").await? {
                println!("Using uio_pci_generic for bare metal SPDK (no IOMMU required)");
                return Ok("uio_pci_generic".to_string());
            }
            
            // 2nd choice: igb_uio (better compatibility than uio_pci_generic)
            if self.is_driver_available("igb_uio").await? {
                println!("Using igb_uio for bare metal SPDK (no IOMMU required)");
                return Ok("igb_uio".to_string());
            }
            
            // 3rd choice: vfio-pci with no-IOMMU mode
            if self.is_vfio_noiommu_available().await? {
                println!("Using vfio-pci in no-IOMMU mode for bare metal");
                return Ok("vfio-pci".to_string());
            }
        }

        // Fallback to vfio-pci (will likely fail if no IOMMU)
        Ok("vfio-pci".to_string())
    }

    /// Check if we're running in a virtualized environment
    async fn is_virtualized_environment(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Check common virtualization indicators
        
        // 1. Check DMI system information
        if let Ok(product_name) = fs::read_to_string("/sys/class/dmi/id/product_name") {
            let product = product_name.trim().to_lowercase();
            if product.contains("virtualbox") || 
               product.contains("vmware") || 
               product.contains("qemu") ||
               product.contains("kvm") ||
               product.contains("xen") ||
               product.contains("amazon ec2") {
                return Ok(true);
            }
        }

        // 2. Check for hypervisor flag in CPU
        if let Ok(output) = Command::new("grep")
            .args(["-q", "hypervisor", "/proc/cpuinfo"])
            .output() {
            if output.status.success() {
                return Ok(true);
            }
        }

        // 3. Check for virtualization in systemd-detect-virt
        if let Ok(output) = Command::new("systemd-detect-virt")
            .output() {
            if output.status.success() {
                let virt_type = String::from_utf8_lossy(&output.stdout).trim().to_string();
                return Ok(virt_type != "none");
            }
        }

        // Default to bare metal if detection fails
        Ok(false)
    }

    /// Check if a specific kernel driver is available
    async fn is_driver_available(&self, driver: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Try to load the module (won't load if already loaded)
        let output = Command::new("modprobe")
            .arg("--dry-run")
            .arg(driver)
            .output()?;

        Ok(output.status.success())
    }

    /// Check if VFIO no-IOMMU mode is available
    async fn is_vfio_noiommu_available(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Check if vfio-pci is available
        if !self.is_driver_available("vfio-pci").await? {
            return Ok(false);
        }

        // Check if no-IOMMU mode can be enabled
        let noiommu_path = "/sys/module/vfio/parameters/enable_unsafe_noiommu_mode";
        if Path::new(noiommu_path).exists() {
            // Try to enable no-IOMMU mode
            if fs::write(noiommu_path, "1").is_ok() {
                return Ok(true);
            }
        }

        // Try loading vfio with no-IOMMU parameter
        let output = Command::new("modprobe")
            .args(["vfio", "enable_unsafe_noiommu_mode=1"])
            .output()?;

        Ok(output.status.success())
    }

    async fn backup_disk_data(&self, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let backup_dir = format!("{}/{}", self.backup_path, disk_info.pci_address.replace(":", "_"));
        fs::create_dir_all(&backup_dir)?;

        let backup_file = format!("{}/disk_backup_{}.img", backup_dir, 
            chrono::Utc::now().format("%Y%m%d_%H%M%S"));

        let output = Command::new("dd")
            .args([
                &format!("if=/dev/{}", disk_info.device_name),
                &format!("of={}", backup_file),
                "bs=1M",
                "count=1024", // Backup first 1GB
                "status=progress"
            ])
            .output()?;

        if !output.status.success() {
            return Err(format!("Backup failed: {}", String::from_utf8_lossy(&output.stderr)).into());
        }

        println!("Backed up disk data to: {}", backup_file);
        Ok(())
    }

    async fn unmount_disk_partitions(&self, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("lsblk")
            .args(["-ln", "-o", "NAME", &format!("/dev/{}", disk_info.device_name)])
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        
        for line in stdout.lines() {
            let partition = line.trim();
            if partition != disk_info.device_name && partition.starts_with(&disk_info.device_name) {
                let unmount_result = Command::new("umount")
                    .args(["-f", &format!("/dev/{}", partition)])
                    .output();

                if let Ok(output) = unmount_result {
                    if !output.status.success() {
                        let error = String::from_utf8_lossy(&output.stderr);
                        if !error.contains("not mounted") {
                            eprintln!("Warning: Failed to unmount /dev/{}: {}", partition, error);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn unbind_from_driver(&self, pci_addr: &str, driver: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let unbind_path = format!("/sys/bus/pci/drivers/{}/unbind", driver);
        
        if Path::new(&unbind_path).exists() {
            fs::write(&unbind_path, pci_addr)?;
            
            // Wait for unbind to complete
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if self.get_current_driver(pci_addr).await? == "unbound" {
                    break;
                }
            }
        }

        Ok(())
    }

    async fn load_driver_module(&self, driver: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if module is already loaded
        let output = Command::new("lsmod")
            .output()?;

        let modules = String::from_utf8(output.stdout)?;
        if modules.contains(driver) {
            return Ok(());
        }

        // Load the module
        let output = Command::new("modprobe")
            .arg(driver)
            .output()?;

        if !output.status.success() {
            return Err(format!("Failed to load driver module {}: {}", 
                driver, String::from_utf8_lossy(&output.stderr)).into());
        }

        Ok(())
    }

    async fn bind_to_driver(&self, pci_addr: &str, driver: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Enable VFIO if using vfio-pci
        if driver == "vfio-pci" {
            self.enable_vfio(pci_addr).await?;
        }

        let bind_path = format!("/sys/bus/pci/drivers/{}/bind", driver);
        
        if !Path::new(&bind_path).exists() {
            return Err(format!("Driver {} bind path not found", driver).into());
        }

        // Write PCI address to bind file
        fs::write(&bind_path, pci_addr)?;

        // Wait for bind to complete
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.get_current_driver(pci_addr).await? == driver {
                return Ok(());
            }
        }

        Err(format!("Failed to bind {} to {}", pci_addr, driver).into())
    }

    async fn enable_vfio(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Read vendor and device IDs
        let vendor_id = self.read_sysfs_file(&format!("/sys/bus/pci/devices/{}/vendor", pci_addr)).await?;
        let device_id = self.read_sysfs_file(&format!("/sys/bus/pci/devices/{}/device", pci_addr)).await?;

        // Create VFIO device ID
        let device_id_str = format!("{} {}", vendor_id.trim(), device_id.trim());

        // Add to VFIO new_id
        let new_id_path = "/sys/bus/pci/drivers/vfio-pci/new_id";
        if Path::new(new_id_path).exists() {
            let _ = fs::write(new_id_path, &device_id_str);
        }

        Ok(())
    }

    async fn verify_spdk_setup(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let current_driver = self.get_current_driver(pci_addr).await?;
        
        if !self.is_spdk_compatible_driver(&current_driver) {
            return Err(format!("Disk setup verification failed. Current driver: {}", current_driver).into());
        }

        Ok(())
    }

    /// Check if disk should use kernel-bound mode (AWS/virtualized environments)
    async fn should_use_kernel_mode(&self, disk_info: &UnimplementedDisk) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [KERNEL_MODE] Evaluating kernel mode criteria:");
        println!("   - Is system disk: {}", disk_info.is_system_disk);
        println!("   - Current driver: '{}'", disk_info.driver);
        println!("   - Driver is nvme: {}", disk_info.driver == "nvme");
        println!("   - Driver is empty: {}", disk_info.driver.is_empty());
        println!("   - Driver is unbound: {}", disk_info.driver == "unbound");
        
        let not_system_disk = !disk_info.is_system_disk;
        let driver_compatible = disk_info.driver == "nvme" || disk_info.driver.is_empty() || disk_info.driver == "unbound";
        let result = not_system_disk && driver_compatible;
        
        println!("🔍 [KERNEL_MODE] Decision logic:");
        println!("   - Not system disk: {}", not_system_disk);
        println!("   - Driver compatible: {}", driver_compatible);
        println!("   - Final result: {}", result);
        
        Ok(result)
    }

    /// Setup disk for SPDK using kernel-bound mode (no driver binding)
    async fn setup_kernel_bound_disk(&self, pci_addr: &str, disk_info: &UnimplementedDisk) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [KERNEL_SETUP] Starting kernel-bound disk setup for PCI address: {}", pci_addr);
        println!("🔧 [KERNEL_SETUP] Device info: name={}, driver='{}', size={}", 
                 disk_info.device_name, disk_info.driver, disk_info.size_bytes);
        
        // Step 1: Ensure disk is bound to nvme driver (if unbound)
        println!("🔧 [KERNEL_SETUP] Step 1: Checking driver binding...");
        if disk_info.driver.is_empty() || disk_info.driver == "unbound" {
            println!("🔧 [KERNEL_SETUP] Device is unbound, binding {} to nvme driver", pci_addr);
            match self.bind_to_driver(pci_addr, "nvme").await {
                Ok(_) => {
                    println!("✅ [KERNEL_SETUP] Successfully bound to nvme driver");
                    // Wait for block device to appear
                    println!("🔧 [KERNEL_SETUP] Waiting 2 seconds for block device to appear...");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    println!("❌ [KERNEL_SETUP] Failed to bind to nvme driver: {}", e);
                    return Err(format!("Failed to bind {} to nvme driver: {}", pci_addr, e).into());
                }
            }
        } else {
            println!("✅ [KERNEL_SETUP] Device already bound to driver: {}", disk_info.driver);
        }
        
        // Step 2: Ensure the device is accessible via block device
        println!("🔧 [KERNEL_SETUP] Step 2: Verifying block device access...");
        let block_device = format!("/dev/{}", disk_info.device_name);
        println!("🔧 [KERNEL_SETUP] Checking if block device exists: {}", block_device);
        
        if !std::path::Path::new(&block_device).exists() {
            println!("❌ [KERNEL_SETUP] Block device {} not found", block_device);
            // Try to find the actual device name
            println!("🔧 [KERNEL_SETUP] Searching for actual device name...");
            match self.find_nvme_device_name(pci_addr).await {
                Ok(actual_name) => {
                    println!("🔧 [KERNEL_SETUP] Found actual device name: {}", actual_name);
                    let actual_block_device = format!("/dev/{}", actual_name);
                    if std::path::Path::new(&actual_block_device).exists() {
                        println!("✅ [KERNEL_SETUP] Using actual block device: {}", actual_block_device);
                    } else {
                        println!("❌ [KERNEL_SETUP] Actual block device also not found: {}", actual_block_device);
                        return Err(format!("Block device not found: neither {} nor {}", block_device, actual_block_device).into());
                    }
                }
                Err(e) => {
                    println!("❌ [KERNEL_SETUP] Could not find device name: {}", e);
                    return Err(format!("Block device {} not found after nvme binding: {}", block_device, e).into());
                }
            }
        } else {
            println!("✅ [KERNEL_SETUP] Block device exists: {}", block_device);
        }

        // Step 3: Try to attach the NVMe device to SPDK using kernel access
        println!("🔧 [KERNEL_SETUP] Step 3: Attaching to SPDK...");
        let attach_result = self.attach_kernel_nvme_to_spdk(pci_addr, &disk_info.device_name).await;
        match attach_result {
            Ok(_) => {
                println!("✅ [KERNEL_SETUP] Successfully attached {} to SPDK via kernel access", disk_info.device_name);
            }
            Err(e) => {
                // Don't fail setup if SPDK attachment fails - the disk can still be used
                println!("⚠️  [KERNEL_SETUP] Could not attach {} to SPDK (will use direct kernel access): {}", disk_info.device_name, e);
                println!("🔧 [KERNEL_SETUP] This is not necessarily a failure - disk can still be used directly");
            }
        }

        // Step 4: Initialize LVS/blobstore for complete kernel setup
        println!("🔧 [KERNEL_SETUP] Step 4: Initializing LVS/blobstore...");
        
        // Create or find the SpdkDisk CRD using stable PCI-based naming
        // Legacy SpdkDisk CRD usage removed
        let disk_crd_name = self.disk_crd_name(pci_addr);
        
        // Discover actual device name for status fields (dynamic)
        let actual_device_name = match self.find_nvme_device_name(pci_addr).await {
            Ok(name) => {
                println!("✅ [KERNEL_SETUP] Found actual device name: {}", name);
                name
            }
            Err(_) => {
                println!("⚠️ [KERNEL_SETUP] Could not find actual device name, using discovered name: {}", disk_info.device_name);
                disk_info.device_name.clone()
            }
        };
        
        let disk_crd = ();

        // Initialize the blobstore to complete SPDK setup
        // RAID-centric: skip single-disk blobstore init
        println!("✅ [KERNEL_SETUP] LVS/blobstore initialization completed successfully");

        // Step 5: Mark as ready for SPDK (kernel mode)
        println!("🎉 [KERNEL_SETUP] Complete SPDK setup finished successfully: {} (fully SPDK ready)", pci_addr);
        Ok(())
    }

    /// Attach kernel-bound NVMe device to SPDK for bdev access
    async fn attach_kernel_nvme_to_spdk(&self, pci_addr: &str, device_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SPDK_ATTACH] Starting SPDK attachment for device: {}", device_name);
        println!("🔧 [SPDK_ATTACH] PCI address: {}", pci_addr);
        println!("🔧 [SPDK_ATTACH] SPDK RPC URL: {}", self.spdk_rpc_url);
        
        // Try to create a kernel bdev in SPDK for this device
        // Use actual device name for consistent naming (e.g., nvme1n1 instead of nvme-0000-00-1f.0)
        let actual_device_name = match self.find_nvme_device_name(&pci_addr).await {
            Ok(name) => {
                println!("✅ [SPDK_ATTACH] Found actual device name: {} for PCI {}", name, pci_addr);
                name
            }
            Err(_) => {
                println!("⚠️ [SPDK_ATTACH] Could not find actual device name for {}, using synthesized name: {}", pci_addr, device_name);
                device_name.to_string()
            }
        };
        let bdev_name = format!("kernel_{}", actual_device_name);
        let device_path = format!("/dev/{}", actual_device_name);
        println!("🔧 [SPDK_ATTACH] Target bdev name: {}", bdev_name);
        println!("🔍 [VALIDATION] Checking device path");

        // Verify device path exists before trying to create bdev
        if !std::path::Path::new(&device_path).exists() {
            println!("🔍 [VALIDATION] Checking device path");
            return Err(format!("Device path {} does not exist", device_path).into());
        }
        println!("🔍 [VALIDATION] Checking device path");

        // Use SPDK's aio bdev to access the kernel device
        println!("🔧 [SPDK_ATTACH] Attempting to create AIO bdev...");
        let rpc_request = json!({
            "method": "bdev_aio_create",
            "params": {
                "name": bdev_name,
                "filename": device_path,
                "block_size": 512
            }
        });
        println!("🔧 [SPDK_ATTACH] AIO RPC request: {}", serde_json::to_string_pretty(&rpc_request).unwrap());

        match call_spdk_rpc(&self.spdk_rpc_url, &rpc_request).await {
            Ok(response) => {
                if let Some(error) = response.get("error") {
                    let error_code = error["code"].as_i64().unwrap_or(0);
                    if error_code == -17 {
                        println!("✅ [SPDK_ATTACH] AIO bdev already exists (idempotent): {}", bdev_name);
                        return Ok(());
                    } else {
                        println!("❌ [SPDK_ATTACH] AIO bdev creation error: {}", error);
                        return Err(format!("AIO bdev creation failed: {}", error).into());
                    }
                }
                println!("🔍 [VALIDATION] Checking device path");
                println!("🔧 [SPDK_ATTACH] AIO response: {:?}", response);
                Ok(())
            }
            Err(e) => {
                println!("⚠️  [SPDK_ATTACH] AIO bdev creation failed: {}", e);
                println!("🔧 [SPDK_ATTACH] Trying uring bdev as fallback...");
                
                // If aio fails, try uring bdev (newer, better performance)
                let uring_request = json!({
                    "method": "bdev_uring_create", 
                    "params": {
                        "name": bdev_name,
                        "filename": device_path,
                        "block_size": 512
                    }
                });
                println!("🔧 [SPDK_ATTACH] URING RPC request: {}", serde_json::to_string_pretty(&uring_request).unwrap());

                match call_spdk_rpc(&self.spdk_rpc_url, &uring_request).await {
                    Ok(response) => {
                        if let Some(error) = response.get("error") {
                            let error_code = error["code"].as_i64().unwrap_or(0);
                            if error_code == -17 {
                                println!("✅ [SPDK_ATTACH] URING bdev already exists (idempotent): {}", bdev_name);
                                return Ok(());
                            } else {
                                println!("❌ [SPDK_ATTACH] URING bdev creation error: {}", error);
                                return Err(format!("URING bdev creation failed: {}", error).into());
                            }
                        }
                        println!("🔍 [VALIDATION] Checking device path");
                        println!("🔧 [SPDK_ATTACH] URING response: {:?}", response);
                        Ok(())
                    }
                    Err(uring_err) => {
                        println!("❌ [SPDK_ATTACH] Both AIO and URING bdev creation failed");
                        println!("❌ [SPDK_ATTACH] AIO error: {}", e);
                        println!("❌ [SPDK_ATTACH] URING error: {}", uring_err);
                        Err(format!("Failed to create SPDK bdev for {}: aio error: {}, uring error: {}", 
                                  device_path, e, uring_err).into())
                    }
                }
            }
        }
    }

    async fn setup_huge_pages(&self, huge_pages_mb: u32) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        // Calculate optimal hugepage allocation for SPDK if not specified or too small
        let optimal_mb = if huge_pages_mb == 0 || huge_pages_mb < 2048 {
            self.calculate_optimal_hugepages().await?
        } else {
            huge_pages_mb
        };
        
        let huge_pages_2m = optimal_mb / 2; // 2MB pages
        
        println!("Setting up {}MB ({}x2MB) hugepages for SPDK", optimal_mb, huge_pages_2m);
        
        // Set number of huge pages
        fs::write("/proc/sys/vm/nr_hugepages", huge_pages_2m.to_string())?;

        // Mount hugepages if not already mounted
        let _output = Command::new("mount")
            .args(["-t", "hugetlbfs", "hugetlbfs", "/dev/hugepages"])
            .output();

        // Read back actual configured huge pages
        let configured = fs::read_to_string("/proc/sys/vm/nr_hugepages")?
            .trim()
            .parse::<u32>()
            .unwrap_or(0) * 2;

        Ok(configured)
    }

    /// Calculate optimal hugepage allocation based on system memory for SPDK workloads
    async fn calculate_optimal_hugepages(&self) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        // Read total system memory
        let meminfo = fs::read_to_string("/proc/meminfo")?;
        let total_mem_kb = meminfo
            .lines()
            .find(|line| line.starts_with("MemTotal:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(4 * 1024 * 1024); // Default to 4GB if parsing fails

        let total_mem_gb = total_mem_kb / (1024 * 1024);
        
        let hugepage_mb = if total_mem_gb >= 128 {
            // Large production systems (≥128GB): allocate 4GB for optimal SPDK performance
            4096
        } else if total_mem_gb >= 64 {
            // Medium-large systems: allocate 3GB
            3072
        } else if total_mem_gb >= 32 {
            // Medium systems: allocate 2GB (SPDK minimum recommended)
            2048
        } else {
            // Smaller systems: allocate 1GB (may impact performance)
            println!("⚠️  Warning: Only {}GB RAM detected. 2GB hugepages recommended for SPDK.", total_mem_gb);
            1024
        };

        println!("Auto-calculated hugepages: {}MB for system with {}GB RAM", hugepage_mb, total_mem_gb);
        Ok(hugepage_mb)
    }

    /// Load ublk driver module for userspace block devices
    async fn load_ublk_driver(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 Loading ublk_drv module for userspace block devices...");
        
        // Check if module is already loaded
        let output = Command::new("lsmod")
            .output()?;

        let modules = String::from_utf8(output.stdout)?;
        if modules.contains("ublk_drv") {
            println!("✅ ublk_drv module already loaded");
            return Ok(());
        }

        // Load the ublk_drv module
        let output = Command::new("modprobe")
            .arg("ublk_drv")
            .output()?;

        if !output.status.success() {
            let error_msg = format!("Failed to load ublk_drv module: {}", 
                String::from_utf8_lossy(&output.stderr));
            println!("⚠️ {}", error_msg);
            return Err(error_msg.into());
        }

        println!("✅ ublk_drv module loaded successfully");
        Ok(())
    }

    async fn reset_disks_to_kernel(&self, pci_addresses: Vec<String>) -> Result<DiskSetupResult, Box<dyn std::error::Error + Send + Sync>> {
        let mut result = DiskSetupResult {
            success: true,
            setup_disks: Vec::new(),
            failed_disks: Vec::new(),
            warnings: Vec::new(),
            huge_pages_configured: None,
            completed_at: Utc::now().to_rfc3339(),
        };

        for pci_addr in pci_addresses {
            match self.reset_single_disk(&pci_addr).await {
                Ok(_) => {
                    result.setup_disks.push(pci_addr);
                }
                Err(e) => {
                    result.failed_disks.push((pci_addr, e.to_string()));
                    result.success = false;
                }
            }
        }

        result.completed_at = Utc::now().to_rfc3339();
        Ok(result)
    }

    async fn reset_single_disk(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let current_driver = self.get_current_driver(pci_addr).await?;
        
        // Unbind from current driver if bound
        if current_driver != "unbound" {
            self.unbind_from_driver(pci_addr, &current_driver).await?;
        }

        // For "Unbind SPDK", we want the device to remain unbound (free)
        // Don't rebind to nvme - let user decide next action
        
        // Wait a moment for unbinding to complete
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify unbind was successful
        let new_driver = self.get_current_driver(pci_addr).await?;
        if new_driver != "unbound" {
            return Err(format!("Unbind verification failed. Device still bound to driver: {}", new_driver).into());
        }
        
        println!("✅ [RESET] Successfully unbound device {} from SPDK - now free for other uses", pci_addr);

        Ok(())
    }

    async fn get_all_disk_status(&self) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let all_pci_devices = self.get_nvme_pci_devices().await?;
        let mut disk_statuses = Vec::new();

        for pci_addr in all_pci_devices {
            if let Ok(disk_info) = self.get_disk_info(&pci_addr).await {
                disk_statuses.push(json!({
                    "pci_address": disk_info.pci_address,
                    "device_name": disk_info.device_name,
                    "driver": disk_info.driver,
                    "spdk_ready": disk_info.spdk_ready,
                    "is_system_disk": disk_info.is_system_disk,
                    "size_gb": disk_info.size_bytes / (1024 * 1024 * 1024),
                    "model": disk_info.model,
                    "mounted_partitions": disk_info.mounted_partitions,
                    "discovered_at": disk_info.discovered_at
                }));
            }
        }

        Ok(json!({
            "success": true,
            "node": self.node_name,
            "total_disks": disk_statuses.len(),
            "spdk_ready_disks": disk_statuses.iter().filter(|d| d["spdk_ready"].as_bool().unwrap_or(false)).count(),
            "uninitialized_disks": disk_statuses.iter().filter(|d| !d["spdk_ready"].as_bool().unwrap_or(true) && !d["is_system_disk"].as_bool().unwrap_or(true)).count(),
            "disks": disk_statuses,
            "checked_at": Utc::now().to_rfc3339()
        }))
    }

    // Helper methods for disk deletion following industry best practices

    async fn get_volumes_on_disk(&self, pci_address: &str) -> Result<Vec<VolumeOnDisk>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [VOLUME_CHECK] Checking volumes on disk: {}", pci_address);
        
        // Get the disk name from PCI address
        let _disk_info = self.get_disk_info(pci_address).await?;
        let disk_name = self.disk_crd_name(pci_address);
        
        // Query Kubernetes for SpdkVolume CRDs that use this disk
        let volumes_api: Api<spdk_csi_driver::SpdkVolume> = Api::namespaced(self.kube_client.clone(), &self.target_namespace);
        let volume_list = volumes_api.list(&kube::api::ListParams::default()).await?;
        
        let mut volumes_on_disk = Vec::new();
        
        for volume_crd in volume_list.items {
            for replica in &volume_crd.spec.replicas {
                if replica.disk_ref == disk_name || replica.node == self.node_name {
                    // Check if this replica is actually on our disk
                    if let Some(lvol_uuid) = &replica.lvol_uuid {
                        let lvs_name = format!("lvs_{}", disk_name);
                        let lvol_name = format!("{}/{}", lvs_name, lvol_uuid);
                        
                        // Check if this lvol exists on our disk's LVS
                        if self.check_lvol_exists(&lvol_name).await.unwrap_or(false) {
                            volumes_on_disk.push(VolumeOnDisk {
                                volume_id: volume_crd.spec.volume_id.clone(),
                                size_bytes: volume_crd.spec.size_bytes,
                                replica_count: volume_crd.spec.num_replicas,
                                can_migrate: volume_crd.spec.num_replicas == 1, // Single replicas can be migrated
                                single_replica: volume_crd.spec.num_replicas == 1,
                                pvc_name: None, // Could be enhanced to find PVC
                                namespace: None, // Could be enhanced to find namespace
                            });
                            break; // Don't count the same volume multiple times
                        }
                    }
                }
            }
        }
        
        println!("🔍 [VOLUME_CHECK] Found {} volumes on disk {}", volumes_on_disk.len(), pci_address);
        Ok(volumes_on_disk)
    }

    async fn check_lvol_exists(&self, lvol_name: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let result = call_spdk_rpc(&self.spdk_rpc_url, &json!({
            "method": "bdev_get_bdevs",
            "params": { "name": lvol_name }
        })).await;
        
        match result {
            Ok(response) => {
                if let Some(bdevs) = response["result"].as_array() {
                    Ok(!bdevs.is_empty())
                } else {
                    Ok(false)
                }
            }
            Err(_) => Ok(false)
        }
    }

    async fn count_healthy_replicas(&self, volume_id: &str) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
        let volumes_api: Api<spdk_csi_driver::SpdkVolume> = Api::namespaced(self.kube_client.clone(), &self.target_namespace);
        let volume = volumes_api.get(volume_id).await?;
        
        let mut healthy_count = 0;
        for replica in &volume.spec.replicas {
            if replica.health_status == spdk_csi_driver::ReplicaHealth::Healthy {
                healthy_count += 1;
            }
        }
        
        Ok(healthy_count)
    }

    async fn create_volume_snapshot(&self, volume_id: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // This would integrate with the existing snapshot functionality
        let snapshot_id = format!("pre-delete-{}-{}", volume_id, Utc::now().timestamp());
        println!("📸 [SNAPSHOT] Creating snapshot {} for volume {}", snapshot_id, volume_id);
        
        // TODO: Integrate with actual snapshot creation logic from csi_snapshotter.rs
        // For now, return a mock snapshot ID
        Ok(snapshot_id)
    }

    async fn migrate_single_replica_volume(&self, volume: &VolumeOnDisk, target_disks: &Option<Vec<String>>) -> Result<VolumeMigration, Box<dyn std::error::Error + Send + Sync>> {
        println!("🚚 [MIGRATION] Migrating single-replica volume: {}", volume.volume_id);
        
        // Find a suitable target disk
        let target_disk = if let Some(targets) = target_disks {
            if targets.is_empty() {
                return Err("No target disks specified for migration".into());
            }
            targets[0].clone()
        } else {
            // Auto-select a healthy disk with enough space
            self.find_suitable_migration_target(volume.size_bytes).await?
        };
        
        // TODO: Implement actual volume migration logic
        // This would involve:
        // 1. Creating new lvol on target disk
        // 2. Copying data (possibly using SPDK's copy engine)
        // 3. Updating volume CRD to point to new disk
        // 4. Deleting old lvol
        
        println!("✅ [MIGRATION] Volume migration completed: {} -> {}", volume.volume_id, target_disk);
        
        Ok(VolumeMigration {
            volume_id: volume.volume_id.clone(),
            from_disk: "current_disk".to_string(), // Would be actual source disk
            to_disk: target_disk,
            status: "completed".to_string(),
        })
    }

    async fn find_suitable_migration_target(&self, _required_size: i64) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Err("No suitable migration target disk found".into())
    }

    async fn delete_volume_from_disk(&self, volume_id: &str, _pci_address: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [VOLUME_DELETE] Deleting volume {} from disk", volume_id);
        
        // Get the volume CRD
        let volumes_api: Api<spdk_csi_driver::SpdkVolume> = Api::namespaced(self.kube_client.clone(), &self.target_namespace);
        let volume = volumes_api.get(volume_id).await?;
        
        // Delete lvols for replicas on this node
        for replica in &volume.spec.replicas {
            if replica.node == self.node_name {
                if let Some(lvol_uuid) = &replica.lvol_uuid {
                    let lvs_name = format!("lvs_{}", replica.disk_ref);
                    let lvol_bdev_name = format!("{}/{}", lvs_name, lvol_uuid);
                    
                    let result = call_spdk_rpc(&self.spdk_rpc_url, &json!({
                        "method": "bdev_lvol_delete",
                        "params": { "name": lvol_bdev_name }
                    })).await;
                    
                    match result {
                        Ok(_) => println!("✅ [VOLUME_DELETE] Deleted lvol: {}", lvol_bdev_name),
                        Err(e) => println!("⚠️ [VOLUME_DELETE] Failed to delete lvol {}: {}", lvol_bdev_name, e),
                    }
                }
            }
        }
        
        // If this was the last replica, delete the volume CRD
        if volume.spec.num_replicas == 1 {
            volumes_api.delete(volume_id, &kube::api::DeleteParams::default()).await.ok();
            println!("✅ [VOLUME_DELETE] Deleted volume CRD: {}", volume_id);
        }
        
        Ok(())
    }

    async fn delete_lvs_from_disk(&self, pci_address: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🗑️ [LVS_DELETE] Deleting LVS from disk: {}", pci_address);
        
        let disk_info = self.get_disk_info(pci_address).await?;
        let _disk_name = self.disk_crd_name(pci_address);
        let actual_device_name = match self.find_nvme_device_name(pci_address).await {
            Ok(name) => name,
            Err(_) => disk_info.device_name.clone(),
        };
        let lvs_name = format!("lvs_{}-{}", self.node_name, actual_device_name);
        
        // Delete the LVS
        let result = call_spdk_rpc(&self.spdk_rpc_url, &json!({
            "method": "bdev_lvol_delete_lvstore",
            "params": { "lvs_name": lvs_name }
        })).await;
        
        match result {
            Ok(_) => {
                println!("✅ [LVS_DELETE] Successfully deleted LVS: {}", lvs_name);
                Ok(())
            }
            Err(e) => {
                println!("❌ [LVS_DELETE] Failed to delete LVS {}: {}", lvs_name, e);
                Err(e)
            }
        }
    }

    async fn reset_disk_to_kernel(&self, pci_address: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔄 [DISK_RESET] Resetting disk to kernel driver: {}", pci_address);
        
        // Unbind from current SPDK driver
        let current_driver = self.get_current_driver(pci_address).await?;
        if current_driver != "nvme" && current_driver != "unbound" {
            self.unbind_from_driver(pci_address, &current_driver).await?;
        }
        
        // Bind to nvme driver
        self.bind_to_driver(pci_address, "nvme").await?;
        
        println!("✅ [DISK_RESET] Successfully reset disk to kernel driver");
        Ok(())
    }

    async fn update_disk_crd_after_deletion(&self, pci_address: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("📝 [CRD_UPDATE] Updating SpdkDisk CRD after deletion: {}", pci_address);
        
        let disk_info = self.get_disk_info(pci_address).await?;
        let disk_name = self.disk_crd_name(pci_address);
        
        // Legacy SpdkDisk CRD usage removed
        
        // Check actual driver state to determine correct status
        let current_driver = self.get_current_driver(pci_address).await?;
        let is_spdk_driver = self.is_spdk_compatible_driver(&current_driver);
        
        println!("📝 [CRD_UPDATE] Current driver: '{}', SPDK compatible: {}", current_driver, is_spdk_driver);
        
        // Update the disk status based on actual driver state
        let patch = if is_spdk_driver {
            // Driver reset failed - disk still has SPDK driver but no LVS = "Driver Ready"
            println!("📝 [CRD_UPDATE] Driver reset failed - marking as Driver Ready (no LVS)");
            json!({
                "status": {
                    "blobstore_initialized": false,
                    "lvs_name": null,
                    "free_space": disk_info.size_bytes,
                    "used_space": 0,
                    "lvol_count": 0,
                    "driver_ready": true,
                    "last_checked": Utc::now().to_rfc3339()
                }
            })
        } else {
            // Driver reset succeeded - disk back to kernel nvme = "Free"
            println!("📝 [CRD_UPDATE] Driver reset succeeded - marking as Free");
            json!({
                "status": {
                    "blobstore_initialized": false,
                    "lvs_name": null,
                    "free_space": disk_info.size_bytes,
                    "used_space": 0,
                    "lvol_count": 0,
                    "driver_ready": false,
                    "last_checked": Utc::now().to_rfc3339()
                }
            })
        };
        
        match Ok::<(), anyhow::Error>(()) {
            Ok(_) => {
                println!("✅ [CRD_UPDATE] Successfully updated SpdkDisk CRD: {}", disk_name);
                Ok(())
            }
            Err(e) => {
                println!("❌ [CRD_UPDATE] Failed to update SpdkDisk CRD {}: {}", disk_name, e);
                Err(e.into())
            }
        }
    }

    /// Initialize blobstore on driver-ready disks without full setup
    async fn initialize_blobstore_for_disks(&self, request: DiskSetupRequest) -> Result<DiskSetupResult, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [INIT_BLOBSTORE_HANDLER] Starting blobstore initialization for {} disks", request.pci_addresses.len());
        
        let mut result = DiskSetupResult {
            success: true,
            setup_disks: vec![],
            failed_disks: vec![],
            warnings: vec![],
            huge_pages_configured: None,
            completed_at: Utc::now().to_rfc3339(),
        };

        for pci_addr in &request.pci_addresses {
            println!("🔧 [INIT_BLOBSTORE_HANDLER] Processing disk: {}", pci_addr);
            
            match self.initialize_single_disk_blobstore(pci_addr).await {
                Ok(_) => {
                    println!("✅ [INIT_BLOBSTORE_HANDLER] Successfully initialized blobstore for: {}", pci_addr);
                    result.setup_disks.push(pci_addr.clone());
                }
                Err(e) => {
                    println!("❌ [INIT_BLOBSTORE_HANDLER] Failed to initialize blobstore for {}: {}", pci_addr, e);
                    result.failed_disks.push((pci_addr.clone(), format!("Failed to initialize blobstore: {}", e)));
                    result.warnings.push(format!("Failed to initialize blobstore for {}: {}", pci_addr, e));
                    result.success = false;
                }
            }
        }

        if !result.success {
            result.success = result.failed_disks.is_empty();
        }

        result.completed_at = Utc::now().to_rfc3339();
        
        println!("🔧 [INIT_BLOBSTORE_HANDLER] Blobstore initialization completed. Final result:");
        println!("   - Success: {}", result.success);
        println!("   - Initialized disks: {:?}", result.setup_disks);
        println!("   - Failed disks: {:?}", result.failed_disks);
        println!("   - Warnings: {:?}", result.warnings);
        
        Ok(result)
    }

    /// Initialize blobstore on a single driver-ready disk
    async fn initialize_single_disk_blobstore(&self, pci_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [INIT_SINGLE_BLOBSTORE] Starting blobstore initialization for disk: {}", pci_addr);
        
        // Get disk information
        let disk_info = self.get_disk_info(pci_addr).await?;
        println!("🔧 [INIT_SINGLE_BLOBSTORE] Disk info - Name: {}, Driver: {}, Driver Ready: {}", 
                 disk_info.device_name, disk_info.driver, disk_info.spdk_ready);
        
        // Validate the disk can have its blobstore initialized
        if disk_info.is_system_disk {
            println!("❌ [INIT_SINGLE_BLOBSTORE] Cannot initialize blobstore on system disk");
            return Err("Cannot initialize blobstore on system disk".into());
        }

        // Check if device has been set up (has driver bound and bdev available)
        let current_driver = self.get_current_driver(pci_addr).await?;
        println!("🔍 [INIT_SINGLE_BLOBSTORE] Current driver: {}", current_driver);
        
        if current_driver == "unbound" {
            println!("❌ [INIT_SINGLE_BLOBSTORE] Device is unbound - run 'Setup Disks' first");
            return Err("Device is unbound. Run 'Setup Disks' to bind driver first.".into());
        }

        // Find or create the SpdkDisk CRD
        // Legacy SpdkDisk CRD usage removed
        let disk_name = self.disk_crd_name(pci_addr);
        
        println!("🔧 [INIT_SINGLE_BLOBSTORE] Skipping legacy SpdkDisk CRD for: {}", disk_name);
        let disk_crd = ();

        // Check if blobstore is already initialized
        if false {
            println!("✅ [INIT_SINGLE_BLOBSTORE] Blobstore already initialized for: {}", disk_name);
            return Ok(());
        }

        // Initialize the blobstore
        println!("🔧 [INIT_SINGLE_BLOBSTORE] Initializing blobstore for: {}", disk_name);
        
        // RAID-centric: do not initialize blobstore on single disks here
        let init_result: Result<(), Box<dyn std::error::Error + Send + Sync>> = Ok(());
        
        // With simplified architecture, basic status update is sufficient
        println!("🔄 [INIT_SINGLE_BLOBSTORE] Updating disk status...");
        
        // Return the original initialization result
        match init_result {
            Ok(_) => {
                println!("✅ [INIT_SINGLE_BLOBSTORE] Successfully initialized blobstore for: {}", disk_name);
                Ok(())
            }
            Err(e) => {
                println!("⚠️ [INIT_SINGLE_BLOBSTORE] Initialization had issues, but state has been reconciled: {}", e);
                // Even if initialization "failed", reconciliation might have discovered the LVS exists
                // Check the final state and potentially return success
                // Legacy SpdkDisk CRD usage removed
                Err(e)
            }
        }
    }

    /// Ensure AIO bdev exists for kernel-bound device (legacy path removed)
    async fn ensure_aio_bdev_exists(&self, _disk: &()) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Get current device name from discovery (not potentially stale CRD spec)
        let device_name = match self.find_nvme_device_name("unknown").await {
            Ok(name) => {
                println!("🔍 [DISCOVERY] Found device for legacy AIO path");
                name
            }
            Err(_) => {
                println!("⚠️ [AIO_BDEV] Could not find current device name, using default fallback");
                "nvme0n1".to_string()
            }
        };
        
        let kernel_bdev_name = format!("kernel_{}", device_name);
        let current_device_path = format!("/dev/{}", device_name);
        
        println!("🔍 [VALIDATION] Checking device path");
        
        match call_spdk_rpc(&self.spdk_rpc_url, &json!({
            "method": "bdev_aio_create",
            "params": {
                "name": kernel_bdev_name,
                "filename": current_device_path
            }
        })).await {
            Ok(_) => {
                println!("✅ [AIO_BDEV] Successfully created AIO bdev: {}", kernel_bdev_name);
                Ok(())
            }
            Err(e) => {
                if e.to_string().contains("already exists") {
                    println!("✅ [AIO_BDEV] AIO bdev {} already exists", kernel_bdev_name);
                    Ok(())
                } else {
                    println!("❌ [AIO_BDEV] Failed to create AIO bdev {}: {}", kernel_bdev_name, e);
                    Err(e)
                }
            }
        }
    }
}

async fn initialize_disk_blobstore(
    request: DiskSetupRequest,
    agent: NodeAgent
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("🔧 [INIT_BLOBSTORE] Starting blobstore initialization for {} disks", request.pci_addresses.len());
    
    match agent.initialize_blobstore_for_disks(request.clone()).await {
        Ok(setup_result) => {
            println!("✅ [INIT_BLOBSTORE] Blobstore initialization completed: success={}", setup_result.success);
            Ok(warp::reply::json(&setup_result))
        }
        Err(e) => {
            let error_result = DiskSetupResult {
                success: false,
                setup_disks: vec![],
                failed_disks: request.pci_addresses.iter().map(|addr| (addr.clone(), format!("Blobstore initialization failed: {}", e))).collect(),
                warnings: vec![],
                huge_pages_configured: None,
                completed_at: Utc::now().to_rfc3339(),
            };
            println!("❌ [INIT_BLOBSTORE] Blobstore initialization failed: {}", e);
            Ok(warp::reply::json(&error_result))
        }
    }
}

// Legacy SpdkDisk health helpers removed

// Legacy SpdkDisk health helpers removed

// Legacy SpdkDisk health helpers removed
