// Reliable and safe SPDK CSI attach implementation with comprehensive error handling

use std::time::{Instant, Duration};
use std::sync::Arc;
use tokio::sync::{RwLock, Mutex, Semaphore};
use tonic::{Request, Response, Status};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

// Reliability tracking and validation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachValidation {
    pub volume_id: String,
    pub device_path: String,
    pub filesystem_type: Option<String>,
    pub mount_point: String,
    pub validation_timestamp: String,
    pub checksum: String,
    pub replica_health: Vec<ReplicaValidation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaValidation {
    pub node: String,
    pub device_path: String,
    pub health_status: String,
    pub last_io_test: String,
    pub data_integrity_check: bool,
}

// Enhanced error types for better reliability
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("Device discovery failed: {0}")]
    DeviceDiscovery(String),
    #[error("Device validation failed: {0}")]
    DeviceValidation(String),
    #[error("Filesystem operation failed: {0}")]
    FilesystemError(String),
    #[error("Mount operation failed: {0}")]
    MountError(String),
    #[error("Data integrity check failed: {0}")]
    DataIntegrityError(String),
    #[error("Timeout during operation: {0}")]
    TimeoutError(String),
    #[error("Replica health check failed: {0}")]
    ReplicaHealthError(String),
    #[error("Concurrent operation conflict: {0}")]
    ConcurrencyError(String),
}

// Reliability configuration
#[derive(Debug, Clone)]
pub struct ReliabilityConfig {
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    pub operation_timeout_s: u64,
    pub health_check_timeout_s: u64,
    pub data_integrity_checks: bool,
    pub concurrent_operations_limit: usize,
    pub validation_required: bool,
    pub backup_replica_fallback: bool,
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_delay_ms: 1000,
            operation_timeout_s: 30,
            health_check_timeout_s: 10,
            data_integrity_checks: true,
            concurrent_operations_limit: 10,
            validation_required: true,
            backup_replica_fallback: true,
        }
    }
}

impl SpdkCsiDriver {
    // Reliable node stage implementation with comprehensive validation
    async fn node_stage_volume_reliable(&self, request: Request<csi::NodeStageVolumeRequest>) 
        -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        let start_time = Instant::now();
        let req = request.into_inner();
        let volume_id = req.volume_id.clone();
        
        // Acquire semaphore to limit concurrent operations
        let _permit = self.operation_semaphore.acquire().await
            .map_err(|e| Status::resource_exhausted(format!("Too many concurrent operations: {}", e)))?;
        
        // Check for conflicting operations
        if self.is_operation_in_progress(&volume_id).await {
            return Err(Status::already_exists(
                format!("Volume {} operation already in progress", volume_id)
            ));
        }
        
        // Mark operation as in progress
        self.mark_operation_in_progress(&volume_id).await;
        
        // Execute with timeout and cleanup on failure
        let result = tokio::time::timeout(
            Duration::from_secs(self.reliability_config.operation_timeout_s),
            self.execute_reliable_stage_operation(req)
        ).await;
        
        // Always cleanup operation tracking
        self.mark_operation_complete(&volume_id).await;
        
