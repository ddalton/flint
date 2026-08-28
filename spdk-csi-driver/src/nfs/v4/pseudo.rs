// NFSv4 Pseudo-Filesystem Implementation
//
// Per RFC 7530 Section 7, NFSv4 servers MUST present a pseudo-filesystem
// that provides a unified namespace for all exports.
//
// This module implements:
// - Pseudo-filesystem root with synthetic attributes
// - Export registry and lookup
// - Future pNFS layout support hooks

use crate::nfs::v4::protocol::Nfs4FileHandle;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

/// Pseudo-filesystem root file ID (synthetic, always 1)
pub const PSEUDO_ROOT_FILEID: u64 = 1;

/// Pseudo-filesystem FSID (synthetic, {0, 0} indicates pseudo-fs)
pub const PSEUDO_ROOT_FSID: (u64, u64) = (0, 0);

/// Marker byte in filehandle to identify pseudo-root
const PSEUDO_ROOT_MARKER: u8 = 0xFF;

/// Export information in the pseudo-filesystem
#[derive(Debug, Clone)]
pub struct Export {
    /// Export ID (unique identifier)
    pub id: u32,
    
    /// Name in pseudo-filesystem (e.g., "volume", "data")
    pub name: String,
    
    /// Actual filesystem path being exported
    pub path: PathBuf,
    
    /// Export creation time (for attributes)
    pub create_time: u64,
    
    /// pNFS: Whether this export supports direct data access
    pub supports_pnfs: bool,
}

// NOTE: an `Export.layout_types` field and a
// `PseudoFilesystem::get_layout_types` method used to live here, both
// defaulting to `[2, 1]` with 2 mislabeled "BLOCK" (RFC 8881 §3.3.13:
// OSD2=2, BLOCK=3). Neither ever reached the wire — the real
// advertisement is `encode_fs_layout_types` in operations/fileops.rs,
// the single source of truth. Deleted 2026-08-09 so a stale second
// "source of truth" can't be resurrected; per-volume advertisement for
// the pnfs-block class extends the fileops.rs helper, not this struct.

impl Export {
    /// Create a new export
    pub fn new(id: u32, name: String, path: PathBuf) -> Self {
        let create_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        Self {
            id,
            name,
            path,
            create_time,
            supports_pnfs: true, // SPDK enables high-performance pNFS
        }
    }
}

/// NFSv4 Pseudo-Filesystem
///
/// Provides a virtual root filesystem that unifies all exports
/// under a single namespace per RFC 7530 Section 7.
pub struct PseudoFilesystem {
    /// Exports registry (name -> Export)
    exports: Arc<RwLock<HashMap<String, Export>>>,
    
    /// Reverse lookup (export_id -> name)
    export_ids: Arc<RwLock<HashMap<u32, String>>>,
    
    /// Server instance ID (for filehandle uniqueness)
    instance_id: u64,
    
    /// Pseudo-root creation time
    root_create_time: u64,
    
    /// pNFS: Whether server supports parallel NFS
    pnfs_enabled: bool,
    
    /// pNFS: Maximum number of layout segments per LAYOUTGET
    pnfs_max_layouts: u32,
}

impl PseudoFilesystem {
    /// Create a new pseudo-filesystem.
    ///
    /// `instance_id` MUST be the server's stable per-volume id — the same
    /// one `FileHandleManager` embeds in real-fs handles
    /// (`stable_nfs_instance_id` / `PNFS_INSTANCE_ID`). The old
    /// constructor stamped `SystemTime::now()` at every boot, so the
    /// pseudo root's identity (handle bytes, create/change attrs) flapped
    /// on every server restart: reconnecting clients had their cached
    /// mount-root attrs invalidated for no reason, and stale-handle
    /// triage (drill 3.2) had one more instance-varying surface to rule
    /// out. With the stable id, a replacement server presents an
    /// identical pseudo root.
    pub fn new(instance_id: u64) -> Self {
        // Synthetic-but-sane creation time DERIVED from the stable id
        // (the id is a 64-bit hash, not epoch seconds): stable across
        // restarts, plausible as a date.
        let root_create_time = 1_700_000_000 + (instance_id % 31_536_000);

        info!("🌳 Pseudo-filesystem created (instance_id={})", instance_id);
        info!("   RFC 7530 Section 7: Unified namespace for NFSv4 exports");
        
        Self {
            exports: Arc::new(RwLock::new(HashMap::new())),
            export_ids: Arc::new(RwLock::new(HashMap::new())),
            instance_id,
            root_create_time,
            pnfs_enabled: true, // Enable for SPDK/NVMe performance
            pnfs_max_layouts: 128, // Allow many parallel operations
        }
    }
    
    /// Add an export to the pseudo-filesystem
    pub fn add_export(&self, export: Export) -> Result<(), String> {
        let name = export.name.clone();
        let id = export.id;
        
        info!("📁 Adding export to pseudo-filesystem:");
        info!("   Name: {}", name);
        info!("   Path: {:?}", export.path);
        info!("   ID: {}", id);
        if export.supports_pnfs {
            info!("   pNFS: Enabled");
        }
        
        let mut exports = self.exports.write().unwrap();
        let mut export_ids = self.export_ids.write().unwrap();
        
        if exports.contains_key(&name) {
            return Err(format!("Export '{}' already exists", name));
        }
        
        exports.insert(name.clone(), export);
        export_ids.insert(id, name);
        
        Ok(())
    }
    
