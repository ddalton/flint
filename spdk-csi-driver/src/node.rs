// node.rs - CSI Node service implementation with dynamic RAID1 creation via NVMe-oF
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::Mutex;
use std::path::Path;
use crate::driver::SpdkCsiDriver;
use spdk_csi_driver::csi::{
    node_server::Node,
    *,
};
use tonic::{Request, Response, Status};
use kube::{Api, api::{Patch, PatchParams}};
use reqwest::Client as HttpClient;
use serde_json::json;
use spdk_csi_driver::models::*;
use chrono::Utc;
use tokio::fs;
use tokio::process::Command;

/// Unified SPDK RPC helper that works with both Unix sockets and HTTP
async fn call_spdk_rpc(
    spdk_rpc_url: &str,
    rpc_request: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    if spdk_rpc_url.starts_with("unix://") {
        // Unix socket connection
        use std::os::unix::net::UnixStream;
        use std::io::{Write, Read};
        
        let socket_path = spdk_rpc_url.trim_start_matches("unix://");
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
        // HTTP connection
        let http_client = HttpClient::new();
        let response = http_client
            .post(spdk_rpc_url)
            .json(rpc_request)
            .send()
            .await?;
        
        if !response.status().is_success() {
            return Err(format!("HTTP request failed with status: {}", response.status()).into());
        }
        
        let json_response: serde_json::Value = response.json().await?;
        Ok(json_response)
    }
}

pub struct NodeService {
    driver: Arc<SpdkCsiDriver>,
}

