// main.rs - Entry point for Minimal State SPDK CSI Driver
use std::sync::Arc;
use tonic::transport::Server;
use kube::Client;
use warp::Filter;
use tracing_subscriber;

// Import minimal state components from library
use spdk_csi_driver::node_agent::NodeAgent;
use spdk_csi_driver::driver::{SpdkCsiDriver, NvmeofConnectionInfo};
use spdk_csi_driver::spdk_dashboard_backend_minimal::start_minimal_dashboard_backend;
use spdk_csi_driver::ReplicaInfo;

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

/// Cleanup any ghost mounts at startup
/// Ghost mounts are mount table entries that reference non-existent ublk devices
async fn cleanup_ghost_mounts() {
    println!("🧹 [STARTUP] Scanning for ghost mounts...");
    
    // Get all mount entries
    let mount_output = match std::process::Command::new("mount").output() {
        Ok(output) => output,
        Err(e) => {
            println!("⚠️ [STARTUP] Failed to read mount table: {}", e);
            return;
        }
    };
    
    let mount_text = String::from_utf8_lossy(&mount_output.stdout);
    let mut ghost_count = 0;
    let mut cleaned_count = 0;
    
    // Parse mount output and look for ublk devices
    for line in mount_text.lines() {
        if line.contains("/dev/ublkb") {
            // Parse the mount line: /dev/ublkbXXXXX on /path/to/mount type ext4 (options)
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let device = parts[0];
                let mount_point = parts[2];
                
                // Check if the device actually exists
                let device_exists = std::path::Path::new(device).exists();
                
                if !device_exists {
                    ghost_count += 1;
                    println!("👻 [STARTUP] Found ghost mount: {} -> {} (device doesn't exist)", device, mount_point);
                    
                    // Try to lazy unmount the ghost mount
                    let unmount_result = std::process::Command::new("umount")
                        .arg("-l")
                        .arg(mount_point)
                        .output();
                    
                    match unmount_result {
                        Ok(output) if output.status.success() => {
                            cleaned_count += 1;
                            println!("✅ [STARTUP] Cleaned ghost mount: {}", mount_point);
                        }
                        Ok(output) => {
                            let error = String::from_utf8_lossy(&output.stderr);
                            println!("⚠️ [STARTUP] Failed to clean ghost mount {}: {}", mount_point, error);
                        }
                        Err(e) => {
                            println!("⚠️ [STARTUP] Failed to execute umount for {}: {}", mount_point, e);
                        }
                    }
                    
                    // Small delay to allow cleanup to propagate
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                } else {
                    println!("✅ [STARTUP] Valid ublk mount: {} -> {}", device, mount_point);
                }
            }
        }
    }
    
    if ghost_count == 0 {
        println!("✅ [STARTUP] No ghost mounts found");
    } else {
        println!("📊 [STARTUP] Ghost mount cleanup: found {}, cleaned {}", ghost_count, cleaned_count);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing subscriber for better log formatting
    // This adds timestamps to all println!/eprintln! output
    // Future: migrate to tracing::info!, tracing::debug!, etc. for proper log levels
    // Configure via RUST_LOG env var (default: info level)
    tracing_subscriber::fmt()
        .with_target(false)  // Don't show module paths (cleaner output)
        .with_thread_ids(false)  // Don't show thread IDs (cleaner for CSI)
        .with_line_number(false)  // Don't show line numbers (we have emojis for context)
        .with_ansi(true)  // Enable colors in terminal
        .init();
    
    let kube_client = Client::try_default().await?;
    let node_id = std::env::var("NODE_ID")
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or("unknown-node".to_string()));
    
    // Detect the namespace for custom resources
    let target_namespace = get_current_namespace().await?;
    
    let spdk_socket_path = std::env::var("SPDK_RPC_URL").unwrap_or("unix:///var/tmp/spdk.sock".to_string());
    
    // Create minimal state driver
    let driver = Arc::new(SpdkCsiDriver::new(
        kube_client.clone(),
        target_namespace.clone(),
        node_id.clone(),
        spdk_socket_path.clone(),
        "tcp".to_string(), // nvmeof_transport
        4420, // nvmeof_target_port
    ));
    
    println!("🎯 [CONFIG] Using namespace for custom resources: {}", driver.target_namespace);
    
    // Initialize driver (warm up capacity cache, start background tasks)
    println!("🚀 [MAIN] Initializing CSI driver...");
    driver.initialize().await.map_err(|e| {
        eprintln!("❌ [MAIN] Failed to initialize driver: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    })?;
    println!("✅ [MAIN] CSI driver initialization complete");
    
    // Start health server for Kubernetes liveness probes
    tokio::spawn(async move {
        start_health_server().await;
    });

    // Start dashboard backend (if enabled)
    let enable_dashboard = std::env::var("ENABLE_DASHBOARD")
        .unwrap_or("false".to_string())
        .parse()
        .unwrap_or(false);
    
    if enable_dashboard {
        let dashboard_port = std::env::var("DASHBOARD_PORT")
            .unwrap_or("8080".to_string())
            .parse()
            .unwrap_or(8080);
        
        println!("📊 [DASHBOARD] Starting minimal dashboard backend on port {}", dashboard_port);
        tokio::spawn(async move {
            if let Err(e) = start_minimal_dashboard_backend(dashboard_port).await {
                eprintln!("❌ [DASHBOARD] Failed to start: {}", e);
            }
        });
    }
    
    let mode = std::env::var("CSI_MODE").unwrap_or("all".to_string());
    let endpoint = std::env::var("CSI_ENDPOINT")
        .unwrap_or("unix:///csi/csi.sock".to_string());
    
    // Cleanup any ghost mounts from previous runs (only in node mode)
    if mode == "node" || mode == "all" {
        cleanup_ghost_mounts().await;
    }
    
    // Start node agent (if in node mode)
    if mode == "node" || mode == "all" {
        let node_agent = Arc::new(NodeAgent::new(
            node_id.clone(),
            spdk_socket_path.clone(),
            driver.clone(),
        ));
        
        println!("🔧 [NODE_AGENT] Starting node agent on port 8081");
        let node_agent_clone = node_agent.clone();
        tokio::spawn(async move {
            if let Err(e) = node_agent_clone.start().await {
                eprintln!("❌ [NODE_AGENT] Failed to start: {}", e);
            }
        });
    }
    
    // Create minimal CSI services
    let identity_service = MinimalIdentityService::new(driver.clone());
    let controller_service = MinimalControllerService::new(driver.clone());
    let node_service = MinimalNodeService::new(driver.clone());
    
    // Build the router with services
    let mut router = Server::builder()
        .add_service(IdentityServer::new(identity_service));
    
    if mode == "controller" || mode == "all" {
        println!("Starting in Controller mode...");
        router = router.add_service(ControllerServer::new(controller_service));
    }
    
    if mode == "node" || mode == "all" {
        println!("Starting in Node mode...");
        router = router.add_service(NodeServer::new(node_service));
    }
    
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("✅ [CSI_SERVER] Minimal State SPDK CSI Driver starting");
    eprintln!("   Mode: {}", mode);
    eprintln!("   Endpoint: {}", endpoint);
    eprintln!("   Node ID: {}", node_id);
    eprintln!("   Clone Detection: ENABLED (commit c03bba7)");
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    
    // Handle different endpoint types
    if endpoint.starts_with("unix://") {
        let socket_path = endpoint.trim_start_matches("unix://");
        
        eprintln!("🔧 [CSI_SERVER] Setting up Unix socket: {}", socket_path);
        
        // Remove existing socket file if it exists
        if std::path::Path::new(socket_path).exists() {
            eprintln!("   Removing existing socket file");
            std::fs::remove_file(socket_path)?;
        }
        
        // Create parent directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            eprintln!("   Creating parent directory: {:?}", parent);
            std::fs::create_dir_all(parent)?;
        }
        
        // Use UnixListener for Unix domain socket
        use tokio::net::UnixListener;
        use tokio_stream::wrappers::UnixListenerStream;
        
        eprintln!("   Binding to socket...");
        let listener = UnixListener::bind(socket_path)?;
        let stream = UnixListenerStream::new(listener);
        
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("✅ [CSI_SERVER] CSI gRPC server listening on: {}", socket_path);
        eprintln!("   Waiting for CSI requests from kubelet...");
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
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

/// Minimal Identity Service Implementation
struct MinimalIdentityService {
    driver: Arc<SpdkCsiDriver>,
}

impl MinimalIdentityService {
    fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }
}

#[tonic::async_trait]
impl spdk_csi_driver::csi::identity_server::Identity for MinimalIdentityService {
    async fn get_plugin_info(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::GetPluginInfoRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::GetPluginInfoResponse>, tonic::Status> {
        println!("🔵 [GRPC] Identity.GetPluginInfo called");
        Ok(tonic::Response::new(spdk_csi_driver::csi::GetPluginInfoResponse {
            name: "flint.csi.storage.io".to_string(),
            vendor_version: env!("CARGO_PKG_VERSION").to_string(),
            manifest: std::collections::HashMap::new(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::GetPluginCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::GetPluginCapabilitiesResponse>, tonic::Status> {
        println!("🔵 [GRPC] Identity.GetPluginCapabilities called");
        use spdk_csi_driver::csi::{plugin_capability::service::Type as ServiceType, PluginCapability, plugin_capability::Service};
        
        let capabilities = vec![
            PluginCapability {
                r#type: Some(spdk_csi_driver::csi::plugin_capability::Type::Service(Service {
                    r#type: ServiceType::ControllerService as i32,
                })),
            },
        ];
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::GetPluginCapabilitiesResponse { capabilities }))
    }

    async fn probe(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ProbeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ProbeResponse>, tonic::Status> {
        println!("🔵 [GRPC] Identity.Probe called");
        Ok(tonic::Response::new(spdk_csi_driver::csi::ProbeResponse { ready: Some(true) }))
    }
}

/// Minimal Controller Service Implementation  
struct MinimalControllerService {
    driver: Arc<SpdkCsiDriver>,
}

impl MinimalControllerService {
    fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }

    /// Create volume from snapshot by cloning the snapshot
    async fn create_volume_from_snapshot(
        &self,
        volume_id: &str,
        snapshot_id: &str,
        size_bytes: u64,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::CreateVolumeResponse>, tonic::Status> {
        println!("🔄 [CONTROLLER] Creating volume {} from snapshot {}", volume_id, snapshot_id);

        // Step 1: Find which node has the snapshot
        let nodes = self.driver.get_all_nodes().await
            .map_err(|e| tonic::Status::internal(format!("Failed to list nodes: {}", e)))?;

        let mut snapshot_node = None;
        
        for node in &nodes {
            let payload = serde_json::json!({
                "snapshot_uuid": snapshot_id
            });
            
            match self.driver.call_node_agent(node, "/api/snapshots/get_info", &payload).await {
                Ok(_) => {
                    snapshot_node = Some(node.clone());
                    println!("✅ [CONTROLLER] Found snapshot on node: {}", node);
                    break;
                }
                Err(_) => continue,
            }
        }
        
        let node_name = snapshot_node
            .ok_or_else(|| tonic::Status::not_found(format!("Snapshot {} not found", snapshot_id)))?;

        // Step 2: Clone the snapshot to create a new writable volume
        let clone_name = format!("vol_{}", volume_id);
        
        let payload = serde_json::json!({
            "snapshot_uuid": snapshot_id,
            "clone_name": clone_name
        });
        
        let response = self.driver
            .call_node_agent(&node_name, "/api/snapshots/clone", &payload)
            .await
            .map_err(|e| tonic::Status::internal(format!("Failed to clone snapshot: {}", e)))?;
        
        let clone_uuid = response["clone_uuid"].as_str()
            .ok_or_else(|| tonic::Status::internal("No clone UUID in response"))?
            .to_string();
        
        let actual_size = response["size_bytes"].as_i64().unwrap_or(size_bytes as i64);
        
        // Get lvs_name from the response (node agent provides this from snapshot info)
        let lvs_name = response["lvs_name"].as_str()
            .ok_or_else(|| tonic::Status::internal(format!(
                "No lvs_name in clone response. Clone UUID: {}. This should not happen - the snapshot service should populate lvs_name",
                clone_uuid
            )))?
            .to_string();
        
        println!("✅ [CONTROLLER] Volume {} created from snapshot (clone UUID: {}, lvs: {})", 
                 volume_id, clone_uuid, lvs_name);
        
        // Step 3: Build volume_context with metadata (critical for attach operations)
        let mut volume_context = std::collections::HashMap::new();
        
        // Single replica (snapshot clones are always single replica)
        volume_context.insert(
            "flint.csi.storage.io/replica-count".to_string(),
            "1".to_string(),
        );
        volume_context.insert(
            "flint.csi.storage.io/node-name".to_string(),
            node_name.clone(),
        );
        volume_context.insert(
            "flint.csi.storage.io/lvol-uuid".to_string(),
            clone_uuid.clone(),
        );
        volume_context.insert(
            "flint.csi.storage.io/lvs-name".to_string(),
            lvs_name.clone(),
        );
        
        // CRITICAL: Mark filesystem as initialized (clone has filesystem from snapshot)
        // Without this, node can't distinguish SPDK block reuse from real filesystem
        volume_context.insert(
            "flint.csi.storage.io/filesystem-initialized".to_string(),
            "true".to_string(),
        );
        volume_context.insert(
            "flint.csi.storage.io/source-snapshot".to_string(),
            snapshot_id.to_string(),
        );
        
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("📝 [SNAPSHOT_RESTORE] Volume context populated:");
        eprintln!("   filesystem-initialized: true");
        eprintln!("   source-snapshot: {}", snapshot_id);
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        // Step 4: Return volume with content_source and metadata populated
        let content_source = spdk_csi_driver::csi::VolumeContentSource {
            r#type: Some(spdk_csi_driver::csi::volume_content_source::Type::Snapshot(
                spdk_csi_driver::csi::volume_content_source::SnapshotSource {
                    snapshot_id: snapshot_id.to_string(),
                }
            )),
        };

        let response = spdk_csi_driver::csi::CreateVolumeResponse {
            volume: Some(spdk_csi_driver::csi::Volume {
                volume_id: volume_id.to_string(),
                capacity_bytes: actual_size,
                volume_context,  // Now includes metadata!
                content_source: Some(content_source),
                accessible_topology: vec![],
            }),
        };
        
        println!("🎉 [CONTROLLER] Volume from snapshot created successfully");
        Ok(tonic::Response::new(response))
    }

    /// Create volume from existing volume (PVC clone)
    async fn create_volume_from_volume(
        &self,
        volume_id: &str,
        source_volume_id: &str,
        size_bytes: u64,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::CreateVolumeResponse>, tonic::Status> {
        println!("🔄 [CONTROLLER] Creating volume {} as clone of {}", volume_id, source_volume_id);

        // Step 1: Get source volume metadata to find which node it's on
        // Query Kubernetes API for the source PV
        use kube::Api;
        use k8s_openapi::api::core::v1::PersistentVolume;
        
        let pv_api: Api<PersistentVolume> = Api::all(self.driver.kube_client.clone());
        
        // Find the source PV (volume_id is already in format "pvc-xxxxx")
        let source_pv = pv_api.get(source_volume_id)
            .await
            .map_err(|e| tonic::Status::not_found(format!("Source volume not found: {}", e)))?;
        
        let volume_attributes = source_pv.spec
            .as_ref()
            .and_then(|spec| spec.csi.as_ref())
            .and_then(|csi| csi.volume_attributes.as_ref())
            .ok_or_else(|| tonic::Status::internal("Source volume missing CSI volume attributes"))?;
        
        let source_node = volume_attributes.get("flint.csi.storage.io/node-name")
            .ok_or_else(|| tonic::Status::internal("Source volume missing node metadata"))?
            .clone();
        
        let source_lvol_uuid = volume_attributes.get("flint.csi.storage.io/lvol-uuid")
            .ok_or_else(|| tonic::Status::internal("Source volume missing lvol-uuid"))?
            .clone();

        println!("✅ [CONTROLLER] Found source volume on node: {}, lvol: {}", source_node, source_lvol_uuid);

        // Step 2: Create a temporary snapshot of the source volume
        // NOTE: SPDK bdev_lvol_clone requires a snapshot, can't clone regular lvol directly
        // We create a temp snapshot, clone it, then delete the temp snapshot
        let snapshot_name = format!("temp_pvc_clone_{}", volume_id);
        
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("📸 [PVC_CLONE] Creating temporary snapshot for cloning");
        eprintln!("   Source lvol: {}", source_lvol_uuid);
        eprintln!("   Temp snapshot name: {}", snapshot_name);
        eprintln!("   (Will be deleted after clone succeeds)");
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        let snapshot_payload = serde_json::json!({
            "lvol_name": source_lvol_uuid,  // API expects lvol_name (can be UUID or name)
            "snapshot_name": snapshot_name
        });
        
        let snapshot_response = self.driver
            .call_node_agent(&source_node, "/api/snapshots/create", &snapshot_payload)
            .await
            .map_err(|e| tonic::Status::internal(format!("Failed to create temporary snapshot for PVC clone: {}", e)))?;
        
        let snapshot_uuid = snapshot_response["snapshot_uuid"].as_str()
            .ok_or_else(|| tonic::Status::internal("No snapshot UUID in response"))?
            .to_string();

        println!("✅ [CONTROLLER] Temporary snapshot created: {}", snapshot_uuid);

        // Step 3: Clone the snapshot to create the new volume
        let clone_name = format!("vol_{}", volume_id);
        
        let clone_payload = serde_json::json!({
            "snapshot_uuid": snapshot_uuid,
            "clone_name": clone_name
        });
        
        let clone_response = self.driver
            .call_node_agent(&source_node, "/api/snapshots/clone", &clone_payload)
            .await
            .map_err(|e| tonic::Status::internal(format!("Failed to clone volume: {}", e)))?;
        
        let clone_uuid = clone_response["clone_uuid"].as_str()
            .ok_or_else(|| tonic::Status::internal("No clone UUID in response"))?
            .to_string();
        
        let lvs_name = clone_response["lvs_name"].as_str()
            .ok_or_else(|| tonic::Status::internal("No lvs_name in clone response"))?
            .to_string();

        println!("✅ [CONTROLLER] Volume {} cloned from {} (clone UUID: {})", volume_id, source_volume_id, clone_uuid);

        // Step 3.5: Delete temporary snapshot (cleanup)
        // The clone is now independent, we don't need the temp snapshot anymore
        println!("🧹 [CONTROLLER] Cleaning up temporary snapshot: {}", snapshot_uuid);
        
        let delete_payload = serde_json::json!({
            "snapshot_uuid": snapshot_uuid
        });
        
        match self.driver.call_node_agent(&source_node, "/api/snapshots/delete", &delete_payload).await {
            Ok(_) => {
                println!("✅ [CONTROLLER] Temporary snapshot deleted successfully");
            }
            Err(e) => {
                // Log but don't fail - clone succeeded, snapshot cleanup is nice-to-have
                println!("⚠️ [CONTROLLER] Failed to delete temporary snapshot (non-fatal): {}", e);
                println!("   Snapshot {} may need manual cleanup", snapshot_uuid);
            }
        }

        // Step 4: Build volume_context with metadata
        let mut volume_context = std::collections::HashMap::new();
        
        volume_context.insert("flint.csi.storage.io/replica-count".to_string(), "1".to_string());
        volume_context.insert("flint.csi.storage.io/node-name".to_string(), source_node.clone());
        volume_context.insert("flint.csi.storage.io/lvol-uuid".to_string(), clone_uuid.clone());
        volume_context.insert("flint.csi.storage.io/lvs-name".to_string(), lvs_name.clone());
        
        // CRITICAL: Mark filesystem as initialized (clone has filesystem from source PVC)
        // Without this, node can't distinguish SPDK block reuse from real filesystem
        volume_context.insert("flint.csi.storage.io/filesystem-initialized".to_string(), "true".to_string());
        volume_context.insert("flint.csi.storage.io/source-volume".to_string(), source_volume_id.to_string());
        
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("📝 [PVC_CLONE] Volume context populated:");
        eprintln!("   filesystem-initialized: true");
        eprintln!("   source-volume: {}", source_volume_id);
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        // Step 5: Return volume with content_source and metadata
        let content_source = spdk_csi_driver::csi::VolumeContentSource {
            r#type: Some(spdk_csi_driver::csi::volume_content_source::Type::Volume(
                spdk_csi_driver::csi::volume_content_source::VolumeSource {
                    volume_id: source_volume_id.to_string(),
                }
            )),
        };

        let actual_size = size_bytes as i64;
        
        let response = spdk_csi_driver::csi::CreateVolumeResponse {
            volume: Some(spdk_csi_driver::csi::Volume {
                volume_id: volume_id.to_string(),
                capacity_bytes: actual_size,
                volume_context,
                content_source: Some(content_source),
                accessible_topology: vec![],
            }),
        };

        println!("🎉 [CONTROLLER] PVC clone created successfully");
        Ok(tonic::Response::new(response))
    }
}

#[tonic::async_trait]
impl spdk_csi_driver::csi::controller_server::Controller for MinimalControllerService {
    async fn create_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::CreateVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::CreateVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.name.clone();
        println!("🎯 [CONTROLLER] Creating volume: {}", volume_id);

        // Check if creating from snapshot or volume (PVC clone) first
        if let Some(content_source) = &req.volume_content_source {
            if let Some(source_type) = &content_source.r#type {
                use spdk_csi_driver::csi::volume_content_source::Type;
                
                let size_bytes = req.capacity_range.as_ref()
                    .and_then(|cr| if cr.required_bytes > 0 { Some(cr.required_bytes) } else { Some(cr.limit_bytes) })
                    .unwrap_or(1024 * 1024 * 1024) as u64;
                
                match source_type {
                    Type::Snapshot(snapshot) => {
                        println!("🔄 [CONTROLLER] Creating volume from snapshot: {}", snapshot.snapshot_id);
                        return self.create_volume_from_snapshot(&volume_id, &snapshot.snapshot_id, size_bytes).await;
                    }
                    Type::Volume(volume_source) => {
                        println!("🔄 [CONTROLLER] Creating volume from PVC (clone): {}", volume_source.volume_id);
                        return self.create_volume_from_volume(&volume_id, &volume_source.volume_id, size_bytes).await;
                    }
                }
            }
        }

        // Check if this is an ephemeral volume (CSI inline volume)
        // For CreateVolume, Kubernetes passes this through the parameters field
        let is_ephemeral = req.parameters.get("csi.storage.k8s.io/ephemeral")
            .map(|v| v == "true")
            .unwrap_or(false);

        if is_ephemeral {
            println!("📦 [CONTROLLER] Creating EPHEMERAL volume (will be deleted with Pod)");
        }

        // Extract parameters for normal volume creation
        let size_bytes = req.capacity_range
            .and_then(|cr| if cr.required_bytes > 0 { Some(cr.required_bytes) } else { Some(cr.limit_bytes) })
            .unwrap_or(1024 * 1024 * 1024) as u64; // Default 1GB

        // For ephemeral volumes, optimize by defaulting to single replica unless specified
        let replica_count = if is_ephemeral {
            req.parameters.get("numReplicas")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1) // Ephemeral: default to single replica for fast Pod startup
        } else {
            req.parameters.get("numReplicas")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1) // Persistent: use StorageClass default
        };

        let thin_provision = req.parameters.get("thinProvision")
            .and_then(|s| s.parse::<bool>().ok())
            .unwrap_or(false);

        println!("📊 [CONTROLLER] Volume {} - Size: {} bytes, Replicas: {}, Thin: {}, Ephemeral: {}", 
                 volume_id, size_bytes, replica_count, thin_provision, is_ephemeral);

        // Call the driver's create volume method 
        match self.driver.create_volume(&volume_id, size_bytes, replica_count, thin_provision).await {
            Ok(result) => {
                println!("✅ [CONTROLLER] Volume {} created successfully with {} replica(s)", 
                         volume_id, result.replicas.len());
                
                // Build volume_context with metadata
                let mut volume_context = std::collections::HashMap::new();
                
                // Add replica count
                volume_context.insert(
                    "flint.csi.storage.io/replica-count".to_string(),
                    result.replicas.len().to_string(),
                );

                if result.replicas.len() == 1 {
                    // SINGLE REPLICA: Store simple metadata
                    let replica = &result.replicas[0];
                    volume_context.insert(
                        "flint.csi.storage.io/node-name".to_string(),
                        replica.node_name.clone(),
                    );
                    volume_context.insert(
                        "flint.csi.storage.io/lvol-uuid".to_string(),
                        replica.lvol_uuid.clone(),
                    );
                    volume_context.insert(
                        "flint.csi.storage.io/lvs-name".to_string(),
                        replica.lvs_name.clone(),
                    );
                    
                    println!("📝 [CONTROLLER] Storing metadata in PV: node={}, lvol={}", 
                             replica.node_name, replica.lvol_uuid);
                } else {
                    // MULTI-REPLICA: Store full replica array as JSON (future use)
                    let replicas_json = serde_json::to_string(&result.replicas)
                        .map_err(|e| tonic::Status::internal(format!("Failed to serialize replicas: {}", e)))?;
                    
                    volume_context.insert(
                        "flint.csi.storage.io/replicas".to_string(),
                        replicas_json,
                    );
                }
                
                let response = spdk_csi_driver::csi::CreateVolumeResponse {
                    volume: Some(spdk_csi_driver::csi::Volume {
                        volume_id: volume_id.clone(),
                        capacity_bytes: size_bytes as i64,
                        volume_context,  // Kubernetes stores this in PV.spec.csi.volumeAttributes
                        content_source: None,
                        accessible_topology: vec![],
                    }),
                };
                Ok(tonic::Response::new(response))
            }
            Err(e) => {
                println!("❌ [CONTROLLER] Volume creation failed: {}", e);
                Err(tonic::Status::internal(format!("Volume creation failed: {}", e)))
            }
        }
    }

    async fn delete_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::DeleteVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::DeleteVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        
        println!("🗑️ [CONTROLLER] Deleting volume: {}", volume_id);
        
        // Check if this is a multi-replica volume
        match self.driver.get_replicas_from_pv(&volume_id).await {
            Ok(Some(replicas)) => {
                // MULTI-REPLICA: Delete all replicas
                println!("📊 [CONTROLLER] Deleting multi-replica volume ({} replicas)", replicas.len());
                
                // Delete each replica lvol
                for (i, replica) in replicas.iter().enumerate() {
                    println!("🗑️ [CONTROLLER] Deleting replica {} on node {}", 
                             i + 1, replica.node_name);
                    
                    match self.driver.delete_lvol(&replica.node_name, &replica.lvol_uuid).await {
                        Ok(()) => println!("✅ Deleted replica {} (UUID: {})", i + 1, replica.lvol_uuid),
                        Err(e) => println!("⚠️ Failed to delete replica {}: {}", i + 1, e),
                    }

                    // Cleanup NVMe-oF target if it exists
                    let nqn = format!("nqn.2024-11.com.flint:volume:{}:replica:{}", volume_id, i);
                    let _ = self.driver.remove_nvmeof_target(&replica.node_name, &nqn).await;
                }

                println!("✅ [CONTROLLER] Multi-replica volume deleted: {}", volume_id);
                return Ok(tonic::Response::new(spdk_csi_driver::csi::DeleteVolumeResponse {}));
            }
            Ok(None) => {
                // SINGLE REPLICA: Use existing logic
                println!("📊 [CONTROLLER] Single-replica volume");
            }
            Err(e) => {
                println!("⚠️ [CONTROLLER] Volume not found (may already be deleted): {}", e);
                // Not an error - idempotent delete
                return Ok(tonic::Response::new(spdk_csi_driver::csi::DeleteVolumeResponse {}));
            }
        }

        // SINGLE REPLICA deletion logic (existing code)
        let volume_info = match self.driver.get_volume_info(&volume_id).await {
            Ok(info) => info,
            Err(e) => {
                println!("⚠️ [CONTROLLER] Volume not found (may already be deleted): {}", e);
                // Not an error - idempotent delete
                return Ok(tonic::Response::new(spdk_csi_driver::csi::DeleteVolumeResponse {}));
            }
        };

        println!("📊 [CONTROLLER] Deleting volume on node: {}", volume_info.node_name);

        // DEFENSIVE CLEANUP: Check if volume is still staged (NodeUnstageVolume may not have been called)
        // This happens when PVC is deleted before VolumeAttachment, causing kubelet to skip NodeUnstageVolume
        println!("🔍 [CONTROLLER] Checking if volume is still staged on node (defensive cleanup)...");
        
        let ublk_id = self.driver.generate_ublk_id(&volume_id);
        
        if let Err(e) = self.driver.force_unstage_volume_if_needed(&volume_info.node_name, &volume_id, ublk_id).await {
            println!("⚠️ [CONTROLLER] Force unstaging failed (may not be staged): {}", e);
            // Continue - this is best-effort cleanup
        }

        // Now safe to delete the logical volume on the storage node
        match self.driver.delete_lvol(&volume_info.node_name, &volume_info.lvol_uuid).await {
            Ok(_) => {
                println!("✅ [CONTROLLER] Logical volume deleted successfully");
            }
            Err(e) => {
                // Check if error is "Device or resource busy" - this means volume is still mounted
                let error_msg = format!("{}", e);
                if error_msg.contains("Device or resource busy") || error_msg.contains("busy") {
                    println!("❌ [CONTROLLER] Lvol deletion failed - volume still in use!");
                    println!("🔍 [CONTROLLER] This usually means:");
                    println!("   1. Volume is still mounted somewhere");
                    println!("   2. ublk device still exists and has active I/O");
                    println!("   3. NodeUnstageVolume was not called by kubelet");
                    println!("⚠️ [CONTROLLER] Retrying with more aggressive cleanup...");
                    
                    // Try one more time with explicit cleanup
                    if let Err(cleanup_err) = self.driver.force_cleanup_volume(&volume_info.node_name, &volume_id, ublk_id).await {
                        println!("❌ [CONTROLLER] Aggressive cleanup also failed: {}", cleanup_err);
                        return Err(tonic::Status::internal(format!("Failed to delete volume (still in use): {}", e)));
                    }
                    
                    // Retry lvol deletion after cleanup
                    match self.driver.delete_lvol(&volume_info.node_name, &volume_info.lvol_uuid).await {
                        Ok(_) => println!("✅ [CONTROLLER] Lvol deleted after aggressive cleanup"),
                        Err(retry_err) => {
                            println!("❌ [CONTROLLER] Lvol deletion still failed: {}", retry_err);
                            return Err(tonic::Status::internal(format!("Failed to delete volume: {}", retry_err)));
                        }
                    }
                } else {
                    println!("❌ [CONTROLLER] Failed to delete logical volume: {}", e);
                    return Err(tonic::Status::internal(format!("Failed to delete volume: {}", e)));
                }
            }
        }

        // Clean up any NVMe-oF targets that might still exist
        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
        if let Err(e) = self.driver.remove_nvmeof_target(&volume_info.node_name, &nqn).await {
            println!("⚠️ [CONTROLLER] Failed to remove NVMe-oF target (may not exist): {}", e);
            // Continue anyway - best effort cleanup
        }

        println!("✅ [CONTROLLER] Volume {} deleted successfully", volume_id);
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::ControllerPublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerPublishVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let node_id = req.node_id.clone();
        
        println!("📤 [CONTROLLER] Publishing volume {} to node {}", volume_id, node_id);

        let mut publish_context = std::collections::HashMap::new();

        // Check if this is a multi-replica volume
        match self.driver.get_replicas_from_pv(&volume_id).await {
            Ok(Some(replicas)) => {
                // MULTI-REPLICA: Store replicas as JSON for NodeStage
                println!("📊 [CONTROLLER] Multi-replica volume with {} replicas", replicas.len());
                
                let replicas_json = serde_json::to_string(&replicas)
                    .map_err(|e| tonic::Status::internal(format!("Failed to serialize replicas: {}", e)))?;
                
                publish_context.insert("volumeType".to_string(), "multi-replica".to_string());
                publish_context.insert("replicas".to_string(), replicas_json);
                publish_context.insert("volumeId".to_string(), volume_id.clone());
            }
            Ok(None) => {
                // SINGLE REPLICA: Use existing logic
                let volume_info = match self.driver.get_volume_info(&volume_id).await {
                    Ok(info) => info,
                    Err(e) => {
                        println!("❌ [CONTROLLER] Failed to get volume info: {}", e);
                        return Err(tonic::Status::not_found(format!("Volume not found: {}", e)));
                    }
                };

                println!("📊 [CONTROLLER] Single-replica volume on node {}", volume_info.node_name);

                // Check if pod is on the same node as the logical volume
                if volume_info.node_name == node_id {
                    println!("✅ [CONTROLLER] Volume is local to node - no NVMe-oF needed");
                    
                    // Store volume info in publish context for NodeStage
                    publish_context.insert("volumeType".to_string(), "local".to_string());
                    publish_context.insert("bdevName".to_string(), volume_info.lvol_uuid.clone());
                    publish_context.insert("lvsName".to_string(), volume_info.lvs_name.clone());
                } else {
                    println!("🌐 [CONTROLLER] Volume is remote - setting up NVMe-oF");
                    
                    // Construct bdev name for lvol
                    let bdev_name = volume_info.lvol_uuid.clone();
                    
                    // Setup NVMe-oF target on the node hosting the logical volume
                    let conn_info = match self.driver.setup_nvmeof_target_on_node(
                        &volume_info.node_name,
                        &bdev_name,
                        &volume_id
                    ).await {
                        Ok(info) => info,
                        Err(e) => {
                            println!("❌ [CONTROLLER] Failed to setup NVMe-oF target: {}", e);
                            return Err(tonic::Status::internal(format!("Failed to setup NVMe-oF: {}", e)));
                        }
                    };

                    println!("✅ [CONTROLLER] NVMe-oF target ready: {}", conn_info.nqn);

                    // Store connection info in publish context for NodeStage
                    publish_context.insert("volumeType".to_string(), "remote".to_string());
                    publish_context.insert("nqn".to_string(), conn_info.nqn.clone());
                    publish_context.insert("targetIp".to_string(), conn_info.target_ip.clone());
                    publish_context.insert("targetPort".to_string(), conn_info.target_port.to_string());
                    publish_context.insert("transport".to_string(), conn_info.transport.clone());
                    publish_context.insert("storageNode".to_string(), volume_info.node_name.clone());
                }

                publish_context.insert("volumeId".to_string(), volume_id.clone());
            }
            Err(e) => {
                println!("❌ [CONTROLLER] Failed to get volume replicas: {}", e);
                return Err(tonic::Status::not_found(format!("Volume not found: {}", e)));
            }
        }

        println!("✅ [CONTROLLER] Volume {} published successfully", volume_id);
        
        let response = spdk_csi_driver::csi::ControllerPublishVolumeResponse {
            publish_context,
        };
        
        Ok(tonic::Response::new(response))
    }

    async fn controller_unpublish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::ControllerUnpublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerUnpublishVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let node_id = req.node_id.clone();
        
        println!("📥 [CONTROLLER] Unpublishing volume {} from node {:?}", volume_id, node_id);

        // Get volume information
        let volume_info = match self.driver.get_volume_info(&volume_id).await {
            Ok(info) => info,
            Err(e) => {
                println!("⚠️ [CONTROLLER] Volume not found (may already be deleted): {}", e);
                // Not an error - volume might already be deleted
                return Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerUnpublishVolumeResponse {}));
            }
        };

        // If node_id is specified and volume is remote, we need to cleanup
        if !node_id.is_empty() {
            if volume_info.node_name != node_id {
                println!("🧹 [CONTROLLER] Volume is remote - cleaning up NVMe-oF connections");
                
                let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
                
                // Disconnect from NVMe-oF target on the node where pod was running
                // Note: We need to create a temporary driver instance for the target node
                // For now, we'll use the controller's node_id since this is a cleanup operation
                println!("🔌 [CONTROLLER] Note: NVMe disconnection handled by NodeUnpublish on node {}", node_id);
                
                // Remove the NVMe-oF target from the storage node
                if let Err(e) = self.driver.remove_nvmeof_target(&volume_info.node_name, &nqn).await {
                    println!("⚠️ [CONTROLLER] Failed to remove NVMe-oF target (continuing): {}", e);
                }
            } else {
                println!("ℹ️ [CONTROLLER] Volume is local - no NVMe-oF cleanup needed");
            }
        }

        println!("✅ [CONTROLLER] Volume {} unpublished successfully", volume_id);
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerUnpublishVolumeResponse {}))
    }

    async fn validate_volume_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ValidateVolumeCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ValidateVolumeCapabilitiesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Validate volume capabilities not implemented"))
    }

    async fn list_volumes(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ListVolumesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ListVolumesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("List volumes not implemented"))
    }

    async fn get_capacity(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::GetCapacityRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::GetCapacityResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Get capacity not implemented"))
    }

    async fn controller_get_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerGetCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerGetCapabilitiesResponse>, tonic::Status> {
        println!("🔵 [GRPC] Controller.ControllerGetCapabilities called");
        use spdk_csi_driver::csi::{controller_service_capability::rpc::Type as RpcType, ControllerServiceCapability, controller_service_capability::Rpc};
        
        let capabilities = vec![
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::CreateDeleteVolume as i32,
                })),
            },
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::PublishUnpublishVolume as i32,
                })),
            },
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::CreateDeleteSnapshot as i32,
                })),
            },
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::CloneVolume as i32,
                })),
            },
            ControllerServiceCapability {
                r#type: Some(spdk_csi_driver::csi::controller_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::ExpandVolume as i32,
                })),
            },
        ];
        
        Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerGetCapabilitiesResponse { capabilities }))
    }

    // ============= SNAPSHOT MODULE INTEGRATION =============
    // Delegate to SnapshotController (isolated snapshot module)
    async fn create_snapshot(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::CreateSnapshotRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::CreateSnapshotResponse>, tonic::Status> {
        use spdk_csi_driver::snapshot::SnapshotController;
        let snapshot_controller = SnapshotController::new(self.driver.clone());
        snapshot_controller.create_snapshot(request).await
    }

    async fn delete_snapshot(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::DeleteSnapshotRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::DeleteSnapshotResponse>, tonic::Status> {
        use spdk_csi_driver::snapshot::SnapshotController;
        let snapshot_controller = SnapshotController::new(self.driver.clone());
        snapshot_controller.delete_snapshot(request).await
    }

    async fn list_snapshots(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::ListSnapshotsRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ListSnapshotsResponse>, tonic::Status> {
        use spdk_csi_driver::snapshot::SnapshotController;
        let snapshot_controller = SnapshotController::new(self.driver.clone());
        snapshot_controller.list_snapshots(request).await
    }
    // ============= END SNAPSHOT INTEGRATION =============

    async fn controller_expand_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::ControllerExpandVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerExpandVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let new_size_bytes = req.capacity_range
            .ok_or_else(|| tonic::Status::invalid_argument("capacity_range is required"))?
            .required_bytes as u64;

        println!("📏 [CONTROLLER] Expanding volume {} to {} bytes", volume_id, new_size_bytes);

        // Find which node has the volume
        let volume_info = self.driver.get_volume_info(&volume_id).await
            .map_err(|e| tonic::Status::not_found(format!("Volume not found: {}", e)))?;

        println!("✅ [CONTROLLER] Found volume on node: {}", volume_info.node_name);

        // Check if new size is larger than current size
        if new_size_bytes <= volume_info.size_bytes {
            println!("ℹ️ [CONTROLLER] New size {} <= current size {}, no expansion needed", 
                     new_size_bytes, volume_info.size_bytes);
            // Return current size - CSI spec says this is OK
            return Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerExpandVolumeResponse {
                capacity_bytes: volume_info.size_bytes as i64,
                node_expansion_required: false,
            }));
        }

        // Call node agent to resize the volume
        let payload = serde_json::json!({
            "lvol_uuid": volume_info.lvol_uuid,
            "new_size_bytes": new_size_bytes
        });

        self.driver
            .call_node_agent(&volume_info.node_name, "/api/volumes/resize_lvol", &payload)
            .await
            .map_err(|e| tonic::Status::internal(format!("Failed to resize volume: {}", e)))?;

        println!("✅ [CONTROLLER] Volume {} expanded to {} bytes", volume_id, new_size_bytes);

        // node_expansion_required=true tells Kubernetes to call NodeExpandVolume
        // to resize the filesystem (ext4, xfs, etc.)
        Ok(tonic::Response::new(spdk_csi_driver::csi::ControllerExpandVolumeResponse {
            capacity_bytes: new_size_bytes as i64,
            node_expansion_required: true, // Filesystem resize needed
        }))
    }

    async fn controller_get_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerGetVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerGetVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Controller get volume not implemented"))
    }

    async fn controller_modify_volume(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::ControllerModifyVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::ControllerModifyVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Controller modify volume not implemented"))
    }
}

