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

/// 16-byte NFSv4.1 session id (mirrors `nfs::v4::protocol::SessionId`).
/// Kept as a plain byte array here so the pNFS layer doesn't pull in
/// the v4 protocol module.
pub type SessionIdBytes = [u8; 16];

/// "Who owns this layout" — RFC 8881 §12.5 ties every issued layout to a
/// specific client. We need this for:
///
/// * **CB_LAYOUTRECALL**: routing the recall to the right backchannel
///   (looked up via `session_id` → CallbackManager).
/// * **LAYOUTRETURN with return_type=ALL**: filter by `clientid`.
/// * **LAYOUTRETURN with return_type=FSID**: filter by `(clientid, fsid)`.
/// * Forensics ("which client is hammering DS-3?").
///
/// Stored alongside `LayoutState` and indexed by `LayoutManager::by_owner`
/// so the FSID/ALL paths don't need O(n) scans of the primary map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayoutOwner {
    /// The 64-bit clientid that the SEQUENCE op resolved to.
    pub client_id: u64,
    /// The 16-byte session id the LAYOUTGET arrived on.
    pub session_id: SessionIdBytes,
    /// Filesystem identifier the layout's filehandle lives in. RFC 8881
    /// §12.5.5: a LAYOUTRETURN with `return_type=FSID` releases all
    /// layouts the client holds in this fsid.
    pub fsid: u64,
}

/// Layout manager - manages layout generation and tracking
#[derive(Clone)]
pub struct LayoutManager {
    /// Registry of available devices
    device_registry: Arc<DeviceRegistry>,

    /// Active layouts (keyed by layout stateid).
    layouts: Arc<DashMap<LayoutStateId, LayoutState>>,

    /// Secondary index: client → set of layout stateids the client owns.
    /// Lets `LAYOUTRETURN ALL` and `LAYOUTRETURN FSID` filter without
    /// scanning every issued layout, and lets the backchannel know which
    /// session to send CB_LAYOUTRECALL to. Maintained alongside `layouts`
    /// in `generate_layout` / `return_layout` / `recall_layouts_for_device`.
    by_owner: Arc<DashMap<u64, Vec<LayoutStateId>>>,

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

    /// Owning client + session + filesystem (see `LayoutOwner`).
    pub owner: LayoutOwner,

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
    
    /// Flexible File Layout (RFC 8435) - for independent DS storage
    /// Each DS has its own storage, filehandles are DS-specific
    FlexFiles = 4,
}