        match result {
            Ok(Ok(response)) => {
                let duration = start_time.elapsed();
                println!("RELIABLE_ATTACH: volume={}, success=true, duration={}ms", 
                         volume_id, duration.as_millis());
                Ok(response)
            }
            Ok(Err(e)) => {
                let duration = start_time.elapsed();
                println!("RELIABLE_ATTACH: volume={}, success=false, duration={}ms, error={}", 
                         volume_id, duration.as_millis(), e);
                Err(e)
            }
            Err(_) => {
                println!("RELIABLE_ATTACH: volume={}, success=false, error=timeout", volume_id);
                Err(Status::deadline_exceeded(
                    format!("Volume {} attach operation timed out", volume_id)
                ))
            }
        }
    }
    
    // Core reliable staging logic with retries and validation
    async fn execute_reliable_stage_operation(&self, req: csi::NodeStageVolumeRequest) 
        -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        let volume_id = &req.volume_id;
        
        // Phase 1: Validate prerequisites
        self.validate_stage_prerequisites(&req).await?;
        
        // Phase 2: Execute with retries
        let mut last_error = None;
        for attempt in 1..=self.reliability_config.max_retries {
            match self.attempt_stage_operation(&req, attempt).await {
                Ok(response) => {
                    // Phase 3: Post-operation validation
                    if let Err(e) = self.validate_stage_completion(&req).await {
                        println!("VALIDATION_FAILED: volume={}, attempt={}, error={}", 
                                 volume_id, attempt, e);
                        
                        // Cleanup partial state
                        self.cleanup_partial_stage(&req).await.ok();
                        last_error = Some(e);
                        
                        if attempt < self.reliability_config.max_retries {
                            tokio::time::sleep(Duration::from_millis(
                                self.reliability_config.retry_delay_ms * attempt as u64
                            )).await;
                            continue;
                        }
                    } else {
                        // Success - record validation state
                        self.record_successful_attach(&req).await.ok();
                        return Ok(response);
                    }
                }
                Err(e) => {
                    println!("STAGE_ATTEMPT_FAILED: volume={}, attempt={}, error={}", 
                             volume_id, attempt, e);
                    last_error = Some(e);
                    
                    // Cleanup partial state
                    self.cleanup_partial_stage(&req).await.ok();
                    
                    if attempt < self.reliability_config.max_retries {
                        tokio::time::sleep(Duration::from_millis(
                            self.reliability_config.retry_delay_ms * attempt as u64
                        )).await;
                        continue;
                    }
                }
            }
        }
        
        // All retries exhausted
        Err(last_error.unwrap_or_else(|| Status::internal("Unknown error after retries")))
    }
    
    // Validate prerequisites before attempting attach
    async fn validate_stage_prerequisites(&self, req: &csi::NodeStageVolumeRequest) 
        -> Result<(), Status> {
        let volume_id = &req.volume_id;
        
        // Check if volume exists in Kubernetes
        let crd_api: kube::Api<SpdkVolume> = kube::Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = crd_api.get(volume_id).await
            .map_err(|e| Status::not_found(format!("Volume {} not found: {}", volume_id, e)))?;
        
        // Validate volume is not in failed state
        if let Some(status) = &spdk_volume.status {
            if status.state == "Failed" {
                return Err(Status::failed_precondition(
                    format!("Volume {} is in failed state", volume_id)
                ));
            }
        }
        
        // Check if staging directory already exists and is in use
        if std::path::Path::new(&req.staging_target_path).exists() {
            if self.is_path_mounted(&req.staging_target_path).await? {
                return Err(Status::already_exists(
                    format!("Staging path {} already mounted", req.staging_target_path)
                ));
            }
        }
        
        // Validate volume capability is supported
        if let Some(capability) = &req.volume_capability {
            if !self.is_capability_supported(capability) {
                return Err(Status::invalid_argument(
                    "Volume capability not supported"
                ));
            }
        }
        
        // Check replica health before proceeding
        self.validate_replica_health(&spdk_volume).await?;
        
        Ok(())
    }
    
    // Single attempt at staging operation
    async fn attempt_stage_operation(&self, req: &csi::NodeStageVolumeRequest, attempt: u32) 
        -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        let volume_id = &req.volume_id;
        println!("STAGE_ATTEMPT: volume={}, attempt={}", volume_id, attempt);
        
        // Step 1: Discover and validate device path
        let device_path = self.discover_and_validate_device_path(req).await?;
        
        // Step 2: Test device accessibility and health
        self.test_device_health(&device_path).await?;
        
        // Step 3: Handle filesystem operations if needed
        if req.volume_capability.as_ref().and_then(|vc| vc.mount.as_ref()).is_some() {
            self.setup_filesystem_reliable(&device_path, req).await?;
            self.mount_volume_reliable(&device_path, req).await?;
        }
        
        // Step 4: Verify the operation completed successfully
        self.verify_staging_success(req, &device_path).await?;
        
        Ok(Response::new(csi::NodeStageVolumeResponse {}))
    }
    
    // Comprehensive device discovery with validation
    async fn discover_and_validate_device_path(&self, req: &csi::NodeStageVolumeRequest) 
        -> Result<String, Status> {
        let volume_id = &req.volume_id;
        let context = &req.volume_context;
        
        // Get all potential device paths
        let mut device_candidates = self.get_all_device_candidates(context).await?;
        
        // Sort by preference (local > cached remote > new remote)
        device_candidates.sort_by_key(|candidate| candidate.priority);
        
        let mut last_error = None;
        
        for candidate in device_candidates {
            match self.validate_device_candidate(&candidate).await {
                Ok(validated_path) => {
                    println!("DEVICE_SELECTED: volume={}, path={}, type={:?}", 
                             volume_id, validated_path, candidate.device_type);
                    return Ok(validated_path);
                }
                Err(e) => {
                    println!("DEVICE_REJECTED: volume={}, path={}, error={}", 
                             volume_id, candidate.path, e);
                    last_error = Some(e);
                    continue;
                }
            }
        }
        
        Err(Status::unavailable(
            format!("No valid device path found for volume {}: {}", 
                    volume_id, 
                    last_error.map(|e| e.to_string()).unwrap_or_else(|| "Unknown error".to_string()))
        ))
    }
    
    // Validate individual device candidate
    async fn validate_device_candidate(&self, candidate: &DeviceCandidate) 
        -> Result<String, AttachError> {
        
        // Basic accessibility check
        if !self.verify_device_accessible(&candidate.path).await? {
            return Err(AttachError::DeviceValidation(
                format!("Device {} not accessible", candidate.path)
            ));
        }
        
        // Check device is not in use by another volume
        if self.is_device_in_use(&candidate.path).await? {
            return Err(AttachError::DeviceValidation(
                format!("Device {} already in use", candidate.path)
            ));
        }
        
        // For NVMe-oF devices, validate connection health
        if matches!(candidate.device_type, DeviceType::NVMeoF | DeviceType::NVMeoFCached) {
            self.validate_nvmf_connection_health(&candidate).await?;
        }
        
        // Test basic I/O capability
        self.test_device_io_capability(&candidate.path).await?;
        
        // Data integrity check if enabled
        if self.reliability_config.data_integrity_checks {
            self.perform_data_integrity_check(&candidate.path).await?;
        }
        
        Ok(candidate.path.clone())
    }
    
    // Test device I/O capability
    async fn test_device_io_capability(&self, device_path: &str) 
        -> Result<(), AttachError> {
        // Perform a safe read test
        let test_result = tokio::time::timeout(
            Duration::from_secs(5),
            self.perform_read_test(device_path)
        ).await;
        
        match test_result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(AttachError::DeviceValidation(
                format!("Device I/O test failed: {}", e)
            )),
            Err(_) => Err(AttachError::TimeoutError(
                "Device I/O test timed out".to_string()
            )),
        }
    }
    
    // Safe read test that doesn't modify data
    async fn perform_read_test(&self, device_path: &str) 
        -> Result<(), Box<dyn std::error::Error>> {
        use tokio::fs::File;
        use tokio::io::AsyncReadExt;
        
        let mut file = File::open(device_path).await?;
        let mut buffer = vec![0u8; 4096]; // Read 4KB
        
        // Read from the beginning (safe for all devices)
        let bytes_read = file.read(&mut buffer).await?;
        
        if bytes_read == 0 {
            return Err("Device appears to be empty or inaccessible".into());
        }
        
        // Optional: Verify the read data makes sense (has some non-zero bytes)
        let non_zero_bytes = buffer.iter().filter(|&&b| b != 0).count();
        if non_zero_bytes == 0 {
            println!("WARNING: Device {} appears to contain only zeros", device_path);
        }
        
        Ok(())
    }
    
    // Data integrity check using checksums
    async fn perform_data_integrity_check(&self, device_path: &str) 
        -> Result<(), AttachError> {
        // Read a small portion and calculate checksum
        let checksum = self.calculate_device_checksum(device_path, 0, 1024*1024).await?; // 1MB
        
        // Store checksum for later verification
        self.store_device_checksum(device_path, &checksum).await?;
        
        Ok(())
    }
    
    // Reliable filesystem setup with verification
    async fn setup_filesystem_reliable(&self, device_path: &str, req: &csi::NodeStageVolumeRequest) 
        -> Result<(), Status> {
        let mount_info = req.volume_capability.as_ref()
            .and_then(|vc| vc.mount.as_ref())
            .ok_or_else(|| Status::invalid_argument("Missing mount information"))?;
        
        let fs_type = &mount_info.fs_type;
        
        // Check if filesystem already exists and is valid
        match self.detect_existing_filesystem(device_path).await {
            Ok(Some(existing_fs)) => {
                if existing_fs == *fs_type {
                    // Filesystem exists and matches - validate it
                    self.validate_existing_filesystem(device_path, fs_type).await?;
                    return Ok(());
                } else {
                    return Err(Status::failed_precondition(
                        format!("Device has {} filesystem, expected {}", existing_fs, fs_type)
                    ));
                }
            }
            Ok(None) => {
                // No filesystem - create one
                self.create_filesystem_reliable(device_path, fs_type).await?;
            }
            Err(e) => {
                return Err(Status::internal(
                    format!("Failed to detect filesystem: {}", e)
                ));
            }
        }
        
        Ok(())
    }
    
    // Create filesystem with verification
    async fn create_filesystem_reliable(&self, device_path: &str, fs_type: &str) 
        -> Result<(), Status> {
        println!("CREATING_FILESYSTEM: device={}, type={}", device_path, fs_type);
        
        // Create filesystem with verification
        let format_result = tokio::time::timeout(
            Duration::from_secs(60), // Filesystem creation can take time
            self.format_device_with_verification(device_path, fs_type)
        ).await;
        
        match format_result {
            Ok(Ok(_)) => {
                // Double-check the filesystem was created correctly
                match self.detect_existing_filesystem(device_path).await {
                    Ok(Some(created_fs)) if created_fs == fs_type => {
                        println!("FILESYSTEM_CREATED: device={}, type={}", device_path, fs_type);
                        Ok(())
                    }
                    Ok(Some(wrong_fs)) => {
                        Err(Status::internal(
                            format!("Created {} filesystem but detected {}", fs_type, wrong_fs)
                        ))
                    }
                    Ok(None) => {
                        Err(Status::internal("Filesystem creation appeared to succeed but no filesystem detected"))
                    }
                    Err(e) => {
                        Err(Status::internal(
                            format!("Failed to verify created filesystem: {}", e)
                        ))
                    }
                }
            }
            Ok(Err(e)) => {
                Err(Status::internal(format!("Filesystem creation failed: {}", e)))
            }
            Err(_) => {
                Err(Status::deadline_exceeded("Filesystem creation timed out"))
            }
        }
    }
    
    // Reliable mount operation with verification
    async fn mount_volume_reliable(&self, device_path: &str, req: &csi::NodeStageVolumeRequest) 
        -> Result<(), Status> {
        let staging_path = &req.staging_target_path;
        let mount_info = req.volume_capability.as_ref()
            .and_then(|vc| vc.mount.as_ref())
            .ok_or_else(|| Status::invalid_argument("Missing mount information"))?;
        
        // Create staging directory with proper permissions
        self.create_staging_directory(staging_path).await?;
        
        // Perform mount with retries
        let mut last_error = None;
        for attempt in 1..=3 {
            match self.attempt_mount_operation(device_path, staging_path, mount_info, attempt).await {
                Ok(_) => {
                    // Verify mount succeeded
                    if self.verify_mount_success(staging_path).await? {
                        println!("MOUNT_SUCCESS: device={}, target={}", device_path, staging_path);
                        return Ok(());
                    } else {
                        last_error = Some(Status::internal("Mount verification failed"));
                    }
                }
                Err(e) => {
                    println!("MOUNT_ATTEMPT_FAILED: device={}, target={}, attempt={}, error={}", 
                             device_path, staging_path, attempt, e);
                    last_error = Some(e);
                    
                    // Cleanup failed mount attempt
                    self.cleanup_failed_mount(staging_path).await.ok();
                }
            }
            
            if attempt < 3 {
                tokio::time::sleep(Duration::from_millis(1000 * attempt as u64)).await;
            }
        }
        
        Err(last_error.unwrap_or_else(|| Status::internal("Mount failed after retries")))
    }
    
    // Verify mount operation succeeded
    async fn verify_mount_success(&self, mount_path: &str) -> Result<bool, Status> {
        // Check if path is actually mounted
        if !self.is_path_mounted(mount_path).await? {
            return Ok(false);
        }
        
        // Test basic filesystem operations
        self.test_filesystem_operations(mount_path).await?;
        
        Ok(true)
    }
    
    // Test basic filesystem operations to ensure mount is functional
    async fn test_filesystem_operations(&self, mount_path: &str) -> Result<(), Status> {
        use tokio::fs;
        use tokio::io::AsyncWriteExt;
        
        let test_file = format!("{}/.__csi_test_file", mount_path);
        let test_data = b"CSI mount test";
        
        // Test write
        match fs::File::create(&test_file).await {
            Ok(mut file) => {
                if let Err(e) = file.write_all(test_data).await {
                    return Err(Status::internal(format!("Mount write test failed: {}", e)));
                }
            }
            Err(e) => {
                return Err(Status::internal(format!("Mount create test failed: {}", e)));
            }
        }
        
        // Test read
        match fs::read(&test_file).await {
            Ok(read_data) => {
                if read_data != test_data {
                    return Err(Status::internal("Mount read test data mismatch"));
                }
            }
            Err(e) => {
                return Err(Status::internal(format!("Mount read test failed: {}", e)));
            }
        }
        
        // Cleanup test file
        fs::remove_file(&test_file).await.ok();
        
        Ok(())
    }
    
    // Comprehensive post-operation validation
    async fn validate_stage_completion(&self, req: &csi::NodeStageVolumeRequest) 
        -> Result<(), Status> {
        let volume_id = &req.volume_id;
        let staging_path = &req.staging_target_path;
        
        // Validate staging path is properly set up
        if req.volume_capability.as_ref().and_then(|vc| vc.mount.as_ref()).is_some() {
            // For filesystem volumes, verify mount
            if !self.is_path_mounted(staging_path).await? {
                return Err(Status::internal("Staging path not mounted after operation"));
            }
            
            // Test filesystem accessibility
            self.test_filesystem_operations(staging_path).await?;
        }
        
        // Validate replica health hasn't degraded
        let crd_api: kube::Api<SpdkVolume> = kube::Api::namespaced(self.kube_client.clone(), "default");
        let spdk_volume = crd_api.get(volume_id).await
            .map_err(|e| Status::internal(format!("Failed to get volume status: {}", e)))?;
        
        self.validate_replica_health(&spdk_volume).await?;
        
        // Record successful validation
        let validation = AttachValidation {
            volume_id: volume_id.clone(),
            device_path: "determined_during_attach".to_string(), // Would be actual path
            filesystem_type: req.volume_capability.as_ref()
                .and_then(|vc| vc.mount.as_ref())
                .map(|m| m.fs_type.clone()),
            mount_point: staging_path.clone(),
            validation_timestamp: chrono::Utc::now().to_rfc3339(),
            checksum: "calculated_checksum".to_string(), // Would be actual checksum
            replica_health: vec![], // Would be populated with actual health data
        };
        
        self.store_attach_validation(&validation).await.ok();
        
        Ok(())
    }
    
    // Validate replica health
    async fn validate_replica_health(&self, spdk_volume: &SpdkVolume) -> Result<(), Status> {
        let healthy_replicas = spdk_volume.spec.replicas.iter()
            .filter(|r| matches!(r.health_status, ReplicaHealth::Healthy))
            .count();
        
        if healthy_replicas == 0 {
            return Err(Status::failed_precondition("No healthy replicas available"));
        }
        
        // For RAID-1, we need at least 1 healthy replica, but warn if we're degraded
        if healthy_replicas < spdk_volume.spec.num_replicas as usize {
            println!("WARNING: Volume {} is degraded ({}/{} healthy replicas)", 
                     spdk_volume.spec.volume_id, healthy_replicas, spdk_volume.spec.num_replicas);
        }
        
        Ok(())
    }
    
    // Cleanup operations on failure
    async fn cleanup_partial_stage(&self, req: &csi::NodeStageVolumeRequest) -> Result<(), AttachError> {
        let staging_path = &req.staging_target_path;
        
        // Unmount if mounted
        if self.is_path_mounted(staging_path).await.unwrap_or(false) {
            self.cleanup_failed_mount(staging_path).await.ok();
        }
        
        // Remove staging directory if empty
        if std::path::Path::new(staging_path).exists() {
            tokio::fs::remove_dir(staging_path).await.ok();
        }
        
        Ok(())
    }
    
    // Record successful attach for audit and debugging
    async fn record_successful_attach(&self, req: &csi::NodeStageVolumeRequest) -> Result<(), AttachError> {
        let validation = AttachValidation {
            volume_id: req.volume_id.clone(),
            device_path: "device_path".to_string(), // Would be actual device path
            filesystem_type: req.volume_capability.as_ref()
                .and_then(|vc| vc.mount.as_ref())
                .map(|m| m.fs_type.clone()),
            mount_point: req.staging_target_path.clone(),
            validation_timestamp: chrono::Utc::now().to_rfc3339(),
            checksum: "checksum".to_string(), // Would be actual checksum
            replica_health: vec![], // Would be populated
        };
        
        // Store in persistent storage or Kubernetes annotations
        self.store_attach_validation(&validation).await?;
        
        println!("ATTACH_VALIDATED: volume={}, timestamp={}", 
                 req.volume_id, validation.validation_timestamp);
        
        Ok(())
    }
    
    // Helper method implementations
    async fn is_operation_in_progress(&self, volume_id: &str) -> bool {
        self.active_operations.read().await.contains(volume_id)
    }
    
    async fn mark_operation_in_progress(&self, volume_id: &str) {
        self.active_operations.write().await.insert(volume_id.to_string());
    }
    
    async fn mark_operation_complete(&self, volume_id: &str) {
        self.active_operations.write().await.remove(volume_id);
    }
    
    async fn is_path_mounted(&self, path: &str) -> Result<bool, Status> {
        let output = tokio::process::Command::new("mountpoint")
            .arg("-q")
            .arg(path)
            .output()
            .await
            .map_err(|e| Status::internal(format!("Failed to check mount status: {}", e)))?;
        
        Ok(output.status.success())
    }
}

// Supporting data structures
#[derive(Debug, Clone)]
pub struct DeviceCandidate {
    pub path: String,
    pub device_type: DeviceType,
    pub priority: u32, // Lower = higher priority
    pub node: String,
    pub health_score: f64,
}

#[derive(Debug, Clone)]
pub enum DeviceType {
    LocalNVMe,
    NVMeoFCached,
    NVMeoF,
    RAIDDevice,
}

// Enhanced SpdkCsiDriver structure
impl SpdkCsiDriver {
    pub fn new_reliable() -> Self {
        Self {
            // ... existing fields ...
            reliability_config: ReliabilityConfig::default(),
            operation_semaphore: Arc::new(Semaphore::new(10)), // Limit concurrent operations
            active_operations: Arc::new(RwLock::new(std::collections::HashSet::new())),
            attach_validations: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}