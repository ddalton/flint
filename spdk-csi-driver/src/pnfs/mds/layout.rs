//! Layout Management
//!
//! Manages layout generation and tracking for pNFS.
//! Implements the FILE layout type as per RFC 8881 Chapter 13.
//!
//! # Protocol References
//! - RFC 8881 Section 12.2 - pNFS Definitions
//! - RFC 8881 Chapter 13 - NFSv4.1 File Layout Type
//! - RFC 8881 Section 18.43 - LAYOUTGET operation

use crate::pnfs::mds::device::{DeviceInfo, DeviceRegistry};
use crate::pnfs::config::LayoutPolicy as ConfigLayoutPolicy;
use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, info};

/// Layout state ID (combines with NFSv4 stateid)
pub type LayoutStateId = [u8; 16];

/// Layout manager - manages layout generation and tracking
#[derive(Clone)]
pub struct LayoutManager {
    /// Registry of available devices
    device_registry: Arc<DeviceRegistry>,
    
    /// Active layouts (keyed by layout stateid)
    layouts: Arc<DashMap<LayoutStateId, LayoutState>>,
    
    /// Layout policy
    policy: LayoutPolicyImpl,
    
    /// Stripe size in bytes
    stripe_size: u64,
}

/// Layout state - tracks an active layout issued to a client
#[derive(Debug, Clone)]
pub struct LayoutState {
    /// Layout stateid
    pub stateid: LayoutStateId,
    
    /// File handle this layout applies to
    pub filehandle: Vec<u8>,
    
    /// Layout segments
    pub segments: Vec<LayoutSegment>,
    
    /// I/O mode (read, write, any)
    pub iomode: IoMode,
    
    /// Whether to return layout on close
    pub return_on_close: bool,
}

/// A single layout segment
#[derive(Debug, Clone)]
pub struct LayoutSegment {
    /// Byte offset where this segment starts
    pub offset: u64,
    
    /// Length of this segment (NFS4_UINT64_MAX for "rest of file")
    pub length: u64,
    
    /// I/O mode for this segment
    pub iomode: IoMode,
    
    /// Device ID to use for this segment
    pub device_id: String,
    
    /// Stripe index (for striped layouts)
    pub stripe_index: u32,
    
    /// Pattern offset (for dense striping)
    pub pattern_offset: u64,
}

/// I/O mode as per RFC 8881 Section 3.3.20
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum IoMode {
    /// Read-only access
    Read = 1,
    
    /// Read-write access
    ReadWrite = 2,
    
    /// Any mode (for layout return)
    Any = 3,
}

/// Layout type as per RFC 8881 Section 12.2.3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LayoutType {
    /// NFSv4.1 Files layout (RFC 8881 Chapter 13)
    NfsV4_1Files = 1,
    
    /// Block/volume layout (RFC 5663) - future
    BlockVolume = 2,
    
    /// Object layout (RFC 5664) - future
    Osd2Objects = 3,
}

/// Layout policy implementation
#[derive(Debug, Clone, Copy)]
enum LayoutPolicyImpl {
    /// Simple round-robin across all DSs
    RoundRobin { next_device: usize },
    
    /// Interleaved striping for parallel I/O
    Stripe,
    
    /// Prefer DS on same node as client (future)
    Locality,
}

impl LayoutManager {
    /// Create a new layout manager
    pub fn new(
        device_registry: Arc<DeviceRegistry>,
        policy: ConfigLayoutPolicy,
        stripe_size: u64,
    ) -> Self {
        let policy_impl = match policy {
            ConfigLayoutPolicy::RoundRobin => LayoutPolicyImpl::RoundRobin { next_device: 0 },
            ConfigLayoutPolicy::Stripe => LayoutPolicyImpl::Stripe,
            ConfigLayoutPolicy::Locality => LayoutPolicyImpl::Locality,
        };

        info!(
            "Layout manager initialized: policy={:?}, stripe_size={}",
            policy_impl, stripe_size
        );

        Self {
            device_registry,
            layouts: Arc::new(DashMap::new()),
            policy: policy_impl,
            stripe_size,
        }
    }

