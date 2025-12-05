//! File Handle Management
//!
//! Maps NFSv3 file handles to filesystem paths and maintains the bidirectional mapping.
//!
//! # Design
//!
//! - File handles are opaque 64-byte (max) identifiers for files and directories
//! - Internally, we use inode numbers to uniquely identify files
//! - We maintain a cache mapping file handles ↔ paths for performance

use super::protocol::FileHandle;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// File handle cache - maintains mapping between file handles and paths
#[derive(Clone)]
pub struct HandleCache {
    /// Map from inode → path
    inode_to_path: Arc<RwLock<HashMap<u64, PathBuf>>>,
    
    /// Map from path → inode (reverse lookup)
    path_to_inode: Arc<RwLock<HashMap<PathBuf, u64>>>,
    
    /// Root export path
    root: PathBuf,
}

impl HandleCache {
    /// Create a new handle cache for the given export root
    pub fn new(root: PathBuf) -> Self {
        let mut cache = Self {
            inode_to_path: Arc::new(RwLock::new(HashMap::new())),
            path_to_inode: Arc::new(RwLock::new(HashMap::new())),
            root,
        };
        
        // Register the root directory (use empty path for root)
        if let Ok(metadata) = std::fs::metadata(&cache.root) {
            let root_inode = metadata.ino();
            cache.insert(root_inode, PathBuf::from(""));
        }
        
        cache
    }
    
    /// Get the root file handle
    pub fn root_handle(&self) -> Result<FileHandle, std::io::Error> {
        let metadata = std::fs::metadata(&self.root)?;
        Ok(FileHandle::from_inode(metadata.ino()))
    }
    
    /// Insert a mapping from inode to relative path
    fn insert(&mut self, inode: u64, path: PathBuf) {
        self.inode_to_path.write().unwrap().insert(inode, path.clone());
        self.path_to_inode.write().unwrap().insert(path, inode);
    }
    
    /// Resolve a file handle to an absolute filesystem path
    pub fn resolve(&self, handle: &FileHandle) -> Result<PathBuf, std::io::Error> {
        // Extract inode from file handle
        let inode = self.handle_to_inode(handle)?;
        
        // Look up in cache
        if let Some(rel_path) = self.inode_to_path.read().unwrap().get(&inode) {
            let resolved = self.root.join(rel_path);
            tracing::debug!("Resolved inode {} to path: {:?} (cached)", inode, resolved);
            return Ok(resolved);
        }
        
        // Not in cache - search the filesystem to find it
        tracing::debug!("Inode {} not in cache, searching filesystem...", inode);
        
        match self.find_by_inode(inode) {
            Ok(path) => {
                tracing::debug!("Found inode {} at path: {:?}", inode, path);
                Ok(path)
            }
            Err(e) => {
                tracing::warn!("Stale file handle: inode {} not found on disk", inode);
                tracing::debug!("Cache contents: {:?}", self.inode_to_path.read().unwrap().keys().collect::<Vec<_>>());
                Err(e)
            }
        }
    }
    
    /// Find a file by inode number by recursively searching the export
    /// This is a fallback for when the cache doesn't have the mapping
    fn find_by_inode(&self, target_inode: u64) -> Result<PathBuf, std::io::Error> {
        use std::fs;
        use std::os::unix::fs::MetadataExt;
        
        // Start from root and search
        self.search_dir_for_inode(&self.root, target_inode)
    }
    
