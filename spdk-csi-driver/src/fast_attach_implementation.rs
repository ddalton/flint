// Complete fast attach/detach implementation with performance monitoring

use std::time::Instant;
use std::collections::HashMap;
use tokio::sync::RwLock;
use std::sync::Arc;
use tonic::{Request, Response, Status};

// Performance tracking for attach operations
#[derive(Debug, Clone)]
pub struct AttachMetrics {
    pub total_time_ms: u64,
    pub device_discovery_ms: u64,
    pub filesystem_setup_ms: u64,
    pub mount_time_ms: u64,
    pub attach_type: AttachType,
    pub cache_hit: bool,
}

#[derive(Debug, Clone)]
pub enum AttachType {
    LocalNVMe,
    NVMeoFCached,
    NVMeoFNew,
    RAIDDevice,
}

// Connection cache for NVMe-oF performance
pub struct ConnectionCache {
    nvmf_connections: Arc<RwLock<HashMap<String, NVMeoFConnection>>>,
    device_cache: Arc<RwLock<HashMap<String, String>>>, // volume_id -> device_path
}

#[derive(Debug, Clone)]
pub struct NVMeoFConnection {
    pub nqn: String,
    pub device_path: String,
    pub connected_at: std::time::SystemTime,
    pub last_used: std::time::SystemTime,
    pub pod_count: u32, // Number of pods using this connection
}

impl SpdkCsiDriver {
    // High-performance node stage implementation
    async fn node_stage_volume_fast(&self, request: Request<csi::NodeStageVolumeRequest>) 
        -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        let start_time = Instant::now();
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        
        // Fast path: Check if already staged (common for multi-attach scenarios)
        if self.is_volume_staged(&req.staging_target_path).await {
            let metrics = AttachMetrics {
                total_time_ms: start_time.elapsed().as_millis() as u64,
                device_discovery_ms: 0,
                filesystem_setup_ms: 0,
                mount_time_ms: 0,
                attach_type: AttachType::LocalNVMe, // Cached
                cache_hit: true,
            };
            self.record_attach_metrics(&volume_id, metrics).await;
            return Ok(Response::new(csi::NodeStageVolumeResponse {}));
        }

        let discovery_start = Instant::now();
        
        // Get optimal device path with performance tracking
        let (device_path, attach_type) = self.get_optimal_device_path_with_metrics(&req).await
            .map_err(|e| Status::internal(format!("Device discovery failed: {}", e)))?;
        
        let discovery_time = discovery_start.elapsed().as_millis() as u64;
        
        // Handle different volume types
        if req.volume_capability.as_ref().and_then(|vc| vc.block.as_ref()).is_some() {
            // Block device - no filesystem, immediate return
            let metrics = AttachMetrics {
                total_time_ms: start_time.elapsed().as_millis() as u64,
                device_discovery_ms: discovery_time,
                filesystem_setup_ms: 0,
                mount_time_ms: 0,
                attach_type,
                cache_hit: false,
            };
            self.record_attach_metrics(&volume_id, metrics).await;
            return Ok(Response::new(csi::NodeStageVolumeResponse {}));
        }

        // Filesystem volume - optimize filesystem operations
        let fs_start = Instant::now();
        let fs_type = req.volume_capability.as_ref()
            .and_then(|vc| vc.mount.as_ref())
            .map(|m| m.fs_type.clone())
            .unwrap_or_else(|| "ext4".to_string());

        let filesystem_time = self.setup_filesystem_fast(&device_path, &fs_type).await
            .map_err(|e| Status::internal(format!("Filesystem setup failed: {}", e)))?;

        let mount_start = Instant::now();
        self.mount_volume_fast(&device_path, &req.staging_target_path, &req).await
            .map_err(|e| Status::internal(format!("Mount failed: {}", e)))?;
        let mount_time = mount_start.elapsed().as_millis() as u64;

        let metrics = AttachMetrics {
            total_time_ms: start_time.elapsed().as_millis() as u64,
            device_discovery_ms: discovery_time,
            filesystem_setup_ms: filesystem_time,
            mount_time_ms: mount_time,
            attach_type,
            cache_hit: false,
        };
        
        self.record_attach_metrics(&volume_id, metrics).await;
        
        // Log performance for monitoring
        if metrics.total_time_ms > 1000 { // Log slow operations
            println!("SLOW ATTACH: volume={}, time={}ms, type={:?}", 
                     volume_id, metrics.total_time_ms, metrics.attach_type);
        }