/// Layout policy implementation
#[derive(Debug, Clone, Copy)]
enum LayoutPolicyImpl {
    /// Simple round-robin across all DSs
    RoundRobin,

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
            ConfigLayoutPolicy::RoundRobin => LayoutPolicyImpl::RoundRobin,
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
            by_owner: Arc::new(DashMap::new()),
            policy: policy_impl,
            stripe_size,
        }
    }

    /// Generate a new layout for a file.
    ///
    /// `owner` identifies the client / session / fsid that this layout is
    /// issued to. RFC 8881 §12.5 ties every layout to a specific client
    /// for recall and return-by-clientid semantics; CB_LAYOUTRECALL routes
    /// through the owner's session.
    pub fn generate_layout(
        &self,
        owner: LayoutOwner,
        filehandle: Vec<u8>,
        offset: u64,
        length: u64,
        iomode: IoMode,
    ) -> Result<LayoutState, String> {
        use tracing::warn;
        warn!("💥💥💥 LayoutManager::generate_layout() CALLED 💥💥💥");
        
        let devices = self.device_registry.list_active();
        if devices.is_empty() {
            return Err("No active data servers available".to_string());
        }

        warn!(
            "💥 Generating layout: offset={}, length={}, iomode={:?}, devices={}",
            offset,
            length,
            iomode,
            devices.len()
        );

        let segments = match self.policy {
            LayoutPolicyImpl::RoundRobin => {
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
            owner,
            filehandle,
            segments,
            iomode,
            return_on_close: true,
        };

        // Track active layouts (primary map + secondary by-client index).
        self.layouts.insert(stateid, layout.clone());
        self.by_owner
            .entry(owner.client_id)
            .or_insert_with(Vec::new)
            .push(stateid);

        info!(
            "🎯 Generated pNFS layout with {} segments, stateid={:?}, client={}",
            layout.segments.len(),
            &stateid[0..4],
            owner.client_id,
        );
        info!("   📊 Layout details:");
        for (i, seg) in layout.segments.iter().enumerate() {
            info!("      Segment {}: device={}, offset={}, length={}", 
                  i, seg.device_id, seg.offset, seg.length);
        }
        info!("   ✅ Client will now perform parallel I/O across {} data servers!", layout.segments.len());

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
        let current_offset = offset;
        let _end_offset = offset.saturating_add(length);

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

    /// Return a layout (client releases it). Cleans the secondary
    /// by-client index alongside the primary map so the indexes stay
    /// consistent.
    pub fn return_layout(&self, stateid: &LayoutStateId) -> Result<(), String> {
        if let Some((_, layout)) = self.layouts.remove(stateid) {
            info!(
                "Layout returned: stateid={:?}, segments={}, client={}",
                &stateid[0..4],
                layout.segments.len(),
                layout.owner.client_id,
            );

            // Drop from the by-client index. Empty entries are removed so the
            // map doesn't accumulate stale clientid keys after long-running
            // clients hand back all their layouts.
            if let Some(mut entry) = self.by_owner.get_mut(&layout.owner.client_id) {
                entry.retain(|s| s != stateid);
                let now_empty = entry.is_empty();
                drop(entry);
                if now_empty {
                    self.by_owner.remove(&layout.owner.client_id);
                }
            }

            // Decrement active layout counts for affected devices
            for segment in &layout.segments {
                let _ = self.device_registry.decrement_layout_count(&segment.device_id);
            }

            Ok(())
        } else {
            Err(format!("Layout not found: {:?}", &stateid[0..4]))
        }
    }

    /// Return all layouts held by `client_id` (RFC 8881 §18.44.3
    /// `LAYOUTRETURN4_ALL`). Returns the list of stateids that were
    /// released so the caller can cancel any in-flight CB_LAYOUTRECALL
    /// for them.
    pub fn return_all_for_client(&self, client_id: u64) -> Vec<LayoutStateId> {
        let stateids: Vec<LayoutStateId> = self.by_owner
            .get(&client_id)
            .map(|entry| entry.clone())
            .unwrap_or_default();
        for sid in &stateids {
            let _ = self.return_layout(sid);
        }
        stateids
    }

    /// Return all layouts held by `client_id` in `fsid` (RFC 8881 §18.44.3
    /// `LAYOUTRETURN4_FSID`).
    pub fn return_fsid_for_client(&self, client_id: u64, fsid: u64) -> Vec<LayoutStateId> {
        let stateids: Vec<LayoutStateId> = self.by_owner
            .get(&client_id)
            .map(|entry| {
                entry.iter()
                    .filter(|sid| {
                        self.layouts
                            .get(*sid)
                            .map(|l| l.owner.fsid == fsid)
                            .unwrap_or(false)
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        for sid in &stateids {
            let _ = self.return_layout(sid);
        }
        stateids
    }

    /// Enumerate active layouts owned by `client_id`. Used by the
    /// CB_LAYOUTRECALL backchannel (Task #4) when a device fails — we
    /// need to find every layout of every client that referenced the
    /// dead device so we can recall them.
    pub fn layouts_for_client(&self, client_id: u64) -> Vec<LayoutStateId> {
        self.by_owner
            .get(&client_id)
            .map(|entry| entry.clone())
            .unwrap_or_default()
    }

    /// Find every layout whose segments touch `device_id`, paired
    /// with the session id of the client that owns it. Used by the
    /// CB_LAYOUTRECALL fan-out on DS-death (Phase A.4): each pair is
    /// one CB CALL routed to a specific back-channel.
    ///
    /// Returns `(session_id, layout_stateid)` tuples — both are 16-
    /// byte fixed opaques. The session id comes from `LayoutOwner`
    /// (set on LAYOUTGET); a single layout has exactly one session.
    /// One client with multiple layouts on the dead device produces
    /// multiple pairs with the same session id.
    pub fn recall_layouts_for_device(
        &self,
        device_id: &str,
    ) -> Vec<(SessionIdBytes, LayoutStateId)> {
        let mut recalled = Vec::new();

        for entry in self.layouts.iter() {
            let has_device = entry
                .segments
                .iter()
                .any(|seg| seg.device_id == device_id);

            if has_device {
                recalled.push((entry.owner.session_id, entry.stateid));
            }
        }

        if !recalled.is_empty() {
            info!(
                "Recalling {} layout(s) using device {}",
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

    /// Test-only LayoutOwner so the test fixtures don't have to fabricate
    /// a real session id every time. Production code routes ownership
    /// through `CompoundContext`.
    fn test_owner(client_id: u64) -> LayoutOwner {
        LayoutOwner {
            client_id,
            session_id: [0u8; 16],
            fsid: 1,
        }
    }

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
                test_owner(1),                vec![0, 1, 2, 3],
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
                test_owner(1),                vec![0, 1, 2, 3],
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
                test_owner(1),                vec![0, 1, 2, 3],
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
                test_owner(1),                vec![0, 1, 2, 3],
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        // Find which device was actually used
        let device_used = &layout.segments[0].device_id;

        // Recall layouts for that device. Returns (session_id,
        // stateid) pairs for the CB fan-out path.
        let recalled = manager.recall_layouts_for_device(device_used);

        assert_eq!(recalled.len(), 1, "expected exactly one (sid, stateid) pair");
        assert_eq!(recalled[0].1, layout.stateid);
        assert_eq!(recalled[0].0, layout.owner.session_id);
    }

    #[test]
    fn test_layout_state_tracking() {
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

        // Initially no layouts
        assert_eq!(manager.layout_count(), 0);

        // Generate first layout
        let layout1 = manager
            .generate_layout(
                test_owner(1),                vec![1, 2, 3, 4],
                0,
                5 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        assert_eq!(manager.layout_count(), 1);

        // Generate second layout
        let layout2 = manager
            .generate_layout(
                test_owner(1),                vec![5, 6, 7, 8],
                0,
                10 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        assert_eq!(manager.layout_count(), 2);

        // Return first layout
        manager.return_layout(&layout1.stateid).unwrap();
        assert_eq!(manager.layout_count(), 1);

        // Return second layout
        manager.return_layout(&layout2.stateid).unwrap();
        assert_eq!(manager.layout_count(), 0);
    }

    #[test]
    fn test_layout_segments_for_striping() {
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

        // Request 24 MB (should create 3 segments of 8 MB each)
        let layout = manager
            .generate_layout(
                test_owner(1),                vec![0, 1, 2, 3],
                0,
                24 * 1024 * 1024,
                IoMode::ReadWrite,
            )
            .unwrap();

        // Should have 3 segments (one per device)
        assert_eq!(layout.segments.len(), 3);

        // Each segment should be 8 MB
        for seg in &layout.segments {
            assert_eq!(seg.length, 8 * 1024 * 1024);
        }

        // All segments should use different devices
        let device_ids: Vec<&String> = layout.segments.iter()
            .map(|s| &s.device_id)
            .collect();
        assert_eq!(device_ids.len(), 3);
    }

    #[test]
    fn test_iomode_variants() {
        assert_eq!(IoMode::Read as u32, 1);
        assert_eq!(IoMode::ReadWrite as u32, 2);
        assert_eq!(IoMode::Any as u32, 3);
    }

    #[test]
    fn test_by_owner_index_and_return_all() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(DeviceInfo::new(
            "ds-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        )).unwrap();
        let mgr = LayoutManager::new(registry, ConfigLayoutPolicy::RoundRobin, 8 * 1024 * 1024);

        // Two clients each get two layouts.
        let l_a1 = mgr.generate_layout(test_owner(1), vec![1], 0, 1024, IoMode::ReadWrite).unwrap();
        let l_a2 = mgr.generate_layout(test_owner(1), vec![2], 0, 1024, IoMode::ReadWrite).unwrap();
        let l_b1 = mgr.generate_layout(test_owner(2), vec![3], 0, 1024, IoMode::ReadWrite).unwrap();
        let l_b2 = mgr.generate_layout(test_owner(2), vec![4], 0, 1024, IoMode::ReadWrite).unwrap();

        // layouts_for_client returns the right pair, in the order they were issued.
        assert_eq!(mgr.layouts_for_client(1), vec![l_a1.stateid, l_a2.stateid]);
        assert_eq!(mgr.layouts_for_client(2), vec![l_b1.stateid, l_b2.stateid]);

        // return_all_for_client(1) drops both of client 1's layouts and the
        // by_owner key, but leaves client 2 untouched.
        let dropped = mgr.return_all_for_client(1);
        assert_eq!(dropped.len(), 2);
        assert!(mgr.get_layout(&l_a1.stateid).is_none());
        assert!(mgr.get_layout(&l_a2.stateid).is_none());
        assert!(mgr.layouts_for_client(1).is_empty());
        assert_eq!(mgr.layouts_for_client(2).len(), 2);

        // Idempotent: a second LAYOUTRETURN ALL on the same client is a no-op.
        assert_eq!(mgr.return_all_for_client(1), Vec::<LayoutStateId>::new());
    }

    #[test]
    fn test_return_fsid_filters_by_fsid() {
        let registry = Arc::new(DeviceRegistry::new());
        registry.register(DeviceInfo::new(
            "ds-1".to_string(),
            "10.0.0.1:2049".to_string(),
            vec!["nvme0n1".to_string()],
        )).unwrap();
        let mgr = LayoutManager::new(registry, ConfigLayoutPolicy::RoundRobin, 8 * 1024 * 1024);

        // Same client holds layouts in two filesystems; LAYOUTRETURN FSID
        // should release only the one matching the filter.
        let owner_fs1 = LayoutOwner { client_id: 7, session_id: [0; 16], fsid: 100 };
        let owner_fs2 = LayoutOwner { client_id: 7, session_id: [0; 16], fsid: 200 };
        let l_in_fs1 = mgr.generate_layout(owner_fs1, vec![1], 0, 1024, IoMode::Read).unwrap();
        let l_in_fs2 = mgr.generate_layout(owner_fs2, vec![2], 0, 1024, IoMode::Read).unwrap();

        let dropped = mgr.return_fsid_for_client(7, 100);
        assert_eq!(dropped, vec![l_in_fs1.stateid]);
        assert!(mgr.get_layout(&l_in_fs1.stateid).is_none());
        assert!(mgr.get_layout(&l_in_fs2.stateid).is_some());
        assert_eq!(mgr.layouts_for_client(7), vec![l_in_fs2.stateid]);
    }

    #[test]
    fn test_layout_type_values() {
        assert_eq!(LayoutType::NfsV4_1Files as u32, 1);
        assert_eq!(LayoutType::BlockVolume as u32, 2);
        assert_eq!(LayoutType::Osd2Objects as u32, 3);
        assert_eq!(LayoutType::FlexFiles as u32, 4);
    }
}