    /// Generate a new layout for a file
    pub fn generate_layout(
        &self,
        filehandle: Vec<u8>,
        offset: u64,
        length: u64,
        iomode: IoMode,
    ) -> Result<LayoutState, String> {
        let devices = self.device_registry.list_active();
        if devices.is_empty() {
            return Err("No active data servers available".to_string());
        }

        debug!(
            "Generating layout: offset={}, length={}, iomode={:?}, devices={}",
            offset,
            length,
            iomode,
            devices.len()
        );

        let segments = match self.policy {
            LayoutPolicyImpl::RoundRobin { .. } => {
                self.generate_roundrobin_layout(offset, length, &devices)?
            }
            LayoutPolicyImpl::Stripe => {
                self.generate_stripe_layout(offset, length, &devices)?
            }
            LayoutPolicyImpl::Locality => {
                // TODO: Implement locality-aware layout
                self.generate_roundrobin_layout(offset, length, &devices)?
            }
        };

        let stateid = Self::generate_stateid();
        let layout = LayoutState {
            stateid,
            filehandle,
            segments,
            iomode,
            return_on_close: true,
        };

        // Track active layouts
        self.layouts.insert(stateid, layout.clone());

        info!(
            "Generated layout with {} segments, stateid={:?}",
            layout.segments.len(),
            &stateid[0..4]
        );

        Ok(layout)
    }

    /// Generate round-robin layout (simplest policy)
    fn generate_roundrobin_layout(
        &self,
        offset: u64,
        length: u64,
        devices: &[DeviceInfo],
    ) -> Result<Vec<LayoutSegment>, String> {
        if devices.is_empty() {
            return Err("No devices available".to_string());
        }

        let mut segments = Vec::new();
        let mut current_offset = offset;
        let end_offset = offset.saturating_add(length);

        // Simple round-robin: assign entire range to first device
        // In a more sophisticated implementation, we would split across multiple devices
        let device = &devices[0];

        segments.push(LayoutSegment {
            offset: current_offset,
            length: if length == u64::MAX {
                u64::MAX  // NFS4_UINT64_MAX means "rest of file"
            } else {
                length
            },
            iomode: IoMode::ReadWrite,
            device_id: device.device_id.clone(),
            stripe_index: 0,
            pattern_offset: 0,
        });

        Ok(segments)
    }

    /// Generate striped layout for parallel I/O
    fn generate_stripe_layout(
        &self,
        offset: u64,
        length: u64,
        devices: &[DeviceInfo],
    ) -> Result<Vec<LayoutSegment>, String> {
        if devices.is_empty() {
            return Err("No devices available".to_string());
        }

        let mut segments = Vec::new();
        let stripe_size = self.stripe_size;
        let num_devices = devices.len();

        // Align offset to stripe boundary
        let stripe_start = (offset / stripe_size) * stripe_size;
        let mut current_offset = offset;
        let end_offset = if length == u64::MAX {
            u64::MAX
        } else {
            offset.saturating_add(length)
        };

        // If length is u64::MAX (rest of file), create a single segment
        // spanning the entire remaining file across all devices
        if length == u64::MAX {
            for (i, device) in devices.iter().enumerate() {
                segments.push(LayoutSegment {
                    offset: current_offset,
                    length: u64::MAX,
                    iomode: IoMode::ReadWrite,
                    device_id: device.device_id.clone(),
                    stripe_index: i as u32,
                    pattern_offset: stripe_start,
                });
            }
            return Ok(segments);
        }

        // Generate striped segments
        let mut stripe_index = ((offset / stripe_size) % (num_devices as u64)) as usize;

        while current_offset < end_offset {
            let device = &devices[stripe_index % num_devices];
            
            // Calculate segment length (either stripe_size or remaining bytes)
            let remaining = end_offset - current_offset;
            let segment_length = stripe_size.min(remaining);

            segments.push(LayoutSegment {
                offset: current_offset,
                length: segment_length,
                iomode: IoMode::ReadWrite,
                device_id: device.device_id.clone(),
                stripe_index: stripe_index as u32,
                pattern_offset: stripe_start,
            });

            current_offset += segment_length;
            stripe_index += 1;
        }

        debug!(
            "Generated striped layout: {} segments across {} devices",
            segments.len(),
            num_devices
        );

        Ok(segments)
    }

    /// Return a layout (client releases it)
    pub fn return_layout(&self, stateid: &LayoutStateId) -> Result<(), String> {
        if let Some((_, layout)) = self.layouts.remove(stateid) {
            info!(
                "Layout returned: stateid={:?}, segments={}",
                &stateid[0..4],
                layout.segments.len()
            );
            
            // Decrement active layout counts for affected devices
            for segment in &layout.segments {
                let _ = self.device_registry.decrement_layout_count(&segment.device_id);
            }
            
            Ok(())
        } else {
            Err(format!("Layout not found: {:?}", &stateid[0..4]))
        }
    }

