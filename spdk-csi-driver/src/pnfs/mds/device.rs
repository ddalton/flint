//! Device Registry
//!
//! Manages the registry of data servers available to the MDS.
//! Tracks device IDs, endpoints, capacity, and health status.
//!
//! # Protocol References
//! - RFC 8881 Section 12.2.1 - Device IDs
//! - RFC 8881 Section 18.40 - GETDEVICEINFO operation

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Device ID type (16-byte opaque identifier as per RFC 8881)
pub type DeviceId = [u8; 16];

/// Device registry - thread-safe registry of data servers
#[derive(Clone)]
pub struct DeviceRegistry {
    /// Map of device ID to device information
    devices: Arc<DashMap<String, DeviceInfo>>,
}

/// Information about a data server
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Unique device identifier (e.g., "ds-node1-nvme0")
    pub device_id: String,
    
    /// Binary device ID (16 bytes for NFSv4.1 protocol)
    pub binary_device_id: DeviceId,
    
    /// Primary endpoint (IP:port or DNS name)
    pub primary_endpoint: String,
    
    /// Additional endpoints for multipath/RDMA
    pub endpoints: Vec<String>,
    
    /// Block devices this DS serves
    pub bdevs: Vec<String>,
    
    /// Total capacity in bytes
    pub capacity: u64,
    
    /// Used space in bytes
    pub used: u64,
    
    /// Current status
    pub status: DeviceStatus,
    
    /// Last heartbeat timestamp
    pub last_heartbeat: Instant,
    
    /// Number of active layouts using this device
    pub active_layouts: usize,
}

/// Device status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Device is operational
    Active,
    
    /// Device is operational but degraded (e.g., high latency)
    Degraded,
    
    /// Device is offline/unavailable
    Offline,
}

impl DeviceRegistry {
    /// Create a new empty device registry
    pub fn new() -> Self {
        Self {
            devices: Arc::new(DashMap::new()),
        }
    }

    /// Register a new data server
    pub fn register(&self, info: DeviceInfo) -> Result<(), String> {
        let device_id = info.device_id.clone();
        
        if self.devices.contains_key(&device_id) {
            warn!("Device {} already registered, updating", device_id);
        } else {
            info!("Registering new device: {} @ {}", device_id, info.primary_endpoint);
        }
        
        self.devices.insert(device_id, info);
        Ok(())
    }

    /// Unregister a data server
    pub fn unregister(&self, device_id: &str) -> Result<DeviceInfo, String> {
        self.devices
            .remove(device_id)
            .map(|(_, info)| {
                info!("Unregistered device: {}", device_id);
                info
            })
            .ok_or_else(|| format!("Device not found: {}", device_id))
    }

    /// Get device information by ID
    pub fn get(&self, device_id: &str) -> Option<DeviceInfo> {
        self.devices.get(device_id).map(|entry| entry.clone())
    }

    /// Get device information by binary device ID
    pub fn get_by_binary_id(&self, binary_id: &DeviceId) -> Option<DeviceInfo> {
        self.devices
            .iter()
            .find(|entry| &entry.binary_device_id == binary_id)
            .map(|entry| entry.clone())
    }

    /// List all devices
    pub fn list(&self) -> Vec<DeviceInfo> {
        self.devices.iter().map(|entry| entry.clone()).collect()
    }

    /// List all active devices
    pub fn list_active(&self) -> Vec<DeviceInfo> {
        self.devices
            .iter()
            .filter(|entry| entry.status == DeviceStatus::Active)
            .map(|entry| entry.clone())
            .collect()
    }

    /// Update device heartbeat
    pub fn heartbeat(&self, device_id: &str) -> Result<(), String> {
        if let Some(mut entry) = self.devices.get_mut(device_id) {
            entry.last_heartbeat = Instant::now();
            debug!("Heartbeat received from device: {}", device_id);
            
            // If device was offline, bring it back online
            if entry.status == DeviceStatus::Offline {
                info!("Device {} is back online", device_id);
                entry.status = DeviceStatus::Active;
            }
            
            Ok(())
        } else {
            Err(format!("Device not found: {}", device_id))
        }
    }

    /// Update device capacity
    pub fn update_capacity(&self, device_id: &str, capacity: u64, used: u64) -> Result<(), String> {
        if let Some(mut entry) = self.devices.get_mut(device_id) {
            entry.capacity = capacity;
            entry.used = used;
            debug!(
                "Updated capacity for device {}: {} / {} bytes",
                device_id, used, capacity
            );
            Ok(())
        } else {
            Err(format!("Device not found: {}", device_id))
        }
    }

    /// Update device status
    pub fn update_status(&self, device_id: &str, status: DeviceStatus) -> Result<(), String> {
        if let Some(mut entry) = self.devices.get_mut(device_id) {
            if entry.status != status {
                info!("Device {} status changed: {:?} -> {:?}", device_id, entry.status, status);
                entry.status = status;
            }
            Ok(())
        } else {
            Err(format!("Device not found: {}", device_id))
        }
    }

    /// Check for stale devices (no heartbeat within timeout)
    pub fn check_stale_devices(&self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let mut stale_devices = Vec::new();

        for mut entry in self.devices.iter_mut() {
            if entry.status != DeviceStatus::Offline {
                let elapsed = now.duration_since(entry.last_heartbeat);
                if elapsed > timeout {
                    warn!(
                        "Device {} heartbeat timeout: {} seconds (threshold: {} seconds)",
                        entry.device_id,
                        elapsed.as_secs(),
                        timeout.as_secs()
                    );
                    entry.status = DeviceStatus::Offline;
                    stale_devices.push(entry.device_id.clone());
                }
            }
        }

        stale_devices
    }