impl NodeService {
    pub fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }

    /// Creates a RAID1 bdev dynamically when a multi-replica volume is staged
    async fn create_raid1_bdev_for_volume(&self, volume: &SpdkVolume) -> Result<String, Status> {
        if volume.spec.num_replicas <= 1 {
            return Err(Status::invalid_argument("Cannot create RAID1 for single replica volume"));
        }

        let raid_name = &volume.spec.volume_id;
        let mut base_bdevs = Vec::new();

        // Prepare base bdevs for RAID1 creation
        for replica in &volume.spec.replicas {
            let base_bdev_name = if replica.node == self.driver.node_id {
                // Local replica: use the logical volume's UUID as bdev name
                if let Some(lvol_uuid) = &replica.lvol_uuid {
                    // Each replica is a separate logical volume with its own UUID
                    lvol_uuid.clone()
                } else {
                    return Err(Status::internal(format!("Local replica missing lvol_uuid")));
                }
            } else {
                // Remote replica: create NVMe-oF target on-demand, then connect
                if let Some(nqn) = &replica.nqn {
                    let nvmf_bdev_name = format!("nvmf_{}", replica.raid_member_index);
                    
                    // Ensure NVMe-oF target exists on the remote node
                    self.ensure_nvmeof_target_if_needed(replica, volume).await?;
                    
                    // Connect to remote NVMe-oF target
                    self.connect_nvmeof_target(
                        &nvmf_bdev_name,
                        nqn,
                        replica.ip.as_deref().unwrap_or("unknown"),
                        replica.port.as_deref().unwrap_or("4420"),
                        &volume.spec.nvmeof_transport.as_deref().unwrap_or("tcp"),
                        Some(&replica.node),
                    ).await?;
                    
                    nvmf_bdev_name
                } else {
                    return Err(Status::internal(format!("Remote replica missing NQN")));
                }
            };
            
            base_bdevs.push(base_bdev_name);
        }

        // Create RAID1 bdev
        // Convert base_bdevs array to space-separated string as required by SPDK
        let base_bdevs_str = base_bdevs.join(" ");
        
        call_spdk_rpc(&self.driver.spdk_rpc_url, &json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid_name,
                "raid_level": "1",  // Fixed: RAID level must be string
                "base_bdevs": base_bdevs_str,  // Fixed: Use space-separated string instead of array
                "strip_size_kb": 64,
                "superblock": true
            }
        })).await
        .map_err(|e| Status::internal(format!("SPDK RPC call failed: {}", e)))?;

        println!("Created RAID1 bdev '{}' with base bdevs: {:?}", raid_name, base_bdevs);
        Ok(raid_name.clone())
    }



    /// Connect to NVMe-oF target with comprehensive logging and metrics
    /// Create NVMe-oF target on-demand based on specific rules:
    /// - Single replica volumes: Only when pod runs on different node than replica
    /// - Multi-replica volumes: Only for remote replica members in RAID bdev
    async fn ensure_nvmeof_target_if_needed(
        &self,
        replica: &Replica,
        volume: &SpdkVolume,
    ) -> Result<(), Status> {
        // Determine if we need an NVMe-oF target
        let needs_nvmeof_target = if volume.spec.num_replicas == 1 {
            // Single replica: Only if replica is on different node than this pod
            replica.node != self.driver.node_id
        } else {
            // Multi-replica: Only if this specific replica is on a remote node
            replica.node != self.driver.node_id
        };

        if !needs_nvmeof_target {
            println!("📋 [NVMEOF_ONDEMAND] No NVMe-oF target needed for replica on node {} (pod on node {})", 
                replica.node, self.driver.node_id);
            return Ok(());
        }

        println!("🔧 [NVMEOF_ONDEMAND] Creating NVMe-oF target for remote replica on node {}", replica.node);

        // Get the target node's SPDK RPC URL
        let rpc_url = self.driver.get_rpc_url_for_node(&replica.node).await
            .map_err(|e| Status::internal(format!("Failed to get RPC URL for node {}: {}", replica.node, e)))?;

        // Create a temporary driver instance for the target node's SPDK
        let node_driver = SpdkCsiDriver {
            spdk_rpc_url: rpc_url,
            nvmeof_transport: self.driver.nvmeof_transport.clone(),
            nvmeof_target_port: self.driver.nvmeof_target_port,
            node_id: replica.node.clone(),
            target_namespace: self.driver.target_namespace.clone(),
            kube_client: self.driver.kube_client.clone(),
            spdk_node_urls: Arc::new(Mutex::new(HashMap::new())),
            ublk_target_initialized: Arc::new(Mutex::new(false)),
        };

        // Get the bdev name and NQN for the target
        let bdev_name = replica.lvol_uuid.as_ref()
            .ok_or_else(|| Status::internal("Replica missing lvol_uuid"))?;
        let nqn = replica.nqn.as_ref()
            .ok_or_else(|| Status::internal("Replica missing NQN"))?;

        // First check if the NVMe-oF target already exists to avoid unnecessary creation attempts
        match Self::check_nvmeof_subsystem_exists(&node_driver, nqn).await {
            Ok(true) => {
                println!("✅ [NVMEOF_ONDEMAND] NVMe-oF target already exists: {}", nqn);
                return Ok(());
            }
            Ok(false) => {
                println!("🔧 [NVMEOF_ONDEMAND] NVMe-oF target does not exist, creating: {}", nqn);
            }
            Err(e) => {
                println!("⚠️ [NVMEOF_ONDEMAND] Failed to check subsystem existence, proceeding with creation: {}", e);
            }
        }

        // Create the NVMe-oF target on the remote node
        match node_driver.create_nvmeof_target(bdev_name, nqn).await {
            Ok(_) => {
                println!("✅ [NVMEOF_ONDEMAND] Successfully created NVMe-oF target: {} -> {}", bdev_name, nqn);
                Ok(())
            }
            Err(e) => {
                // Enhanced error handling for different SPDK error messages
                let error_msg = e.to_string();
                if error_msg.contains("already exists") || 
                   error_msg.contains("File exists") ||
                   error_msg.contains("Subsystem NQN") && error_msg.contains("already exists") {
                    println!("✅ [NVMEOF_ONDEMAND] NVMe-oF target already exists (idempotent): {}", nqn);
                    Ok(())
                } else {
                    println!("❌ [NVMEOF_ONDEMAND] Failed to create NVMe-oF target: {}", e);
                    Err(Status::internal(format!("Failed to create NVMe-oF target on {}: {}", replica.node, e)))
                }
            }
        }
    }

    /// Check if an NVMe-oF subsystem already exists using SPDK RPC
    async fn check_nvmeof_subsystem_exists(
        node_driver: &SpdkCsiDriver,
        nqn: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [NVMEOF_CHECK] Checking if subsystem exists: {}", nqn);
        
        let rpc_request = json!({
            "method": "nvmf_get_subsystems"
        });

        let response = call_spdk_rpc(&node_driver.spdk_rpc_url, &rpc_request).await?;

        // Check for SPDK RPC errors
        if let Some(error) = response.get("error") {
            return Err(format!("SPDK RPC error: {}", error).into());
        }

        // Parse the result array and look for our NQN
        if let Some(subsystems) = response.get("result").and_then(|r| r.as_array()) {
            for subsystem in subsystems {
                if let Some(subsystem_nqn) = subsystem.get("nqn").and_then(|n| n.as_str()) {
                    if subsystem_nqn == nqn {
                        println!("✅ [NVMEOF_CHECK] Found existing subsystem: {}", nqn);
                        return Ok(true);
                    }
                }
            }
            println!("🔍 [NVMEOF_CHECK] Subsystem not found: {}", nqn);
            Ok(false)
        } else {
            Err("Invalid response format from nvmf_get_subsystems".into())
        }
    }

    async fn connect_nvmeof_target(
        &self,
        bdev_name: &str,
        nqn: &str,
        target_ip: &str,
        target_port: &str,
        transport: &str,
        target_node_name: Option<&str>,
    ) -> Result<(), Status> {
        use spdk_csi_driver::nvmeof_utils::*;
        use std::time::Instant;

        let overall_start = Instant::now();
        
        // Create structured logging context
        let ctx = NvmfContext::new(self.driver.node_id.clone(), "connect")
            .with_target(target_ip.to_string(), target_port.to_string())
            .with_nqn(nqn.to_string())
            .with_bdev(bdev_name.to_string());

        let mut metrics = NvmfMetrics::default();

        println!("{}🔗 Starting NVMe-oF target connection", ctx.log_prefix());
        println!("{}   Transport: {}", ctx.log_prefix(), transport);

        // Step 1: Test network connectivity with metrics
        match test_network_connectivity(target_ip, target_port, &ctx).await {
            Ok(duration) => {
                metrics.network_test_time_ms = Some(duration.as_millis() as u64);
            }
            Err(nvmf_error) => {
                return Err(Status::internal(nvmf_error.user_message()));
            }
        }

        // Step 2: Attempt the actual NVMe-oF connection with metrics
        println!("{}🔗 Step 2: Creating SPDK NVMe-oF connection...", ctx.log_prefix());
        let connection_start = Instant::now();
        
        let connect_payload = json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": bdev_name,
                "trtype": transport.to_lowercase(),
                "traddr": target_ip,
                "trsvcid": target_port,
                "subnqn": nqn,
                "adrfam": "ipv4"
            }
        });

        let rpc_start = Instant::now();
        let response = match call_spdk_rpc(&self.driver.spdk_rpc_url, &connect_payload).await {
            Ok(resp) => {
                metrics.rpc_call_time_ms = Some(rpc_start.elapsed().as_millis() as u64);
                resp
            }
            Err(e) => {
                let error_string = e.to_string();
                
                // Check if this is a listener access denial error 
                if error_string.contains("does not allow host") && error_string.contains("to connect at this address") {
                    println!("{}🔐 Connection failed due to missing listener, attempting to add listener...", ctx.log_prefix());
                    
                    // Try to add a listener for the specific connection address
                    match self.add_listener_for_connection(target_node_name, nqn, target_ip, target_port).await {
                        Ok(_) => {
                            println!("{}✅ Listener added, retrying connection...", ctx.log_prefix());
                            
                            // Retry the connection
                            match call_spdk_rpc(&self.driver.spdk_rpc_url, &connect_payload).await {
                                Ok(retry_resp) => {
                                    metrics.rpc_call_time_ms = Some(rpc_start.elapsed().as_millis() as u64);
                                    println!("{}✅ Connection successful after listener addition", ctx.log_prefix());
                                    retry_resp
                                }
                                Err(retry_e) => {
                                    let nvmf_error = NvmfError::from_spdk_error(&retry_e.to_string(), "bdev_nvme_attach_controller");
                                    nvmf_error.log_detailed(&ctx);
                                    return Err(Status::internal(format!("Connection failed even after adding listener: {}", nvmf_error.user_message())));
                                }
                            }
                        }
                        Err(listener_err) => {
                            println!("{}⚠️ Failed to add listener: {}", ctx.log_prefix(), listener_err);
                            let nvmf_error = NvmfError::from_spdk_error(&error_string, "bdev_nvme_attach_controller");
                            nvmf_error.log_detailed(&ctx);
                            return Err(Status::internal(nvmf_error.user_message()));
                        }
                    }
                } else {
                    let nvmf_error = NvmfError::from_spdk_error(&error_string, "bdev_nvme_attach_controller");
                    nvmf_error.log_detailed(&ctx);
                    return Err(Status::internal(nvmf_error.user_message()));
                }
            }
        };

        // Handle SPDK response with centralized error handling
        match handle_spdk_response(response, "bdev_nvme_attach_controller", &ctx).await {
            Ok(_result) => {
                metrics.connection_time_ms = Some(connection_start.elapsed().as_millis() as u64);
                println!("{}✅ SPDK connection established successfully", ctx.log_prefix());
            }
            Err(nvmf_error) => {
                // Handle "already exists" as acceptable
                if matches!(nvmf_error, NvmfError::ConnectionExists { .. }) {
                    println!("{}ℹ️ Connection already exists - proceeding", ctx.log_prefix());
                    metrics.connection_time_ms = Some(connection_start.elapsed().as_millis() as u64);
                } else {
                    return Err(Status::internal(nvmf_error.user_message()));
                }
            }
        }

        // Step 3: Verify the bdev was created with metrics
        println!("{}🔗 Step 3: Verifying remote bdev creation...", ctx.log_prefix());
        let verification_start = Instant::now();
        
        // Allow time for bdev creation
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        
        match self.verify_remote_bdev_exists(bdev_name).await {
            Ok(_) => {
                metrics.verification_time_ms = Some(verification_start.elapsed().as_millis() as u64);
                println!("{}✅ Remote bdev {} verified successfully", ctx.log_prefix(), bdev_name);
            }
            Err(e) => {
                let nvmf_error = NvmfError::BdevNotFound {
                    bdev_name: bdev_name.to_string(),
                    details: e.to_string(),
                };
                nvmf_error.log_detailed(&ctx);
                return Err(Status::internal(nvmf_error.user_message()));
            }
        }

        // Log final metrics and connection health
        metrics.total_time_ms = Some(overall_start.elapsed().as_millis() as u64);
        metrics.log_summary(&ctx);
        
        // Perform connection health check
        log_connection_health(&ctx, &self.driver.spdk_rpc_url).await;

        println!("{}✅ NVMe-oF target connection complete", ctx.log_prefix());
        Ok(())
    }

    /// Verify that a remote bdev exists after NVMe-oF connection
    async fn verify_remote_bdev_exists(&self, bdev_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [BDEV_REMOTE_VERIFY] Verifying remote bdev: {}", bdev_name);
        
        let verify_payload = json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": bdev_name
            }
        });

        let response = call_spdk_rpc(&self.driver.spdk_rpc_url, &verify_payload).await?;
        
        // Check for SPDK RPC errors first
        if let Some(error) = response.get("error") {
            return Err(format!("Failed to query bdev {}: {}", bdev_name, error).into());
        }
        
        if let Some(result) = response.get("result") {
            if let Some(bdev_list) = result.as_array() {
                if bdev_list.is_empty() {
                    return Err(format!("Remote bdev '{}' not found after connection", bdev_name).into());
                }
                
                // Show details of the remote bdev
                for bdev in bdev_list {
                    if let Some(name) = bdev.get("name").and_then(|v| v.as_str()) {
                        let size = bdev.get("num_blocks").and_then(|v| v.as_u64()).unwrap_or(0);
                        let block_size = bdev.get("block_size").and_then(|v| v.as_u64()).unwrap_or(0);
                        println!("🔍 [BDEV_REMOTE_VERIFY] Remote bdev: name={}, blocks={}, block_size={}", 
                                 name, size, block_size);
                    }
                }
            } else {
                return Err(format!("Unexpected response format for bdev {}", bdev_name).into());
            }
        } else {
            return Err(format!("No result field in SPDK response for bdev {}", bdev_name).into());
        }
        
        println!("✅ [BDEV_REMOTE_VERIFY] Remote bdev {} verified successfully", bdev_name);
        Ok(())
    }

    /// Diagnose ublk device creation failures
    async fn diagnose_ublk_failure(&self, bdev_name: &str, ublk_id: u32) {
        println!("🔍 [UBLK_DIAGNOSE] Starting ublk failure diagnosis");
        println!("🔍 [UBLK_DIAGNOSE] Target bdev: {}", bdev_name);
        println!("🔍 [UBLK_DIAGNOSE] Target ublk ID: {}", ublk_id);

        // Check 1: Verify bdev still exists
        println!("🔍 [UBLK_DIAGNOSE] Check 1: Verifying bdev exists...");
        match self.verify_remote_bdev_exists(bdev_name).await {
            Ok(_) => println!("✅ [UBLK_DIAGNOSE] Bdev exists"),
            Err(e) => println!("❌ [UBLK_DIAGNOSE] Bdev missing: {}", e),
        }

        // Check 2: List all current bdevs
        println!("🔍 [UBLK_DIAGNOSE] Check 2: Listing all available bdevs...");
        match call_spdk_rpc(&self.driver.spdk_rpc_url, &json!({
            "method": "bdev_get_bdevs",
            "params": {}
        })).await {
            Ok(response) => {
                if let Some(bdev_list) = response.as_array() {
                    println!("🔍 [UBLK_DIAGNOSE] Found {} total bdevs:", bdev_list.len());
                    for (i, bdev) in bdev_list.iter().enumerate() {
                        if let Some(name) = bdev.get("name").and_then(|v| v.as_str()) {
                            println!("🔍 [UBLK_DIAGNOSE]   {}: {}", i + 1, name);
                        }
                    }
                }
            }
            Err(e) => println!("❌ [UBLK_DIAGNOSE] Failed to list bdevs: {}", e),
        }

        // Check 3: Check if ublk ID is already in use
        println!("🔍 [UBLK_DIAGNOSE] Check 3: Checking ublk device status...");
        if std::path::Path::new(&format!("/dev/ublkc{}", ublk_id)).exists() {
            println!("⚠️ [UBLK_DIAGNOSE] ublk device /dev/ublkc{} already exists", ublk_id);
        } else {
            println!("ℹ️ [UBLK_DIAGNOSE] ublk device /dev/ublkc{} does not exist (expected)", ublk_id);
        }

        println!("🔍 [UBLK_DIAGNOSE] Diagnosis complete");
    }

    /// Deletes the RAID1 bdev and disconnects NVMe-oF targets
    async fn cleanup_raid1_bdev(&self, volume: &SpdkVolume) -> Result<(), Status> {
        let raid_name = &volume.spec.volume_id;

        // Delete RAID1 bdev
        let result = call_spdk_rpc(&self.driver.spdk_rpc_url, &json!({
            "method": "bdev_raid_delete",
            "params": { "name": raid_name }
        })).await;

        match result {
            Ok(_) => println!("Successfully deleted RAID1 bdev: {}", raid_name),
            Err(e) => eprintln!("Warning: Failed to delete RAID1 bdev {}: {}", raid_name, e),
        }

        // Disconnect remote NVMe-oF targets
        for replica in &volume.spec.replicas {
            if replica.node != self.driver.node_id {
                let nvmf_bdev_name = format!("nvmf_{}", replica.raid_member_index);
                
                let result = call_spdk_rpc(&self.driver.spdk_rpc_url, &json!({
                    "method": "bdev_nvme_detach_controller",
                    "params": { "name": nvmf_bdev_name }
                })).await;

                match result {
                    Ok(_) => {
                        println!("Disconnected NVMe-oF target: {}", nvmf_bdev_name);
                    }
                    Err(e) => {
                        eprintln!("Warning: Error disconnecting NVMe-oF {}: {}", nvmf_bdev_name, e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Updates the SpdkVolume CRD to mark pod as scheduled on this node
    async fn update_volume_scheduling_status(&self, volume_id: &str, pod_scheduled: bool) -> Result<(), Status> {
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        match volumes_api.get(volume_id).await {
            Ok(mut volume) => {
                let mut needs_update = false;
                
                // Update replicas on this node
                for replica in &mut volume.spec.replicas {
                    if replica.node == self.driver.node_id {
                        if replica.local_pod_scheduled != pod_scheduled {
                            replica.local_pod_scheduled = pod_scheduled;
                            replica.last_io_timestamp = Some(Utc::now().to_rfc3339());
                            needs_update = true;
                        }
                    }
                }

                if needs_update {
                    let patch = json!({ "spec": volume.spec });
                    volumes_api
                        .patch(volume_id, &PatchParams::default(), &Patch::Merge(patch))
                        .await
                        .map_err(|e| Status::internal(format!("Failed to update volume CRD: {}", e)))?;
                }
            }
            Err(e) => {
                return Err(Status::not_found(format!("Volume {} not found: {}", volume_id, e)));
            }
        }

        Ok(())
    }


    async fn connect_to_target_device(&self, volume: &SpdkVolume) -> Result<String, Status> {
        let ublk_id = self.driver.generate_ublk_id(&volume.spec.volume_id);
        
        let target_device = if volume.spec.num_replicas > 1 {
            // Multi-replica: Create RAID1 bdev and expose via ublk
            self.create_raid1_bdev_for_volume(volume).await?;
            let raid_bdev = &volume.spec.volume_id;
            
            // Create ublk device for RAID bdev
            self.driver.create_ublk_device(raid_bdev, ublk_id).await
                .map_err(|e| Status::internal(format!("Failed to create ublk device: {}", e)))?
        } else {
            // Single replica: Expose lvol via ublk
            let replica = volume.spec.replicas.first()
                .ok_or_else(|| Status::internal("No replicas found"))?;
            
            if replica.node == self.driver.node_id {
                // Local replica: Direct ublk exposure
                if let Some(lvol_uuid) = &replica.lvol_uuid {
                    // Use the logical volume UUID directly - this matches RAID behavior and SPDK's actual bdev naming
                    let lvol_bdev = lvol_uuid.clone();
                    
                    // Create ublk device for lvol
                    self.driver.create_ublk_device(&lvol_bdev, ublk_id).await
                        .map_err(|e| Status::internal(format!("Failed to create ublk device: {}", e)))?
                } else {
                    return Err(Status::internal("Local replica missing lvol_uuid"));
                }
            } else {
                // Remote replica: Create NVMe-oF target on-demand, then connect
                let remote_bdev_name = format!("nvmf_remote_{}", volume.spec.volume_id);
                
                if let (Some(nqn), Some(ip), Some(port)) = (
                    &replica.nqn,
                    &replica.ip, 
                    &replica.port
                ) {
                    println!("🔗 [NVMEOF_CLIENT_DEBUG] Starting NVMe-oF client connection for single replica");
                    println!("🔗 [NVMEOF_CLIENT_DEBUG] Target: {}:{}", ip, port);
                    println!("🔗 [NVMEOF_CLIENT_DEBUG] NQN: {}", nqn);
                    println!("🔗 [NVMEOF_CLIENT_DEBUG] Remote bdev name: {}", remote_bdev_name);
                    
                    // Ensure NVMe-oF target exists on the remote node
                    self.ensure_nvmeof_target_if_needed(replica, volume).await?;
                    
                    self.connect_nvmeof_target(
                        &remote_bdev_name,
                        nqn,
                        ip,
                        port,
                        &self.driver.nvmeof_transport,
                        Some(&replica.node),
                    ).await?;
                    
                    // Then expose the NVMe-oF bdev via ublk with debugging
                    println!("🔗 [UBLK_CREATE_DEBUG] Creating ublk device for remote bdev: {}", remote_bdev_name);
                    println!("🔗 [UBLK_CREATE_DEBUG] ublk ID: {}", ublk_id);
                    
                    match self.driver.create_ublk_device_enhanced(&remote_bdev_name, ublk_id).await {
                        Ok(device_path) => {
                            println!("✅ [UBLK_CREATE_DEBUG] Successfully created ublk device: {}", device_path);
                            device_path
                        }
                        Err(e) => {
                            println!("❌ [UBLK_CREATE_DEBUG] Failed to create ublk device: {}", e);
                            println!("🔍 [UBLK_CREATE_DEBUG] Attempting to diagnose the issue...");
                            
                            // Add diagnostic information
                            self.diagnose_ublk_failure(&remote_bdev_name, ublk_id).await;
                            
                            return Err(Status::internal(format!("Failed to create ublk device: {}", e)));
                        }
                    }
                } else {
                    return Err(Status::internal("Remote replica missing connection details"));
                }
            }
        };

        // Wait for device to appear
        self.wait_for_device(&target_device).await?;
        
        println!("Connected to target device: {} for volume {}", target_device, volume.spec.volume_id);
        Ok(target_device)
    }
    
    /// Clean up ublk devices on unpublish (idempotent with retry)
    async fn cleanup_ublk_device(&self, volume_id: &str) -> Result<(), Status> {
        let ublk_id = self.driver.generate_ublk_id(volume_id);
        
        println!("🗑️ [CLEANUP] Deleting ublk device {} for volume {}", ublk_id, volume_id);
        
        // Retry deletion up to 3 times for robustness
        for attempt in 1..=3 {
            match self.driver.delete_ublk_device(ublk_id).await {
                Ok(_) => {
                    println!("✅ [CLEANUP] Successfully deleted ublk device {} (attempt {})", ublk_id, attempt);
                    return Ok(());
                }
                Err(e) => {
                    let error_str = e.to_string();
                    
                    // If device doesn't exist, that's success (idempotent)
                    if error_str.contains("does not exist") || error_str.contains("not found") {
                        println!("ℹ️ [CLEANUP] ublk device {} already deleted", ublk_id);
                        return Ok(());
                    }
                    
                    // For other errors, retry or fail
                    if attempt == 3 {
                        println!("❌ [CLEANUP] Failed to delete ublk device {} after {} attempts: {}", ublk_id, attempt, error_str);
                        return Err(Status::internal(format!("Failed to delete ublk device after retries: {}", error_str)));
                    } else {
                        println!("⚠️ [CLEANUP] Attempt {} failed for ublk device {}: {}. Retrying...", attempt, ublk_id, error_str);
                        // Sleep between retries
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Waits for a device to appear in the filesystem
    async fn wait_for_device(&self, device_path: &str) -> Result<(), Status> {
        let max_retries = 30; // 30 seconds
        
        for i in 0..max_retries {
            if Path::new(device_path).exists() {
                println!("Device {} is ready", device_path);
                return Ok(());
            }
            
            if i < max_retries - 1 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
        
        Err(Status::deadline_exceeded(format!("Device {} did not appear within timeout", device_path)))
    }

    /// Formats a device if needed
    async fn format_device_if_needed(&self, device_path: &str, fs_type: &str) -> Result<(), Status> {
        // Check if device is already formatted
        let output = Command::new("blkid")
            .arg(device_path)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to check device format: {}", e)))?;

        if output.status.success() {
            // Device is already formatted
            println!("Device {} is already formatted", device_path);
            return Ok(());
        }

        // Format the device
        let format_cmd = match fs_type {
            "ext4" => vec!["mkfs.ext4", "-F", device_path],
            "xfs" => vec!["mkfs.xfs", "-f", device_path],
            _ => return Err(Status::invalid_argument(format!("Unsupported filesystem: {}", fs_type))),
        };

        let output = Command::new(format_cmd[0])
            .args(&format_cmd[1..])
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to format device: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Status::internal(format!("Format failed: {}", stderr)));
        }

        println!("Formatted device {} with {} filesystem", device_path, fs_type);
        Ok(())
    }

    /// Mounts a device to the target path
    async fn mount_device(&self, device_path: &str, target_path: &str, fs_type: &str, mount_options: &[String]) -> Result<(), Status> {
        // Create target directory
        fs::create_dir_all(target_path).await
            .map_err(|e| Status::internal(format!("Failed to create mount directory: {}", e)))?;

        // Prepare mount command
        let mut cmd_args = vec![device_path, target_path];
        
        if !fs_type.is_empty() {
            cmd_args.extend_from_slice(&["-t", fs_type]);
        }
        
        let mount_opts;
        if !mount_options.is_empty() {
            mount_opts = mount_options.join(",");
            cmd_args.extend_from_slice(&["-o", &mount_opts]);
        }

        let output = Command::new("mount")
            .args(&cmd_args)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to mount device: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Status::internal(format!("Mount failed: {}", stderr)));
        }

        println!("Mounted {} to {} ({})", device_path, target_path, fs_type);
        Ok(())
    }

    /// Unmounts a device
    async fn unmount_device(&self, mount_path: &str) -> Result<(), Status> {
        println!("🗂️ [UNMOUNT] Attempting to unmount: {}", mount_path);
        
        let output = Command::new("umount")
            .arg(mount_path)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to unmount: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("not mounted") {
                println!("❌ [UNMOUNT] Failed to unmount {}: {}", mount_path, stderr);
                return Err(Status::internal(format!("Unmount failed: {}", stderr)));
            } else {
                println!("ℹ️ [UNMOUNT] {} was not mounted (already unmounted)", mount_path);
            }
        } else {
            println!("✅ [UNMOUNT] Successfully unmounted {}", mount_path);
        }

        Ok(())
    }

    /// Clean up all SPDK resources for a volume
    async fn cleanup_spdk_resources(&self, volume: &SpdkVolume) -> Result<(), Status> {
        println!("🔧 [SPDK_CLEANUP] Starting SPDK resource cleanup for volume {}", volume.spec.volume_id);
        
        if volume.spec.num_replicas > 1 {
            // Multi-replica: Clean up RAID and remote connections
            println!("🔧 [SPDK_CLEANUP] Multi-replica volume: cleaning up RAID bdev");
            self.cleanup_raid1_bdev(volume).await?;
        } else if volume.spec.num_replicas == 1 {
            // Single replica: Clean up remote NVMe-oF connection if needed
            if let Some(replica) = volume.spec.replicas.first() {
                if replica.node != self.driver.node_id {
                    // Remote replica: disconnect NVMe-oF
                    println!("🔧 [SPDK_CLEANUP] Single remote replica: disconnecting NVMe-oF");
                    let remote_bdev_name = format!("nvmf_remote_{}", volume.spec.volume_id);
                    self.disconnect_nvmeof_bdev(&remote_bdev_name).await?;
                } else {
                    println!("🔧 [SPDK_CLEANUP] Single local replica: no additional cleanup needed");
                }
            }
        }
        
        println!("✅ [SPDK_CLEANUP] Completed SPDK resource cleanup for volume {}", volume.spec.volume_id);
        Ok(())
    }

    /// Disconnect from a remote NVMe-oF bdev
    async fn disconnect_nvmeof_bdev(&self, bdev_name: &str) -> Result<(), Status> {
        println!("Disconnecting NVMe-oF bdev: {}", bdev_name);
        
        let result = call_spdk_rpc(&self.driver.spdk_rpc_url, &json!({
            "method": "bdev_nvme_detach_controller",
            "params": {
                "name": bdev_name
            }
        })).await;

        match result {
            Ok(_) => println!("Successfully disconnected NVMe-oF bdev: {}", bdev_name),
            Err(e) => {
                let error_msg = e.to_string();
                if !error_msg.contains("No such device") {
                    eprintln!("Warning: Failed to detach NVMe-oF controller {}: {}", bdev_name, error_msg);
                }
            }
        }
        
        Ok(())
    }

    /// Add a listener for the specific connection address to fix listener access control
    async fn add_listener_for_connection(
        &self,
        target_node_name: Option<&str>,
        subsystem_nqn: &str,
        target_ip: &str,
        target_port: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔐 Adding listener to subsystem: {} for connection {}:{} (transport: {})", 
                 subsystem_nqn, target_ip, target_port, self.driver.nvmeof_transport);
        
        // Get the target node's RPC URL
        let target_rpc_url = if let Some(node_name) = target_node_name {
            self.driver.get_rpc_url_for_node(node_name).await
                .map_err(|e| format!("Failed to get RPC URL for node {}: {}", node_name, e))?
        } else {
            return Err("Target node name not provided for dynamic listener addition".into());
        };

        // Determine address family based on transport and IP
        let adrfam = Self::determine_address_family(&self.driver.nvmeof_transport, target_ip)?;
        
        let add_listener_payload = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": subsystem_nqn,
                "listen_address": {
                    "trtype": self.driver.nvmeof_transport.to_uppercase(),
                    "traddr": target_ip,  // Use the specific IP that the client is connecting to
                    "trsvcid": target_port,
                    "adrfam": adrfam
                }
            }
        });
        
        let response = call_spdk_rpc(&target_rpc_url, &add_listener_payload).await
            .map_err(|e| format!("Failed to call SPDK RPC: {}", e))?;
        
        // Check for SPDK RPC errors
        if let Some(error) = response.get("error") {
            let error_str = error.to_string();
            
            // Handle "already exists" as success
            if error_str.contains("already exists") || error_str.contains("Listener already exists") {
                println!("🔐 Listener already exists for this address (acceptable)");
                return Ok(());
            } else {
                return Err(format!("Failed to add listener to subsystem: {}", error_str).into());
            }
        }
        
        println!("🔐 Successfully added listener to subsystem");
        Ok(())
    }

    /// Determine the appropriate address family for NVMe-oF transport
    fn determine_address_family(transport: &str, target_addr: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        match transport.to_lowercase().as_str() {
            "tcp" => {
                // TCP transport: determine IPv4 vs IPv6
                if target_addr.contains(':') && !target_addr.starts_with('[') {
                    // Simple IPv6 detection (more sophisticated parsing could be added)
                    Ok("ipv6".to_string())
                } else {
                    Ok("ipv4".to_string())
                }
            }
            "rdma" => {
                // RDMA transport: could be IB, RoCE (IPv4/IPv6), or iWARP
                if target_addr.contains(':') && !target_addr.starts_with('[') {
                    Ok("ipv6".to_string()) // RoCE v2 over IPv6
                } else if target_addr.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    Ok("ipv4".to_string()) // RoCE v2 over IPv4 or iWARP
                } else {
                    // InfiniBand GID or other IB addressing
                    Ok("ib".to_string())
                }
            }
            "fc" => {
                // Fibre Channel
                Ok("fc".to_string())
            }
            _ => {
                // Default to IPv4 for unknown transports
                println!("⚠️ Unknown transport '{}', defaulting to IPv4", transport);
                Ok("ipv4".to_string())
            }
        }
    }
}

#[tonic::async_trait]
impl Node for NodeService {
    async fn node_stage_volume(
        &self,
        request: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let staging_target_path = req.staging_target_path;

        if volume_id.is_empty() || staging_target_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and staging target path are required"));
        }

        println!("Staging volume {} to {}", volume_id, staging_target_path);

        // Get volume information from CRD
        println!("🔍 [DEBUG] NodeStageVolume: Getting volume {} from namespace {}", volume_id, self.driver.target_namespace);
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| {
                println!("❌ [ERROR] NodeStageVolume: Failed to get volume {}: {:?}", volume_id, e);
                Status::not_found(format!("Volume {} not found: {}", volume_id, e))
            })?;
        
        println!("✅ [SUCCESS] NodeStageVolume: Successfully retrieved volume {}", volume_id);

        // Update scheduling status
        self.update_volume_scheduling_status(&volume_id, true).await?;

        // Create RAID1 bdev if this is a multi-replica volume
        if volume.spec.num_replicas > 1 {
            self.create_raid1_bdev_for_volume(&volume).await?;
        }

        // Connect to the target device using ublk (instead of NVMe-oF loopback)
        let device_path = self.connect_to_target_device(&volume).await?;

        // Update volume status with ublk device info
        println!("🔍 [DEBUG] NodeStageVolume: Updating volume status with ublk device info");
        let ublk_id = self.driver.generate_ublk_id(&volume_id);
        let ublk_device = UblkDevice {
            id: ublk_id,
            device_path: device_path.clone(),
            created_at: Utc::now().to_rfc3339(),
            node: self.driver.node_id.clone(),
        };
        println!("🔍 [DEBUG] NodeStageVolume: Created ublk_device: {:?}", ublk_device);
        
        self.update_volume_ublk_status(&volume_id, Some(ublk_device)).await?;
        println!("✅ [SUCCESS] NodeStageVolume: Volume status updated successfully");

        // For filesystem volumes, format and mount
        if let Some(volume_capability) = req.volume_capability {
            if let Some(access_type) = volume_capability.access_type {
                match access_type {
                    volume_capability::AccessType::Mount(mount_config) => {
                        let fs_type = if mount_config.fs_type.is_empty() {
                            "ext4".to_string()  // Default to ext4 if no filesystem specified
                        } else {
                            mount_config.fs_type
                        };
                        let mount_flags = mount_config.mount_flags;

                        // Format device if needed
                        self.format_device_if_needed(&device_path, &fs_type).await?;

                        // Mount device to staging path
                        self.mount_device(&device_path, &staging_target_path, &fs_type, &mount_flags).await?;
                    }
                    volume_capability::AccessType::Block(_) => {
                        // For block volumes, just create symlink to device
                        fs::create_dir_all(&staging_target_path).await
                            .map_err(|e| Status::internal(format!("Failed to create staging directory: {}", e)))?;

                        // Create symlink instead of bind mount for block devices
                        fs::symlink(&device_path, &format!("{}/device", staging_target_path)).await
                            .map_err(|e| Status::internal(format!("Failed to create device symlink: {}", e)))?;
                    }
                }
            }
        }

        println!("Successfully staged volume {} at {}", volume_id, staging_target_path);
        Ok(Response::new(NodeStageVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let staging_target_path = req.staging_target_path;

        if volume_id.is_empty() || staging_target_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and staging target path are required"));
        }

        println!("🚀 [UNSTAGE] Starting unstage for volume {} from {}", volume_id, staging_target_path);

        // Step 1: Unmount the staging path
        println!("📝 [UNSTAGE] Step 1: Unmounting staging path");
        if let Err(e) = self.unmount_device(&staging_target_path).await {
            println!("⚠️ [UNSTAGE] Unmount warning (non-fatal): {}", e);
        }

        // Step 2: Always clean up ublk device first (idempotent - works even if CRD is gone)
        println!("🧹 [UNSTAGE] Cleaning up ublk device for volume: {}", volume_id);
        if let Err(e) = self.cleanup_ublk_device(&volume_id).await {
            println!("⚠️ [UNSTAGE] ublk cleanup warning (non-fatal): {}", e);
            // Continue with other cleanup - don't fail the unstage operation
        }

        // Step 3: Get volume information for additional cleanup (if CRD still exists)
        println!("📝 [UNSTAGE] Step 3: Checking for volume CRD");
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        match volumes_api.get(&volume_id).await {
            Ok(volume) => {
                println!("✅ [UNSTAGE] Found volume CRD, performing complete cleanup");
                
                // Step 4: Clean up SPDK resources
                println!("📝 [UNSTAGE] Step 4: Cleaning up SPDK resources");
                if let Err(e) = self.cleanup_spdk_resources(&volume).await {
                    println!("⚠️ [UNSTAGE] SPDK cleanup warning (non-fatal): {}", e);
                }
                
                // Step 5: Update scheduling status
                println!("📝 [UNSTAGE] Step 5: Updating volume scheduling status");
                if let Err(e) = self.update_volume_scheduling_status(&volume_id, false).await {
                    println!("⚠️ [UNSTAGE] Status update warning (non-fatal): {}", e);
                }

                // Step 6: Clear ublk device status
                println!("📝 [UNSTAGE] Step 6: Clearing ublk device status");
                if let Err(e) = self.update_volume_ublk_status(&volume_id, None).await {
                    println!("⚠️ [UNSTAGE] ublk status clear warning (non-fatal): {}", e);
                }
            }
            Err(e) => {
                println!("ℹ️ [UNSTAGE] Volume {} CRD not found (may be already deleted): {}", volume_id, e);
                println!("ℹ️ [UNSTAGE] This is normal for deleted volumes - basic cleanup completed");
            }
        }

        println!("🎉 [UNSTAGE] Successfully completed unstage for volume {}", volume_id);
        Ok(Response::new(NodeUnstageVolumeResponse {}))
    }


    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let staging_target_path = req.staging_target_path;
        let target_path = req.target_path;

        if volume_id.is_empty() || target_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and target path are required"));
        }

        println!("Publishing volume {} from {} to {}", volume_id, staging_target_path, target_path);

        // Create target directory
        fs::create_dir_all(&target_path).await
            .map_err(|e| Status::internal(format!("Failed to create target directory: {}", e)))?;

        // Determine if this is a block or filesystem volume
        let is_block_volume = req.volume_capability
            .as_ref()
            .and_then(|vc| vc.access_type.as_ref())
            .map(|at| matches!(at, volume_capability::AccessType::Block(_)))
            .unwrap_or(false);

        if is_block_volume {
            // For block volumes, create a bind mount from staging to target
            let output = Command::new("mount")
                .args(["--bind", &staging_target_path, &target_path])
                .output()
                .await
                .map_err(|e| Status::internal(format!("Failed to bind mount: {}", e)))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Status::internal(format!("Bind mount failed: {}", stderr)));
            }
        } else {
            // For filesystem volumes, bind mount the staged filesystem
            let mount_options = req.volume_capability
                .and_then(|vc| match vc.access_type? {
                    volume_capability::AccessType::Mount(mount_config) => Some(mount_config.mount_flags),
                    _ => None,
                })
                .unwrap_or_default();

            let mut cmd_args = vec!["--bind", &staging_target_path, &target_path];
            let mount_opts;
            if !mount_options.is_empty() {
                mount_opts = mount_options.join(",");
                cmd_args.extend_from_slice(&["-o", &mount_opts]);
            }

            let output = Command::new("mount")
                .args(&cmd_args)
                .output()
                .await
                .map_err(|e| Status::internal(format!("Failed to publish volume: {}", e)))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Status::internal(format!("Publish mount failed: {}", stderr)));
            }
        }

        println!("Successfully published volume {} to {}", volume_id, target_path);
        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let target_path = req.target_path;

        if volume_id.is_empty() || target_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and target path are required"));
        }

        println!("Unpublishing volume {} from {}", volume_id, target_path);

        // Just unmount the target path - that's all!
        self.unmount_device(&target_path).await.ok();

        // Remove the target directory if it's empty
        fs::remove_dir(&target_path).await.ok();

        println!("Successfully unpublished volume {} from {}", volume_id, target_path);
        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_volume_stats(
        &self,
        request: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, Status> {
        let req = request.into_inner();
        let volume_path = req.volume_path;

        if volume_path.is_empty() {
            return Err(Status::invalid_argument("Volume path is required"));
        }

        // Get filesystem statistics
        let output = Command::new("df")
            .args(["-B1", &volume_path])
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to get volume stats: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Status::internal(format!("df command failed: {}", stderr)));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        
        if lines.len() < 2 {
            return Err(Status::internal("Invalid df output"));
        }

        let stats_line = lines[1];
        let parts: Vec<&str> = stats_line.split_whitespace().collect();
        
        if parts.len() < 4 {
            return Err(Status::internal("Cannot parse df output"));
        }

        let total_bytes: i64 = parts[1].parse().unwrap_or(0);
        let used_bytes: i64 = parts[2].parse().unwrap_or(0);
        let available_bytes: i64 = parts[3].parse().unwrap_or(0);

        let volume_usage = vec![VolumeUsage {
            available: available_bytes,
            total: total_bytes,
            used: used_bytes,
            unit: volume_usage::Unit::Bytes as i32,
        }];

        let volume_condition = VolumeCondition {
            abnormal: false,
            message: "Volume is healthy".to_string(),
        };

        Ok(Response::new(NodeGetVolumeStatsResponse {
            usage: volume_usage,
            volume_condition: Some(volume_condition),
        }))
    }

    async fn node_expand_volume(
        &self,
        request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = req.volume_id;
        let volume_path = req.volume_path;
        let capacity_range = req.capacity_range;

        if volume_id.is_empty() || volume_path.is_empty() {
            return Err(Status::invalid_argument("Volume ID and volume path are required"));
        }

        println!("Expanding volume {} at path {}", volume_id, volume_path);

        // Get the new capacity
        let new_capacity = capacity_range
            .as_ref()
            .map(|cr| cr.required_bytes)
            .unwrap_or(0);

        if new_capacity <= 0 {
            return Err(Status::invalid_argument("New capacity must be positive"));
        }

        // For filesystem volumes, we need to resize the filesystem
        // First, let's determine the filesystem type
        let output = Command::new("findmnt")
            .args(["-n", "-o", "FSTYPE", &volume_path])
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to determine filesystem type: {}", e)))?;

        if !output.status.success() {
            return Err(Status::internal("Could not determine filesystem type"));
        }

        let fs_type = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Resize the filesystem based on its type
        match fs_type.as_str() {
            "ext4" | "ext3" | "ext2" => {
                let output = Command::new("resize2fs")
                    .arg(&volume_path)
                    .output()
                    .await
                    .map_err(|e| Status::internal(format!("Failed to resize ext filesystem: {}", e)))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(Status::internal(format!("resize2fs failed: {}", stderr)));
                }
            }
            "xfs" => {
                let output = Command::new("xfs_growfs")
                    .arg(&volume_path)
                    .output()
                    .await
                    .map_err(|e| Status::internal(format!("Failed to resize XFS filesystem: {}", e)))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(Status::internal(format!("xfs_growfs failed: {}", stderr)));
                }
            }
            _ => {
                return Err(Status::unimplemented(format!("Filesystem resize not supported for: {}", fs_type)));
            }
        }

        println!("Successfully expanded {} filesystem for volume {}", fs_type, volume_id);

        Ok(Response::new(NodeExpandVolumeResponse {
            capacity_bytes: new_capacity,
        }))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        let capabilities = vec![
            NodeServiceCapability {
                r#type: Some(node_service_capability::Type::Rpc(
                    node_service_capability::Rpc {
                        r#type: node_service_capability::rpc::Type::StageUnstageVolume as i32,
                    },
                )),
            },
            NodeServiceCapability {
                r#type: Some(node_service_capability::Type::Rpc(
                    node_service_capability::Rpc {
                        r#type: node_service_capability::rpc::Type::GetVolumeStats as i32,
                    },
                )),
            },
            NodeServiceCapability {
                r#type: Some(node_service_capability::Type::Rpc(
                    node_service_capability::Rpc {
                        r#type: node_service_capability::rpc::Type::ExpandVolume as i32,
                    },
                )),
            },
            NodeServiceCapability {
                r#type: Some(node_service_capability::Type::Rpc(
                    node_service_capability::Rpc {
                        r#type: node_service_capability::rpc::Type::VolumeCondition as i32,
                    },
                )),
            },
        ];

        Ok(Response::new(NodeGetCapabilitiesResponse { capabilities }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        // Get node topology information
        let mut topology = HashMap::new();
        
        // Check if hostname topology is enabled via environment variable
        // Set USE_HOSTNAME_TOPOLOGY=true for self-managed clusters
        // Leave unset/false for managed clusters (Rancher, EKS, GKE, AKS)
        let use_hostname_topology = std::env::var("USE_HOSTNAME_TOPOLOGY")
            .unwrap_or_default()
            .to_lowercase() == "true";
            
        if use_hostname_topology {
            topology.insert("topology.kubernetes.io/hostname".to_string(), self.driver.node_id.clone());
        } else {
            // Safe fallback for managed clusters that protect topology.kubernetes.io labels
            topology.insert("flint.csi.storage.io/node".to_string(), self.driver.node_id.clone());
        }

        // Try to get zone information
        if let Ok(zone) = std::env::var("NODE_ZONE") {
            topology.insert("topology.kubernetes.io/zone".to_string(), zone);
        }

        // Try to get region information
        if let Ok(region) = std::env::var("NODE_REGION") {
            topology.insert("topology.kubernetes.io/region".to_string(), region);
        }

        // Add SPDK-specific topology
        topology.insert("spdk.io/nvme-transport".to_string(), self.driver.nvmeof_transport.clone());
        topology.insert("spdk.io/nvme-port".to_string(), self.driver.nvmeof_target_port.to_string());

        // Get available capacity from local disks
        let mut max_volumes_per_node = 0i64;
        
        // Query local SPDK disks to determine maximum volumes
        let disks_api: Api<SpdkDisk> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        if let Ok(disk_list) = disks_api.list(&kube::api::ListParams::default()).await {
            let local_disks: Vec<_> = disk_list.items.iter()
                .filter(|disk| disk.spec.node_id == self.driver.node_id)
                .collect();
            
            // Estimate max volumes based on disk capacity and typical volume sizes
            let total_capacity: i64 = local_disks.iter()
                .filter_map(|disk| disk.status.as_ref())
                .map(|status| status.free_space)
                .sum();
            
            // Assume 10GB average volume size for estimation
            max_volumes_per_node = total_capacity / (10 * 1024 * 1024 * 1024);
        }

        Ok(Response::new(NodeGetInfoResponse {
            node_id: self.driver.node_id.clone(),
            max_volumes_per_node,
            accessible_topology: Some(Topology {
                segments: topology,
            }),
        }))
    }
}