/// Minimal Node Service Implementation
struct MinimalNodeService {
    driver: Arc<SpdkCsiDriver>,
}

impl MinimalNodeService {
    fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }
}

#[tonic::async_trait]
impl spdk_csi_driver::csi::node_server::Node for MinimalNodeService {
    async fn node_stage_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodeStageVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeStageVolumeResponse>, tonic::Status> {
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("🔵 [GRPC] *** NodeStageVolume CALLED ***");
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let staging_target_path = req.staging_target_path.clone();
        let publish_context = req.publish_context.clone();
        let volume_context = req.volume_context.clone();
        
        eprintln!("📦 [NODE_STAGE] Volume ID: {}", volume_id);
        eprintln!("📦 [NODE_STAGE] Staging path: {}", staging_target_path);
        eprintln!("📦 [NODE_STAGE] Publish context keys: {:?}", publish_context.keys().collect::<Vec<_>>());

        // Check if this is an ephemeral volume
        let is_ephemeral = volume_context.get("csi.storage.k8s.io/ephemeral")
            .map(|v| v == "true")
            .unwrap_or(false);
        
        if is_ephemeral {
            println!("📦 [NODE_STAGE] Ephemeral volume detected (no PV exists)");
        }

        // For ephemeral volumes (attachRequired=false), publish_context is empty
        // because ControllerPublishVolume is never called. Treat as local volume.
        let volume_type = if publish_context.is_empty() {
            println!("📦 [NODE_STAGE] Empty publish_context - treating as local volume");
            "local"
        } else {
            publish_context.get("volumeType")
                .ok_or_else(|| tonic::Status::invalid_argument("No volumeType in publish context"))?
        };

        let bdev_name = if volume_type == "multi-replica" {
            // MULTI-REPLICA: Create RAID 1 from replicas
            println!("🔧 [NODE] Multi-replica volume - creating RAID");
            
            let replicas_json = publish_context.get("replicas")
                .ok_or_else(|| tonic::Status::invalid_argument("No replicas in publish context"))?;
            
            let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)
                .map_err(|e| tonic::Status::internal(format!("Failed to parse replicas: {}", e)))?;
            
            println!("📊 [NODE] Volume has {} replicas", replicas.len());
            for (i, replica) in replicas.iter().enumerate() {
                println!("   Replica {}: node={}, lvol={}", 
                         i + 1, replica.node_name, replica.lvol_uuid);
            }
            
            // Create RAID 1 bdev with mixed local/remote access
            match self.driver.create_raid_from_replicas(&volume_id, &replicas).await {
                Ok(raid_bdev) => {
                    println!("✅ [NODE] RAID created: {}", raid_bdev);
                    raid_bdev
                }
                Err(e) => {
                    println!("❌ [NODE] Failed to create RAID: {}", e);
                    return Err(tonic::Status::internal(format!("Failed to create RAID: {}", e)));
                }
            }
        } else if volume_type == "local" {
            // Local volume - bdev is the lvol UUID
            let bdev = if let Some(bdev_name) = publish_context.get("bdevName") {
                // From ControllerPublishVolume
                println!("✅ [NODE] Local volume - using bdev from publish_context: {}", bdev_name);
                bdev_name.clone()
            } else {
                // Ephemeral volume (attachRequired=false) - query SPDK directly
                println!("📦 [NODE] Ephemeral volume - querying local SPDK");
                
                // The lvol name follows convention: vol_{volume_id}
                let lvol_name = format!("vol_{}", volume_id);
                
                // Query SPDK to find this lvol and get its UUID
                let spdk_params = serde_json::json!({
                    "method": "bdev_get_bdevs",
                    "params": {
                        "name": lvol_name
                    }
                });
                
                let bdev_response = self.driver.call_node_agent(&self.driver.node_id, "/api/spdk/rpc", &spdk_params).await
                    .map_err(|e| tonic::Status::not_found(format!("Ephemeral volume not found: {}", e)))?;
                
                let lvol_uuid = bdev_response["result"][0]["uuid"].as_str()
                    .ok_or_else(|| tonic::Status::internal("Failed to get lvol UUID from SPDK"))?
                    .to_string();
                
                println!("✅ [NODE] Found ephemeral volume: lvol={}, uuid={}", lvol_name, lvol_uuid);
                lvol_uuid
            };
            bdev
        } else if volume_type == "remote" {
            // Remote volume - need to connect to NVMe-oF target first
            println!("🌐 [NODE] Remote volume - connecting to NVMe-oF target");
            
            let nqn = publish_context.get("nqn")
                .ok_or_else(|| tonic::Status::invalid_argument("No nqn in publish context"))?;
            let target_ip = publish_context.get("targetIp")
                .ok_or_else(|| tonic::Status::invalid_argument("No targetIp in publish context"))?;
            let target_port = publish_context.get("targetPort")
                .ok_or_else(|| tonic::Status::invalid_argument("No targetPort in publish context"))?
                .parse::<u16>()
                .map_err(|e| tonic::Status::invalid_argument(format!("Invalid targetPort: {}", e)))?;
            let transport = publish_context.get("transport")
                .ok_or_else(|| tonic::Status::invalid_argument("No transport in publish context"))?;

            let conn_info = NvmeofConnectionInfo {
                nqn: nqn.clone(),
                target_ip: target_ip.clone(),
                target_port,
                transport: transport.clone(),
            };

            // Connect to NVMe-oF target
            match self.driver.connect_to_nvmeof_target(&conn_info).await {
                Ok(bdev) => {
                    println!("✅ [NODE] Connected to NVMe-oF target, bdev: {}", bdev);
                    bdev
                }
                Err(e) => {
                    println!("❌ [NODE] Failed to connect to NVMe-oF target: {}", e);
                    return Err(tonic::Status::internal(format!("Failed to connect to NVMe-oF: {}", e)));
                }
            }
        } else {
            return Err(tonic::Status::invalid_argument(format!("Unknown volume type: {}", volume_type)));
        };