    /// Recall layouts (e.g., on device failure)
    pub fn recall_layouts_for_device(&self, device_id: &str) -> Vec<LayoutStateId> {
        let mut recalled = Vec::new();

        for entry in self.layouts.iter() {
            let has_device = entry
                .segments
                .iter()
                .any(|seg| seg.device_id == device_id);

            if has_device {
                recalled.push(entry.stateid);
            }
        }

        if !recalled.is_empty() {
            info!(
                "Recalling {} layouts using device {}",
                recalled.len(),
                device_id
            );
        }

        recalled
    }

    /// Get layout by stateid
    pub fn get_layout(&self, stateid: &LayoutStateId) -> Option<LayoutState> {
        self.layouts.get(stateid).map(|entry| entry.clone())
    }

    /// Get all active layouts
    pub fn active_layouts(&self) -> Vec<LayoutState> {
        self.layouts.iter().map(|entry| entry.clone()).collect()
    }

    /// Get layout count
    pub fn layout_count(&self) -> usize {
        self.layouts.len()
    }

    /// Generate a unique layout stateid
    fn generate_stateid() -> LayoutStateId {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut stateid = [0u8; 16];
        rng.fill(&mut stateid);
        stateid
    }
}

impl Default for LayoutManager {
    fn default() -> Self {
        Self::new(
            Arc::new(DeviceRegistry::new()),
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024, // 8 MB default stripe size
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnfs::mds::device::DeviceInfo;

    #[test]
    fn test_layout_generation_single_device() {
        let registry = Arc::new(DeviceRegistry::new());
        let device = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );
        registry.register(device).unwrap();

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
        );

        let layout = manager
            .generate_layout(
                vec![0, 1, 2, 3],
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        assert!(!layout.segments.is_empty());
        assert_eq!(layout.iomode, IoMode::ReadWrite);
    }

    #[test]
    fn test_layout_generation_striped() {
        let registry = Arc::new(DeviceRegistry::new());
        
        // Register 3 devices
        for i in 1..=3 {
            let device = DeviceInfo::new(
                format!("ds-test-{}", i),
                format!("10.0.0.{}:2049", i),
                vec![format!("nvme{}n1", i)],
            );
            registry.register(device).unwrap();
        }

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::Stripe,
            8 * 1024 * 1024,
        );

        let layout = manager
            .generate_layout(
                vec![0, 1, 2, 3],
                0,
                24 * 1024 * 1024, // 24 MB across 3 devices
                IoMode::ReadWrite,
            )
            .unwrap();

        // Should have 3 segments (one per device)
        assert_eq!(layout.segments.len(), 3);
    }

    #[test]
    fn test_layout_return() {
        let registry = Arc::new(DeviceRegistry::new());
        let device = DeviceInfo::new(
            "ds-test-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        );
        registry.register(device).unwrap();

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
        );

        let layout = manager
            .generate_layout(
                vec![0, 1, 2, 3],
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        let stateid = layout.stateid;
        
        // Return the layout
        assert!(manager.return_layout(&stateid).is_ok());
        
        // Should no longer exist
        assert!(manager.get_layout(&stateid).is_none());
    }

    #[test]
    fn test_layout_recall() {
        let registry = Arc::new(DeviceRegistry::new());
        
        // Register 2 devices
        for i in 1..=2 {
            let device = DeviceInfo::new(
                format!("ds-test-{}", i),
                format!("10.0.0.{}:2049", i),
                vec![format!("nvme{}n1", i)],
            );
            registry.register(device).unwrap();
        }

        let manager = LayoutManager::new(
            registry,
            ConfigLayoutPolicy::RoundRobin,
            8 * 1024 * 1024,
        );

        // Generate layout (will use available devices)
        let layout = manager
            .generate_layout(
                vec![0, 1, 2, 3],
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        // Find which device was actually used
        let device_used = &layout.segments[0].device_id;
        
        // Recall layouts for that device
        let recalled = manager.recall_layouts_for_device(device_used);
        
        // Should have recalled the layout
        assert!(!recalled.is_empty(), "Expected to recall layout for device {}", device_used);
    }
}


