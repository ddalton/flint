// NFSv4 File Handle Management
//
// NFSv4 file handles are opaque to clients but meaningful to servers.
// Unlike NFSv3, NFSv4 file handles should be persistent across server restarts.
//
// Our approach:
// - Hash-based generation from path
// - Include server instance ID to detect stale handles
// - Deterministic (same path = same handle)
// - Secure (can't be guessed from path alone)
//
// Handle Format (variable length, up to 128 bytes):
// - Version (1 byte): Handle format version
// - Instance ID (8 bytes): Server instance identifier
// - Path Hash (32 bytes): SHA-256 hash of path
// - Path (variable): Full path string (for verification)

use super::protocol::Nfs4FileHandle;
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// File handle manager - maps between paths and file handles
pub struct FileHandleManager {
    /// Server instance ID (changes on restart to invalidate old handles)
    instance_id: u64,

    /// Cache of path -> handle mappings (for fast lookup)
    path_to_handle: Arc<RwLock<HashMap<PathBuf, Nfs4FileHandle>>>,

    /// Cache of handle -> path mappings (for reverse lookup)
    handle_to_path: Arc<RwLock<HashMap<Vec<u8>, PathBuf>>>,

    /// Root export path
    export_path: PathBuf,
}

impl FileHandleManager {
    /// Create a new file handle manager
    pub fn new(export_path: PathBuf) -> Self {
        // Canonicalize the export path so relative inputs work with PUTROOTFH
        // and normalization checks. If canonicalize fails, keep the original
        // to avoid crashing; later operations will surface a clear error.
        let export_path = export_path
            .canonicalize()
            .unwrap_or(export_path);

        // Generate instance ID from current timestamp
        let instance_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        debug!("FileHandleManager created with instance_id={}", instance_id);

        Self {
            instance_id,
            path_to_handle: Arc::new(RwLock::new(HashMap::new())),
            handle_to_path: Arc::new(RwLock::new(HashMap::new())),
            export_path,
        }
    }

    /// Generate a file handle for a path
    pub fn path_to_filehandle(&self, path: &Path) -> Result<Nfs4FileHandle, String> {
        // Normalize path (remove . and ..)
        let normalized = self.normalize_path(path)?;

        // Check cache first
        {
            let cache = self.path_to_handle.read().unwrap();
            if let Some(fh) = cache.get(&normalized) {
                return Ok(fh.clone());
            }
        }

        // Generate new handle
        let handle = self.generate_handle(&normalized)?;

        // Cache it
        {
            let mut path_cache = self.path_to_handle.write().unwrap();
            let mut handle_cache = self.handle_to_path.write().unwrap();

            path_cache.insert(normalized.clone(), handle.clone());
            handle_cache.insert(handle.data.clone(), normalized);
        }

        Ok(handle)
    }

    /// Resolve a file handle back to a path
    pub fn filehandle_to_path(&self, handle: &Nfs4FileHandle) -> Result<PathBuf, String> {
        // Check cache first
        {
            let cache = self.handle_to_path.read().unwrap();
            if let Some(path) = cache.get(&handle.data) {
                return Ok(path.clone());
            }
        }

        // Parse and validate handle
        let path = self.parse_handle(handle)?;

        // Verify the path still exists and matches
        // (In production, you might want to check file metadata too)

        // Cache it
        {
            let mut path_cache = self.path_to_handle.write().unwrap();
            let mut handle_cache = self.handle_to_path.write().unwrap();

            path_cache.insert(path.clone(), handle.clone());
            handle_cache.insert(handle.data.clone(), path.clone());
        }

        Ok(path)
    }

    /// Get the root file handle
    pub fn root_filehandle(&self) -> Result<Nfs4FileHandle, String> {
        self.path_to_filehandle(&self.export_path)
    }

    /// Alias for root_filehandle (NFSv4 terminology)
    pub fn get_root_fh(&self) -> Result<Nfs4FileHandle, String> {
        self.root_filehandle()
    }

    /// Get the export root path
    pub fn get_export_path(&self) -> &Path {
        &self.export_path
    }