    /// Get total available capacity across all active devices
    pub fn total_capacity(&self) -> u64 {
        self.devices
            .iter()
            .filter(|entry| entry.status == DeviceStatus::Active)
            .map(|entry| entry.capacity)
            .sum()
    }

    /// Get total used capacity across all active devices
    pub fn total_used(&self) -> u64 {
        self.devices
            .iter()
            .filter(|entry| entry.status == DeviceStatus::Active)
            .map(|entry| entry.used)
            .sum()
    }

    /// Increment active layout count for a device
    pub fn increment_layout_count(&self, device_id: &str) -> Result<(), String> {
        if let Some(mut entry) = self.devices.get_mut(device_id) {
            entry.active_layouts += 1;
            Ok(())
        } else {
            Err(format!("Device not found: {}", device_id))
        }
    }

    /// Decrement active layout count for a device
    pub fn decrement_layout_count(&self, device_id: &str) -> Result<(), String> {
        if let Some(mut entry) = self.devices.get_mut(device_id) {
            if entry.active_layouts > 0 {
                entry.active_layouts -= 1;
            }
            Ok(())
        } else {
            Err(format!("Device not found: {}", device_id))
        }
    }

    /// Get device count by status
    pub fn count_by_status(&self, status: DeviceStatus) -> usize {
        self.devices
            .iter()
            .filter(|entry| entry.status == status)
            .count()
    }

    /// Get total device count
    pub fn count(&self) -> usize {
        self.devices.len()
    }
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceInfo {
    /// Create a new device info
    pub fn new(
        device_id: String,
        primary_endpoint: String,
        bdevs: Vec<String>,
    ) -> Self {
        Self {
            binary_device_id: Self::generate_binary_id(&device_id),
            device_id,
            primary_endpoint,
            endpoints: Vec::new(),
            bdevs,
            capacity: 0,
            used: 0,
            status: DeviceStatus::Active,
            last_heartbeat: Instant::now(),
            active_layouts: 0,
        }
    }

    /// Generate a 16-byte binary device ID from string ID
    /// 
    /// As per RFC 8881 Section 12.2.1, device IDs are 16-byte opaque identifiers.
    /// We use a simple hash of the string ID for now.
    fn generate_binary_id(device_id: &str) -> DeviceId {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        device_id.hash(&mut hasher);
        let hash = hasher.finish();

        let mut binary_id = [0u8; 16];
        // Use hash for first 8 bytes, repeat for second 8 bytes
        binary_id[0..8].copy_from_slice(&hash.to_be_bytes());
        binary_id[8..16].copy_from_slice(&hash.to_be_bytes());

        binary_id
    }

    /// Get available capacity
    pub fn available_capacity(&self) -> u64 {
        self.capacity.saturating_sub(self.used)
    }

    /// Get utilization percentage (0-100)
    pub fn utilization_percentage(&self) -> f64 {
        if self.capacity == 0 {
            0.0
        } else {
            (self.used as f64 / self.capacity as f64) * 100.0
        }
    }

    /// Check if device is available for use
    pub fn is_available(&self) -> bool {
        self.status == DeviceStatus::Active && self.available_capacity() > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_registry_register() {
        let registry = DeviceRegistry::new();
        let info = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );

        assert!(registry.register(info).is_ok());
        assert_eq!(registry.count(), 1);
    }

    #[test]
    fn test_device_registry_get() {
        let registry = DeviceRegistry::new();
        let info = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );

        registry.register(info.clone()).unwrap();
        
        let retrieved = registry.get("ds-test-1").unwrap();
        assert_eq!(retrieved.device_id, "ds-test-1");
        assert_eq!(retrieved.primary_endpoint, "10.0.0.1:2049");
    }

    #[test]
    fn test_device_registry_heartbeat() {
        let registry = DeviceRegistry::new();
        let info = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );

        registry.register(info).unwrap();
        
        // Simulate heartbeat
        std::thread::sleep(Duration::from_millis(10));
        registry.heartbeat("ds-test-1").unwrap();
        
        let device = registry.get("ds-test-1").unwrap();
        assert_eq!(device.status, DeviceStatus::Active);
    }

    #[test]
    fn test_device_status_transitions() {
        let registry = DeviceRegistry::new();
        let info = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );

        registry.register(info).unwrap();

        // Test status transitions
        registry.update_status("ds-test-1", DeviceStatus::Degraded).unwrap();
        assert_eq!(registry.get("ds-test-1").unwrap().status, DeviceStatus::Degraded);

        registry.update_status("ds-test-1", DeviceStatus::Offline).unwrap();
        assert_eq!(registry.get("ds-test-1").unwrap().status, DeviceStatus::Offline);

        registry.update_status("ds-test-1", DeviceStatus::Active).unwrap();
        assert_eq!(registry.get("ds-test-1").unwrap().status, DeviceStatus::Active);
    }

    #[test]
    fn test_device_capacity_tracking() {
        let info = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );

        assert_eq!(info.available_capacity(), 0);
        assert_eq!(info.utilization_percentage(), 0.0);

        let mut info = info;
        info.capacity = 1000;
        info.used = 300;

        assert_eq!(info.available_capacity(), 700);
        assert_eq!(info.utilization_percentage(), 30.0);
    }
}