        Ok(Response::new(csi::NodeStageVolumeResponse {}))
    }

    // Optimized device discovery with caching and performance tracking
    async fn get_optimal_device_path_with_metrics(&self, req: &csi::NodeStageVolumeRequest) 
        -> Result<(String, AttachType), Box<dyn std::error::Error>> {
        let volume_id = &req.volume_id;
        let context = &req.volume_context;
        
        // Check device cache first
        {
            let cache = self.device_cache.read().await;
            if let Some(cached_path) = cache.get(volume_id) {
                if self.verify_device_accessible(cached_path).await? {
                    return Ok((cached_path.clone(), AttachType::LocalNVMe));
                }
            }
        }

        // Get pod node for locality optimization
        let pod_node = self.get_current_node().await?;
        let num_replicas = context.get("numReplicas")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(2);

        // Priority 1: Local NVMe (fastest path)
        for i in 0..num_replicas {
            if let Some(nvme_addr) = context.get(&format!("nvmeAddr{}", i)) {
                if let Some(bdev_name) = context.get(&format!("lvolBdev{}", i)) {
                    if self.is_replica_local(volume_id, i, &pod_node).await? {
                        let device_path = self.setup_local_nvme_fast(nvme_addr, bdev_name).await?;
                        
                        // Cache the result
                        self.device_cache.write().await.insert(volume_id.clone(), device_path.clone());
                        
                        return Ok((device_path, AttachType::LocalNVMe));
                    }
                }
            }
        }

        // Priority 2: NVMe-oF (check cache first)
        for i in 0..num_replicas {
            if let (Some(nqn), Some(ip), Some(port)) = (
                context.get(&format!("nvmfNQN{}", i)),
                context.get(&format!("nvmfIP{}", i)),
                context.get(&format!("nvmfPort{}", i))
            ) {
                // Check if connection already exists
                {
                    let connections = self.connection_cache.nvmf_connections.read().await;
                    if let Some(conn) = connections.get(nqn) {
                        if self.verify_device_accessible(&conn.device_path).await? {
                            // Update usage tracking
                            self.update_connection_usage(nqn).await;
                            
                            return Ok((conn.device_path.clone(), AttachType::NVMeoFCached));
                        }
                    }
                }

                // Create new NVMe-oF connection
                let device_path = self.setup_nvmf_connection_fast(nqn, ip, port).await?;
                return Ok((device_path, AttachType::NVMeoFNew));
            }
        }

        // Priority 3: RAID device (fallback)
        let raid_device = format!("/dev/spdk/{}", volume_id);
        if self.verify_device_accessible(&raid_device).await? {
            return Ok((raid_device, AttachType::RAIDDevice));
        }

        Err("No accessible device path found".into())
    }

    // Optimized local NVMe setup
    async fn setup_local_nvme_fast(&self, pcie_addr: &str, bdev_name: &str) 
        -> Result<String, Box<dyn std::error::Error>> {
        
        // Check if already bound to kernel
        let sys_driver_path = format!("/sys/bus/pci/devices/{}/driver", pcie_addr);
        if std::path::Path::new(&sys_driver_path).exists() {
            // Already bound, find device quickly
            return self.find_nvme_device_fast(pcie_addr).await;
        }

        // Bind to kernel driver (typically ~10ms)
        let bind_start = Instant::now();
        self.bind_nvme_to_kernel(pcie_addr).await?;
        let bind_time = bind_start.elapsed().as_millis();
        
        if bind_time > 100 {
            println!("SLOW NVMe bind: addr={}, time={}ms", pcie_addr, bind_time);
        }

        self.find_nvme_device_fast(pcie_addr).await
    }

    // Fast NVMe-oF connection with caching
    async fn setup_nvmf_connection_fast(&self, nqn: &str, ip: &str, port: &str) 
        -> Result<String, Box<dyn std::error::Error>> {
        let connect_start = Instant::now();
        
        // Use nvme-cli with optimized parameters
        let output = tokio::process::Command::new("nvme")
            .args([
                "connect",
                "-t", "tcp",
                "-n", nqn,
                "-a", ip,
                "-s", port,
                "--hostnqn", &self.get_host_nqn(),
                "--ctrl-loss-tmo", "10", // Fast timeout for failures
                "--keep-alive-tmo", "30",
                "--reconnect-delay", "2",
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Err(format!("NVMe-oF connection failed: {}", 
                              String::from_utf8_lossy(&output.stderr)).into());
        }

        let connect_time = connect_start.elapsed().as_millis();
        if connect_time > 200 {
            println!("SLOW NVMe-oF connect: nqn={}, time={}ms", nqn, connect_time);
        }

        // Find the device path
        let device_path = self.find_nvmf_device_fast(nqn).await?;
        
        // Cache the connection
        let connection = NVMeoFConnection {
            nqn: nqn.to_string(),
            device_path: device_path.clone(),
            connected_at: std::time::SystemTime::now(),
            last_used: std::time::SystemTime::now(),
            pod_count: 1,
        };
        
        self.connection_cache.nvmf_connections.write().await
            .insert(nqn.to_string(), connection);

        Ok(device_path)
    }

    // Optimized filesystem setup
    async fn setup_filesystem_fast(&self, device_path: &str, fs_type: &str) 
        -> Result<u64, Box<dyn std::error::Error>> {
        let fs_start = Instant::now();
        
        // Check if already formatted (very common case)
        if self.is_device_formatted_fast(device_path).await? {
            return Ok(0); // No time spent
        }

        // Format with optimized parameters for speed
        let format_args = match fs_type {
            "ext4" => vec![
                "-F", // Force
                "-E", "lazy_itable_init=0,lazy_journal_init=0", // No lazy initialization
                "-O", "^has_journal", // No journal for speed (can be risky)
                device_path,
            ],
            "xfs" => vec![
                "-f", // Force
                "-K", // Don't attempt to discard blocks
                device_path,
            ],
            _ => vec!["-F", device_path],
        };

        let mut cmd = tokio::process::Command::new(&format!("mkfs.{}", fs_type));
        cmd.args(&format_args);
        
        let output = cmd.output().await?;
        if !output.status.success() {
            return Err(format!("Filesystem creation failed: {}", 
                              String::from_utf8_lossy(&output.stderr)).into());
        }

        let format_time = fs_start.elapsed().as_millis() as u64;
        if format_time > 500 {
            println!("SLOW format: device={}, fs={}, time={}ms", 
                     device_path, fs_type, format_time);
        }

        Ok(format_time)
    }

    // Fast mount with optimized options
    async fn mount_volume_fast(&self, device_path: &str, target_path: &str, req: &csi::NodeStageVolumeRequest) 
        -> Result<(), Box<dyn std::error::Error>> {
        
        // Create target directory
        tokio::fs::create_dir_all(target_path).await?;

        let volume_capability = req.volume_capability.as_ref()
            .ok_or("Missing volume capability")?;
        
        let mount_info = volume_capability.mount.as_ref()
            .ok_or("Missing mount information")?;

        // Build optimized mount flags
        let mut mount_flags = mount_info.mount_flags.clone();
        
        // Add performance optimizations
        if !mount_flags.iter().any(|f| f.starts_with("noatime")) {
            mount_flags.push("noatime".to_string()); // Disable access time updates
        }
        
        // Use optimized mount command
        let mut cmd = tokio::process::Command::new("mount");
        cmd.arg("-t").arg(&mount_info.fs_type);
        
        for flag in &mount_flags {
            cmd.arg("-o").arg(flag);
        }
        
        cmd.arg(device_path).arg(target_path);
        
        let output = cmd.output().await?;
        if !output.status.success() {
            return Err(format!("Mount failed: {}", 
                              String::from_utf8_lossy(&output.stderr)).into());
        }

        Ok(())
    }

    // Fast device verification
    async fn verify_device_accessible(&self, device_path: &str) -> Result<bool, Box<dyn std::error::Error>> {
        // Quick check - just verify the device node exists and is readable
        match tokio::fs::metadata(device_path).await {
            Ok(metadata) => {
                use std::os::unix::fs::FileTypeExt;
                Ok(metadata.file_type().is_block_device())
            },
            Err(_) => Ok(false),
        }
    }

    // Performance monitoring
    async fn record_attach_metrics(&self, volume_id: &str, metrics: AttachMetrics) {
        // Store metrics for monitoring/alerting
        println!("ATTACH_METRICS: volume={}, total={}ms, discovery={}ms, fs={}ms, mount={}ms, type={:?}, cache_hit={}", 
                 volume_id, metrics.total_time_ms, metrics.device_discovery_ms, 
                 metrics.filesystem_setup_ms, metrics.mount_time_ms, 
                 metrics.attach_type, metrics.cache_hit);
        
        // Send to performance monitor
        if let Some(monitor) = &self.performance_monitor {
            monitor.record_operation(metrics).await;
        }
    }

    // Fast detach implementation
    async fn node_unstage_volume_fast(&self, request: Request<csi::NodeUnstageVolumeRequest>) 
        -> Result<Response<csi::NodeUnstageVolumeResponse>, Status> {
        let start_time = Instant::now();
        let req = request.into_inner();
        
        // Fast unmount
        let unmount_result = tokio::process::Command::new("umount")
            .arg(&req.staging_target_path)
            .output()
            .await;

        match unmount_result {
            Ok(output) if output.status.success() => {},
            Ok(output) => {
                let error = String::from_utf8_lossy(&output.stderr);
                if !error.contains("not mounted") {
                    return Err(Status::internal(format!("Unmount failed: {}", error)));
                }
            },
            Err(e) => return Err(Status::internal(format!("Unmount command failed: {}", e))),
        }

        // Update connection usage (don't disconnect - keep for reuse)
        self.update_connection_usage_on_detach(&req.volume_id).await;

        let detach_time = start_time.elapsed().as_millis() as u64;
        println!("DETACH_METRICS: volume={}, time={}ms", req.volume_id, detach_time);

        if detach_time > 500 {
            println!("SLOW DETACH: volume={}, time={}ms", req.volume_id, detach_time);
        }

        Ok(Response::new(csi::NodeUnstageVolumeResponse {}))
    }

    // Helper methods for performance optimization
    async fn find_nvme_device_fast(&self, pcie_addr: &str) -> Result<String, Box<dyn std::error::Error>> {
        // Use a more efficient device discovery method
        let _normalized_addr = pcie_addr.replace(":", "_");
        
        // Check common paths first
        let common_paths = [
            "/dev/nvme0n1".to_string(),
            "/dev/nvme1n1".to_string(),
            "/dev/nvme2n1".to_string(),
        ];
        
        for path in &common_paths {
            if self.verify_device_pcie_match(path, pcie_addr).await? {
                return Ok(path.clone());
            }
        }
        
        // Fallback to full scan
        self.scan_for_nvme_device(pcie_addr).await
    }

    async fn find_nvmf_device_fast(&self, nqn: &str) -> Result<String, Box<dyn std::error::Error>> {
        // Wait briefly for device to appear after connection
        for attempt in 0..10 {
            // Check /dev/nvme* devices
            let mut dir = tokio::fs::read_dir("/dev").await?;
            while let Some(entry) = dir.next_entry().await? {
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                
                if name.starts_with("nvme") && name.ends_with("n1") {
                    let device_path = format!("/dev/{}", name);
                    if self.verify_device_nqn_match(&device_path, nqn).await? {
                        return Ok(device_path);
                    }
                }
            }
            
            if attempt < 9 {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
        
        Err("NVMe-oF device not found after connection".into())
    }

    async fn is_device_formatted_fast(&self, device_path: &str) -> Result<bool, Box<dyn std::error::Error>> {
        // Quick filesystem detection using blkid
        let output = tokio::process::Command::new("blkid")
            .arg(device_path)
            .output()
            .await?;
        
        Ok(output.status.success() && !output.stdout.is_empty())
    }

    async fn verify_device_pcie_match(&self, device_path: &str, pcie_addr: &str) -> Result<bool, Box<dyn std::error::Error>> {
        // Quick check via sysfs
        let device_name = std::path::Path::new(device_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("Invalid device path")?;
        
        let controller_name = device_name.trim_end_matches("n1");
        let sys_path = format!("/sys/class/nvme/{}/address", controller_name);
        
        match tokio::fs::read_to_string(&sys_path).await {
            Ok(addr) => Ok(addr.trim() == pcie_addr),
            Err(_) => Ok(false),
        }
    }

    async fn verify_device_nqn_match(&self, device_path: &str, expected_nqn: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let device_name = std::path::Path::new(device_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("Invalid device path")?;
        
        let controller_name = device_name.trim_end_matches("n1");
        let sys_path = format!("/sys/class/nvme/{}/subsysnqn", controller_name);
        
        match tokio::fs::read_to_string(&sys_path).await {
            Ok(nqn) => Ok(nqn.trim() == expected_nqn),
            Err(_) => Ok(false),
        }
    }

    async fn bind_nvme_to_kernel(&self, pcie_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Unbind from any existing driver first
        let unbind_path = format!("/sys/bus/pci/devices/{}/driver/unbind", pcie_addr);
        tokio::fs::write(&unbind_path, pcie_addr).await.ok(); // Ignore errors
        
        // Bind to nvme driver
        let bind_path = "/sys/bus/pci/drivers/nvme/bind";
        tokio::fs::write(bind_path, pcie_addr).await?;
        
        // Wait for device to appear
        for _ in 0..20 {
            let driver_path = format!("/sys/bus/pci/devices/{}/driver", pcie_addr);
            if std::path::Path::new(&driver_path).exists() {
                return Ok(());
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        
        Err("Device failed to bind to nvme driver".into())
    }

    async fn update_connection_usage(&self, nqn: &str) {
        let mut connections = self.connection_cache.nvmf_connections.write().await;
        if let Some(conn) = connections.get_mut(nqn) {
            conn.last_used = std::time::SystemTime::now();
            conn.pod_count += 1;
        }
    }

    async fn update_connection_usage_on_detach(&self, volume_id: &str) {
        // Find NQN for this volume and decrement usage
        let mut connections = self.connection_cache.nvmf_connections.write().await;
        for conn in connections.values_mut() {
            if conn.pod_count > 0 {
                conn.pod_count -= 1;
                conn.last_used = std::time::SystemTime::now();
                break; // Assume one connection per volume for simplicity
            }
        }
    }

    fn get_host_nqn(&self) -> String {
        // Generate consistent host NQN for this node
        format!("nqn.2014-08.org.nvmexpress:uuid:{}", 
                self.node_id)
    }

    async fn scan_for_nvme_device(&self, pcie_addr: &str) -> Result<String, Box<dyn std::error::Error>> {
        // Fallback device scan - slower but thorough
        let output = tokio::process::Command::new("find")
            .args(["/dev", "-name", "nvme*n1"])
            .output()
            .await?;
        
        let devices = String::from_utf8(output.stdout)?;
        for device in devices.lines() {
            if self.verify_device_pcie_match(device, pcie_addr).await? {
                return Ok(device.to_string());
            }
        }
        
        Err("NVMe device not found".into())
    }

    async fn get_current_node(&self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(self.node_id.clone())
    }

    async fn is_volume_staged(&self, staging_path: &str) -> bool {
        // Check if the staging path is already mounted
        if let Ok(output) = tokio::process::Command::new("mountpoint")
            .arg("-q")
            .arg(staging_path)
            .output()
            .await
        {
            output.status.success()
        } else {
            false
        }
    }

    // Connection cleanup for efficiency
    async fn cleanup_unused_connections(&self) {
        let mut connections = self.connection_cache.nvmf_connections.write().await;
        let now = std::time::SystemTime::now();
        
        connections.retain(|nqn, conn| {
            if conn.pod_count == 0 {
                if let Ok(duration) = now.duration_since(conn.last_used) {
                    if duration.as_secs() > 300 { // 5 minutes
                        println!("Cleaning up unused NVMe-oF connection: {}", nqn);
                        // In real implementation, would disconnect the device
                        return false;
                    }
                }
            }
            true
        });
    }
}

// Performance monitoring and alerting
pub struct PerformanceMonitor {
    attach_times: Arc<RwLock<Vec<AttachMetrics>>>,
    slow_operation_threshold: u64,
}

impl PerformanceMonitor {
    pub fn new() -> Self {
        Self {
            attach_times: Arc::new(RwLock::new(Vec::new())),
            slow_operation_threshold: 1000, // 1 second
        }
    }
    
    pub async fn record_operation(&self, metrics: AttachMetrics) {
        let mut times = self.attach_times.write().await;
        
        // Alert on slow operations
        if metrics.total_time_ms > self.slow_operation_threshold {
            self.alert_slow_operation(&metrics).await;
        }
        
        times.push(metrics);
        
        // Keep only recent metrics (last 1000 operations)
        if times.len() > 1000 {
            times.drain(0..100);
        }
    }
    
    async fn alert_slow_operation(&self, metrics: &AttachMetrics) {
        // Send alert to monitoring system
        eprintln!("ALERT: Slow attach operation detected - {}ms", metrics.total_time_ms);
        
        // Could integrate with:
        // - Prometheus AlertManager
        // - PagerDuty
        // - Slack notifications
        // - Custom webhook
    }
    
    pub async fn get_performance_stats(&self) -> PerformanceStats {
        let times = self.attach_times.read().await;
        
        if times.is_empty() {
            return PerformanceStats::default();
        }
        
        let total_ops = times.len();
        let total_time: u64 = times.iter().map(|m| m.total_time_ms).sum();
        let avg_time = total_time / total_ops as u64;
        
        let mut sorted_times: Vec<u64> = times.iter().map(|m| m.total_time_ms).collect();
        sorted_times.sort_unstable();
        
        let p50 = sorted_times[total_ops / 2];
        let p95 = sorted_times[(total_ops * 95) / 100];
        let p99 = sorted_times[(total_ops * 99) / 100];
        
        let cache_hit_rate = times.iter()
            .filter(|m| m.cache_hit)
            .count() as f64 / total_ops as f64;
        
        PerformanceStats {
            total_operations: total_ops,
            average_time_ms: avg_time,
            p50_time_ms: p50,
            p95_time_ms: p95,
            p99_time_ms: p99,
            cache_hit_rate,
            local_nvme_ratio: times.iter()
                .filter(|m| matches!(m.attach_type, AttachType::LocalNVMe))
                .count() as f64 / total_ops as f64,
        }
    }
}

#[derive(Debug, Default)]
pub struct PerformanceStats {
    pub total_operations: usize,
    pub average_time_ms: u64,
    pub p50_time_ms: u64,
    pub p95_time_ms: u64,
    pub p99_time_ms: u64,
    pub cache_hit_rate: f64,
    pub local_nvme_ratio: f64,
}

// Example usage and benchmarking
#[cfg(test)]
mod performance_tests {
    use super::*;
    
    #[tokio::test]
    async fn benchmark_attach_performance() {
        let driver = SpdkCsiDriver::new().await.unwrap();
        
        // Simulate attach operations
        let num_operations = 100;
        let mut total_time = 0;
        
        for i in 0..num_operations {
            let start = Instant::now();
            
            // Mock attach operation
            let request = create_mock_stage_request(&format!("vol-{}", i));
            let _response = driver.node_stage_volume_fast(request).await.unwrap();
            
            let elapsed = start.elapsed().as_millis() as u64;
            total_time += elapsed;
            
            println!("Operation {}: {}ms", i, elapsed);
        }
        
        let avg_time = total_time / num_operations;
        println!("Average attach time: {}ms", avg_time);
        
        // Assert performance expectations
        assert!(avg_time < 500, "Average attach time too slow: {}ms", avg_time);
    }
    
    fn create_mock_stage_request(volume_id: &str) -> Request<csi::NodeStageVolumeRequest> {
        Request::new(csi::NodeStageVolumeRequest {
            volume_id: volume_id.to_string(),
            staging_target_path: format!("/tmp/staging/{}", volume_id),
            volume_capability: Some(csi::VolumeCapability {
                access_type: Some(csi::volume_capability::AccessType::Mount(
                    csi::volume_capability::MountVolume {
                        fs_type: "ext4".to_string(),
                        mount_flags: vec!["rw".to_string(), "noatime".to_string()],
                    }
                )),
                access_mode: Some(csi::volume_capability::AccessMode {
                    mode: csi::volume_capability::access_mode::Mode::SingleNodeWriter as i32,
                }),
            }),
            secrets: HashMap::new(),
            volume_context: HashMap::from([
                ("replicaNodes".to_string(), "node-a,node-b".to_string()),
                ("nvmeAddr0".to_string(), "0000:01:00.0".to_string()),
                ("lvolBdev0".to_string(), "lvs_node-a/vol_test".to_string()),
            ]),
        })
    }
}

// Integration with Prometheus metrics
pub mod prometheus_integration {
    use prometheus::{Counter, Histogram, Gauge, register_counter, register_histogram, register_gauge};
    use std::sync::Once;
    
    static INIT: Once = Once::new();
    
    pub struct PrometheusMetrics {
        pub attach_operations_total: Counter,
        pub attach_duration_seconds: Histogram,
        pub cache_hit_rate: Gauge,
        pub local_replica_ratio: Gauge,
        pub slow_operations_total: Counter,
    }
    
    impl PrometheusMetrics {
        pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
            INIT.call_once(|| {
                // Initialize Prometheus registry
                prometheus::default_registry();
            });
            
            Ok(Self {
                attach_operations_total: register_counter!(
                    "spdk_csi_attach_operations_total",
                    "Total number of volume attach operations"
                )?,
                
                attach_duration_seconds: register_histogram!(
                    "spdk_csi_attach_duration_seconds",
                    "Time spent on volume attach operations",
                    vec![0.001, 0.01, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0]
                )?,
                
                cache_hit_rate: register_gauge!(
                    "spdk_csi_cache_hit_rate",
                    "Rate of cache hits for device discovery"
                )?,
                
                local_replica_ratio: register_gauge!(
                    "spdk_csi_local_replica_ratio", 
                    "Ratio of local vs remote replica usage"
                )?,
                
                slow_operations_total: register_counter!(
                    "spdk_csi_slow_operations_total",
                    "Total number of slow attach operations (>1s)"
                )?,
            })
        }
        
        pub fn record_attach_operation(&self, metrics: &super::AttachMetrics) {
            self.attach_operations_total.inc();
            self.attach_duration_seconds.observe(metrics.total_time_ms as f64 / 1000.0);
            
            if metrics.total_time_ms > 1000 {
                self.slow_operations_total.inc();
            }
            
            // Update ratios (in real implementation, you'd track these over time)
            if metrics.cache_hit {
                self.cache_hit_rate.set(1.0);
            }
            
            if matches!(metrics.attach_type, super::AttachType::LocalNVMe) {
                self.local_replica_ratio.set(1.0);
            }
        }
    }
}

// Health checking and monitoring endpoints
pub mod health_monitoring {
    use warp::Filter;
    use serde_json::json;
    use super::*;
    
    pub async fn start_health_server(driver: Arc<SpdkCsiDriver>) -> Result<(), Box<dyn std::error::Error>> {
        let driver_filter = warp::any().map(move || driver.clone());
        
        // Health endpoint
        let health = warp::path("health")
            .and(driver_filter.clone())
            .and_then(health_check);
        
        // Readiness endpoint
        let ready = warp::path("ready")
            .and(driver_filter.clone())
            .and_then(readiness_check);
        
        // Metrics endpoint
        let metrics = warp::path("metrics")
            .and(driver_filter.clone())
            .and_then(prometheus_metrics);
        
        // Performance stats endpoint
        let stats = warp::path("stats")
            .and(driver_filter.clone())
            .and_then(performance_stats);
        
        let routes = health
            .or(ready)
            .or(metrics)
            .or(stats)
            .with(warp::cors().allow_any_origin());
        
        println!("Starting health monitoring server on :8080");
        warp::serve(routes)
            .run(([0, 0, 0, 0], 8080))
            .await;
        
        Ok(())
    }
    
    async fn health_check(driver: Arc<SpdkCsiDriver>) -> Result<impl warp::Reply, warp::Rejection> {
        // Perform comprehensive health check
        let health_result = perform_health_check(&driver).await;
        
        let response = json!({
            "status": if health_result.healthy { "healthy" } else { "unhealthy" },
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "checks": {
                "spdk_connectivity": health_result.spdk_reachable,
                "kubernetes_connectivity": health_result.k8s_reachable,
                "disk_availability": health_result.disks_available,
                "performance": health_result.performance_ok
            },
            "details": health_result.details
        });
        
        let status_code = if health_result.healthy { 200 } else { 503 };
        Ok(warp::reply::with_status(warp::reply::json(&response), 
                                   warp::http::StatusCode::from_u16(status_code).unwrap()))
    }
    
    async fn readiness_check(driver: Arc<SpdkCsiDriver>) -> Result<impl warp::Reply, warp::Rejection> {
        // Quick readiness check for Kubernetes
        let ready = driver.check_spdk_health().await.is_ok();
        
        let response = json!({
            "ready": ready,
            "timestamp": chrono::Utc::now().to_rfc3339()
        });
        
        let status_code = if ready { 200 } else { 503 };
        Ok(warp::reply::with_status(warp::reply::json(&response),
                                   warp::http::StatusCode::from_u16(status_code).unwrap()))
    }
    
    async fn prometheus_metrics(_driver: Arc<SpdkCsiDriver>) -> Result<impl warp::Reply, warp::Rejection> {
        // Return Prometheus metrics
        let encoder = prometheus::TextEncoder::new();
        let metric_families = prometheus::gather();
        let metrics = encoder.encode_to_string(&metric_families)
            .map_err(|_| warp::reject::reject())?;
        
        Ok(warp::reply::with_header(metrics, "content-type", "text/plain"))
    }
    
    async fn performance_stats(driver: Arc<SpdkCsiDriver>) -> Result<impl warp::Reply, warp::Rejection> {
        let stats = if let Some(monitor) = &driver.performance_monitor {
            monitor.get_performance_stats().await
        } else {
            PerformanceStats::default()
        };
        
        let response = json!({
            "performance_stats": {
                "total_operations": stats.total_operations,
                "average_time_ms": stats.average_time_ms,
                "percentiles": {
                    "p50_ms": stats.p50_time_ms,
                    "p95_ms": stats.p95_time_ms,
                    "p99_ms": stats.p99_time_ms
                },
                "cache_hit_rate": stats.cache_hit_rate,
                "local_nvme_ratio": stats.local_nvme_ratio
            },
            "timestamp": chrono::Utc::now().to_rfc3339()
        });
        
        Ok(warp::reply::json(&response))
    }
    
    #[derive(Debug)]
    struct HealthCheckResult {
        healthy: bool,
        spdk_reachable: bool,
        k8s_reachable: bool,
        disks_available: bool,
        performance_ok: bool,
        details: Vec<String>,
    }
    
    async fn perform_health_check(driver: &SpdkCsiDriver) -> HealthCheckResult {
        let mut result = HealthCheckResult {
            healthy: true,
            spdk_reachable: false,
            k8s_reachable: false,
            disks_available: false,
            performance_ok: false,
            details: Vec::new(),
        };
        
        // Check SPDK connectivity
        match driver.check_spdk_health().await {
            Ok(_) => {
                result.spdk_reachable = true;
                result.details.push("SPDK RPC connectivity OK".to_string());
            }
            Err(e) => {
                result.healthy = false;
                result.details.push(format!("SPDK RPC failed: {}", e));
            }
        }
        
        // Check Kubernetes connectivity
        match driver.kube_client.apiserver_version().await {
            Ok(_) => {
                result.k8s_reachable = true;
                result.details.push("Kubernetes API connectivity OK".to_string());
            }
            Err(e) => {
                result.healthy = false;
                result.details.push(format!("Kubernetes API failed: {}", e));
            }
        }
        
        // Check disk availability
        if result.k8s_reachable {
            let disks: kube::Api<crate::SpdkDisk> = kube::Api::namespaced(driver.kube_client.clone(), "default");
            match disks.list(&kube::api::ListParams::default()).await {
                Ok(disk_list) => {
                    let healthy_disks = disk_list.items.iter()
                        .filter(|d| d.status.as_ref().map(|s| s.healthy).unwrap_or(false))
                        .count();
                    
                    if healthy_disks > 0 {
                        result.disks_available = true;
                        result.details.push(format!("{} healthy disks available", healthy_disks));
                    } else {
                        result.healthy = false;
                        result.details.push("No healthy disks available".to_string());
                    }
                }
                Err(e) => {
                    result.healthy = false;
                    result.details.push(format!("Failed to list disks: {}", e));
                }
            }
        }
        
        // Check performance metrics
        if let Some(monitor) = &driver.performance_monitor {
            let stats = monitor.get_performance_stats().await;
            if stats.total_operations > 0 {
                if stats.average_time_ms < 2000 { // Less than 2 seconds average
                    result.performance_ok = true;
                    result.details.push(format!("Performance OK: avg {}ms", stats.average_time_ms));
                } else {
                    result.healthy = false;
                    result.details.push(format!("Performance degraded: avg {}ms", stats.average_time_ms));
                }
            } else {
                result.performance_ok = true; // No operations yet
                result.details.push("No performance data available".to_string());
            }
        }
        
        result
    }
}

// Configuration and initialization
impl SpdkCsiDriver {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let kube_client = kube::Client::try_default().await?;
        let node_id = std::env::var("NODE_ID")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "unknown-node".to_string());
        
        let connection_cache = ConnectionCache {
            nvmf_connections: Arc::new(RwLock::new(HashMap::new())),
            device_cache: Arc::new(RwLock::new(HashMap::new())),
        };
        
        let performance_monitor = Some(Arc::new(PerformanceMonitor::new()));
        
        Ok(Self {
            node_id,
            kube_client,
            spdk_rpc_url: std::env::var("SPDK_RPC_URL")
                .unwrap_or("http://localhost:5260".to_string()),
            write_sequence_counter: Arc::new(tokio::sync::Mutex::new(0)),
            local_blobstore_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            connection_cache,
            device_cache: connection_cache.device_cache.clone(),
            performance_monitor,
        })
    }
}