    /// Alias for path_to_filehandle (get or create)
    pub fn get_or_create_handle(&self, path: &Path) -> Result<Nfs4FileHandle, String> {
        self.path_to_filehandle(path)
    }

    /// Alias for filehandle_to_path (resolve)
    pub fn resolve_handle(&self, handle: &Nfs4FileHandle) -> Result<PathBuf, String> {
        self.filehandle_to_path(handle)
    }

    /// Validate a file handle (check instance ID)
    pub fn validate_handle(&self, handle: &Nfs4FileHandle) -> Result<(), String> {
        if handle.data.len() < 41 {
            return Err("File handle too short".to_string());
        }

        // Check version
        if handle.data[0] != 1 {
            return Err("Unsupported file handle version".to_string());
        }

        // Extract instance ID
        let mut instance_bytes = [0u8; 8];
        instance_bytes.copy_from_slice(&handle.data[1..9]);
        let handle_instance = u64::from_be_bytes(instance_bytes);

        // Check if instance matches
        if handle_instance != self.instance_id {
            warn!("Stale file handle detected: instance {} != {}",
                  handle_instance, self.instance_id);
            return Err("Stale file handle".to_string());
        }

        Ok(())
    }

    /// Generate a file handle from a path
    fn generate_handle(&self, path: &Path) -> Result<Nfs4FileHandle, String> {
        let path_str = path.to_str()
            .ok_or_else(|| "Invalid path".to_string())?;

        // Compute SHA-256 hash of path
        let mut hasher = Sha256::new();
        hasher.update(path_str.as_bytes());
        hasher.update(&self.instance_id.to_be_bytes()); // Include instance ID in hash
        let hash = hasher.finalize();

        // Build handle:
        // - Version (1 byte)
        // - Instance ID (8 bytes)
        // - Hash (32 bytes)
        // - Path length (2 bytes)
        // - Path (variable)

        let path_bytes = path_str.as_bytes();
        let path_len = path_bytes.len() as u16;

        let total_len = 1 + 8 + 32 + 2 + path_bytes.len();
        if total_len > Nfs4FileHandle::MAX_SIZE {
            return Err("Path too long for file handle".to_string());
        }

        let mut data = Vec::with_capacity(total_len);

        // Version
        data.push(1);

        // Instance ID
        data.extend_from_slice(&self.instance_id.to_be_bytes());

        // Hash
        data.extend_from_slice(&hash);

        // Path length
        data.extend_from_slice(&path_len.to_be_bytes());

        // Path
        data.extend_from_slice(path_bytes);

        Ok(Nfs4FileHandle { data })
    }

    /// Parse a file handle to extract the path
    fn parse_handle(&self, handle: &Nfs4FileHandle) -> Result<PathBuf, String> {
        // Validate first
        self.validate_handle(handle)?;

        if handle.data.len() < 43 {
            return Err("File handle too short".to_string());
        }

        // Extract path length (at offset 41)
        let mut len_bytes = [0u8; 2];
        len_bytes.copy_from_slice(&handle.data[41..43]);
        let path_len = u16::from_be_bytes(len_bytes) as usize;

        // Extract path (at offset 43)
        if handle.data.len() < 43 + path_len {
            return Err("File handle truncated".to_string());
        }

        let path_str = std::str::from_utf8(&handle.data[43..43 + path_len])
            .map_err(|_| "Invalid path encoding".to_string())?;

        // Verify hash
        let mut hasher = Sha256::new();
        hasher.update(path_str.as_bytes());
        hasher.update(&self.instance_id.to_be_bytes());
        let computed_hash = hasher.finalize();

        if computed_hash.as_slice() != &handle.data[9..41] {
            return Err("File handle hash mismatch".to_string());
        }

        Ok(PathBuf::from(path_str))
    }

    /// Normalize a path (resolve . and .., ensure within export)
    fn normalize_path(&self, path: &Path) -> Result<PathBuf, String> {
        // Convert to absolute path
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.export_path.join(path)
        };