        // Check if ublk device already exists (idempotency)
        let ublk_id = self.driver.generate_ublk_id(&volume_id);
        let expected_device_path = format!("/dev/ublkb{}", ublk_id);
        
        let device_path = if std::path::Path::new(&expected_device_path).exists() {
            println!("✅ [NODE] ublk device already exists (idempotent): {}", expected_device_path);
            expected_device_path
        } else {
            // Create ublk device from the bdev
            println!("🔧 [NODE] Creating ublk device for bdev: {}", bdev_name);
            
            match self.driver.create_ublk_device(&bdev_name, ublk_id).await {
                Ok(path) => {
                    println!("✅ [NODE] ublk device created: {}", path);
                    path
                }
                Err(e) => {
                    println!("❌ [NODE] Failed to create ublk device: {}", e);
                    return Err(tonic::Status::internal(format!("Failed to create ublk device: {}", e)));
                }
            }
        };
        
        // Device now exists (either created or already existed)
        {
            println!("✅ [NODE] ublk device available: {}", device_path);
            
            // SOLUTION TO UBLK ID REUSE: Detect clones by querying SPDK metadata
            //
            // PROBLEM: ublk IDs are hash-based (deterministic from volume ID)
            // When ublk device is deleted and recreated with same ID, kernel can cache
            // stale filesystem signatures from PREVIOUS volumes that used this ublk ID
            //
            // UNIFIED SOLUTION: Single "filesystem-initialized" attribute
            // - Controller sets filesystem-initialized=true for clones (snapshot/PVC)
            // - Node runs wipefs ONLY on brand new volumes (filesystem-initialized missing or false)
            // - Works for thin AND non-thin volumes (doesn't depend on allocation semantics)
            //
            
            // UNIFIED CACHE CLEARING LOGIC
            // 
            // CRITICAL: Must use filesystem-initialized attribute!
            // blkid CANNOT distinguish:
            // - SPDK block reuse (old corrupted signatures) vs
            // - Real valid filesystem (clone/restage)
            //
            // STRATEGY:
            // - Clones (filesystem-initialized=true): blockdev only, skip wipefs
            // - Regular volumes: ALWAYS wipefs (clears SPDK block reuse)
            //
            
            // Wait a moment for device to be ready
            std::thread::sleep(std::time::Duration::from_millis(300));
            
            // Check filesystem-initialized from volume_context (clones) OR PV annotations (regular volumes)
            let fs_initialized_from_context = req.volume_context.get("flint.csi.storage.io/filesystem-initialized")
                .map(|v| v == "true")
                .unwrap_or(false);
            
            // Also check PV annotations (set after formatting regular volumes)
            let fs_initialized_from_pv = self.driver.check_pv_filesystem_initialized(&volume_id).await.unwrap_or(false);
            
            let fs_initialized = fs_initialized_from_context || fs_initialized_from_pv;
            
            if fs_initialized {
                eprintln!("✅ [WIPEFS_CHECK] filesystem-initialized detected");
                eprintln!("   From volume_context: {}", fs_initialized_from_context);
                eprintln!("   From PV annotations: {}", fs_initialized_from_pv);
            } else {
                eprintln!("🆕 [WIPEFS_CHECK] Brand new volume (no filesystem-initialized marker)");
            }
            
            if fs_initialized {
                // Filesystem exists (clone/snapshot/previously formatted) - only flush cache
                eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                eprintln!("🧹 [CACHE_CLEAR] Filesystem initialized - blockdev flush only");
                eprintln!("   Device: {}", device_path);
                eprintln!("   Volume: {}", volume_id);
                eprintln!("   filesystem-initialized: true");
                eprintln!("   Action: blockdev --flushbufs (preserves filesystem)");
                eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                
                let flush_output = std::process::Command::new("blockdev")
                    .arg("--flushbufs")
                    .arg(&device_path)
                    .output();
                
                match flush_output {
                    Ok(output) if output.status.success() => {
                        eprintln!("✅ [BLOCKDEV] Kernel cache flushed successfully");
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        eprintln!("⚠️ [BLOCKDEV] Flush error (continuing): {}", stderr.trim());
                    }
                    Err(e) => {
                        eprintln!("⚠️ [BLOCKDEV] Flush failed (continuing): {}", e);
                    }
                }
            } else {
                // Brand new volume - run wipefs (clears SPDK block reuse + kernel cache)
                eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                eprintln!("🧹 [CACHE_CLEAR] Brand new volume - wipefs");
                eprintln!("   Device: {}", device_path);
                eprintln!("   Volume: {}", volume_id);
                eprintln!("   filesystem-initialized: false");
                eprintln!("   Action: wipefs (clears SPDK block reuse + kernel cache)");
                eprintln!("   Note: PV will be updated after formatting completes");
                eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                
                let wipefs_output = std::process::Command::new("wipefs")
                    .arg("--all")
                    .arg("--force")
                    .arg(&device_path)
                    .output();
                
                match wipefs_output {
                    Ok(output) if output.status.success() => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if !stdout.trim().is_empty() {
                            eprintln!("🧹 [WIPEFS] Cleared SPDK block reuse signatures:");
                            eprintln!("{}", stdout.trim());
                        } else {
                            eprintln!("✅ [WIPEFS] Device was clean (no SPDK block reuse)");
                        }
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stderr.contains("No such file") && !stderr.trim().is_empty() {
                            eprintln!("ℹ️ [WIPEFS] Output: {}", stderr.trim());
                        }
                    }
                    Err(e) => {
                        eprintln!("⚠️ [WIPEFS] Command failed (continuing): {}", e);
                    }
                }
            }
            
