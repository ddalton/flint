// node.rs - CSI Node service implementation with dynamic RAID1 creation via NVMe-oF
use std::sync::Arc;
use std::collections::HashMap;
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
                // Local replica: use direct lvol access for better performance
                if let Some(lvol_uuid) = &replica.lvol_uuid {
                    let lvs_name = format!("lvs_{}", replica.disk_ref);
                    format!("{}/{}", lvs_name, lvol_uuid)
                } else {
                    return Err(Status::internal(format!("Local replica missing lvol_uuid")));
                }
            } else {
                // Remote replica: use NVMe-oF
                if let Some(nqn) = &replica.nqn {
                    let nvmf_bdev_name = format!("nvmf_{}", replica.raid_member_index);
                    
                    // Connect to remote NVMe-oF target
                    self.connect_nvmeof_target(
                        &nvmf_bdev_name,
                        nqn,
                        replica.ip.as_deref().unwrap_or("unknown"),
                        replica.port.as_deref().unwrap_or("4420"),
                        &volume.spec.nvmeof_transport.as_deref().unwrap_or("tcp"),
                    ).await?;
                    
                    nvmf_bdev_name
                } else {
                    return Err(Status::internal(format!("Remote replica missing NQN")));
                }
            };
            
            base_bdevs.push(base_bdev_name);
        }

        // Create RAID1 bdev
        call_spdk_rpc(&self.driver.spdk_rpc_url, &json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid_name,
                "raid_level": 1,
                "base_bdevs": base_bdevs,
                "strip_size_kb": 64,
                "superblock": true
            }
        })).await
        .map_err(|e| Status::internal(format!("SPDK RPC call failed: {}", e)))?;

        println!("Created RAID1 bdev '{}' with base bdevs: {:?}", raid_name, base_bdevs);
        Ok(raid_name.clone())
    }

    /// Connects to a remote NVMe-oF target
    async fn connect_nvmeof_target(
        &self,
        bdev_name: &str,
        nqn: &str,
        target_ip: &str,
        target_port: &str,
        transport: &str,
    ) -> Result<(), Status> {
        call_spdk_rpc(&self.driver.spdk_rpc_url, &json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": bdev_name,
                "trtype": transport.to_uppercase(),
                "traddr": target_ip,
                "trsvcid": target_port,
                "subnqn": nqn,
                "adrfam": "ipv4"
            }
        })).await
        .map_err(|e| Status::internal(format!("Failed to connect NVMe-oF: {}", e)))?;

        println!("Connected to NVMe-oF target: {} -> {}", nqn, bdev_name);
        Ok(())
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
                    let lvs_name = format!("lvs_{}", replica.disk_ref);
                    let lvol_bdev = format!("{}/{}", lvs_name, lvol_uuid);
                    
                    // Create ublk device for lvol
                    self.driver.create_ublk_device(&lvol_bdev, ublk_id).await
                        .map_err(|e| Status::internal(format!("Failed to create ublk device: {}", e)))?
                } else {
                    return Err(Status::internal("Local replica missing lvol_uuid"));
                }
            } else {
                // Remote replica: Still need NVMe-oF for remote access
                // First connect to remote NVMe-oF target as bdev
                let remote_bdev_name = format!("nvmf_remote_{}", volume.spec.volume_id);
                
                if let (Some(nqn), Some(ip), Some(port)) = (
                    &replica.nqn,
                    &replica.ip, 
                    &replica.port
                ) {
                    self.connect_nvmeof_target(
                        &remote_bdev_name,
                        nqn,
                        ip,
                        port,
                        &self.driver.nvmeof_transport,
                    ).await?;
                    
                    // Then expose the NVMe-oF bdev via ublk
                    self.driver.create_ublk_device(&remote_bdev_name, ublk_id).await
                        .map_err(|e| Status::internal(format!("Failed to create ublk device: {}", e)))?
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
    
    /// Clean up ublk devices on unpublish
    async fn cleanup_ublk_device(&self, volume_id: &str) -> Result<(), Status> {
        let ublk_id = self.driver.generate_ublk_id(volume_id);
        
        self.driver.delete_ublk_device(ublk_id).await
            .map_err(|e| Status::internal(format!("Failed to delete ublk device: {}", e)))?;
            
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
        if let Some(parent) = Path::new(target_path).parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| Status::internal(format!("Failed to create mount directory: {}", e)))?;
        }

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
        let output = Command::new("umount")
            .arg(mount_path)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to unmount: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("not mounted") {
                return Err(Status::internal(format!("Unmount failed: {}", stderr)));
            }
        }

        println!("Unmounted {}", mount_path);
        Ok(())
    }

    /// Clean up all SPDK resources for a volume
    async fn cleanup_spdk_resources(&self, volume: &SpdkVolume) -> Result<(), Status> {
        if volume.spec.num_replicas > 1 {
            // Multi-replica: Clean up RAID and remote connections
            self.cleanup_raid1_bdev(volume).await?;
        } else if volume.spec.num_replicas == 1 {
            // Single replica: Clean up remote NVMe-oF connection if needed
            if let Some(replica) = volume.spec.replicas.first() {
                if replica.node != self.driver.node_id {
                    // Remote replica: disconnect NVMe-oF
                    let remote_bdev_name = format!("nvmf_remote_{}", volume.spec.volume_id);
                    self.disconnect_nvmeof_bdev(&remote_bdev_name).await?;
                }
                // Local replica: no additional cleanup needed (lvol remains for future use)
            }
        }
        
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
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        let volume = volumes_api.get(&volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;

        // Update scheduling status
        self.update_volume_scheduling_status(&volume_id, true).await?;

        // Create RAID1 bdev if this is a multi-replica volume
        if volume.spec.num_replicas > 1 {
            self.create_raid1_bdev_for_volume(&volume).await?;
        }

        // Connect to the target device using ublk (instead of NVMe-oF loopback)
        let device_path = self.connect_to_target_device(&volume).await?;

        // Update volume status with ublk device info
        let ublk_id = self.driver.generate_ublk_id(&volume_id);
        self.update_volume_ublk_status(&volume_id, Some(UblkDevice {
            id: ublk_id,
            device_path: device_path.clone(),
            created_at: Utc::now().to_rfc3339(),
            node: self.driver.node_id.clone(),
        })).await?;

        // For filesystem volumes, format and mount
        if let Some(volume_capability) = req.volume_capability {
            if let Some(access_type) = volume_capability.access_type {
                match access_type {
                    volume_capability::AccessType::Mount(mount_config) => {
                        let fs_type = mount_config.fs_type;
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

        println!("Unstaging volume {} from {}", volume_id, staging_target_path);

        // Step 1: Unmount the staging path
        self.unmount_device(&staging_target_path).await.ok();

        // Step 2: Get volume information for cleanup decisions
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        match volumes_api.get(&volume_id).await {
            Ok(volume) => {
                // Step 3: Clean up ublk device (replaces NVMe-oF loopback cleanup)
                self.cleanup_ublk_device(&volume_id).await?;
                
                // Step 4: Clean up SPDK resources
                self.cleanup_spdk_resources(&volume).await?;
                
                // Step 5: Update scheduling status
                self.update_volume_scheduling_status(&volume_id, false).await?;

                // Clear ublk device status
                self.update_volume_ublk_status(&volume_id, None).await?;
            }
            Err(e) => {
                println!("Volume {} not found during unstage, skipping cleanup: {}", volume_id, e);
            }
        }

        println!("Successfully unstaged volume {}", volume_id);
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
        if let Some(parent) = Path::new(&target_path).parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| Status::internal(format!("Failed to create target directory: {}", e)))?;
        }

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
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.driver.kube_client.clone(), &self.driver.target_namespace);
        
        // Get current volume
        let volume = volumes_api.get(volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;
        
        // Update status
        let mut status = volume.status.unwrap_or_default();
        status.ublk_device = ublk_device;
        
        // Patch the status
        let patch = json!({ "status": status });
        volumes_api
            .patch_status(volume_id, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| Status::internal(format!("Failed to update volume status: {}", e)))?;
        
        Ok(())
    }
}