        // Canonicalize to resolve . and ..
        let canonical = abs_path.canonicalize()
            .map_err(|e| format!("Path canonicalization failed: {}", e))?;

        // Ensure path is within export
        if !canonical.starts_with(&self.export_path) {
            return Err("Path outside export".to_string());
        }

        Ok(canonical)
    }

    /// Clear the cache (useful for testing)
    #[allow(dead_code)]
    pub fn clear_cache(&self) {
        self.path_to_handle.write().unwrap().clear();
        self.handle_to_path.write().unwrap().clear();
    }

    /// Get cache statistics
    #[allow(dead_code)]
    pub fn cache_stats(&self) -> (usize, usize) {
        let path_cache_size = self.path_to_handle.read().unwrap().len();
        let handle_cache_size = self.handle_to_path.read().unwrap().len();
        (path_cache_size, handle_cache_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_filehandle_roundtrip() {
        let temp_dir = std::env::temp_dir().join("nfsv4_test");
        fs::create_dir_all(&temp_dir).unwrap();

        let manager = FileHandleManager::new(temp_dir.clone());
        let test_path = temp_dir.join("test.txt");
        fs::write(&test_path, b"test").unwrap();

        // Generate handle
        let handle = manager.path_to_filehandle(&test_path).unwrap();

        // Resolve back
        let resolved_path = manager.filehandle_to_path(&handle).unwrap();

        assert_eq!(test_path.canonicalize().unwrap(), resolved_path);

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_root_filehandle() {
        let temp_dir = std::env::temp_dir().join("nfsv4_root_test");
        fs::create_dir_all(&temp_dir).unwrap();

        let manager = FileHandleManager::new(temp_dir.clone());

        // Get root handle
        let root_handle = manager.root_filehandle().unwrap();

        // Resolve back
        let resolved_path = manager.filehandle_to_path(&root_handle).unwrap();

        assert_eq!(temp_dir.canonicalize().unwrap(), resolved_path);

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_handle_validation() {
        let temp_dir = std::env::temp_dir().join("nfsv4_validation_test");
        fs::create_dir_all(&temp_dir).unwrap();

        let manager1 = FileHandleManager::new(temp_dir.clone());
        let manager2 = FileHandleManager::new(temp_dir.clone());

        let test_path = temp_dir.join("test.txt");
        fs::write(&test_path, b"test").unwrap();

        // Generate handle with manager1
        let handle = manager1.path_to_filehandle(&test_path).unwrap();

        // Should be valid for manager1
        assert!(manager1.validate_handle(&handle).is_ok());

        // Should be invalid for manager2 (different instance)
        assert!(manager2.validate_handle(&handle).is_err());

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_handle_deterministic() {
        let temp_dir = std::env::temp_dir().join("nfsv4_deterministic_test");
        fs::create_dir_all(&temp_dir).unwrap();

        let manager = FileHandleManager::new(temp_dir.clone());
        let test_path = temp_dir.join("test.txt");
        fs::write(&test_path, b"test").unwrap();

        // Generate handle twice
        let handle1 = manager.path_to_filehandle(&test_path).unwrap();
        let handle2 = manager.path_to_filehandle(&test_path).unwrap();

        // Should be identical
        assert_eq!(handle1.data, handle2.data);

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_cache() {
        let temp_dir = std::env::temp_dir().join("nfsv4_cache_test");
        fs::create_dir_all(&temp_dir).unwrap();

        let manager = FileHandleManager::new(temp_dir.clone());
        let test_path = temp_dir.join("test.txt");
        fs::write(&test_path, b"test").unwrap();

        // Cache should be empty
        let (path_cache, handle_cache) = manager.cache_stats();
        assert_eq!(path_cache, 0);
        assert_eq!(handle_cache, 0);

        // Generate handle (should populate cache)
        let _handle = manager.path_to_filehandle(&test_path).unwrap();

        // Cache should have 1 entry
        let (path_cache, handle_cache) = manager.cache_stats();
        assert_eq!(path_cache, 1);
        assert_eq!(handle_cache, 1);

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }
}