            println!("🔍 [NODE] Checking filesystem state from lvol");
                
                // Create staging directory if it doesn't exist
                if let Err(e) = std::fs::create_dir_all(&staging_target_path) {
                    println!("⚠️ [NODE] Failed to create staging directory (may exist): {}", e);
                }

                // For filesystem volumes, format and mount the device
                // Check if this is a filesystem volume by looking at volume_capability
                if let Some(volume_capability) = req.volume_capability {
                    if let Some(access_type) = volume_capability.access_type {
                        match access_type {
                            spdk_csi_driver::csi::volume_capability::AccessType::Mount(mount_config) => {
                                let fs_type = if mount_config.fs_type.is_empty() {
                                    "ext4".to_string()
                                } else {
                                    mount_config.fs_type
                                };
                                
                                // Wait a moment for device to be ready
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                
                                // Check if device already has a valid filesystem
                                // This preserves data across pod migrations and restages
                                let blkid_output = std::process::Command::new("blkid")
                                    .arg(&device_path)
                                    .output()
                                    .map_err(|e| tonic::Status::internal(format!("Failed to check filesystem: {}", e)))?;
                                
                                let has_filesystem = blkid_output.status.success();
                                
                                let should_format = if has_filesystem {
                                    let blkid_info = String::from_utf8_lossy(&blkid_output.stdout);
                                    println!("📁 [NODE] Device {} already has filesystem: {}", device_path, blkid_info.trim());
                                    
                                    // GEOMETRY MISMATCH DETECTION
                                    // Get actual device size
                                    let blockdev_output = std::process::Command::new("blockdev")
                                        .arg("--getsize64")
                                        .arg(&device_path)
                                        .output()
                                        .ok();
                                    
                                    let mut needs_reformat = false;
                                    
                                    if let Some(output) = blockdev_output {
                                        if let Ok(size_str) = String::from_utf8(output.stdout) {
                                            if let Ok(device_size) = size_str.trim().parse::<u64>() {
                                                // Get filesystem size for ext4
                                                let fs_size_output = std::process::Command::new("dumpe2fs")
                                                    .arg("-h")
                                                    .arg(&device_path)
                                                    .output()
                                                    .ok();
                                                
                                                if let Some(fs_output) = fs_size_output {
                                                    let fs_info = String::from_utf8_lossy(&fs_output.stdout);
                                                    // Parse block count and block size
                                                    let mut block_count = 0u64;
                                                    let mut block_size = 0u64;
                                                    
                                                    for line in fs_info.lines() {
                                                        if line.starts_with("Block count:") {
                                                            block_count = line.split_whitespace().nth(2)
                                                                .and_then(|s| s.parse().ok()).unwrap_or(0);
                                                        }
                                                        if line.starts_with("Block size:") {
                                                            block_size = line.split_whitespace().nth(2)
                                                                .and_then(|s| s.parse().ok()).unwrap_or(0);
                                                        }
                                                    }
                                                    
                                                    let fs_size = block_count * block_size;
                                                    
                                                    if fs_size > 0 && device_size > 0 {
                                                        // CRITICAL: Only reformat if filesystem thinks it's LARGER than device
                                                        // If device > filesystem, that's normal during volume expansion
                                                        // (NodeExpandVolume will resize the filesystem later)
                                                        if fs_size > device_size {
                                                            let size_diff = fs_size - device_size;
                                                            let diff_percent = (size_diff as f64 / device_size as f64) * 100.0;
                                                            
                                                            if diff_percent > 10.0 {
                                                                println!("⚠️ [NODE] GEOMETRY MISMATCH DETECTED!");
                                                                println!("⚠️ [NODE] Device size: {} bytes", device_size);
                                                                println!("⚠️ [NODE] Filesystem thinks: {} bytes", fs_size);
                                                                println!("⚠️ [NODE] Difference: {:.1}%", diff_percent);
                                                                println!("🔧 [NODE] This indicates ublk ID reuse - will reformat to fix");
                                                                needs_reformat = true;
                                                            }
                                                        } else if device_size > fs_size {
                                                            let diff_percent = ((device_size - fs_size) as f64 / device_size as f64) * 100.0;
                                                            println!("✅ [NODE] Device larger than filesystem (diff: {:.1}%) - normal for expansion", diff_percent);
                                                            println!("   Device: {} bytes, Filesystem: {} bytes", device_size, fs_size);
                                                            println!("   NodeExpandVolume will resize filesystem after mounting");
                                                        } else {
                                                            println!("✅ [NODE] Filesystem size matches device exactly");
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    
                                    // Verify filesystem type matches
                                    if !needs_reformat && !blkid_info.contains(&format!("TYPE=\"{}\"", fs_type)) {
                                        println!("⚠️ [NODE] Warning: Expected {} but found different filesystem type", fs_type);
                                        println!("⚠️ [NODE] This may indicate ublk ID reuse");
                                        needs_reformat = true;
                                    }
                                    
                                    if !needs_reformat {
                                        println!("✅ [NODE] Preserving existing filesystem (data persistence)");
                                    }
                                    
                                    needs_reformat
                                } else {
                                    println!("📁 [NODE] No filesystem found on {}", device_path);
                                    true // Need to format
                                };
                                
                                if should_format {
                                    println!("🔧 [NODE] Formatting device {} with {}", device_path, fs_type);
                                    
                                    let mkfs_output = if fs_type == "ext4" {
                                        std::process::Command::new("mkfs.ext4")
                                            .arg("-F")  // Force - don't ask for confirmation
                                            .arg(&device_path)
                                            .output()
                                            .map_err(|e| tonic::Status::internal(format!("Failed to format device: {}", e)))?
                                    } else if fs_type == "xfs" {
                                        std::process::Command::new("mkfs.xfs")
                                            .arg("-f")  // Force
                                            .arg(&device_path)
                                            .output()
                                            .map_err(|e| tonic::Status::internal(format!("Failed to format device: {}", e)))?
                                    } else {
                                        std::process::Command::new(format!("mkfs.{}", fs_type))
                                            .arg(&device_path)
                                            .output()
                                            .map_err(|e| tonic::Status::internal(format!("Failed to format device: {}", e)))?
                                    };
                                    
                                    if !mkfs_output.status.success() {
                                        let error = String::from_utf8_lossy(&mkfs_output.stderr);
                                        println!("❌ [NODE] Format failed: {}", error);
                                        return Err(tonic::Status::internal(format!("Failed to format device: {}", error)));
                                    }
                                    println!("✅ [NODE] Device formatted successfully with {}", fs_type);
                                    
                                    // CRITICAL: Update PV to mark filesystem as initialized
                                    // This prevents wipefs from running on future restaging
                                    // Skip for ephemeral volumes (no PV exists)
                                    if !fs_initialized && !is_ephemeral {
                                        println!("📝 [NODE] Updating PV to mark filesystem as initialized...");
                                        match self.driver.update_pv_filesystem_initialized(&volume_id).await {
                                            Ok(_) => {
                                                println!("✅ [NODE] PV updated with filesystem-initialized=true");
                                            }
                                            Err(e) => {
                                                println!("⚠️ [NODE] Failed to update PV (continuing): {}", e);
                                                println!("   Volume will work but wipefs may run on next restaging");
                                            }
                                        }
                                    } else if is_ephemeral {
                                        println!("ℹ️ [NODE] Skipping PV update (ephemeral volume - no PV exists)");
                                    }
                                }
                                
                                // Check if already mounted (idempotency)
                                let is_mounted = std::process::Command::new("mountpoint")
                                    .arg("-q")
                                    .arg(&staging_target_path)
                                    .status()
                                    .map(|s| s.success())
                                    .unwrap_or(false);
                                
                                if is_mounted {
                                    println!("✅ [NODE] Staging path already mounted (idempotent)");
                                } else {
                                    // Mount the device to staging path
                                    println!("🔧 [NODE] Mounting {} to {}", device_path, staging_target_path);
                                    let mount_output = std::process::Command::new("mount")
                                        .arg(&device_path)
                                        .arg(&staging_target_path)
                                        .output()
                                        .map_err(|e| tonic::Status::internal(format!("Failed to mount device: {}", e)))?;
                                    
                                    if !mount_output.status.success() {
                                        let error = String::from_utf8_lossy(&mount_output.stderr);
                                        println!("❌ [NODE] Mount failed: {}", error);
                                        return Err(tonic::Status::internal(format!("Failed to mount device: {}", error)));
                                    }
                                    
                                    println!("✅ [NODE] Device mounted to staging path");
                                }
                            }
                            spdk_csi_driver::csi::volume_capability::AccessType::Block(_) => {
                                println!("ℹ️ [NODE] Block volume - no filesystem mounting needed");
                            }
                        }
                    }
                }

        println!("✅ [NODE] Volume {} staged successfully", volume_id);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodeStageVolumeResponse {});
        println!("🔵 [GRPC] NodeStageVolume returning success response");
        Ok(response)
        }
    }

    async fn node_unstage_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodeUnstageVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeUnstageVolumeResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeUnstageVolume called");
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let staging_target_path = req.staging_target_path.clone();
        
        println!("📤 [NODE] Unstaging volume {} from {}", volume_id, staging_target_path);

        // Check if staging path is actually mounted before attempting unmount
        if std::path::Path::new(&staging_target_path).exists() {
            let is_mounted = std::process::Command::new("mountpoint")
                .arg("-q")
                .arg(&staging_target_path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            
            if is_mounted {
                println!("🔧 [NODE] Staging path is mounted, attempting unmount: {}", staging_target_path);
                
                // Try normal unmount with retry (3 attempts)
                let mut unmount_success = false;
                for attempt in 1..=3 {
                    println!("🔄 [NODE] Unmount attempt {}/3", attempt);
                    let success = std::process::Command::new("umount")
                        .arg(&staging_target_path)
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    
                    if success {
                        println!("✅ [NODE] Unmount succeeded on attempt {}", attempt);
                        unmount_success = true;
                        break;
                    }
                    
                    if attempt < 3 {
                        println!("⚠️ [NODE] Unmount failed, retrying in 100ms...");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                
                // If normal unmount failed, try lazy unmount as fallback
                if !unmount_success {
                    println!("⚠️ [NODE] Normal unmount failed, trying lazy unmount (-l)...");
                    let lazy_success = std::process::Command::new("umount")
                        .arg("-l")
                        .arg(&staging_target_path)
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    
                    if lazy_success {
                        println!("✅ [NODE] Lazy unmount succeeded, waiting for cleanup...");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    } else {
                        println!("❌ [NODE] Lazy unmount also failed");
                    }
                }
                
                // CRITICAL: Verify unmount was successful before proceeding
                let still_mounted = std::process::Command::new("mountpoint")
                    .arg("-q")
                    .arg(&staging_target_path)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                
                if still_mounted {
                    return Err(tonic::Status::internal(
                        format!("Failed to unmount staging path: {} - refusing to delete ublk device to prevent ghost mount", 
                                staging_target_path)
                    ));
                }
                
                println!("✅ [NODE] Verified staging path is no longer mounted");
            } else {
                println!("ℹ️ [NODE] Staging path exists but is not mounted");
            }
        } else {
            println!("ℹ️ [NODE] Staging path does not exist, skipping unmount");
        }

        // Only delete the ublk device after verified unmount
        let ublk_id = self.driver.generate_ublk_id(&volume_id);
        
        match self.driver.delete_ublk_device(ublk_id).await {
            Ok(_) => {
                println!("✅ [NODE] ublk device stopped successfully");
            }
            Err(e) => {
                println!("⚠️ [NODE] Failed to stop ublk device (may not exist): {}", e);
                // Continue anyway - best effort cleanup
            }
        }

        // Disconnect from NVMe-oF if this was a remote volume
        let nqn = format!("nqn.2024-11.com.flint:volume:{}", volume_id);
        if let Err(e) = self.driver.disconnect_from_nvmeof_target(&nqn).await {
            println!("⚠️ [NODE] Failed to disconnect from NVMe-oF (may not be connected): {}", e);
            // Continue anyway - best effort cleanup
        }

        println!("✅ [NODE] Volume {} unstaged successfully", volume_id);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodeUnstageVolumeResponse {});
        println!("🔵 [GRPC] NodeUnstageVolume returning success response");
        Ok(response)
    }

    async fn node_publish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodePublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodePublishVolumeResponse>, tonic::Status> {
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("🔵 [GRPC] *** NodePublishVolume CALLED ***");
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        let req = request.into_inner();
        eprintln!("📦 [NODE_PUBLISH] Volume ID: {}", req.volume_id);
        eprintln!("📦 [NODE_PUBLISH] Target path: {}", req.target_path);
        let volume_id = req.volume_id.clone();
        let target_path = req.target_path.clone();
        let staging_target_path = req.staging_target_path.clone();
        
        // Check if this is an ephemeral volume
        let is_ephemeral = req.volume_context.get("csi.storage.k8s.io/ephemeral")
            .map(|v| v == "true")
            .unwrap_or(false);
        
        if is_ephemeral {
            println!("📦 [NODE] Publishing EPHEMERAL volume {} to {}", volume_id, target_path);
        } else {
            println!("📋 [NODE] Publishing volume {} to {}", volume_id, target_path);
        }
        println!("📋 [NODE] Staging path: {}", staging_target_path);

        // Create target directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&target_path) {
            println!("⚠️ [NODE] Failed to create target directory (may exist): {}", e);
        }

        // Determine if this is a filesystem or block volume  
        let is_block_volume = if let Some(ref volume_capability) = req.volume_capability {
            matches!(volume_capability.access_type, 
                Some(spdk_csi_driver::csi::volume_capability::AccessType::Block(_)))
        } else {
            false // Default to filesystem
        };

        // Check if staging was skipped (happens with attachRequired=false for ephemeral volumes)
        let staging_skipped = staging_target_path.is_empty();
        
        if staging_skipped {
            // EPHEMERAL VOLUME: No staging - mount directly
            println!("📦 [NODE] No staging path - ephemeral volume, mounting directly");
            
            // For ephemeral volumes, NodeStageVolume is not called, so we need to:
            // 1. Query SPDK directly for volume info (no PV exists for ephemeral volumes)
            // 2. Create ublk device  
            // 3. Format filesystem
            // 4. Mount to target
            
            // Query local SPDK for ephemeral volume (no PV exists)
            println!("📦 [NODE] Querying local SPDK for ephemeral volume: {}", volume_id);
            
            // The lvol name follows convention: vol_{volume_id}
            let lvol_name = format!("vol_{}", volume_id);
            
            // Query SPDK via node agent to find this lvol
            let spdk_params = serde_json::json!({
                "method": "bdev_get_bdevs",
                "params": {
                    "name": lvol_name
                }
            });
            
            let bdev_response = self.driver.call_node_agent(&self.driver.node_id, "/api/spdk/rpc", &spdk_params).await
                .map_err(|e| tonic::Status::not_found(format!("Ephemeral volume not found in SPDK: {}", e)))?;
            
            let lvol_uuid = bdev_response["result"][0]["uuid"].as_str()
                .ok_or_else(|| tonic::Status::internal("Failed to get lvol UUID from SPDK"))?
                .to_string();
            
            println!("✅ [NODE] Found ephemeral volume: lvol={}, uuid={}", lvol_name, lvol_uuid);
            
            let ublk_id = self.driver.generate_ublk_id(&volume_id);
            println!("📦 [NODE] Creating ublk device {} for lvol {}", ublk_id, lvol_uuid);
            
            // Create ublk device
            self.driver.create_ublk_device(&lvol_uuid, ublk_id).await
                .map_err(|e| tonic::Status::internal(format!("Failed to create ublk device: {}", e)))?;
            
            let device_path = format!("/dev/ublkb{}", ublk_id);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            
            if is_block_volume {
                // Block mode - bind mount device
                println!("📋 [NODE] Ephemeral block volume - bind mounting {}", device_path);
                let mount_output = std::process::Command::new("mount")
                    .args(["--bind", &device_path, &target_path])
                    .output()
                    .map_err(|e| tonic::Status::internal(format!("Failed to mount: {}", e)))?;
                
                if !mount_output.status.success() {
                    let error = String::from_utf8_lossy(&mount_output.stderr);
                    return Err(tonic::Status::internal(format!("Mount failed: {}", error)));
                }
            } else {
                // Filesystem mode - format and mount
                let fs_type = req.volume_capability
                    .as_ref()
                    .and_then(|vc| {
                        if let Some(spdk_csi_driver::csi::volume_capability::AccessType::Mount(ref m)) = vc.access_type {
                            Some(m.fs_type.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "ext4".to_string());
                
                println!("📦 [NODE] Formatting ephemeral volume with {}", fs_type);
                
                // Format the device
                let format_cmd = format!("mkfs.{}", fs_type);
                let format_output = std::process::Command::new(&format_cmd)
                    .arg(&device_path)
                    .output()
                    .map_err(|e| tonic::Status::internal(format!("Failed to format: {}", e)))?;
                
                if !format_output.status.success() {
                    let error = String::from_utf8_lossy(&format_output.stderr);
                    return Err(tonic::Status::internal(format!("Format failed: {}", error)));
                }
                
                println!("✅ [NODE] Device formatted, mounting to {}", target_path);
                
                // Mount directly to target
                let mount_output = std::process::Command::new("mount")
                    .args([&device_path, &target_path])
                    .output()
                    .map_err(|e| tonic::Status::internal(format!("Failed to mount: {}", e)))?;
                
                if !mount_output.status.success() {
                    let error = String::from_utf8_lossy(&mount_output.stderr);
                    return Err(tonic::Status::internal(format!("Mount failed: {}", error)));
                }
            }
        } else if is_block_volume {
            // WITH STAGING: Block volume - bind mount the device directly
            let ublk_id = self.driver.generate_ublk_id(&volume_id);
            let device_path = format!("/dev/ublkb{}", ublk_id);
            
            println!("📋 [NODE] Block volume - bind mounting device {} to {}", device_path, target_path);
            
            if !std::path::Path::new(&device_path).exists() {
                println!("❌ [NODE] ublk device {} does not exist", device_path);
                return Err(tonic::Status::internal(format!("ublk device {} not found", device_path)));
            }
            
            let mount_output = std::process::Command::new("mount")
                .args(["--bind", &device_path, &target_path])
                .output()
                .map_err(|e| tonic::Status::internal(format!("Failed to execute mount: {}", e)))?;

            if !mount_output.status.success() {
                let error = String::from_utf8_lossy(&mount_output.stderr);
                println!("❌ [NODE] Mount failed: {}", error);
                return Err(tonic::Status::internal(format!("Failed to mount: {}", error)));
            }
        } else {
            // WITH STAGING: Filesystem volume - bind mount from staging path
            println!("📋 [NODE] Filesystem volume - bind mounting staging path to target");
            
            // Verify staging path exists and is mounted
            if !std::path::Path::new(&staging_target_path).exists() {
                println!("❌ [NODE] Staging path {} does not exist", staging_target_path);
                return Err(tonic::Status::internal(format!("Staging path {} not found", staging_target_path)));
            }
            
            let mount_output = std::process::Command::new("mount")
                .args(["--bind", &staging_target_path, &target_path])
                .output()
                .map_err(|e| tonic::Status::internal(format!("Failed to execute mount: {}", e)))?;

            if !mount_output.status.success() {
                let error = String::from_utf8_lossy(&mount_output.stderr);
                println!("❌ [NODE] Mount failed: {}", error);
                return Err(tonic::Status::internal(format!("Failed to mount: {}", error)));
            }
        }

        println!("✅ [NODE] Volume {} published successfully at {}", volume_id, target_path);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodePublishVolumeResponse {});
        println!("🔵 [GRPC] NodePublishVolume returning success response");
        Ok(response)
    }

    async fn node_unpublish_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodeUnpublishVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeUnpublishVolumeResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeUnpublishVolume called");
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        let target_path = req.target_path.clone();
        
        println!("📤 [NODE] Unpublishing volume {} from {}", volume_id, target_path);
        println!("🔍 [DEBUG] Target path: {}", target_path);

        // Check if target path exists BEFORE unmount
        let path_exists_before = std::path::Path::new(&target_path).exists();
        println!("🔍 [DEBUG] Target path exists before unmount: {}", path_exists_before);
        
        if path_exists_before {
            // Check if it's actually mounted
            let mount_check = std::process::Command::new("mountpoint")
                .arg("-q")
                .arg(&target_path)
                .status();
            let is_mounted = mount_check.map(|s| s.success()).unwrap_or(false);
            println!("🔍 [DEBUG] Target path is mounted: {}", is_mounted);
            
            if is_mounted {
                println!("🔧 [NODE] Unmounting target path: {}", target_path);
                let umount_output = std::process::Command::new("umount")
                    .arg(&target_path)
                    .output()
                    .map_err(|e| tonic::Status::internal(format!("Failed to execute umount: {}", e)))?;
                
                if !umount_output.status.success() {
                    let error = String::from_utf8_lossy(&umount_output.stderr);
                    let stdout = String::from_utf8_lossy(&umount_output.stdout);
                    println!("⚠️ [NODE] Unmount failed - stderr: {}", error);
                    println!("⚠️ [NODE] Unmount failed - stdout: {}", stdout);
                    println!("⚠️ [NODE] Unmount exit code: {:?}", umount_output.status.code());
                    // Continue anyway - best effort cleanup
                } else {
                    println!("✅ [NODE] Target path unmounted successfully");
                    
                    // Verify it's actually unmounted
                    let verify_mount = std::process::Command::new("mountpoint")
                        .arg("-q")
                        .arg(&target_path)
                        .status();
                    let still_mounted = verify_mount.map(|s| s.success()).unwrap_or(false);
                    if still_mounted {
                        println!("⚠️ [NODE] WARNING: Path still shows as mounted after umount!");
                    } else {
                        println!("✅ [NODE] Verified: Target path is no longer mounted");
                    }
                }
            } else {
                println!("ℹ️ [NODE] Target path exists but is not mounted, skipping umount");
            }
            
            // Check directory state before removal
            let is_dir = std::path::Path::new(&target_path).is_dir();
            println!("🔍 [DEBUG] Target path is directory: {}", is_dir);
            
            // Try to remove the directory
            match std::fs::remove_dir(&target_path) {
                Ok(_) => {
                    println!("✅ [NODE] Target directory removed successfully");
                    
                    // Verify removal
                    let still_exists = std::path::Path::new(&target_path).exists();
                    if still_exists {
                        println!("⚠️ [NODE] WARNING: Directory still exists after removal!");
                    } else {
                        println!("✅ [NODE] Verified: Directory no longer exists");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Directory already gone - this is success!
                    println!("ℹ️ [NODE] Target directory already removed (not an error)");
                }
                Err(e) => {
                    println!("⚠️ [NODE] Failed to remove target directory: {}", e);
                    println!("🔍 [DEBUG] Error kind: {:?}", e.kind());
                    // Check if directory still exists and what's in it
                    if std::path::Path::new(&target_path).exists() {
                        if let Ok(entries) = std::fs::read_dir(&target_path) {
                            let count = entries.count();
                            println!("🔍 [DEBUG] Directory still exists with {} entries", count);
                            // If directory not empty, we can't remove it
                            // This might be why kubelet retries!
                            if count > 0 {
                                println!("⚠️ [NODE] CRITICAL: Directory not empty! This may cause kubelet retries!");
                            }
                        }
                    }
                }
            }
        } else {
            println!("ℹ️ [NODE] Target path does not exist, nothing to clean up");
        }
        
        // Final state check
        let path_exists_after = std::path::Path::new(&target_path).exists();
        println!("🔍 [DEBUG] Target path exists after cleanup: {}", path_exists_after);

        println!("✅ [NODE] Volume {} unpublished successfully", volume_id);
        
        let response = tonic::Response::new(spdk_csi_driver::csi::NodeUnpublishVolumeResponse {});
        println!("🔵 [GRPC] NodeUnpublishVolume returning success response");
        Ok(response)
    }

    async fn node_get_volume_stats(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetVolumeStatsRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetVolumeStatsResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeGetVolumeStats called");
        Err(tonic::Status::unimplemented("Node get volume stats not implemented"))
    }

    async fn node_expand_volume(
        &self,
        request: tonic::Request<spdk_csi_driver::csi::NodeExpandVolumeRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeExpandVolumeResponse>, tonic::Status> {
        let req = request.into_inner();
        println!("🔵 [GRPC] Node.NodeExpandVolume called for volume: {}", req.volume_id);
        println!("   Volume path: {}", req.volume_path);
        println!("   Capacity range: {:?}", req.capacity_range);
        
        // Get the target capacity
        let target_bytes = req.capacity_range
            .as_ref()
            .and_then(|cr| Some(cr.required_bytes))
            .unwrap_or(0);
        
        println!("   Target capacity: {} bytes", target_bytes);
        
        // The volume_path is the mount point (e.g., /var/lib/kubelet/pods/.../volumes/...)
        // We need to find the underlying block device and resize the filesystem
        
        // Find the block device for this mount point
        let findmnt_output = std::process::Command::new("findmnt")
            .args(&["-n", "-o", "SOURCE", &req.volume_path])
            .output()
            .map_err(|e| tonic::Status::internal(format!("Failed to find block device: {}", e)))?;
        
        if !findmnt_output.status.success() {
            return Err(tonic::Status::internal("Failed to find mount source"));
        }
        
        let block_device = String::from_utf8_lossy(&findmnt_output.stdout).trim().to_string();
        println!("   Block device: {}", block_device);
        
        // Detect filesystem type
        let blkid_output = std::process::Command::new("blkid")
            .args(&["-o", "value", "-s", "TYPE", &block_device])
            .output()
            .map_err(|e| tonic::Status::internal(format!("Failed to detect filesystem: {}", e)))?;
        
        let fs_type = String::from_utf8_lossy(&blkid_output.stdout).trim().to_string();
        println!("   Detected filesystem type: {}", fs_type);
        
        // Resize based on filesystem type
        // The underlying block device should already be resized by ControllerExpandVolume
        let result = if fs_type == "ext4" || fs_type == "ext3" || fs_type == "ext2" {
            // For ext filesystems, use resize2fs on the block device
            println!("   Running resize2fs on {}", block_device);
            std::process::Command::new("resize2fs")
                .arg(&block_device)
                .output()
        } else if fs_type == "xfs" {
            // For XFS, use xfs_growfs on the mount point
            println!("   Running xfs_growfs on {}", req.volume_path);
            std::process::Command::new("xfs_growfs")
                .arg(&req.volume_path)
                .output()
        } else {
            return Err(tonic::Status::unimplemented(format!("Unsupported filesystem type: {}", fs_type)));
        };
        
        match result {
            Ok(output) if output.status.success() => {
                println!("✅ [GRPC] Filesystem resized successfully");
                println!("   Output: {}", String::from_utf8_lossy(&output.stdout));
                Ok(tonic::Response::new(spdk_csi_driver::csi::NodeExpandVolumeResponse {
                    capacity_bytes: target_bytes,
                }))
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("❌ [GRPC] Filesystem resize failed: {}", stderr);
                Err(tonic::Status::internal(format!("Filesystem resize failed: {}", stderr)))
            }
            Err(e) => {
                eprintln!("❌ [GRPC] Failed to execute resize command: {}", e);
                Err(tonic::Status::internal(format!("Failed to execute resize command: {}", e)))
            }
        }
    }

    async fn node_get_capabilities(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetCapabilitiesRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetCapabilitiesResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeGetCapabilities called");
        use spdk_csi_driver::csi::{node_service_capability::rpc::Type as RpcType, NodeServiceCapability, node_service_capability::Rpc};
        
        let capabilities = vec![
            NodeServiceCapability {
                r#type: Some(spdk_csi_driver::csi::node_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::StageUnstageVolume as i32,
                })),
            },
            NodeServiceCapability {
                r#type: Some(spdk_csi_driver::csi::node_service_capability::Type::Rpc(Rpc {
                    r#type: RpcType::ExpandVolume as i32,
                })),
            },
        ];
        
        println!("✅ [GRPC] Node.NodeGetCapabilities returning: StageUnstageVolume, ExpandVolume capabilities");
        Ok(tonic::Response::new(spdk_csi_driver::csi::NodeGetCapabilitiesResponse { capabilities }))
    }

    async fn node_get_info(
        &self,
        _request: tonic::Request<spdk_csi_driver::csi::NodeGetInfoRequest>,
    ) -> Result<tonic::Response<spdk_csi_driver::csi::NodeGetInfoResponse>, tonic::Status> {
        println!("🔵 [GRPC] Node.NodeGetInfo called");
        Ok(tonic::Response::new(spdk_csi_driver::csi::NodeGetInfoResponse {
            node_id: self.driver.node_id.clone(),
            max_volumes_per_node: 0, // 0 means unlimited
            accessible_topology: None,
        }))
    }
}