    /// Generate pseudo-root filehandle
    ///
    /// This handle is special:
    /// - Starts with PSEUDO_ROOT_MARKER (0xFF)
    /// - Contains instance_id for uniqueness
    /// - Recognized by is_pseudo_root()
    pub fn get_pseudo_root_handle(&self) -> Nfs4FileHandle {
        let mut data = Vec::with_capacity(17);
        
        // Version byte with pseudo-root marker
        data.push(PSEUDO_ROOT_MARKER);
        
        // Instance ID (8 bytes)
        data.extend_from_slice(&self.instance_id.to_be_bytes());
        
        // Pseudo-root marker again (for validation)
        data.extend_from_slice(b"PSEUDO_ROOT");
        
        debug!("Generated pseudo-root filehandle: {} bytes", data.len());
        Nfs4FileHandle { data }
    }
    
    /// Check if a filehandle represents the pseudo-root
    pub fn is_pseudo_root(&self, handle: &Nfs4FileHandle) -> bool {
        if handle.data.is_empty() {
            return false;
        }
        
        // Check for pseudo-root marker
        // The slice below runs to 20, so 17 was not enough: a PUTFH
        // carrying a 17-, 18- or 19-byte handle whose first byte is the
        // marker panicked here, before any credential or state was
        // consulted. The minted handle is 20 bytes.
        handle.data[0] == PSEUDO_ROOT_MARKER && 
        handle.data.len() >= 20 &&
        &handle.data[9..20] == b"PSEUDO_ROOT"
    }
    
    /// Lookup an export by name (for LOOKUP from pseudo-root)
    pub fn lookup_export(&self, name: &str) -> Option<Export> {
        let exports = self.exports.read().unwrap();
        exports.get(name).cloned()
    }
    
    /// Get export by ID
    pub fn get_export_by_id(&self, id: u32) -> Option<Export> {
        let export_ids = self.export_ids.read().unwrap();
        let name = export_ids.get(&id)?;
        
        let exports = self.exports.read().unwrap();
        exports.get(name).cloned()
    }
    
    /// List all export names (for READDIR on pseudo-root)
    pub fn list_exports(&self) -> Vec<String> {
        let exports = self.exports.read().unwrap();
        exports.keys().cloned().collect()
    }
    
    /// Get pseudo-root attributes
    ///
    /// Returns synthetic attributes for the virtual root:
    /// - FSID: {0, 0} (indicates pseudo-filesystem)
    /// - FILEID: 1 (synthetic root ID)
    /// - TYPE: NF4DIR (directory)
    /// - SIZE: 4096 (standard directory size)
    /// - MTIME: Server creation time
    /// - NLINK: 2 + number of exports
    pub fn get_pseudo_root_attrs(&self) -> PseudoRootAttrs {
        let exports = self.exports.read().unwrap();
        let nlink = 2 + exports.len() as u32; // . + .. + exports
        
        PseudoRootAttrs {
            fsid: PSEUDO_ROOT_FSID,
            fileid: PSEUDO_ROOT_FILEID,
            nlink,
            size: 4096,
            create_time: self.root_create_time,
            instance_id: self.instance_id,
        }
    }
    
    /// Check if server supports pNFS
    pub fn supports_pnfs(&self) -> bool {
        self.pnfs_enabled
    }

    /// Get maximum layouts per LAYOUTGET (for pNFS)
    pub fn get_max_layouts(&self) -> u32 {
        self.pnfs_max_layouts
    }
}

/// Pseudo-root synthetic attributes
#[derive(Debug, Clone)]
pub struct PseudoRootAttrs {
    pub fsid: (u64, u64),
    pub fileid: u64,
    pub nlink: u32,
    pub size: u64,
    pub create_time: u64,
    pub instance_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bounds check said 17; the slice runs to 20. A PUTFH carrying
    /// a 17-, 18- or 19-byte handle whose first byte is the marker
    /// panicked here — before any credential, session or state was
    /// consulted, so the whole reach was "can send bytes to 2049".
    ///
    /// The loop covers 0..=24 so the leg also pins the two ends: short
    /// handles answer false, and a real 20-byte marker handle still
    /// answers true (asserted separately below, since a bare "no panic"
    /// oracle would pass against a function that always returned false).
    #[test]
    fn a_short_handle_carrying_the_marker_byte_is_not_a_pseudo_root() {
        let pfs = PseudoFilesystem::new(TEST_INSTANCE);
        for len in 0usize..=24 {
            let handle = Nfs4FileHandle {
                data: std::iter::once(PSEUDO_ROOT_MARKER)
                    .chain(std::iter::repeat(0u8))
                    .take(len)
                    .collect(),
            };
            assert!(
                !pfs.is_pseudo_root(&handle),
                "a {len}-byte all-zero-tail handle is not the pseudo root"
            );
        }
        // The other direction: the genuine article still resolves, so
        // the loop above is not passing because the check is dead.
        assert!(pfs.is_pseudo_root(&pfs.get_pseudo_root_handle()));
    }

