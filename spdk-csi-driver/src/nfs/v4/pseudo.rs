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
    
    /// pNFS: Layout type support (for future)
    /// - LAYOUT4_NFSV4_1_FILES (1): File-based layouts
    /// - LAYOUT4_BLOCK_VOLUME (2): Block layouts (SPDK/NVMe)
    /// - LAYOUT4_SCSI (3): SCSI layouts
    pub layout_types: Vec<u32>,
    
    /// pNFS: Whether this export supports direct data access
    pub supports_pnfs: bool,
}

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
            // For SPDK/NVMe backend, block layouts are ideal
            layout_types: vec![
                2, // LAYOUT4_BLOCK_VOLUME - Direct block access
                1, // LAYOUT4_NFSV4_1_FILES - Fallback for compatibility
            ],
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
    /// Create a new pseudo-filesystem
    pub fn new() -> Self {
        let instance_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let root_create_time = instance_id;
        
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
            info!("   pNFS: Enabled (layout types: {:?})", export.layout_types);
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
        handle.data[0] == PSEUDO_ROOT_MARKER && 
        handle.data.len() >= 17 &&
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
    
    /// Get supported layout types (for pNFS)
    pub fn get_layout_types(&self) -> Vec<u32> {
        if !self.pnfs_enabled {
            return vec![];
        }
        
        // For SPDK/NVMe backend:
        vec![
            2, // LAYOUT4_BLOCK_VOLUME - Direct block device access
            1, // LAYOUT4_NFSV4_1_FILES - File-based fallback
        ]
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
    
    #[test]
    fn test_pseudo_root_handle() {
        let pseudo_fs = PseudoFilesystem::new();
        let handle = pseudo_fs.get_pseudo_root_handle();
        
        assert!(pseudo_fs.is_pseudo_root(&handle));
        assert_eq!(handle.data[0], PSEUDO_ROOT_MARKER);
    }
    
    #[test]
    fn test_add_export() {
        let pseudo_fs = PseudoFilesystem::new();
        let export = Export::new(1, "volume".to_string(), PathBuf::from("/data"));
        
        pseudo_fs.add_export(export).unwrap();
        
        let found = pseudo_fs.lookup_export("volume");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "volume");
    }
    
    #[test]
    fn test_list_exports() {
        let pseudo_fs = PseudoFilesystem::new();
        pseudo_fs.add_export(Export::new(1, "vol1".to_string(), PathBuf::from("/data1"))).unwrap();
        pseudo_fs.add_export(Export::new(2, "vol2".to_string(), PathBuf::from("/data2"))).unwrap();
        
        let exports = pseudo_fs.list_exports();
        assert_eq!(exports.len(), 2);
        assert!(exports.contains(&"vol1".to_string()));
        assert!(exports.contains(&"vol2".to_string()));
    }
    
    #[test]
    fn test_pseudo_root_attrs() {
        let pseudo_fs = PseudoFilesystem::new();
        pseudo_fs.add_export(Export::new(1, "volume".to_string(), PathBuf::from("/data"))).unwrap();
        
        let attrs = pseudo_fs.get_pseudo_root_attrs();
        
        assert_eq!(attrs.fsid, PSEUDO_ROOT_FSID);
        assert_eq!(attrs.fileid, PSEUDO_ROOT_FILEID);
        assert_eq!(attrs.nlink, 3); // . + .. + 1 export
    }
    
    #[test]
    fn test_pnfs_support() {
        let pseudo_fs = PseudoFilesystem::new();
        
        assert!(pseudo_fs.supports_pnfs());
        
        let layout_types = pseudo_fs.get_layout_types();
        assert!(layout_types.contains(&1)); // FILES
        assert!(layout_types.contains(&2)); // BLOCK
    }
}