// Helper functions for the NodeService
impl NodeService {

    // Add method to update ublk device status
    async fn update_volume_ublk_status(
        &self,
        volume_id: &str,
        ublk_device: Option<UblkDevice>,
    ) -> Result<(), Status> {
        println!("🔍 [DEBUG] Starting update_volume_ublk_status for volume: {}", volume_id);
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        // Get current volume
        println!("🔍 [DEBUG] Getting volume {} from namespace: {}", volume_id, self.driver.target_namespace);
        let volume = volumes_api.get(volume_id).await
            .map_err(|e| {
                println!("❌ [ERROR] Failed to get volume {}: {:?}", volume_id, e);
                Status::not_found(format!("Volume {} not found: {}", volume_id, e))
            })?;
        
        println!("🔍 [DEBUG] Successfully retrieved volume, current status: {:?}", volume.status);
        
        // Update status
        let mut status = volume.status.unwrap_or_else(|| {
            println!("🔍 [DEBUG] No existing status, creating default with state='creating'");
            let mut default_status = SpdkVolumeStatus::default();
            default_status.state = "creating".to_string(); // Set valid state instead of empty string
            default_status
        });
        
        println!("🔍 [DEBUG] Current status state before update: '{}'", status.state);
        status.ublk_device = ublk_device.clone();
        println!("🔍 [DEBUG] Updated ublk_device: {:?}", ublk_device);
        
        // Patch the status
        let patch = json!({ "status": status });
        println!("🔍 [DEBUG] Attempting to patch status with: {}", serde_json::to_string_pretty(&patch).unwrap_or_else(|_| "serialization failed".to_string()));
        
        match volumes_api.patch_status(volume_id, &PatchParams::default(), &Patch::Merge(patch)).await {
            Ok(updated_volume) => {
                println!("✅ [SUCCESS] Successfully updated volume status for {}", volume_id);
                println!("🔍 [DEBUG] Updated volume status: {:?}", updated_volume.status);
                Ok(())
            }
            Err(e) => {
                println!("❌ [ERROR] Failed to patch volume status for {}: {:?}", volume_id, e);
                println!("❌ [ERROR] Error details: {}", e);
                
                // e is already a kube::Error, so we can examine it directly
                match &e {
                    kube::Error::Api(api_err) => {
                        println!("❌ [ERROR] Kubernetes API error - code: {}, message: {}", api_err.code, api_err.message);
                        println!("❌ [ERROR] API error reason: {}", api_err.reason);
                    }
                    kube::Error::Service(service_err) => {
                        println!("❌ [ERROR] Kubernetes service error: {:?}", service_err);
                    }
                    _ => {
                        println!("❌ [ERROR] Other kubernetes error type: {:?}", e);
                    }
                }
                
                Err(Status::internal(format!("Failed to update volume status: {}", e)))
            }
        }
    }
}