    /// Stands in for `stable_nfs_instance_id(volume_id)` — an arbitrary
    /// 64-bit hash, NOT epoch seconds.
    const TEST_INSTANCE: u64 = 0xDEAD_BEEF_CAFE_F00D;

    /// 3.2 hardening: the pseudo root must be IDENTICAL across server
    /// restarts when constructed with the same stable instance id — same
    /// handle bytes, same synthetic attrs (create_time included). The old
    /// boot-time stamp made every restart mint a "different" root, so
    /// reconnecting clients invalidated their cached mount-root for no
    /// reason.
    #[test]
    fn restart_with_same_instance_id_preserves_root_identity() {
        let a = PseudoFilesystem::new(TEST_INSTANCE);
        let b = PseudoFilesystem::new(TEST_INSTANCE);
        assert_eq!(
            a.get_pseudo_root_handle().data,
            b.get_pseudo_root_handle().data,
            "root handle bytes must survive a server restart"
        );
        let (aa, ba) = (a.get_pseudo_root_attrs(), b.get_pseudo_root_attrs());
        assert_eq!(aa.create_time, ba.create_time, "root create_time must not flap per boot");
        assert_eq!(aa.instance_id, ba.instance_id);
        assert_eq!(aa.fileid, ba.fileid);
        assert_eq!(aa.fsid, ba.fsid);
    }

    /// A client's cached root handle from the PREVIOUS server incarnation
    /// must still be recognized after a restart — mount-root continuity
    /// is what lets reconnecting clients resume without a remount.
    /// (Pinned: is_pseudo_root validates the marker, not the embedded
    /// id — a strict id compare here would brick every reconnect.)
    #[test]
    fn old_incarnation_root_handle_still_recognized() {
        let old = PseudoFilesystem::new(1111);
        let newer = PseudoFilesystem::new(2222);
        assert!(newer.is_pseudo_root(&old.get_pseudo_root_handle()));
    }

    /// The derived create_time must be plausible epoch seconds for any
    /// hash-shaped instance id (the raw id is NOT a timestamp).
    #[test]
    fn derived_create_time_is_sane_epoch_seconds() {
        for id in [0u64, 1, TEST_INSTANCE, u64::MAX] {
            let t = PseudoFilesystem::new(id).get_pseudo_root_attrs().create_time;
            assert!((1_700_000_000..1_731_536_000).contains(&t), "id {id} → time {t}");
        }
    }

    #[test]
    fn test_pseudo_root_handle() {
        let pseudo_fs = PseudoFilesystem::new(TEST_INSTANCE);
        let handle = pseudo_fs.get_pseudo_root_handle();
        
        assert!(pseudo_fs.is_pseudo_root(&handle));
        assert_eq!(handle.data[0], PSEUDO_ROOT_MARKER);
    }
    
    #[test]
    fn test_add_export() {
        let pseudo_fs = PseudoFilesystem::new(TEST_INSTANCE);
        let export = Export::new(1, "volume".to_string(), PathBuf::from("/data"));
        
        pseudo_fs.add_export(export).unwrap();
        
        let found = pseudo_fs.lookup_export("volume");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "volume");
    }
    
    #[test]
    fn test_list_exports() {
        let pseudo_fs = PseudoFilesystem::new(TEST_INSTANCE);
        pseudo_fs.add_export(Export::new(1, "vol1".to_string(), PathBuf::from("/data1"))).unwrap();
        pseudo_fs.add_export(Export::new(2, "vol2".to_string(), PathBuf::from("/data2"))).unwrap();
        
        let exports = pseudo_fs.list_exports();
        assert_eq!(exports.len(), 2);
        assert!(exports.contains(&"vol1".to_string()));
        assert!(exports.contains(&"vol2".to_string()));
    }
    
    #[test]
    fn test_pseudo_root_attrs() {
        let pseudo_fs = PseudoFilesystem::new(TEST_INSTANCE);
        pseudo_fs.add_export(Export::new(1, "volume".to_string(), PathBuf::from("/data"))).unwrap();
        
        let attrs = pseudo_fs.get_pseudo_root_attrs();
        
        assert_eq!(attrs.fsid, PSEUDO_ROOT_FSID);
        assert_eq!(attrs.fileid, PSEUDO_ROOT_FILEID);
        assert_eq!(attrs.nlink, 3); // . + .. + 1 export
    }
    
    #[test]
    fn test_pnfs_support() {
        let pseudo_fs = PseudoFilesystem::new(TEST_INSTANCE);
        assert!(pseudo_fs.supports_pnfs());
        // The layout-type advertisement itself is fileops.rs's
        // encode_fs_layout_types — the deleted get_layout_types here
        // returned [2,1] with the values mislabeled and never reached
        // the wire.
    }
}