    /// Recursively search a directory for a specific inode
    fn search_dir_for_inode(&self, dir: &Path, target_inode: u64) -> Result<PathBuf, std::io::Error> {
        use std::fs;
        use std::os::unix::fs::MetadataExt;
        
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;
            
            if metadata.ino() == target_inode {
                // Found it! Cache the mapping
                let rel_path = path
                    .strip_prefix(&self.root)
                    .unwrap_or(Path::new(""))
                    .to_path_buf();
                
                self.inode_to_path.write().unwrap().insert(target_inode, rel_path.clone());
                self.path_to_inode.write().unwrap().insert(rel_path, target_inode);
                
                tracing::debug!("Cached found inode {} -> {:?}", target_inode, path);
                return Ok(path);
            }
            
            // Recurse into subdirectories (but limit depth for safety)
            if metadata.is_dir() {
                if let Ok(found) = self.search_dir_for_inode(&path, target_inode) {
                    return Ok(found);
                }
            }
        }
        
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Inode not found in export",
        ))
    }
    
    /// Create a file handle from a path
    /// This will stat the file to get its inode and cache the mapping
    pub fn handle_from_path(&self, path: &Path) -> Result<FileHandle, std::io::Error> {
        // Get full path
        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        
        // Stat the file to get inode
        let metadata = std::fs::metadata(&full_path)?;
        let inode = metadata.ino();
        
        // Get relative path from root
        let rel_path = full_path
            .strip_prefix(&self.root)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        
        // Cache the mapping
        self.inode_to_path.write().unwrap().insert(inode, rel_path.clone());
        self.path_to_inode.write().unwrap().insert(rel_path, inode);
        
        Ok(FileHandle::from_inode(inode))
    }
    
    /// Look up a child file/directory within a parent directory
    pub fn lookup_child(
        &self,
        parent_handle: &FileHandle,
        name: &str,
    ) -> Result<FileHandle, std::io::Error> {
        // Block explicit ".." to prevent traversal
        if name == ".." {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Path traversal not allowed",
            ));
        }
        
        // Resolve parent directory
        let parent_path = self.resolve(parent_handle)?;
        
        // Construct child path
        let child_path = parent_path.join(name);
        
        // Validate the child is within the export
        if !child_path.starts_with(&self.root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Path traversal attempt",
            ));
        }
        
        // Get child's inode (use symlink_metadata to not follow symlinks)
        let metadata = std::fs::symlink_metadata(&child_path)?;
        let inode = metadata.ino();
        
        // Cache the mapping
        let rel_path = child_path
            .strip_prefix(&self.root)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        
        tracing::debug!(
            "Caching file handle: inode={}, name={}, rel_path={:?}",
            inode, name, rel_path
        );
        
        self.inode_to_path.write().unwrap().insert(inode, rel_path.clone());
        self.path_to_inode.write().unwrap().insert(rel_path, inode);
        
        Ok(FileHandle::from_inode(inode))
    }
    
    /// Extract inode number from file handle
    fn handle_to_inode(&self, handle: &FileHandle) -> Result<u64, std::io::Error> {
        let bytes = handle.as_bytes();
        if bytes.len() < 8 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid file handle",
            ));
        }
        
        // Inode is stored as little-endian u64
        let mut inode_bytes = [0u8; 8];
        inode_bytes.copy_from_slice(&bytes[0..8]);
        Ok(u64::from_le_bytes(inode_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    
    #[test]
    fn test_root_handle() {
        let tmpdir = tempfile::tempdir().unwrap();
        let cache = HandleCache::new(tmpdir.path().to_path_buf());
        
        let root_handle = cache.root_handle().unwrap();
        let resolved = cache.resolve(&root_handle).unwrap();
        
        assert_eq!(resolved, tmpdir.path());
    }
    
    #[test]
    fn test_handle_from_path() {
        let tmpdir = tempfile::tempdir().unwrap();
        fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();
        
        let cache = HandleCache::new(tmpdir.path().to_path_buf());
        let handle = cache.handle_from_path(Path::new("test.txt")).unwrap();
        let resolved = cache.resolve(&handle).unwrap();
        
        assert_eq!(resolved, tmpdir.path().join("test.txt"));
    }
    
    #[test]
    fn test_lookup_child() {
        let tmpdir = tempfile::tempdir().unwrap();
        fs::write(tmpdir.path().join("file.txt"), "data").unwrap();
        
        let cache = HandleCache::new(tmpdir.path().to_path_buf());
        let root_handle = cache.root_handle().unwrap();
        let child_handle = cache.lookup_child(&root_handle, "file.txt").unwrap();
        let resolved = cache.resolve(&child_handle).unwrap();
        
        assert_eq!(resolved, tmpdir.path().join("file.txt"));
    }
    
    #[test]
    fn test_path_traversal_blocked() {
        let tmpdir = tempfile::tempdir().unwrap();
        let cache = HandleCache::new(tmpdir.path().to_path_buf());
        let root_handle = cache.root_handle().unwrap();
        
        // Try to traverse outside export
        let result = cache.lookup_child(&root_handle, "..");
        assert!(result.is_err());
    }
}

